# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# What goes INTO a guest. Not a script -- source it (scripts/lib/rig.sh
# does):
#   source "$LEA_ROOT/scripts/lib/provision.sh"
#
# ONE implementation of each of these, used by build.sh (the image bake),
# showcase.sh (a running guest) and the gates:
#   shipping        lea_guest_tar / lea_guest_cc / lea_soname_link
#   userspace       lea_payload_stage (compute) / lea_gl_stage (GL, EGL, Vulkan)
#                   lea_gl_audit
#   the dev guest   lea_guest_setup (payload, probes, nvrm_nodes.ko)
#                   lea_libcuda_check (guest libcuda == host libcuda, by hash)
#   guest modules   lea_guest_build_nvrm / lea_guest_build_nvkms
#   the display     lea_display_stage / lea_display_modules / lea_display_x
#                   lea_display_status / lea_display_down / lea_display_up
#   the desktop     lea_desktop_up / lea_desktop_recycle
#
# Every function takes an instance NAME first (see lea_inst in rig.sh) and
# talks to the guest over lea_ssh. Files shipped into the guest come from
# scripts/guest/.

[[ -n ${_LEA_PROVISION_LOADED:-} ]] && return 0
_LEA_PROVISION_LOADED=1

# shellcheck source=scripts/lib/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

LEA_GUEST_FILES=$LEA_ROOT/scripts/guest

# _lea_ip NAME -- the instance's IP, checked reachable.
_lea_ip() {
    lea_inst "$1" || return 1
    lea_ssh "$INST_IP" true 2>/dev/null || { error "$1 ($INST_IP) does not answer over SSH"; return 1; }
    echo "$INST_IP"
}

# ---- shipping ---------------------------------------------------------------
# lea_guest_dns_ok NAME -- can the guest resolve? (quietly)
lea_guest_dns_ok() {
    local ip; ip=$(_lea_ip "$1") || return 1
    lea_ssh "$ip" 'getent hosts archive.ubuntu.com >/dev/null 2>&1'
}

# lea_guest_fix_dns NAME -- find a resolver the GUEST can actually use.
#
# WHY THIS EXISTS RATHER THAN A BETTER GUESS IN THE SEED. The seed's resolvers
# are applied by cloud-init on FIRST BOOT only, so changing them does nothing
# for a disk that already exists -- and a rig that has to be recreated to fix
# its DNS is not a rig most people can use. Worse, the host's own resolver is
# often 127.0.0.53 (systemd-resolved's stub), which resolves perfectly on the
# host and is nothing at all from inside a guest, so "use the host's resolver"
# silently degrades to the hard-coded public pair -- which is exactly what a
# network that blocks 8.8.8.8 refuses.
#
# So: ask the GUEST, which is the only party whose answer settles it, and try
# candidates until one works. Reported 2026-08-21 from a university host where
# NAT, ip_forward and both FORWARD rules were correct and 8.8.8.8 was blocked.
#
# Ordered cheapest-and-most-likely first. Nothing here needs sudo on the host
# and nothing changes host configuration, which is the point.
lea_guest_fix_dns() {
    local name=$1 ip cand c
    ip=$(_lea_ip "$name") || return 1
    lea_guest_dns_ok "$name" && return 0

    local -a cands=()
    # Deliberate word splitting on a comma-separated knob, done with `read -a`
    # rather than silenced with a directive. (A comment line must not BEGIN
    # with the checker's name -- that is parsed as a directive and makes it
    # give up on the whole file, which is the trap check.yml already records.)
    if [[ -n ${LEA_GUEST_DNS:-} ]]; then
        local -a _lea_dns_req
        read -r -a _lea_dns_req <<<"${LEA_GUEST_DNS//,/ }"
        cands+=("${_lea_dns_req[@]}")
    fi
    # The host's upstreams, loopback and IPv6 dropped for the reasons in
    # _lea_guest_dns.
    while read -r c; do cands+=("$c"); done < <(
        { resolvectl dns 2>/dev/null | tr ' ' '\n'
          grep -E '^nameserver' /etc/resolv.conf 2>/dev/null | awk '{print $2}'
        } | grep -E '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$' | grep -v '^127\.')
    # THE DEFAULT-ROUTE GATEWAY. On most LANs -- university ones especially --
    # the router forwards DNS, and it is reachable from the guest through the
    # same NAT the guest already uses. It is also derivable without sudo and
    # without any host configuration, which the public resolvers are not.
    c=$(ip route get 1.1.1.1 2>/dev/null | awk '{for(i=1;i<NF;i++) if($i=="via") print $(i+1); exit}')
    [[ -n $c ]] && cands+=("$c")
    cands+=(8.8.8.8 1.1.1.1)

    local -A seen=()
    for c in "${cands[@]}"; do
        [[ -n $c && -z ${seen[$c]:-} ]] || continue
        seen[$c]=1
        # `options timeout:1 attempts:1` so a dead candidate costs a second,
        # not the five glibc would otherwise spend on it.
        if lea_ssh "$ip" "printf 'nameserver %s\noptions timeout:1 attempts:1\n' $c \
                | sudo tee /etc/resolv.conf >/dev/null
             getent hosts archive.ubuntu.com >/dev/null 2>&1"; then
            info "  $name: resolver $c works from the guest"
            return 0
        fi
    done
    return 1
}

# lea_guest_apt NAME PKG... -- install packages in an Ubuntu guest, once,
# with the three things every bare `apt-get install` here was missing.
#
# WHAT WENT WRONG WITHOUT IT, reported 2026-08-21: provisioning appeared to
# HANG for minutes at "installing build tools and kernel headers" and then
# failed with `apt-get (build-essential, linux-headers) failed` and no reason,
# on a guest whose network was fine.
#
#   1. THE DPKG LOCK. An Ubuntu cloud image runs `apt-daily` and
#      `unattended-upgrades` on first boot, and they hold
#      /var/lib/dpkg/lock-frontend for as long as they take. An apt-get that
#      starts in that window waits, and the wait is invisible. `cloud-init
#      status --wait` lets first boot finish, and `DPkg::Lock::Timeout` makes
#      apt WAIT for the lock with a bound instead of blocking forever or
#      failing immediately -- which one you got depended on timing, and that
#      is exactly why it looked intermittent.
#   2. THE OUTPUT WENT TO /dev/null. Every call redirected stdout AND stderr
#      away, so a failure could not say whether it was DNS, a mirror, a held
#      lock or a missing package. The output goes to the caller now, which is
#      the setup log the error message already points at.
#   3. NO TIMEOUT. A hang had nothing to stop it. `timeout` bounds the whole
#      thing, so a stuck mirror ends as a failure with a message rather than
#      as a session somebody cancels twice.
#
# DEBIAN_FRONTEND=noninteractive so a package that wants to ask something
# fails instead of waiting for a terminal that is not there.
lea_guest_apt() {
    local name=$1; shift
    local ip rc
    ip=$(_lea_ip "$name") || return 1
    [[ $# -gt 0 ]] || return 0
    # PREFLIGHT: can the guest reach the archive AT ALL?
    #
    # Without this the answer to "no route out" is a 900-second timeout, or --
    # worse -- apt's own "Temporary failure resolving" buried in a log, which
    # names DNS and not the reason DNS cannot work. Reported 2026-08-21 by an
    # operator whose guest had an address, answered ssh, and had no NAT in
    # front of it. Two seconds here replaces minutes of waiting with the
    # sentence that identifies the problem AND says where to fix it -- on the
    # HOST, which is the part that is not obvious from inside the guest.
    if ! lea_guest_fix_dns "$name"; then
        error "the guest cannot resolve archive.ubuntu.com, and no resolver worked.
Tried, in order: LEA_GUEST_DNS if set, this host's own upstreams, the default
route's gateway, then 8.8.8.8 and 1.1.1.1.
The guest has an address and answers ssh, so this is NOT the VM: it is
routing on the HOST. Check, in this order:
  scripts/showcase.sh net status        # uplink, NAT and the FORWARD rules
  scripts/showcase.sh net up            # re-add them (needs sudo)
  sysctl net.ipv4.ip_forward            # must be 1
  sudo iptables -S FORWARD | head       # a DROP policy with no ACCEPT for
                                        # 192.168.100.0/24 is the usual cause
                                        # on a host that runs Docker
If 'net up' printed a sudo prompt nobody answered, the rules were never
added and every guest on this bridge is in the same state."
        return 1
    fi
    lea_ssh "$ip" "sudo cloud-init status --wait >/dev/null 2>&1 || true
        export DEBIAN_FRONTEND=noninteractive
        sudo -E timeout ${LEA_APT_TIMEOUT:-900} apt-get -q \
             -o DPkg::Lock::Timeout=${LEA_APT_LOCK_WAIT:-600} update
        sudo -E timeout ${LEA_APT_TIMEOUT:-900} apt-get -q -y \
             -o DPkg::Lock::Timeout=${LEA_APT_LOCK_WAIT:-600} install $*"
    rc=$?
    if [[ $rc -eq 124 ]]; then
        error "apt-get timed out after ${LEA_APT_TIMEOUT:-900}s in the guest.
Usually the guest cannot reach the archive. Check from inside it:
  scripts/showcase.sh ssh --name $name -- 'getent hosts archive.ubuntu.com; ip route'
and on the host that NAT is up: scripts/showcase.sh net status"
        return 1
    fi
    return $rc
}

# lea_guest_tar NAME SRCDIR DESTDIR [tar options and members...]
# tar, NOT scp -r: scp DEREFERENCES symlinks, so a library staged with its
# SONAME and bare-name links would arrive three times as full copies
# (measured: 197 MiB became 573 MiB), and tar lands IN the target rather
# than below it. With no members named, the whole SRCDIR goes.
lea_guest_tar() {
    local name=$1 src=$2 dest=$3 ip; shift 3
    ip=$(_lea_ip "$name") || return 1
    [[ $# -gt 0 ]] || set -- .
    lea_ssh "$ip" "mkdir -p $dest" || return 1
    tar -C "$src" -cf - "$@" | lea_ssh "$ip" "tar -C $dest -xf -"
    # PIPESTATUS: a tar that fails leaves ssh extracting nothing, successfully.
    [[ ${PIPESTATUS[0]} -eq 0 && ${PIPESTATUS[1]} -eq 0 ]]
}

# lea_guest_cc NAME FILE.c [gcc args...] -- ship one probe SOURCE from
# scripts/guest/ and compile it in the guest as ~/<name>. Sources rather
# than binaries: the guest's glibc is not this host's, and a probe that
# needs a matching toolchain is a probe that stops running the day the
# image moves.
lea_guest_cc() {
    local name=$1 file=$2 ip base; shift 2
    base=${file%.c}
    ip=$(_lea_ip "$name") || return 1
    lea_ssh "$ip" "cat > ~/$file" < "$LEA_GUEST_FILES/$file" || return 1
    lea_ssh "$ip" "gcc -O2 -Wall -Wextra -o ~/$base ~/$file $*" \
        || { error "$name: $file did not compile in the guest"; return 1; }
}

# lea_soname_link SRC DESTDIR -- copy one library (dereferenced) and add the
# SONAME symlink and the bare .so name.
#
# WARNING: the SONAME link only when it DIFFERS from the file itself.
# libnvidia-glcore's SONAME *is* libnvidia-glcore.so.<version> -- linking
# that name onto itself replaces the real file with a self-referential
# symlink, and the first symptom is scp reporting "Too many levels of
# symbolic links" after the library is already gone.
lea_soname_link() {
    local src=$1 dest=$2 base soname stem
    base=$(basename "$src")
    cp -L "$src" "$dest/" || return 1
    soname=$(objdump -p "$src" 2>/dev/null | awk '/SONAME/{print $2}')
    [[ -n $soname && $soname != "$base" ]] && ln -sf "$base" "$dest/$soname"
    stem=${base%%.so*}
    [[ $stem != "$base" ]] && ln -sf "$base" "$dest/$stem.so"
    return 0
}

# ---- the NVIDIA userspace ---------------------------------------------------
# lea_payload_stage DIR -- stage NVIDIA's COMPUTE userspace from the host
# into DIR/nv/{lib,bin}. The guest gets no NVIDIA kernel driver -- Leandro
# replaces it -- but it does get exactly the libcuda that matches the HOST
# kernel driver. The version is checked hard against DRIVER_VERSION: a
# mismatched libcuda is not a comfort problem here, it is misread struct
# offsets. The library directory is DISCOVERED (lea_nvidia_libdir).
lea_payload_stage() {
    local dest=$1 want libdir missing=0 l src
    want=$(lea_want_driver)
    libdir=$(lea_nvidia_libdir "$want") || {
        error "no libcuda.so.$want found on this host.
Looked at: LEA_NVIDIA_LIB_DIR, ldconfig, /usr/lib, /usr/lib64,
           /usr/lib/x86_64-linux-gnu, /run/opengl-driver/lib.
The guest is handed the HOST's libcuda -- it cannot be shipped
(LICENSES.md) and it must match the running kernel driver exactly.
Fix: install the matching driver userspace, or set LEA_NVIDIA_LIB_DIR=<dir>"
        return 1
    }
    info "nvidia userspace: $libdir"

    # libnvidia-encode and libnvcuvid joined the list on 2026-08-06, once
    # NVENC and NVDEC actually ran in a guest. They belong here for the same
    # reason libcuda does: they are the HOST driver's userspace and must
    # match DRIVER_VERSION exactly. Without them the `encode` gate stage has
    # nothing to run, and a guest that can compute still cannot encode.
    local -a libs=(libcuda libnvidia-ml libnvidia-cfg libnvidia-nvvm libnvidia-ptxjitcompiler
                   libnvidia-encode libnvcuvid)
    # What libcuda DLOPENS on top of what it links. `objdump -p` on libcuda
    # names no NVIDIA dependency at all -- every one of these is opened by
    # name at runtime, so a missing one is a silently absent feature and
    # never a link error. Read out of the binary itself (2026-08-18):
    #     strings libcuda.so.$want | grep -oE 'libnvidia-[a-z0-9-]+\.so[.0-9]*'
    # Optional on purpose: absence is named and staging continues. They are
    # features (JIT fallback, tiled raster, PKCS#11 crypto), not the CUDA
    # core, and a host packaging them differently must not fail the payload.
    # The audit CONVERGES rather than terminating in one pass: each library
    # staged brings its own dlopen names with it (lea_gl_audit).
    local -a optional=(libnvidia-tileiras libnvidia-nvvm70 libnvidia-pkcs11
                       libnvidia-pkcs11-openssl3 libcudadebugger
                       libnvidia-opencl libnvidia-vksc-core
                       # NVOFA's userspace: the 64-bit half of a library the
                       # 32-bit set already staged, and the same oversight as
                       # the GLES pair above. No workload for it is
                       # procurable in this environment (probe nvofa), so it
                       # is staged without being exercised -- the absence was
                       # measurable, the presence is not.
                       libnvidia-opticalflow)
    mkdir -p "$dest/nv/lib" "$dest/nv/bin"
    for l in "${libs[@]}"; do
        src="$libdir/$l.so.$want"
        [[ -f $src ]] || { echo "missing: $src"; missing=1; continue; }
        lea_soname_link "$src" "$dest/nv/lib"
    done
    for l in "${optional[@]}"; do
        src="$libdir/$l.so.$want"
        # nvvm70 carries a bare SONAME version, not the driver version.
        [[ -f $src ]] || src=$(ls "$libdir/$l.so."* 2>/dev/null | head -1)
        [[ -n $src && -f $src ]] || { echo "optional, absent on this host: $l"; continue; }
        lea_soname_link "$src" "$dest/nv/lib"
    done
    if src=$(lea_nvidia_bin nvidia-smi); then
        # -L: on NixOS this is a symlink into the store, and the guest has
        # no store to follow it into.
        cp -L "$src" "$dest/nv/bin/"
    else
        echo "missing: nvidia-smi (not in PATH, /usr/bin or /run/current-system/sw/bin)"
        missing=1
    fi
    if [[ $missing -ne 0 ]]; then
        error "NVIDIA userspace $want is incomplete in $libdir. Fix: install the matching nvidia-utils (see DRIVER_VERSION)."
        return 1
    fi
    info "OK  $dest/nv ($want, $(du -sh "$dest/nv" | cut -f1))"
}

# lea_gl_stage NAME [--dest DIR] [--check] [--system] [--with-32bit]
# Stage NVIDIA's OpenGL/EGL/Vulkan userspace into a RUNNING guest, in a
# directory of its own (/opt/nvrm-gl, removable with rm -rf). The compute
# payload stays byte for byte what it was, because that is the path the
# GPU gate walks.
#
# WHAT IT STAGES, and every entry was measured rather than guessed --
# `strace -f -y -e openat` on `eglinfo` with the NVIDIA vendor forced and
# /dev/dri replaced by an empty tmpfs (2026-08-06): entry points
# libEGL_nvidia/libGLX_nvidia, their ldd closure, and what they dlopen.
# The GLVND JSON files matter as much as the libraries: without
# 10_nvidia.json, libEGL picks Mesa and every measurement measures Mesa.
# They are REWRITTEN to point at the staged path, not copied.
#
# --system wires it in the way a real driver install does, and no more:
# ld.so.conf.d, the vendor manifests beside Mesa's, and /usr/local/bin/
# nvidia-run (PRIME render offload for ONE program -- the two offload
# variables set globally would put the compositor on NVIDIA too).
lea_gl_stage() {
    local name=$1; shift
    local dest=/opt/nvrm-gl check=0 system=0 bits32=0 ip want libdir
    while [[ $# -gt 0 ]]; do
        case $1 in
            --dest)   dest=$2; shift 2 ;;
            --check)  check=1; shift ;;
            --system) system=1; shift ;;
            --with-32bit) bits32=1; shift ;;
            *) die "lea_gl_stage: unknown option $1" ;;
        esac
    done
    ip=$(_lea_ip "$name") || return 1
    want=$(lea_want_driver)
    libdir=$(lea_nvidia_libdir "$want") || { error "no libcuda.so.$want on this host"; return 1; }

    if [[ $check -eq 1 ]]; then
        info "staged in $name:$dest"
        lea_ssh "$ip" "ls -l $dest/lib 2>/dev/null | tail -n +2 | wc -l; ls $dest 2>/dev/null"
        return 0
    fi

    # Driver-versioned: they must match DRIVER_VERSION exactly, for the same
    # reason libcuda does.
    local -a versioned=(libEGL_nvidia libGLX_nvidia libnvidia-eglcore libnvidia-glcore
        libnvidia-glsi libnvidia-gpucomp libnvidia-tls libnvidia-allocator libnvidia-glvkspirv
        # libnvidia-rtcore is dlopened the moment a client enables
        # VK_KHR_acceleration_structure -- right after the 4 GiB VA
        # reservation, and no earlier. No trace before 2026-08-15 ever did
        # (glxgears, CUDA, vkcube), and CS2 -- which enables the extension
        # whenever the driver offers it -- said "Failed to initialize Vulkan"
        # (OPEN-QUESTIONS nr 11). Found with strace in the guest, not with
        # any RM trace: an ENOENT is not an ioctl.
        libnvidia-rtcore
        # NvFBC, NVIDIA's own frame capture. Sunshine looks for it BY NAME and
        # logs its absence, so it belongs in the set even though NvFBC is
        # restricted on GeForce -- an absent library and a refused one are
        # different findings, and only one of them is ours.
        libnvidia-fbc
        # The GLES vendor libraries. They were staged for 32-bit clients and
        # for nobody else, so a 64-bit client that links the vendor SONAME
        # found nothing (matrix TASKS-610.57.04, task 3) -- an oversight, and
        # this is the fix. Measured 2026-08-20: no GLVND client reaches them.
        # `es2_info` opens libGLESv2.so.2, lands on libEGL_nvidia and from
        # there on eglcore/glsi, and no driver library names libGLESv*_nvidia
        # in its dlopen strings at all. They are the pre-GLVND direct-link
        # ABI, which is who they are staged for -- and why no probe covers
        # them.
        libGLESv1_CM_nvidia libGLESv2_nvidia)
    # Independently versioned: they come from egl-wayland / egl-gbm, not
    # from the driver package, and their SONAME is .so.1.
    local -a loose=(libnvidia-egl-gbm.so.1 libnvidia-egl-wayland.so.1
        libnvidia-egl-wayland2.so.1 libnvidia-egl-xcb.so.1 libnvidia-egl-xlib.so.1)
    # The 32-bit half. Most Steam titles are 32-bit or drag 32-bit
    # dependencies, and without these they land on llvmpipe -- silently,
    # because a missing 32-bit libGLX_nvidia is not an error, it is a
    # fallback. Same version rule as the 64-bit set.
    local -a versioned32=(libGLX_nvidia libEGL_nvidia libnvidia-glcore libnvidia-eglcore
        libnvidia-glsi libnvidia-tls libnvidia-gpucomp libnvidia-allocator libnvidia-glvkspirv libcuda
        # The JIT chain 32-bit libcuda DLOPENS. Not NEEDED entries -- found by
        # reading the dlopen names out of the binary (2026-08-18). Shipping
        # libcuda without them means a 32-bit CUDA client cannot JIT, silently.
        libnvidia-nvvm libnvidia-ptxjitcompiler libnvidia-tileiras)
    # Worth having, must not break a host that lacks it. libnvidia-rtcore is
    # deliberately NOT here: it has no 32-bit build in the driver package.
    local -a optional32=(libnvidia-fbc libnvidia-encode libnvcuvid libnvidia-ml
        libnvidia-opticalflow libGLESv2_nvidia libGLESv1_CM_nvidia)

    local stage missing=0 l src p f lib
    stage=$(mktemp -d)
    # shellcheck disable=SC2064
    trap "rm -rf '$stage'" RETURN
    mkdir -p "$stage/lib" "$stage/egl_vendor.d" "$stage/egl_external_platform.d" "$stage/vulkan_icd.d"
    for l in "${versioned[@]}"; do
        src="$libdir/$l.so.$want"
        [[ -f $src ]] || { echo "missing: $src"; missing=1; continue; }
        lea_soname_link "$src" "$stage/lib"
    done
    for l in "${loose[@]}"; do
        src=$libdir/$l
        [[ -e $src ]] || { echo "missing: $src"; missing=1; continue; }
        cp -L "$src" "$stage/lib/$l"
    done
    [[ $missing -eq 0 ]] || { error "NVIDIA GL userspace $want is incomplete in $libdir"; return 1; }

    # The vendor JSON, rewritten rather than copied: the host path is not the
    # guest path, and a JSON pointing at a library that is not there makes
    # libEGL fall back to Mesa SILENTLY.
    #
    # WARNING: `__EGL_VENDOR_LIBRARY_DIRS` REPLACES the search path, it does
    # not add to it. So the guest's own manifests are copied in beside
    # NVIDIA's, and the directory is a SUPERSET rather than a replacement.
    cat > "$stage/egl_vendor.d/10_nvidia.json" <<JSON
{
    "file_format_version" : "1.0.0",
    "ICD" : {
        "library_path" : "$dest/lib/libEGL_nvidia.so.0"
    }
}
JSON
    for p in gbm wayland wayland2 xcb xlib; do
        case $p in
            gbm)      f=15_nvidia_gbm.json;      lib=libnvidia-egl-gbm.so.1 ;;
            wayland)  f=10_nvidia_wayland.json;  lib=libnvidia-egl-wayland.so.1 ;;
            wayland2) f=09_nvidia_wayland2.json; lib=libnvidia-egl-wayland2.so.1 ;;
            xcb)      f=20_nvidia_xcb.json;      lib=libnvidia-egl-xcb.so.1 ;;
            xlib)     f=20_nvidia_xlib.json;     lib=libnvidia-egl-xlib.so.1 ;;
        esac
        cat > "$stage/egl_external_platform.d/$f" <<JSON
{
    "file_format_version" : "1.0.0",
    "ICD" : {
        "library_path" : "$dest/lib/$lib"
    }
}
JSON
    done
    # The Vulkan ICD is the SAME library -- nvidia_icd.json points at
    # libGLX_nvidia.so.0. The library_path is RELATIVE on purpose: an
    # absolute $dest path works in the plain guest and breaks inside Steam's
    # pressure-vessel sandbox, where $dest does not exist and the loader
    # skips the ICD silently -- measured 2026-08-16 as CS2 saying "Failed to
    # initialize Vulkan" while vulkaninfo in the session lists the RTX 2070.
    local api
    api=$(python3 -c "
import json
try:
    print(json.load(open('/usr/share/vulkan/icd.d/nvidia_icd.json'))['ICD']['api_version'])
except Exception:
    print('1.3.0')" 2>/dev/null || echo 1.3.0)
    cat > "$stage/vulkan_icd.d/nvidia_icd.json" <<JSON
{
    "file_format_version" : "1.0.1",
    "ICD": {
        "library_path": "libGLX_nvidia.so.0",
        "api_version" : "$api"
    }
}
JSON
    cat > "$stage/env.sh" <<ENV
# source this before an NVIDIA GL/EGL run in the guest
export LD_LIBRARY_PATH=$dest/lib\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}
export __EGL_VENDOR_LIBRARY_DIRS=$dest/egl_vendor.d
export __EGL_EXTERNAL_PLATFORM_CONFIG_DIRS=$dest/egl_external_platform.d
# NVIDIA is ADDED to the Vulkan ICD list, never put in place of the guest's
# own. Use VK_DRIVER_FILES (not the deprecated VK_ICD_FILENAMES) to pin one.
# WARNING: unset both display variables. NVIDIA's EGL only reaches a GL
# context without a DRM node on the SURFACELESS platform.
ENV
    # Does the guest ALREADY have an NVIDIA Vulkan manifest? Ubuntu's image
    # ships one naming `libGLX_nvidia.so.0` by bare name, which the staged
    # LD_LIBRARY_PATH resolves. Adding ours on top makes the loader find the
    # same ICD twice and vulkaninfo report the card TWICE (measured).
    if lea_ssh "$ip" "grep -lq libGLX_nvidia /usr/share/vulkan/icd.d/*.json 2>/dev/null"; then
        rm -f "$stage/vulkan_icd.d/nvidia_icd.json"
    else
        echo "export VK_ADD_DRIVER_FILES=$dest/vulkan_icd.d/nvidia_icd.json" >> "$stage/env.sh"
    fi

    if [[ $bits32 -eq 1 ]]; then
        mkdir -p "$stage/lib32"
        local m32=0
        for l in "${versioned32[@]}"; do
            src="$LEA_NVIDIA_LIB32_DIR/$l.so.$want"
            [[ -f $src ]] || { echo "missing (32 bit): $src"; m32=1; continue; }
            lea_soname_link "$src" "$stage/lib32"
        done
        [[ $m32 -eq 0 ]] || { error "32-bit NVIDIA userspace $want is incomplete in $LEA_NVIDIA_LIB32_DIR
       On Arch that is lib32-nvidia-utils; LEA_NVIDIA_LIB32_DIR overrides."; return 1; }
        for l in "${optional32[@]}"; do
            src="$LEA_NVIDIA_LIB32_DIR/$l.so.$want"
            [[ -f $src ]] || { echo "   (32 bit, optional) absent: $l"; continue; }
            lea_soname_link "$src" "$stage/lib32"
        done
        info "32-bit set staged ($(du -sh "$stage/lib32" | cut -f1))"
    fi

    # The guest's existing EGL vendors, so the staged directory holds them too.
    for f in $(lea_ssh "$ip" "ls /usr/share/glvnd/egl_vendor.d/ 2>/dev/null" 2>/dev/null); do
        [[ $f == *nvidia* ]] && continue      # ours replaces theirs
        lea_ssh "$ip" "cat /usr/share/glvnd/egl_vendor.d/$f" > "$stage/egl_vendor.d/$f" 2>/dev/null
    done
    info "vendor manifests staged: $(ls "$stage/egl_vendor.d" | tr '\n' ' ')"
    info "staging $(du -sh "$stage" | cut -f1) -> $name:$dest"
    lea_ssh "$ip" "sudo rm -rf $dest && sudo mkdir -p $dest && sudo chown $LEA_GUEST_USER $dest" \
        || { error "cannot create $dest in the guest"; return 1; }
    lea_guest_tar "$name" "$stage" "$dest" || { error "copy failed"; return 1; }
    info "staged:"
    lea_ssh "$ip" "ls $dest/lib | wc -l | xargs echo '  libraries:'
        echo '  vendor json:' \$(ls $dest/egl_vendor.d)
        echo '  platform json:' \$(ls $dest/egl_external_platform.d | tr '\n' ' ')"
    info "use: source $dest/env.sh"

    [[ $system -eq 1 ]] || return 0
    info "wiring into the system (ld.so.conf.d, egl_vendor.d, nvidia-run)"
    lea_ssh "$ip" "set -e
        { echo '$dest/lib'; [ -d $dest/lib32 ] && echo '$dest/lib32'; } \
            | sudo tee /etc/ld.so.conf.d/nvrm-gl.conf >/dev/null
        sudo ldconfig
        sudo install -d /usr/share/glvnd/egl_vendor.d /usr/share/egl/egl_external_platform.d
        sudo cp $dest/egl_vendor.d/10_nvidia.json /usr/share/glvnd/egl_vendor.d/
        sudo cp $dest/egl_external_platform.d/*.json /usr/share/egl/egl_external_platform.d/
        if [ -f $dest/vulkan_icd.d/nvidia_icd.json ]; then
            sudo install -d /usr/share/vulkan/icd.d
            sudo cp $dest/vulkan_icd.d/nvidia_icd.json /usr/share/vulkan/icd.d/
        fi
        sudo tee /usr/local/bin/nvidia-run >/dev/null <<'RUN'
#!/bin/sh
# Run ONE program on the NVIDIA card.
#   nvidia-run glxgears
#   nvidia-run glmark2
#
# ONE variable, and it is the one that does something here:
# __GLX_VENDOR_LIBRARY_NAME picks NVIDIA's GLX vendor through GLVND.
#
# __NV_PRIME_RENDER_OFFLOAD and __VK_LAYER_NV_optimus USED TO BE HERE and are
# gone, because they were measured inert (OPEN-QUESTIONS number 57). The first
# is the enable_environment of VK_LAYER_NV_optimus; the second configures that
# layer. The layer is not registered in this guest -- and registering it
# changes nothing either: measured 2026-08-21, vulkaninfo --summary in a guest
# gave 893 ioctls and 111 signatures with the layer absent, with it registered,
# and with it registered AND __NV_PRIME_RENDER_OFFLOAD=1 set. The signature
# sets were byte-identical in all three.
#
# A wrapper whose name promises offload and whose variables do nothing is worse
# than a shorter one: it invites the reader to believe a mechanism is in play.
exec env __GLX_VENDOR_LIBRARY_NAME=nvidia \"\$@\"
RUN
        sudo chmod +x /usr/local/bin/nvidia-run
        ldconfig -p | grep -c libEGL_nvidia >/dev/null && echo '  libEGL_nvidia is on the loader path'"
    info "  nvidia-run <program> runs that program on the card"
}

# lea_gl_audit NAME -- which NVIDIA userspace names does the guest REFERENCE
# but not resolve? `ldd` is not enough: NVIDIA's libraries name most of
# their siblings at RUNTIME, not in DT_NEEDED, so a missing one is never a
# link error -- it is a feature that silently is not there (OPEN-QUESTIONS
# 11: libnvidia-rtcore, found with strace and not with any RM trace).
lea_gl_audit() {
    local ip; ip=$(_lea_ip "$1") || return 1
    lea_ssh "$ip" 'bash -s' <<'REMOTE'
audit() {
    local label=$1 bits=$2; shift 2
    local dirs=("$@") path
    if [[ $bits == 64 ]]; then
        path="/opt/nvrm/lib /opt/nvrm-gl/lib /usr/lib/x86_64-linux-gnu /usr/lib /lib/x86_64-linux-gnu"
    else
        path="/opt/nvrm-gl/lib32 /usr/lib/i386-linux-gnu /usr/lib32"
    fi
    lea_head "$label"
    local refs=""
    for d in "${dirs[@]}"; do
        [[ -d $d ]] || continue
        for f in "$d"/*.so.*; do
            [[ -f $f ]] || continue
            refs+=$'\n'$(objdump -p "$f" 2>/dev/null | awk '/NEEDED/{print $2}')
            refs+=$'\n'$(strings "$f" 2>/dev/null \
                | grep -oE '^lib(nvidia|cuda|nvcuvid)[a-zA-Z0-9._-]*\.so\.[0-9][0-9.]*$')
        done
    done
    local miss=0
    for r in $(printf '%s\n' "$refs" | grep -iE 'nvidia|cuda|nvcuvid' | sort -u); do
        local found=""
        for d in $path; do [[ -e $d/$r ]] && { found=$d; break; }; done
        [[ -n $found ]] || { echo "   MISSING: $r"; miss=$((miss+1)); }
    done
    [[ $miss -eq 0 ]] && echo "   all referenced NVIDIA names resolve"
    return 0
}
audit "64-bit (compute + GL payload)" 64 /opt/nvrm/lib /opt/nvrm-gl/lib
audit "32-bit (GL payload)"           32 /opt/nvrm-gl/lib32
REMOTE
}

# ---- is the guest's libcuda the host's libcuda? ------------------------------
# lea_libcuda_check NAME -- hash the libcuda that the GUEST actually loads
# against the one the HOST actually loads, and print both.
#
# WHY A HASH AND NOT A VERSION. "610.43.03 == 610.43.03" is the check this
# project already fails at: the version string says which ABI was intended,
# not which bytes are there. A repackaged, patched or half-copied library
# reports the same version and misreads struct offsets, and misread offsets
# are silent -- the ioctl succeeds and the numbers are wrong. So the claim
# being made is the strong one, "the same build", and it is measured.
#
# WHAT IS HASHED is not a path somebody picked. dlopen("libcuda.so.1") is
# performed and the file the LOADER MAPPED is read back out of
# /proc/self/maps -- on both sides, with the same program text. A search path
# that resolves to a different file than the one that was staged is exactly
# the failure this exists to catch, and asking a path would hide it.
_LEA_LIBCUDA_PY='
import ctypes, hashlib, sys
try:
    ctypes.CDLL("libcuda.so.1")
except OSError as e:
    sys.exit("dlopen(libcuda.so.1) failed: %s" % e)
path = None
for line in open("/proc/self/maps"):
    f = line.rstrip("\n").split(" ", 5)[-1].strip()
    if "/libcuda.so." in f:
        path = f
        break
if path is None:
    sys.exit("libcuda.so.1 opened but not mapped from a file")
h = hashlib.sha256(open(path, "rb").read()).hexdigest()
print("%s %s" % (h, path))
'
lea_libcuda_check() {
    local name=$1 ip host_out guest_out host_sum guest_sum host_path guest_path
    ip=$(_lea_ip "$name") || return 1
    host_out=$(python3 -c "$_LEA_LIBCUDA_PY" 2>&1)         || { error "host: $host_out"; return 1; }
    guest_out=$(lea_ssh "$ip" "python3 -c '$_LEA_LIBCUDA_PY'" 2>&1)         || { error "$name: $guest_out"; return 1; }
    host_sum=${host_out%% *};  host_path=${host_out#* }
    guest_sum=${guest_out%% *}; guest_path=${guest_out#* }
    echo "libcuda host:  $host_sum  $host_path"
    echo "libcuda guest: $guest_sum  $guest_path"
    if [[ $host_sum == "$guest_sum" ]]; then
        info "  libcuda: guest and host load the same build ($host_sum)"
        return 0
    fi
    error "$name: the guest's libcuda is NOT the host's build.
       host  $host_sum  $host_path
       guest $guest_sum  $guest_path
       The guest is handed the HOST's libcuda for a reason: the ioctl structs
       have no ABI stability guarantee, so two builds of one version number
       still misread each other's offsets -- silently. Re-stage the payload
       (showcase.sh up), or fix where the guest's userspace comes from
       (services.leandro-guest.nvidiaUserspaceDir on a NixOS guest)."
    return 1
}

# ---- the dev guest ----------------------------------------------------------
# lea_games_mount NAME IP -- mount the Steam library disk, if this instance
# was given one, and point every Steam root at it.
#
# Only when `vm/<name>/games` says so. A guest that was started without
# --games is not touched at all, and that guard is the important one: this
# function FORMATS a disk, and "find the empty one" is a rule that a root
# disk with a partition table also satisfies (`lsblk` reports no FSTYPE for
# it, because the filesystems are on its partitions). The candidate must
# have no partitions, no filesystem AND no mountpoint, all three.
#
# WHERE IT GOES, and this is the part that was wrong first: NOT at one
# hard-coded Steam path. Ubuntu's Steam package keeps its root at
# ~/.steam/debian-installation (with ~/.steam/steam a symlink to it) and
# never looks at ~/.local/share/Steam, so a disk mounted there stayed empty
# at 186 GiB free while the guest's 38 GiB root disk filled up with
# Proton, the Steam runtime and a game (measured 2026-08-20). The disk is
# therefore mounted at a neutral /games and every Steam root gets its
# `steamapps` pointed there by symlink -- including roots that do not exist
# yet, so the first start of Steam already lands on the disk.
#
# A steamapps directory that already HAS something in it is never touched.
# Moving a live library is the operator's call, and the message says so.
lea_games_mount() {
    local name=$1 ip=$2
    [[ -f $(lea_inst_dir "$name")/games ]] || return 0
    lea_ssh "$ip" "LABEL='$LEA_GAMES_LABEL' bash -s" <<'REMOTE'
set -u
mnt=/games

dev=$(lsblk -ndo NAME,LABEL,TYPE | awk -v l="$LABEL" '$3=="disk" && $2==l {print $1; exit}')
if [ -z "$dev" ]; then
    for d in $(lsblk -ndo NAME,TYPE | awk '$2=="disk" {print $1}'); do
        [ -z "$(lsblk -no NAME "/dev/$d" | tail -n +2)" ] || continue   # has partitions
        [ -z "$(lsblk -no FSTYPE "/dev/$d" | tr -d ' \n')" ] || continue
        [ -z "$(lsblk -no MOUNTPOINT "/dev/$d" | tr -d ' \n')" ] || continue
        dev=$d; break
    done
    [ -n "$dev" ] || { echo "no unformatted disk to use as the games library"; exit 1; }
    echo "formatting /dev/$dev as the games library (label $LABEL)"
    sudo mkfs.ext4 -q -L "$LABEL" "/dev/$dev" || exit 1
fi

sudo mkdir -p "$mnt"
# REPLACE any earlier entry for this label rather than adding a second one:
# an older line may name a different mountpoint, and then `mount $mnt` finds
# nothing in fstab while the disk sits mounted somewhere nobody looks.
# "Empty" ignores lost+found: mkfs.ext4 makes it, and a freshly formatted
# disk would otherwise count as carrying a library.
has_content() { [ -n "$(ls -A "$1" 2>/dev/null | grep -v '^lost+found$')" ]; }

old_mnt=$(awk -v l="LABEL=$LABEL" '$1==l {print $2}' /etc/fstab 2>/dev/null | head -1)
if [ -n "$old_mnt" ] && [ "$old_mnt" != "$mnt" ]; then
    if has_content "$old_mnt"; then
        echo "NOTE: the library is mounted at $old_mnt and is not empty."
        echo "      Leaving it there; move it by hand if you want it at $mnt."
        mnt=$old_mnt
    else
        mountpoint -q "$old_mnt" && sudo umount "$old_mnt"
        sudo sed -i "\|^LABEL=$LABEL |d" /etc/fstab
        old_mnt=""
    fi
fi
grep -q "^LABEL=$LABEL $mnt " /etc/fstab 2>/dev/null || \
    echo "LABEL=$LABEL $mnt ext4 defaults,nofail,x-systemd.device-timeout=10 0 2" \
    | sudo tee -a /etc/fstab >/dev/null
mountpoint -q "$mnt" || sudo mount "$mnt" || exit 1
sudo chown "$(id -u):$(id -g)" "$mnt"

# Every Steam root this guest might use, existing or not.
for root in "$HOME/.steam/debian-installation" "$HOME/.local/share/Steam" \
            "$HOME/.steam/root" "$HOME/.var/app/com.valvesoftware.Steam/data/Steam"; do
    sa=$root/steamapps
    [ "$sa" = "$mnt" ] && continue                  # the mount itself
    if [ -L "$sa" ]; then
        continue                                    # already pointed somewhere
    elif [ -d "$sa" ] && has_content "$sa"; then
        echo "NOTE: $sa has content and was left alone."
        echo "      To move it onto the library disk, with Steam CLOSED:"
        echo "        mv $sa/* $mnt/ && rmdir $sa && ln -s $mnt $sa"
        continue
    fi
    rmdir "$sa" 2>/dev/null
    mkdir -p "$root"
    ln -s "$mnt" "$sa" 2>/dev/null && echo "steamapps -> $mnt  ($root)"
done
echo "games library: $(df -h "$mnt" | awk 'NR==2 {print $2" total, "$4" free"}') on /dev/$dev"
REMOTE
}

# lea_guest_shell_env IP -- let an INTERACTIVE shell in the guest run the
# probes without three exports first.
#
# `showcase.sh ssh <guest>` used to drop you in $HOME with nothing set, and
# every probe then died on a libcuda it could not find or a PTX it was never
# told about -- while the gates ran the same probes happily, because each of
# them exports LD_LIBRARY_PATH, NVPROBE_PTX and the venv's python inside its
# own command string. The environment existed; it just was not reachable by
# a person.
#
# In .bashrc, and that placement is the whole safety argument: bash reads it
# for interactive shells only. Every gate, bench and script in this tree
# reaches the guest as `ssh host command`, which reads neither .bashrc nor
# .profile, so nothing that is measured can be moved by what is convenient
# here. /etc/profile.d would have been the other candidate and is the wrong
# one -- a gdm session reads it too, and the desktop guest's NVIDIA loader
# wiring (ld.so.conf.d, lea_guest_setup above) would then have a second
# opinion about where libcuda lives.
#
# Idempotent: the block is delimited and replaced, never appended twice.
lea_guest_shell_env() {
    local ip=$1
    lea_ssh "$ip" 'set -e
        f=$HOME/.bashrc
        touch "$f"
        sed -i "/^# >>> leandro >>>$/,/^# <<< leandro <<</d" "$f"
        cat >> "$f" <<'"'"'EOF'"'"'
# >>> leandro >>>
# Interactive shells only -- lea_guest_shell_env in scripts/lib/provision.sh
# says why. `ssh <guest> <command>` does not read this file.
if [ -d "$HOME/gpu" ]; then
    export LD_LIBRARY_PATH="$HOME/gpu/nv/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    export NVPROBE_PTX="$HOME/gpu/kernels.ptx"
    # The venv FIRST, so that the probes\047 `#!/usr/bin/env python3` finds the
    # torch that the gate compares against, not the system one without it.
    [ -x "$HOME/gpu/venv/bin/python" ] && PATH="$HOME/gpu/venv/bin:$PATH"
    PATH="$PATH:$HOME/gpu"
    export PATH
fi
# <<< leandro <<<
EOF'
}

# lea_guest_setup NAME [--with-torch] -- make a running guest ready for CUDA:
# userspace, the probes and the helper module (nvrm_nodes.ko). The GPU path
# itself is brought in by virtio_nvrm (lea_guest_build_nvrm). Idempotent:
# the payload is synced into ~/gpu when it changed, the torch venv is left
# alone; --with-torch creates it if missing (downloads ~2.5 GiB).
#
# ONE function for BOTH guests, and that is deliberate rather than accidental.
# What reaches the guest -- the NVIDIA userspace, the probe binaries, the
# probe sources, params.txt, the manifest that decides whether any of it has
# to be re-sent -- is identical, because the gate compares a guest run
# against a NATIVE host run of THE SAME binaries. Three things genuinely
# differ, and each is a `case $os` below with its reason on the spot:
#   the loader wiring   Ubuntu has /etc/ld.so.conf.d and NixOS has no FHS
#                       ld.so.conf at all
#   nvrm_nodes.ko       built in the Ubuntu guest from shipped sources,
#                       already in the NixOS image (boot.extraModulePackages)
#   the missing tools   apt-get on Ubuntu; on NixOS they are in the image or
#                       they are a bug in nix/guest-image.nix
# What does NOT differ is the payload path, and that is why the probes are
# host-built ELF on both and why the image carries nix-ld.
lea_guest_setup() {
    local name=$1; shift
    local torch=0 ip stage
    while [[ $# -gt 0 ]]; do
        case $1 in
            --with-torch) torch=1; shift ;;
            *) die "lea_guest_setup: unknown option $1" ;;
        esac
    done
    ip=$(_lea_ip "$name") || return 1
    # NAME-BASED, not INST_GUEST. _lea_ip calls lea_inst inside a command
    # substitution, so lea_inst's INST_* assignments land in a subshell and
    # are gone by the time this line runs -- reading them here would pick up
    # whatever the CALLER happened to leave in the globals, or abort under
    # `set -u` when the caller never called lea_inst at all. That is what
    # lea_guest_os and lea_transport_of exist for.
    local os; os=$(lea_guest_os "$name")
    local tr; tr=$(lea_transport_of "$name")

    # Build the probes the gate greps before copying them: a stale prebuilt
    # binary with outdated output strings makes the gate fail (or pass) on
    # text that no longer exists in the sources.
    #
    # UNLESS THEY WERE BUILT ALREADY, by build.sh package, from the very tree
    # that is packaged beside them. Then LEA_PROBE_BIN points into the package
    # and LEA_ROOT is read-only, so `make` could not write probe/bin even if
    # it had anything to do. The invariant the Makefile's warning protects --
    # binaries that match the sources next to them -- is kept by the package
    # being built in one step and recording its commit, not by rebuilding on a
    # compute node that has no CUDA headers to rebuild with.
    if [[ $LEA_PROBE_BIN == "$LEA_ROOT/probe/bin" ]]; then
        make -C "$LEA_ROOT/probe" all-probes >/dev/null || { error "make -C probe failed"; return 1; }
    fi
    [[ -x $LEA_PROBE_BIN/nvprobe ]] || {
        error "$LEA_PROBE_BIN/nvprobe missing.
       In a checkout that is 'make -C probe all-probes'; from a package it
       means the package was built without the probes (build.sh package)."
        return 1; }

    # Assemble the payload: NVIDIA userspace + probes.
    stage=$(mktemp -d)
    # shellcheck disable=SC2064
    trap "rm -rf '$stage'" RETURN
    lea_payload_stage "$stage" >/dev/null || return 1
    # The guest keeps a FLAT ~/gpu: the repo side is sorted, the guest side
    # is not, because the gate and the probes address each other by bare
    # name there.
    local p
    cp "$LEA_PROBE_BIN"/nvprobe "$LEA_ROOT"/probe/kernels/kernels.ptx "$stage"/
    for p in hostregprobe oomprobe managedprobe ioctlping ctrlping; do
        [[ -x $LEA_PROBE_BIN/$p ]] && cp "$LEA_PROBE_BIN/$p" "$stage"/
    done
    # mmapping is a Rust binary, not in probe/ -- it uses the proven map path
    # from nvrm-client instead of transcribing the NVOS33 constants a second
    # time in C. smipids asks the two controls nvidia-smi uses for its
    # process list; the same binary runs on the host, so the two answers are
    # comparable without a second implementation.
    for p in mmapping smipids; do
        [[ -x $LEA_BIN_DIR/$p ]] && cp "$LEA_BIN_DIR/$p" "$stage"/
    done
    cp "$LEA_ROOT"/probe/python/{rlprobe,torchprobe,convburn,convoom,mmsweep,pinwin,streamprobe,vramcap}.py \
       "$stage"/ 2>/dev/null || true
    cp "$LEA_ROOT"/probe/suites/test_vram_churn.py "$stage"/ 2>/dev/null || true
    cp "$LEA_GUEST_FILES/nvrm-setup.sh" "$stage"/
    cat /proc/driver/nvidia/params > "$stage/params.txt"

    # Sync into the guest -- but only when the payload CHANGED. The payload
    # is ~220 MB and every earlier caller re-sent it on every run, baked
    # image or not. A manifest (names, sizes, mtimes of everything staged)
    # is cheap to compare and honest about what would arrive.
    local manifest have
    manifest=$(cd "$stage" && find . -type f -printf '%p %s %T@\n' | sort | sha256sum | cut -d' ' -f1)
    have=$(lea_ssh "$ip" 'cat ~/gpu/.manifest 2>/dev/null' | tr -d '[:space:]')
    if [[ $have == "$manifest" ]]; then
        info "  payload unchanged (manifest $manifest) -- not re-sent"
    else
        lea_guest_tar "$name" "$stage" '$HOME/gpu' || return 1
        lea_ssh "$ip" "chmod +x ~/gpu/nvrm-setup.sh; echo $manifest > ~/gpu/.manifest"
    fi

    # Put the libraries on the LOADER's search path. Without this the payload
    # lands in ~/gpu/nv/lib and is found by nothing, so every single command
    # needs LD_LIBRARY_PATH -- and `./nv/bin/nvidia-smi` fails with "couldn't
    # find libnvidia-ml.so", which reads like a broken driver rather than an
    # unset variable. /opt/nvrm/{lib,bin} is the same directory on both
    # guests; only the way the loader is told about it differs, and each way
    # is CHECKED BACK rather than assumed.
    case $os in
        ubuntu)
            # Symlinks rather than copies; and ld.so.conf.d, NOT
            # LD_LIBRARY_PATH, which is lost across sudo, su and systemd
            # units -- which is exactly where it is missed.
            lea_ssh "$ip" 'set -e
                sudo mkdir -p /opt/nvrm/lib /opt/nvrm/bin
                sudo ln -sfn "$HOME"/gpu/nv/lib/*.so* /opt/nvrm/lib/
                sudo ln -sfn "$HOME"/gpu/nv/bin/nvidia-smi /opt/nvrm/bin/nvidia-smi
                sudo ln -sfn /opt/nvrm/bin/nvidia-smi /usr/local/bin/nvidia-smi
                echo /opt/nvrm/lib | sudo tee /etc/ld.so.conf.d/nvrm.conf >/dev/null
                sudo ldconfig
                ldconfig -p | grep -q "libcuda.so.1" \
                    || { echo "ERROR: libcuda still not on the loader search path"; exit 1; }' || return 1
            ;;
        nixos)
            # THE REAL DIFFERENCE, and it is not a detail. NixOS has no FHS
            # /etc/ld.so.conf and its ldconfig cache is not what the store's
            # ld.so consults, so there is nothing to write a .conf into. The
            # image instead puts /opt/nvrm/lib into LD_LIBRARY_PATH through
            # environment.sessionVariables (services.leandro-guest.
            # nvidiaUserspaceDir) -- which reaches a non-interactive
            # `ssh host cmd` because NixOS applies sessionVariables through
            # pam_env rather than through /etc/profile. Verified 2026-08-18 on
            # a booted guest: `ssh nix0 'echo $LD_LIBRARY_PATH'` answers
            # /opt/nvrm/lib.
            #
            # WHAT THE CHECK HAS TO ASK, and the first version of it asked
            # the wrong thing. `ldd nvidia-smi` was measured on 2026-08-18 and
            # lists glibc and nothing else: every NVIDIA library in the
            # payload is DLOPENED BY NAME at runtime, never linked -- which is
            # the same fact lea_payload_stage's `optional` list is built on.
            # So a link-time check is green on a guest that cannot open a
            # single one of them. dlopen of the bare SONAME is the operation
            # that actually happens, so that is what is asked, of libcuda and
            # libnvidia-ml both. It needs no GPU and no module: dlopen
            # resolves and relocates, it does not talk to the device.
            lea_ssh "$ip" 'set -e
                # The wheels'"'"' C++/OpenMP runtime goes into the SAME directory as
                # the NVIDIA payload, and that is not tidiness. The gpu gate runs
                # every stage with `export LD_LIBRARY_PATH=$PWD/nv/lib` -- it
                # REPLACES the variable rather than appending to it, so anything
                # the image puts in LD_LIBRARY_PATH is gone for the duration of a
                # gate stage. A library the guest needs has to be in the directory
                # the gate names, or it is not there when it counts.
                [ -d /opt/nvrm/wheel-runtime ] \
                    && ln -sfn /opt/nvrm/wheel-runtime/*.so* "$HOME"/gpu/nv/lib/ || true
                sudo mkdir -p /opt/nvrm/lib /opt/nvrm/bin
                sudo ln -sfn "$HOME"/gpu/nv/lib/*.so* /opt/nvrm/lib/
                sudo ln -sfn "$HOME"/gpu/nv/bin/nvidia-smi /opt/nvrm/bin/nvidia-smi
                [ -e /opt/nvrm/lib/libcuda.so.1 ] \
                    || { echo "ERROR: /opt/nvrm/lib/libcuda.so.1 missing after staging"; exit 1; }
                for so in libcuda.so.1 libnvidia-ml.so.1; do
                    python3 -c "import ctypes,sys; ctypes.CDLL(sys.argv[1])" "$so" || {
                        echo "ERROR: dlopen($so) failed in the guest."
                        echo "       LD_LIBRARY_PATH is [$LD_LIBRARY_PATH]; it has to contain /opt/nvrm/lib,"
                        echo "       which comes from services.leandro-guest.nvidiaUserspaceDir in the image."
                        exit 1; }
                done
                echo "  dlopen: libcuda.so.1 and libnvidia-ml.so.1 resolve by SONAME"' || return 1
            # The params the boot unit provisions BEFORE the first CUDA start.
            # The host's own copy, dropped where the module's boot script
            # looks for it (services.leandro-guest.params.runtimeFile) -- the
            # built-in copy in the image is only the shape of the file.
            lea_ssh "$ip" 'sudo install -D -m444 ~/gpu/params.txt /var/lib/leandro/params.txt' || return 1
            ;;
    esac

    # ---- the manifests the compute payload needs ---------------------------
    # A STAGED LIBRARY THAT NO MANIFEST NAMES IS AN ABSENT LIBRARY THAT COSTS
    # DISK (number 53). The EGL and Vulkan halves of that rule have been in
    # lea_gl_stage since the vendor JSONs -- "without 10_nvidia.json, libEGL
    # picks Mesa, silently". It generalises to every loader that finds its
    # vendor through a FILE rather than through a SONAME, and it had not been
    # generalised: libnvidia-opencl has been in `optional` above since the
    # payload existed, and `oclprobe` in a guest reported "no OpenCL platform
    # (loader found no vendor library)" -- clean status codes, zero ioctls,
    # and a trace that looks like a feature nobody used.
    #
    # The set is not a list somebody maintains. It is what a HOST with the
    # driver installed has: every file under the loader directories that
    # names an NVIDIA library, minus the six lea_gl_stage already writes.
    # Measured 2026-08-20 --
    #     grep -rl libnvidia /etc/OpenCL /usr/share/glvnd /usr/share/egl \
    #                        /usr/share/vulkan /usr/share/vulkansc
    # -- ten files, six of them lea_gl_stage's. Two of the remaining four are
    # written here; the third and fourth are the two halves of
    # /usr/share/vulkan/implicit_layer.d/nvidia_layers.json, deliberately not
    # written, and number 57 says why.
    #
    # DERIVED, not declared: each manifest is written only when the library it
    # names actually resolves under /opt/nvrm/lib, and removed when it does
    # not, so a payload staged without an optional library leaves no manifest
    # pointing at nothing. The path inside is the BARE SONAME, the way the
    # host's own manifest has it: /opt/nvrm/lib is on the guest's loader path
    # on both guests, and an absolute path would go stale the moment the
    # payload moves.
    local vksc_api
    vksc_api=$(python3 -c "
import json
try:
    print(json.load(open('/usr/share/vulkansc/icd.d/nvidia_icd_vksc.json'))['ICD']['api_version'])
except Exception:
    print('1.0.12')" 2>/dev/null || echo 1.0.12)
    # Over stdin rather than as an argument: the bodies are JSON, and a JSON
    # document through two levels of shell quoting is a trap with no upside.
    lea_ssh "$ip" 'bash -s' <<REG || return 1
set -e
# reg SONAME DIR FILE -- write the manifest on stdin, but only if the library
# it names is staged; otherwise take a stale one away.
reg() {
    if [ -e /opt/nvrm/lib/"\$1" ]; then
        sudo install -d "\$2"
        sudo tee "\$2/\$3" >/dev/null
        echo "  manifest \$2/\$3 -> \$1"
    else
        cat >/dev/null
        sudo rm -f "\$2/\$3"
        echo "  not staged, so no manifest: \$1"
    fi
}
# OpenCL: the ICD loader reads /etc/OpenCL/vendors/*.icd, and the file is one
# line holding the vendor library's SONAME. Same shape as the host's.
reg libnvidia-opencl.so.1 /etc/OpenCL/vendors nvidia.icd <<'ICD'
libnvidia-opencl.so.1
ICD
# Vulkan SC: the same registration one loader further on. NOTHING IN THIS TREE
# EXERCISES IT -- probe vk-sc is declared-unsupported, no workload exists -- so
# this is the rule applied, not a measurement, and it is labelled as such.
reg libnvidia-vksc-core.so.1 /usr/share/vulkansc/icd.d nvidia_icd_vksc.json <<'VKSC'
{
    "file_format_version" : "1.0.1",
    "ICD": {
        "library_path": "libnvidia-vksc-core.so.1",
        "api_version" : "$vksc_api"
    }
}
VKSC
REG

    case $os in
        ubuntu)
            # The guest helper module: device nodes, /proc/devices, params --
            # and the reason CUDA runs in the guest as a normal user.
            # Mandatory, and BUILT HERE because the cloud image's kernel is
            # whatever Canonical shipped that month.
            lea_guest_tar "$name" "$LEA_ROOT/guest-module" '$HOME/guest-module' nvrm_nodes || return 1

            # Build tools and kernel headers, if they are missing. The cloud
            # image ships NEITHER; the image bake puts both in, which makes
            # this a fallback rather than the normal path.
            lea_ssh "$ip" 'command -v make >/dev/null && test -f /lib/modules/$(uname -r)/build/Makefile' || {
                info "installing build tools and kernel headers in the guest ..."
                local kver
                # The GUEST's kernel, asked of the guest. It cannot be
                # expanded on this side and it must not be left empty:
                # `linux-headers-` is a package that does not exist and the
                # failure would name apt rather than the missing answer.
                kver=$(lea_ssh "$ip" uname -r | tr -d '\r')
                [[ -n $kver ]] || { error "cannot read the guest's kernel version"; return 1; }
                lea_guest_apt "$name" build-essential "linux-headers-$kver" \
                    || { error "apt-get (build-essential, linux-headers-$kver) failed -- the reason is above"; return 1; }
            }
            lea_ssh "$ip" 'command -v make >/dev/null && test -f /lib/modules/$(uname -r)/build/Makefile' \
                || { error "no make or no kernel headers in the guest -- without nvrm_nodes.ko there is no GPU path."; return 1; }
            # ffmpeg is what the `encode` gate stage runs, and the only
            # consumer of libnvidia-encode/libnvcuvid from the payload.
            # Installed on demand rather than assumed.
            lea_ssh "$ip" 'command -v ffmpeg >/dev/null' || {
                info "installing ffmpeg in the guest (the encode gate stage needs it) ..."
                lea_guest_apt "$name" ffmpeg \
                    || warn "apt-get ffmpeg failed -- the encode gate stage will fail"
            }
            lea_ssh "$ip" 'make -C ~/guest-module/nvrm_nodes >/dev/null 2>&1 && cd ~/gpu && ./nvrm-setup.sh' \
                || { error "module setup failed"; return 1; }
            ;;
        nixos)
            # NOTHING IS BUILT IN THIS GUEST. Both modules are in the image,
            # built against its own 6.12 by boot.extraModulePackages, and
            # loaded in the right order with the right parameters by
            # leandro-nvrm.service. Shipping the sources and a toolchain here
            # would be building a SECOND nvrm_nodes.ko against the same
            # kernel and hoping the two agree.
            #
            # The compatibility copy under ~/guest-module is not decoration:
            # the gpu gate's counter-check stage ends with `sudo insmod
            # ~/guest-module/virtio_nvrm/virtio_nvrm.ko`, and the gate is not
            # to be weakened for a second guest. insmod takes a path, so the
            # store's .ko under that path IS the module the image booted.
            local kmod_dir
            kmod_dir=$(lea_ssh "$ip" 'ls -d /run/booted-system/kernel-modules/lib/modules/*/extra 2>/dev/null | head -1' | tr -d "[:space:]")
            [[ -n $kmod_dir ]] || { error "$name: no /run/booted-system/.../extra in the guest -- the image carries no leandro-guest-modules (boot.extraModulePackages)"; return 1; }
            lea_ssh "$ip" "set -e
                mkdir -p ~/guest-module/nvrm_nodes ~/guest-module/virtio_nvrm
                cp -f $kmod_dir/nvrm_nodes.ko   ~/guest-module/nvrm_nodes/nvrm_nodes.ko
                cp -f $kmod_dir/virtio_nvrm.ko  ~/guest-module/virtio_nvrm/virtio_nvrm.ko
                chmod +w ~/guest-module/nvrm_nodes/nvrm_nodes.ko ~/guest-module/virtio_nvrm/virtio_nvrm.ko
                ln -sf \"\$(command -v nvrm-nodes-tool)\" ~/guest-module/nvrm_nodes/nvrm-nodes-tool" \
                || { error "$name: could not place the image's modules under ~/guest-module"; return 1; }
            lea_ssh "$ip" 'cd ~/gpu && ./nvrm-setup.sh' \
                || { error "module setup failed"; return 1; }
            ;;
    esac

    if [[ $torch -eq 1 ]] && ! lea_ssh "$ip" 'test -x ~/gpu/venv/bin/python'; then
        # A VSOCK GUEST HAS NO NETWORK DEVICE, so it has no route to pypi and
        # this cannot be done there at all -- it is not slow, it is
        # impossible. Refused by name rather than left to fail as a pip
        # timeout ten minutes in. The frozen base already carries the venv,
        # and that is the same answer a cluster node needs, where there is
        # usually no outbound route either: everything is in the image or in
        # the base, and nothing is downloaded where the job runs.
        if [[ $tr == vsock ]]; then
            error "$name: --with-torch cannot work over the vsock transport.
       The guest has no network device at all, so pip has nowhere to fetch
       from. Overlay a base that already has the venv instead:
         showcase.sh up --name $name --guest nixos --transport vsock --fresh \\
             --base $LEA_NIXOS_FLEET_BASE
       (that is the frozen disk a NixOS fleet member overlays; build.sh bake
       --nixos plus one --transport ip run with --with-torch makes one.)"
            return 1
        fi
        info "creating the torch venv (downloads ~2.5 GiB) ..."
        # THE SAME torch, from the same wheels, on both guests -- the gate's
        # torch stage compares the guest's numbers against a NATIVE host run
        # from vendor/hostvenv, and a guest running nixpkgs' torch instead of
        # the wheel would be comparing two libraries rather than two
        # transport paths. On NixOS the wheels' own .so files find their
        # libstdc++ and libgomp through nix-ld, which is the same reason the
        # probe binaries run there at all.
        case $os in
            ubuntu) lea_guest_apt "$name" python3-venv || return 1 ;;
            nixos)  ;;   # python3 in the image brings venv with it
        esac
        lea_ssh "$ip" 'python3 -m venv ~/gpu/venv &&
                     ~/gpu/venv/bin/pip install --quiet torch numpy' || return 1
    fi
    lea_guest_shell_env "$ip" || return 1
    # The library disk, when the instance was given one. Silent when it has
    # none: --games is a request, not a promise.
    lea_games_mount "$name" "$ip" || warn "$name: the games disk did not mount"

    # The last thing, and a hard one: the guest must load the HOST's libcuda,
    # not merely one with the same version number.
    lea_libcuda_check "$name" || return 1
    # Say whether the torch venv is there, because "run rlprobe.py" is the
    # first thing anyone tries in a guest and it is the one piece the
    # payload does not carry.
    local venv="no torch venv -- add --with-torch (downloads ~2.5 GiB) or overlay a base that has one"
    lea_ssh "$ip" 'test -x ~/gpu/venv/bin/python' 2>/dev/null \
        && venv="torch venv ready -- an interactive shell finds its python first"
    info "$name: provisioned (GPU path: lea_guest_build_nvrm); $venv"
}

# ---- guest modules ------------------------------------------------------------
# lea_guest_build_nvrm NAME [--no-load] [--max-pin-mib N] -- bring
# virtio_nvrm.ko into the guest, build it there and load it.
#
# --max-pin-mib raises the GUEST MODULE's cap on concurrently pinned memory
# (module parameter max_pin_mib, default 1024 MiB; LEA_GUEST_MAX_PIN_MIB from
# the environment). WARNING: not LEA_MAX_PIN_MIB, which is a HOST variable
# read by vhost-user-nvrm and limits a SINGLE arena to 256 MiB by default.
# Both report CUDA error 304 when hit; the backend log tells them apart.
#
# The coexistence order (OPEN-QUESTIONS no. 2): nvrm_nodes.ko stays loaded
# and supplies /proc/driver/nvidia/params, but releases the device nodes
# (create_nodes=0); virtio_nvrm.ko owns the nodes and the forwarding.
lea_guest_build_nvrm() {
    local name=$1; shift
    local load=1 pin=${LEA_GUEST_MAX_PIN_MIB:-} ip
    while [[ $# -gt 0 ]]; do
        case $1 in
            --no-load)     load=0; shift ;;
            --max-pin-mib) pin=$2; shift 2 ;;
            *) die "lea_guest_build_nvrm: unknown option $1" ;;
        esac
    done
    ip=$(_lea_ip "$name") || return 1
    # Name-based for the same reason as in lea_guest_setup: _lea_ip resolved
    # the instance in a subshell, so the INST_* globals are not ours to read.
    local os; os=$(lea_guest_os "$name")
    # The coexistence needs the helper module and the host's params, both of
    # which lea_guest_setup puts in place. Say so instead of failing three
    # steps later on an insmod that reads like a broken build.
    lea_ssh "$ip" 'test -f ~/guest-module/nvrm_nodes/nvrm_nodes.ko && test -f ~/gpu/params.txt' 2>/dev/null \
        || { error "$name is not provisioned (no nvrm_nodes.ko / params.txt) -- lea_guest_setup first (showcase.sh up without --no-provision)"; return 1; }
    case $os in
        ubuntu)
            lea_guest_tar "$name" "$LEA_ROOT/guest-module" '$HOME/guest-module' virtio_nvrm || return 1
            # Drop the old object FIRST. Without that, a build that fails still
            # leaves the previous .ko lying there, `test -f` is happy, and the run
            # continues with a module that does not contain the change under test.
            lea_ssh "$ip" 'rm -f ~/guest-module/virtio_nvrm/virtio_nvrm.ko'
            lea_ssh "$ip" 'set -o pipefail; make -C ~/guest-module/virtio_nvrm 2>&1 | grep -E "CC |LD |error:" || true'
            lea_ssh "$ip" 'test -f ~/guest-module/virtio_nvrm/virtio_nvrm.ko' \
                || { error "$name: virtio_nvrm.ko was not built"; return 1; }
            ;;
        nixos)
            # WHERE THE MODULE COMES FROM, and this is the one place the two
            # guests are not the same program. On Ubuntu the .ko is compiled
            # in the guest on every `up`, which is what makes "change the
            # module, run showcase.sh up" a ten-second loop. On NixOS it was
            # compiled by the DERIVATION, against the image's own kernel, and
            # lea_guest_setup has already put it under ~/guest-module. So a
            # host-side edit to guest-module/ reaches a NixOS guest only
            # through `build.sh bake --nixos`, and saying so here is cheaper
            # than finding it out by measuring an old module.
            local built have
            built=$(cd "$LEA_ROOT" && cat guest-module/virtio_nvrm/*.c guest-module/virtio_nvrm/*.h 2>/dev/null | sha256sum | cut -c1-12)
            have=$(lea_ssh "$ip" 'cat /run/booted-system/kernel-modules/lib/modules/*/extra/.leandro-src 2>/dev/null' | tr -d '[:space:]')
            info "  $name: modules come from the image (nix build), not from a guest build"
            [[ -n $have && $have != "$built" ]] && \
                warn "the image's guest modules were built from other sources than this checkout's ($have vs $built) -- scripts/build.sh bake --nixos"
            ;;
    esac
    [[ $load -eq 1 ]] || { info "built (not loaded)."; return 0; }
    # Switch to the coexistence state. Idempotent.
    lea_ssh "$ip" 'set -e
        sudo rmmod virtio_nvrm 2>/dev/null || true
        if lsmod | grep -q "^nvrm_nodes "; then
            if [ "$(cat /sys/module/nvrm_nodes/parameters/create_nodes)" != "N" ]; then
                sudo rmmod nvrm_nodes
                sudo insmod ~/guest-module/nvrm_nodes/nvrm_nodes.ko create_nodes=0
            fi
        else
            sudo insmod ~/guest-module/nvrm_nodes/nvrm_nodes.ko create_nodes=0
        fi
        sudo ~/guest-module/nvrm_nodes/nvrm-nodes-tool provision params ~/gpu/params.txt
        sudo insmod ~/guest-module/virtio_nvrm/virtio_nvrm.ko '"${pin:+max_pin_mib=$pin}"'
        echo "loaded:"; lsmod | grep -E "^(nvrm_nodes|virtio_nvrm) "' || return 1
    lea_head "dmesg"
    lea_ssh "$ip" 'sudo dmesg | grep -E "virtio_nvrm|nvrm_nodes:" | tail -20'
}

# lea_guest_build_nvkms NAME [--no-load] [--no-ship] [--no-drm] [--no-modeset]
# Build NVIDIA's own nvidia-modeset.ko AND nvidia-drm.ko IN THE GUEST, on
# top of virtio_nvrm.ko. This is the real driver: 50 .c files of NVIDIA
# source at DRIVER_VERSION, unpatched. It links against exactly ONE symbol
# of nvidia.ko -- nvidia_get_rm_ops -- and virtio_nvrm.ko exports it.
#
# Two things the guest needs that are easy to miss, both measured: the
# `video` module (backlight symbols; insmod otherwise says "Unknown symbol",
# which looks like a porting problem), and the device node 195:254 that
# nvidia-modprobe would create on a normal system.
lea_guest_build_nvkms() {
    local name=$1; shift
    local load=1 ship=1 drm=1 modeset=1 ip
    while [[ $# -gt 0 ]]; do
        case $1 in
            --no-load)    load=0; shift ;;
            --no-ship)    ship=0; shift ;;
            --no-drm)     drm=0; shift ;;
            --no-modeset) modeset=0; shift ;;
            *) die "lea_guest_build_nvkms: unknown option $1" ;;
        esac
    done
    ip=$(_lea_ip "$name") || return 1
    local vendor=$LEA_ROOT/vendor/open-gpu-kernel-modules
    # Single-quoted on purpose: $HOME must expand in the GUEST shell.
    local src='$HOME/nvkms-src/open-gpu-kernel-modules'
    [[ -d $vendor/kernel-open ]] || { error "no $vendor -- run scripts/build.sh vendor"; return 1; }
    # virtio_nvrm.ko has to exist first -- its Module.symvers is what
    # resolves nvidia_get_rm_ops.
    lea_ssh "$ip" 'test -f ~/guest-module/virtio_nvrm/Module.symvers' || {
        error "no virtio_nvrm Module.symvers in $name -- lea_guest_build_nvrm first"; return 1; }
    lea_ssh "$ip" 'grep -q nvidia_get_rm_ops ~/guest-module/virtio_nvrm/Module.symvers' || {
        error "virtio_nvrm.ko does not export nvidia_get_rm_ops"; return 1; }
    if [[ $ship -eq 1 ]]; then
        lea_head "ship NVIDIA source (build artefacts excluded)"
        # The CONTENTS of the resolved directory: vendor/open-gpu-kernel-modules
        # may be a symlink (a shared checkout), and tar would ship the link.
        # PIPESTATUS, because a tar that fails leaves ssh extracting nothing
        # and reporting success.
        local vend_real; vend_real=$(readlink -f "$vendor")
        tar -C "$vend_real" -cf - \
            --exclude=.git --exclude=_out --exclude='*.o' --exclude='*.ko' \
            --exclude='.*.cmd' --exclude=conftest --exclude=Module.symvers \
            --exclude='*.o_binary' . \
          | lea_ssh "$ip" "rm -rf ~/nvkms-src && mkdir -p ~/nvkms-src/open-gpu-kernel-modules && tar -C ~/nvkms-src/open-gpu-kernel-modules -xf -"
        [[ ${PIPESTATUS[0]} -eq 0 && ${PIPESTATUS[1]} -eq 0 ]] || { error "shipping the NVIDIA source to $name failed"; return 1; }
    fi
    lea_head "build nv-modeset-kernel.o (the OS-agnostic half)"
    lea_ssh "$ip" "cd $src && make -C src/nvidia-modeset -j\$(nproc) 2>&1 | tail -3"
    # kbuild does NOT treat KBUILD_EXTRA_SYMBOLS as a dependency: change
    # virtio_nvrm.ko and rebuild here, and make says "nothing to be done"
    # while the .ko keeps the OLD symbol CRC. The load then fails with
    # "disagrees about version of symbol nvidia_get_rm_ops", which reads
    # like an ABI problem and is a stale object file. Drop the outputs so the
    # link always happens. And conftest/ caches the API probes AND the module
    # list it was made for -- changing NV_KERNEL_MODULES without dropping it
    # reports five API incompatibilities that do not exist. Measured, twice.
    local modlist="nvidia-modeset"
    [[ $drm -eq 1 ]] && modlist="nvidia-modeset nvidia-drm"
    lea_head "build $modlist against virtio_nvrm's symbols"
    lea_ssh "$ip" "set -e
cd $src
rm -rf kernel-open/conftest
rm -f kernel-open/nvidia-modeset.ko kernel-open/nvidia-modeset.o \
      kernel-open/nvidia-modeset.mod.o kernel-open/Module.symvers \
      kernel-open/nvidia-modeset/nv-modeset-interface.o \
      kernel-open/nvidia-drm.ko kernel-open/nvidia-drm.o kernel-open/nvidia-drm.mod.o
ln -sf ../../src/nvidia-modeset/_out/Linux_x86_64/nv-modeset-kernel.o \
       kernel-open/nvidia-modeset/nv-modeset-kernel.o_binary
make -C kernel-open modules -j\$(nproc) NV_KERNEL_MODULES='$modlist' \
     KBUILD_EXTRA_SYMBOLS=\$HOME/guest-module/virtio_nvrm/Module.symvers 2>&1 \
  | grep -E 'LD \[M\]|MODPOST|error|Error' || true
test -f kernel-open/nvidia-modeset.ko" || { error "nvidia-modeset.ko was not built"; return 1; }
    [[ $drm -eq 1 ]] && { lea_ssh "$ip" "test -f $src/kernel-open/nvidia-drm.ko" \
        || { error "nvidia-drm.ko was not built"; return 1; }; }
    lea_head "undefined nvidia symbols (the whole dependency, listed)"
    lea_ssh "$ip" "nm -u $src/kernel-open/nvidia-modeset.ko | grep -iE 'nvidia|nvKms' || echo '  (none besides nvidia_get_rm_ops, already resolved)'"
    [[ $load -eq 1 ]] || { echo "built (not loaded)."; return 0; }
    lea_head "load"
    # WARNING: the teardown is THREE deep -- nvidia_drm, then nvidia_modeset,
    # then virtio_nvrm. Skipping a level makes `rmmod virtio_nvrm` fail
    # silently and the next `insmod` say "File exists", which reads like a
    # doubly loaded module and is a held reference.
    lea_ssh "$ip" "set -e
        sudo rmmod nvidia_drm 2>/dev/null || true
        sudo rmmod nvidia_modeset 2>/dev/null || true
        sudo modprobe video
        sudo insmod $src/kernel-open/nvidia-modeset.ko
        if [ ! -e /dev/nvidia-modeset ]; then
            sudo mknod /dev/nvidia-modeset c 195 254
            sudo chmod 666 /dev/nvidia-modeset
        fi" || return 1
    if [[ $drm -eq 1 ]]; then
        lea_ssh "$ip" "sudo insmod $src/kernel-open/nvidia-drm.ko modeset=$modeset" || return 1
        echo "  nvidia-drm loaded with modeset=$modeset"
    fi
    lea_head "proof"
    lea_ssh "$ip" 'echo "-- lsmod"; lsmod | grep -E "^(nvidia_modeset|virtio_nvrm|video) "
echo "-- /proc/devices"; grep nvidia /proc/devices
echo "-- node"; ls -l /dev/nvidia-modeset
echo "-- dmesg"; sudo dmesg | grep -E "nvidia-modeset|NVKMS|virtio_nvrm:" | tail -8'
}

# ---- the display ------------------------------------------------------------
# lea_display_stage NAME -- ship the five things the guest image does NOT
# have before an X server will bind NVIDIA, each of which cost a measurement:
#
#   1. /usr/share/X11/xorg.conf.d/10-nvidia-drm-outputclass.conf. Matches on
#      MatchDriver "nvidia-drm" -- the DRM DRIVER NAME, not a PCI id -- so X
#      selects nvidia_drv for our node with no PCI trick at all.
#   2. nvidia-drm_gbm.so (libnvidia-gbm1), TWICE: libgbm selects its backend
#      by DRIVER NAME and dlopens <name>_gbm.so. The 32-bit half was never
#      installed, and a 32-bit client (Steam) therefore fell into Mesa's
#      loader, which is the code that asks for the PCI id (2026-08-18).
#   3. /opt/nvrm-gl/lib on the loader path.
#   4. The card's PCI config header, for the mediated identity
#      (scripts/guest/display-identity.sh).
#   5. NVIDIA's X driver and GLX server module (nvidia_drv.so,
#      libglxserver_nvidia.so) -- the server side, which the GL payload
#      does not carry; they used to reach the guest by hand.
# Plus the identity script, the probe sources, and the dev headers they need.
lea_display_stage() {
    local name=$1 ip gbm gbm32 bdf nvcfg
    ip=$(_lea_ip "$name") || return 1
    gbm=$(find /usr/lib /usr/lib64 -name 'nvidia-drm_gbm.so' 2>/dev/null | head -1)
    [[ -n $gbm ]] || { error "no nvidia-drm_gbm.so on this host"; return 1; }
    # NOT fatal when absent: a host without lib32-nvidia-utils can still
    # drive the display gate, it just cannot serve 32-bit GBM clients.
    gbm32=$(find "$LEA_NVIDIA_LIB32_DIR" -name 'nvidia-drm_gbm.so' 2>/dev/null | head -1)

    lea_head "staging the NVIDIA userspace pieces the guest image lacks"
    lea_ssh "$ip" 'cat > /tmp/nvidia-drm_gbm.so' < "$gbm"
    lea_ssh "$ip" 'set -e
        sudo install -Dm755 /tmp/nvidia-drm_gbm.so /usr/lib/x86_64-linux-gnu/gbm/nvidia-drm_gbm.so
        rm -f /tmp/nvidia-drm_gbm.so' || return 1
    if [[ -n $gbm32 ]]; then
        lea_ssh "$ip" 'cat > /tmp/nvidia-drm_gbm32.so' < "$gbm32"
        lea_ssh "$ip" 'set -e
            sudo install -Dm755 /tmp/nvidia-drm_gbm32.so /usr/lib/i386-linux-gnu/gbm/nvidia-drm_gbm.so
            rm -f /tmp/nvidia-drm_gbm32.so'
        echo "   32-bit GBM backend staged (from $gbm32)"
    else
        warn "no 32-bit nvidia-drm_gbm.so on this host. 32-bit GBM clients (Steam) will
         fall back to Mesa and read the virtio PCI id. Install lib32-nvidia-utils,
         or set LEA_NVIDIA_LIB32_DIR."
    fi
    lea_ssh "$ip" 'set -e
        { echo /opt/nvrm-gl/lib; echo /opt/nvrm-gl/lib32; } | sudo tee /etc/ld.so.conf.d/nvrm-gl.conf >/dev/null
        sudo ldconfig
        sudo mkdir -p /usr/share/X11/xorg.conf.d
        sudo tee /usr/share/X11/xorg.conf.d/10-nvidia-drm-outputclass.conf >/dev/null <<EOC
Section "OutputClass"
    Identifier "nvidia"
    MatchDriver "nvidia-drm"
    Driver "nvidia"
    Option "AllowEmptyInitialConfiguration"
    ModulePath "/usr/lib/nvidia/xorg"
    ModulePath "/usr/lib/xorg/modules"
EndSection
EOC' || return 1

    # 5: NVIDIA's X DRIVER and its GLX server module. The GL payload
    # (lea_gl_stage) is the CLIENT side -- libGLX_nvidia and friends -- and
    # says nothing about what the X server loads: `nvidia_drv.so` and
    # `libglxserver_nvidia.so`. Until 2026-08-18 they reached the desktop
    # guest by hand ("host and guest run the same version, so taken from
    # there"), which is why a freshly baked desktop image failed X with
    # `Failed to load module "nvidia" (module does not exist)`. Same version
    # rule as libcuda: this host's copy, into the ModulePath the outputclass
    # above names.
    local xdrv glxs want
    want=$(lea_want_driver)
    xdrv=$(find /usr/lib/nvidia/xorg /usr/lib/xorg/modules/drivers /usr/lib64/xorg/modules/drivers \
                /run/opengl-driver/lib -name nvidia_drv.so 2>/dev/null | head -1)
    glxs=$(find /usr/lib/nvidia/xorg /usr/lib/xorg/modules/extensions /usr/lib64/xorg/modules/extensions \
                /run/opengl-driver/lib -name "libglxserver_nvidia.so.$want" 2>/dev/null | head -1)
    [[ -n $xdrv && -n $glxs ]] || { error "no nvidia_drv.so / libglxserver_nvidia.so.$want on this host -- the X server side of the driver (Arch: nvidia-utils, /usr/lib/nvidia/xorg)"; return 1; }
    lea_ssh "$ip" 'cat > /tmp/nvidia_drv.so' < "$xdrv"
    lea_ssh "$ip" "cat > /tmp/libglxserver_nvidia.so.$want" < "$glxs"
    lea_ssh "$ip" "set -e
        sudo install -Dm755 /tmp/nvidia_drv.so /usr/lib/nvidia/xorg/nvidia_drv.so
        sudo install -Dm755 /tmp/libglxserver_nvidia.so.$want /usr/lib/nvidia/xorg/libglxserver_nvidia.so.$want
        sudo ln -sfn libglxserver_nvidia.so.$want /usr/lib/nvidia/xorg/libglxserver_nvidia.so.1
        sudo ln -sfn libglxserver_nvidia.so.1 /usr/lib/nvidia/xorg/libglxserver_nvidia.so
        rm -f /tmp/nvidia_drv.so /tmp/libglxserver_nvidia.so.$want" || return 1
    echo "  nvidia_drv.so and libglxserver_nvidia.so.$want staged (from $(dirname "$xdrv"))"

    # 4: the config header of the card in THIS machine. 64 bytes are readable
    # without privileges and carry everything the probe reads: vendor,
    # device, class, revision, subsystem. The rest is padded.
    bdf=$(nvidia-smi --query-gpu=pci.bus_id --format=csv,noheader 2>/dev/null | head -1 | tr 'A-F' 'a-f')
    bdf=${bdf#00000000:}; bdf="0000:${bdf}"
    [[ -r /sys/bus/pci/devices/$bdf/config ]] || { error "no config space at /sys/bus/pci/devices/$bdf"; return 1; }
    nvcfg=$(mktemp)
    python3 - "$bdf" <<'PY' > "$nvcfg"
import sys
d = open(f"/sys/bus/pci/devices/{sys.argv[1]}/config", "rb").read()
sys.stdout.buffer.write(d + b"\x00" * (256 - len(d)))
PY
    lea_ssh "$ip" 'cat > ~/.lea-nvcfg.bin' < "$nvcfg"
    rm -f "$nvcfg"
    echo "  config space of $bdf shipped"

    # The identity script and the probes the display gate builds in the
    # guest. Every one of them is a READER: vkprobe is the only thing that
    # reads the swapchain, fencetime the only reader of the EVENT
    # back-channel, atomicflip the reader for the vblank itself (`--watch`
    # needs no DRM master), fbprobe the only one that answers "is there a
    # picture in the buffer" (OPEN-QUESTIONS 17). A reader nobody stages is
    # a reader nobody runs.
    lea_guest_tar "$name" "$LEA_GUEST_FILES" '$HOME' display-identity.sh \
        drm-modeset.c shmprobe.c eglprobe.c vkprobe.c paintprobe.c fencetime.c fbprobe.c atomicflip.c \
        || return 1
    lea_ssh "$ip" 'chmod +x ~/display-identity.sh'
    echo "  display-identity.sh and the probes in place"

    # What the probes need to compile. Missing on the plain cloud image, and
    # each absence costs a gate run to discover.
    lea_ssh "$ip" 'set -e
        need=""
        [ -e /usr/include/X11/extensions/XShm.h ] || need="$need libx11-dev libxext-dev"
        [ -e /usr/include/gbm.h ]                 || need="$need libgbm-dev"
        [ -e /usr/include/EGL/egl.h ]             || need="$need libegl-dev"
        [ -e /usr/include/drm/drm_mode.h ]        || need="$need libdrm-dev"
        [ -e /usr/include/vulkan/vulkan.h ]       || need="$need libvulkan-dev"
        if [ -n "$need" ]; then
            echo "  installing:$need"
            # Same three fixes as lea_guest_apt, which this cannot call
            # because `need` is computed HERE, in the guest: wait out
            # first-boot`s dpkg lock holder, bound the whole thing, and let
            # the output reach the setup log instead of /dev/null.
            sudo cloud-init status --wait >/dev/null 2>&1 || true
            sudo -E DEBIAN_FRONTEND=noninteractive timeout 900 \
                apt-get install -y -q -o DPkg::Lock::Timeout=600 $need || {
                echo "  WARNING: apt-get failed -- the display gate will not build its probes" >&2; }
        else
            echo "  probe headers already present"
        fi'
    echo "staged."
}

# lea_display_modules NAME [--no-modeset] [--no-vdisplay]
#                     [--vdisplay-size WxH] [--vdisplay-hz N]
# Set the virtio_nvrm display parameters and load nvidia-modeset + nvidia-drm.
#
# WARNING: the order is not cosmetic. nvidia-drm asks for the kernel-path RM
# operations WHILE IT LOADS, so `display` has to be 1 before the insmod --
# otherwise the log fills with "kernel RM op 0x21 is not implemented" and the
# screen comes up half-built. And the size/rate have to be in place BEFORE
# nvidia-modeset loads: NVKMS reads the EDID once, while it comes up.
lea_display_modules() {
    local name=$1; shift
    local modeset=1 vdisplay=1 ip
    local vd_w=${LEA_VDISPLAY_SIZE%x*} vd_h=${LEA_VDISPLAY_SIZE#*x} vd_hz=$LEA_VDISPLAY_HZ
    while [[ $# -gt 0 ]]; do
        case $1 in
            --no-modeset)    modeset=0; shift ;;
            --no-vdisplay)   vdisplay=0; shift ;;
            --vdisplay-size) vd_w=${2%x*}; vd_h=${2#*x}; shift 2 ;;
            --vdisplay-hz)   vd_hz=$2; shift 2 ;;
            *) die "lea_display_modules: unknown option $1" ;;
        esac
    done
    ip=$(_lea_ip "$name") || return 1
    # nvidia-drm's own `vblank` parameter defaults to OFF. With it off the
    # flip-completion events carry a frame sequence that never advances
    # (measured 2026-08-17: stuck at 0 across 60 atomic flips). Every
    # Wayland compositor paces its repaint loop on exactly those numbers;
    # weston computed a next repaint 281 seconds into the future and slept,
    # and Sunshine faithfully encoded one still frame sixty times a second.
    # That is OPEN-QUESTIONS 17's black stream. X11 never needed it, which
    # is why it was never set.
    local drmvblank=${LEA_DRM_VBLANK:-1}
    # The two .ko files exist only where lea_guest_build_nvkms has run.
    # Build them here when missing; `--no-load` on purpose, because loading
    # is THIS function's job, in the order the WARNING above insists on.
    if ! lea_ssh "$ip" 'test -f $HOME/nvkms-src/open-gpu-kernel-modules/kernel-open/nvidia-modeset.ko &&
                        test -f $HOME/nvkms-src/open-gpu-kernel-modules/kernel-open/nvidia-drm.ko'; then
        lea_head "nvidia-modeset.ko/nvidia-drm.ko not in the guest -- building them (takes minutes)"
        lea_guest_build_nvkms "$name" --no-load || { error "no display modules to load"; return 1; }
    fi
    # gdm3 first: its gnome-shell opens /dev/dri/card1 the moment nvidia-drm
    # creates it, and then rmmod says "Module nvidia_drm is in use".
    lea_ssh "$ip" "set -e
        sudo systemctl stop gdm3 2>/dev/null || true
        sleep 2
        sudo pkill -9 -x gnome-shell 2>/dev/null || true
        sleep 1
        sudo rmmod nvidia_drm 2>/dev/null || true
        sudo rmmod nvidia_modeset 2>/dev/null || true
        test -e /sys/module/virtio_nvrm/parameters/display && \
            echo 1 | sudo tee /sys/module/virtio_nvrm/parameters/display >/dev/null
        # The virtual display NEEDS \`display\` on as well (set just above):
        # the kernel-path RM operations a display uses are refused without
        # it. Without both the rig brings up the old, screenless path.
        test -e /sys/module/virtio_nvrm/parameters/vdisplay && \
            echo $vdisplay | sudo tee /sys/module/virtio_nvrm/parameters/vdisplay >/dev/null
        for pv in vdisplay_width:$vd_w vdisplay_height:$vd_h vdisplay_vblank_hz:$vd_hz; do
            pn=\${pv%%:*}; pval=\${pv#*:}
            test -n \"\$pval\" -a -e /sys/module/virtio_nvrm/parameters/\$pn && \
                echo \$pval | sudo tee /sys/module/virtio_nvrm/parameters/\$pn >/dev/null
        done
        # The address mediation belongs with the display path: X and NVML
        # only agree about WHERE the card is once both read the same answer.
        test -e /sys/module/virtio_nvrm/parameters/bdf_mediation && \
            echo 1 | sudo tee /sys/module/virtio_nvrm/parameters/bdf_mediation >/dev/null
        SRC=\$HOME/nvkms-src/open-gpu-kernel-modules
        sudo modprobe video
        sudo insmod \$SRC/kernel-open/nvidia-modeset.ko
        if [ ! -e /dev/nvidia-modeset ]; then
            sudo mknod /dev/nvidia-modeset c 195 254
            sudo chmod 666 /dev/nvidia-modeset
        fi
        sudo insmod \$SRC/kernel-open/nvidia-drm.ko modeset=$modeset vblank=$drmvblank" || return 1
    echo "display path on, nvidia-modeset + nvidia-drm loaded (modeset=$modeset)"
    # Read the parameters BACK rather than trusting the writes: an old module
    # silently has no `vdisplay` at all, and the tee above is guarded.
    echo "  virtio_nvrm parameters as the guest now has them:"
    lea_ssh "$ip" 'for p in display vdisplay vdisplay_width vdisplay_height bdf_mediation; do
                       f=/sys/module/virtio_nvrm/parameters/$p
                       if [ -e "$f" ]; then echo "    $p=$(cat $f)"; else echo "    $p: NOT BUILT"; fi
                   done'
    lea_ssh "$ip" "ls /dev/dri"
}

# lea_display_x NAME [--display :N] [--conf FILE] [--virtual WxH]
# Start Xorg inside the mediated PCI identity and nothing else -- no
# session, no compositor. What runs on the screen is the caller's business.
# WARNING: gdm3 respawns X. This stops it first; lea_display_down does NOT
# start it again, because a gdm that comes back mid-measurement is worse
# than no desktop.
lea_display_x() {
    local name=$1; shift
    local disp=:1 conf="" virtual="" ip n
    while [[ $# -gt 0 ]]; do
        case $1 in
            --display) disp=$2; shift 2 ;;
            --conf)    conf=$2; shift 2 ;;
            --virtual) virtual=$2; shift 2 ;;
            *) die "lea_display_x: unknown option $1" ;;
        esac
    done
    ip=$(_lea_ip "$name") || return 1
    n=${disp#:}
    local remote_conf=/etc/X11/lea-display.conf conf_arg=""
    # The guest module's display path is OFF by default. Turning it on here,
    # and only here, is what keeps `display=0` a provable baseline.
    lea_ssh "$ip" "test -e /sys/module/virtio_nvrm/parameters/display && \
               echo 1 | sudo tee /sys/module/virtio_nvrm/parameters/display >/dev/null \
               && echo '  virtio_nvrm display path: on' || \
               echo '  WARNING: virtio_nvrm has no display parameter -- old module?'"
    # gdm3 first: it owns a display and puts a new X back the moment one dies.
    # WARNING: an X server that wedged on a half-built GPU screen does not
    # answer SIGTERM. It then keeps /dev/nvidia* open, `rmmod virtio_nvrm`
    # fails silently, and the next `insmod` says "File exists".
    lea_ssh "$ip" "sudo systemctl stop gdm3 2>/dev/null || true; sleep 2
               sudo pkill -x Xorg 2>/dev/null || true
               sudo pkill -x Xwayland 2>/dev/null || true
               sleep 2
               pgrep -x Xorg >/dev/null && sudo pkill -9 -x Xorg || true
               sleep 1
               sudo rm -f /tmp/.X${n}-lock /tmp/.X11-unix/X${n}"
    if [[ -n $conf ]]; then
        lea_ssh "$ip" "cat > /tmp/lea-display.conf" < "$conf"
        lea_ssh "$ip" "sudo install -m644 /tmp/lea-display.conf $remote_conf"
        conf_arg="-config $remote_conf"
    elif [[ -n $virtual ]]; then
        # No CRTC means no mode, and the driver then picks 640x480 with
        # MetaMode NULL. The size has to be stated. Option UseDisplayDevice
        # "none" USED to be here, and removing it is what made Sunshine
        # possible: measured 2026-08-15, same rig, only this option removed:
        # with it xrandr shows NO output at all, without it DVI-D-0 connected
        # 1920x1080. Sunshine counts monitors, and a screen with no RandR
        # output has none ("Unable to initialize capture method").
        local w=${virtual%x*} h=${virtual#*x}
        lea_ssh "$ip" "sudo tee $remote_conf >/dev/null <<EOC
Section \"Files\"
    ModulePath \"/usr/lib/nvidia/xorg\"
    ModulePath \"/usr/lib/xorg/modules\"
EndSection
Section \"ServerLayout\"
    Identifier \"layout\"
    Screen 0 \"nvscr\"
EndSection
Section \"Device\"
    Identifier \"nv\"
    Driver     \"nvidia\"
EndSection
Section \"Screen\"
    Identifier \"nvscr\"
    Device     \"nv\"
    Option     \"AllowEmptyInitialConfiguration\" \"true\"
    DefaultDepth 24
    SubSection \"Display\"
        Depth 24
        Virtual $w $h
    EndSubSection
EndSection
EOC"
        conf_arg="-config $remote_conf"
    fi
    # WARNING: the parentheses/setsid/nohup shape. Without it ssh keeps the
    # channel open and this call never returns. LEA_NVCFG spelled out: under
    # sudo $HOME is /root, and the blob lives in the login user's home.
    # `sh -c` spelled out, and it is load-bearing: display-identity.sh runs
    # its argument list with exec "$@", so a string full of shell syntax
    # has to be handed to a shell explicitly.
    # THE X SERVER ITSELF, on demand. Ubuntu has defaulted to Wayland since
    # 24.04 and the cloud image ships no X server at all, so `--display` on a
    # plain image failed with `nohup: failed to run command 'Xorg': No such
    # file or directory` -- reported 2026-08-21. The matrix guest path has
    # installed these two on demand all along (ioctl-matrix.sh); the function
    # that actually STARTS Xorg never checked, so the one path whose whole job
    # is the X server was the one that assumed it.
    #
    # Only when missing: a desktop-baked image already has it, and this must
    # not reinstall on every display bring-up.
    if ! lea_ssh "$ip" 'command -v Xorg >/dev/null'; then
        info "  $name: no X server in the guest -- installing xserver-xorg-core"
        lea_guest_apt "$name" xserver-xorg-core xauth || {
            error "cannot install an X server in the guest.
Ubuntu 24.04 defaults to Wayland and its cloud image ships no Xorg, so the
display path has to add one. Either fix the guest's network (the apt error is
above) or use an image that already has a desktop:
  scripts/build.sh bake --with-desktop"
            return 1
        }
    fi
    local home; home=$(lea_ssh "$ip" 'echo $HOME')
    lea_ssh "$ip" "sudo LEA_NVCFG=$home/.lea-nvcfg.bin sh -c '$home/display-identity.sh \
        sh -c \"setsid nohup Xorg $disp $conf_arg -logfile /tmp/lea-xorg.log \
          -novtswitch -sharevts >/tmp/lea-xorg.out 2>&1 </dev/null &\"'" || true
    sleep 8
    if lea_ssh "$ip" "pgrep -x Xorg >/dev/null"; then
        echo "Xorg on $disp is up  (log: /tmp/lea-xorg.log)"
    else
        error "Xorg did not come up. Both paths below are IN THE GUEST:
  scripts/showcase.sh ssh --name $name -- 'cat /tmp/lea-xorg.out'
  scripts/showcase.sh ssh --name $name -- 'sudo cat /tmp/lea-xorg.log'"
        # -logfile is written by Xorg ITSELF, so it does not exist when Xorg
        # never started -- a missing binary, a config it refused, or
        # display-identity.sh failing before the exec. In that case the only
        # evidence is the redirected stdout/stderr, which this used to
        # ignore: the operator saw "No such file or directory" for the log
        # and had nothing else to look at. Report whichever exists, and say
        # which one it was.
        if lea_ssh "$ip" "test -s /tmp/lea-xorg.log"; then
            error "the last (EE) lines of the guest's Xorg log:"
            lea_ssh "$ip" "sudo grep -E '\(EE\)' /tmp/lea-xorg.log | head -10" >&2 || true
        else
            error "no Xorg log in the guest -- Xorg never started. Its output was:"
            lea_ssh "$ip" "cat /tmp/lea-xorg.out 2>/dev/null | tail -20" >&2 || true
            lea_ssh "$ip" "command -v Xorg >/dev/null || echo '  (Xorg is not installed in this guest)'" >&2 || true
        fi
        return 1
    fi
}

lea_display_status() {
    local name=$1 disp=${2:-:7} ip
    ip=$(_lea_ip "$name") || return 1
    lea_ssh "$ip" "echo '-- modules'; lsmod | grep -E '^(nvidia_drm|nvidia_modeset|virtio_nvrm) ' || true
        echo '-- drm'; ls /dev/dri 2>/dev/null || echo 'none'
        echo '-- pci address RM reports'
        nvidia-smi --query-gpu=pci.bus_id --format=csv,noheader 2>/dev/null || true
        echo '-- Xorg'; pgrep -x Xorg >/dev/null && echo 'running' || echo 'stopped'
        echo '-- providers'
        DISPLAY=$disp timeout 15 xrandr --listproviders 2>&1 | head -5"
}

# lea_display_down NAME [--display :N] -- X off, identity namespace gone,
# display path off. gdm3 left stopped on purpose.
lea_display_down() {
    local name=$1 disp=:7 ip n; shift
    while [[ $# -gt 0 ]]; do
        case $1 in
            --display) disp=$2; shift 2 ;;
            *) die "lea_display_down: unknown option $1" ;;
        esac
    done
    ip=$(_lea_ip "$name") || return 1
    n=${disp#:}
    lea_ssh "$ip" "sudo pkill -x Xorg 2>/dev/null || true; sleep 2
               pgrep -x Xorg >/dev/null && sudo pkill -9 -x Xorg || true; sleep 1
               sudo rm -f /tmp/.X${n}-lock /tmp/.X11-unix/X${n}
               sudo rm -rf /run/lea-pci-identity
               test -e /sys/module/virtio_nvrm/parameters/display && \
                 echo 0 | sudo tee /sys/module/virtio_nvrm/parameters/display >/dev/null || true"
    echo "display rig down (gdm3 left stopped on purpose -- start it by hand)"
}

# lea_display_up NAME [--display :N] [--res WxH] [--hz N] -- stage, modules,
# X on the virtual display: the display gate's rig.
#
# X's Virtual must MATCH the module's size -- otherwise the connector offers
# a mode the screen cannot hold and X falls back to 640x480 with no error
# anywhere (measured 2026-08-16). So one knob feeds both.
lea_display_up() {
    local name=$1; shift
    local disp=:7 res=$LEA_VDISPLAY_SIZE hz=$LEA_VDISPLAY_HZ
    while [[ $# -gt 0 ]]; do
        case $1 in
            --display) disp=$2; shift 2 ;;
            --res)     res=$2; shift 2 ;;
            --hz)      hz=$2; shift 2 ;;
            *) die "lea_display_up: unknown option $1" ;;
        esac
    done
    lea_display_stage   "$name" || return 1
    lea_display_modules "$name" --vdisplay-size "$res" --vdisplay-hz "$hz" || return 1
    lea_display_x       "$name" --display "$disp" --virtual "$res" || return 1
}

# ---- the desktop ------------------------------------------------------------
# lea_desktop_up NAME [--session gnome|openbox] [--display :N] [--res WxH]
#                [--hz N] [--with-steam]
# The measured desktop: the display rig, then a session, then Sunshine, then
# a read-back. This is the arrangement that streamed on 2026-08-15 (60 FPS
# over Moonlight, input confirmed by hand), written down so that "it works"
# stops being a statement about one hand-run.
#
# GNOME needs the module's vblank service: without it gnome-shell renders
# one frame and input dies (OPEN-QUESTIONS nr 7; measured fixed 2026-08-16).
# The rig X on the display is started EITHER WAY: it is the one place that
# writes lea-display.conf, and proving that server comes up is worth the
# seconds. With gnome it is then stopped again and gdm3 takes over on :0
# with the SAME configuration (the xorg.conf copy). --session openbox keeps
# the hand-run arrangement: the rig X stays, openbox and Sunshine run on it.
#
# --with-steam starts Steam LAST. Historical reason: a Steam start used to
# kill the SHMEM channel (OPEN-QUESTIONS nr 9); the backend probes every
# mmap itself since 2026-08-16, and the order stays because it costs nothing.
lea_desktop_up() {
    local name=$1; shift
    local session=gnome disp=:7 res=$LEA_VDISPLAY_SIZE hz=$LEA_VDISPLAY_HZ steam=0 wayland=0 ip
    while [[ $# -gt 0 ]]; do
        case $1 in
            --session)    session=$2; shift 2 ;;
            --display)    disp=$2; shift 2 ;;
            --res)        res=$2; shift 2 ;;
            --hz)         hz=$2; shift 2 ;;
            --with-steam) steam=1; shift ;;
            --wayland)    wayland=1; shift ;;
            *) die "lea_desktop_up: unknown option $1" ;;
        esac
    done
    case $session in gnome|openbox) ;; *) die "--session wants gnome or openbox" ;; esac
    [[ $wayland -eq 1 && $session != gnome ]] && die "--wayland is for --session gnome"
    ip=$(_lea_ip "$name") || return 1

    lea_head "$name: display rig: stage, modules, X on $disp"
    lea_display_up "$name" --display "$disp" --res "$res" --hz "$hz" || return 1

    # The guest state the image lacks. Hand-set on 2026-08-15, collected here
    # so a fresh image gets it too. All idempotent.
    lea_head "$name: guest state: xorg.conf, gdm3 autologin, identity drop-in, xdotool"
    lea_ssh "$ip" 'set -e
        # A display manager starts X with no way to pass -config: it reads
        # /etc/X11/xorg.conf. Same configuration as the hand-started server.
        sudo cp /etc/X11/lea-display.conf /etc/X11/xorg.conf
        sudo mkdir -p /etc/gdm3
        sudo tee /etc/gdm3/custom.conf >/dev/null <<EOC
[daemon]
AutomaticLoginEnable=true
AutomaticLogin='"$LEA_GUEST_USER"'
WaylandEnable='"$( [[ $wayland -eq 1 ]] && echo true || echo false )"'
EOC
        # Which session gdm starts for the autologin user. NOT "ubuntu":
        # that name exists in BOTH /usr/share/xsessions and
        # /usr/share/wayland-sessions on Ubuntu 24.04, and gdm resolved it to
        # the X11 one -- WaylandEnable=true, a Wayland session file present,
        # and the session still came up as Type=x11 (measured 2026-08-20).
        # `ubuntu-wayland` and `ubuntu-xorg` are unambiguous. AccountsService
        # keeps it per user and it OUTLIVES the custom.conf setting: a guest
        # that once logged into ubuntu-xorg keeps doing so even with
        # WaylandEnable=true, which is the failure that looks like "the flag
        # does nothing".
        sudo mkdir -p /var/lib/AccountsService/users
        sudo tee /var/lib/AccountsService/users/'"$LEA_GUEST_USER"' >/dev/null <<EOC
[User]
Session='"$( [[ $wayland -eq 1 ]] && echo ubuntu-wayland || echo ubuntu-xorg )"'
XSession='"$( [[ $wayland -eq 1 ]] && echo ubuntu-wayland || echo ubuntu-xorg )"'
SystemAccount=false
EOC
        # And make the DAEMON re-read it, or the write above is undone.
        #
        # accounts-daemon keeps its own copy of this file in memory and
        # writes that copy back when the user logs in. So a file written
        # underneath a RUNNING daemon survives exactly until gdm restarts,
        # which is the next thing this function does.
        #
        # Measured 2026-08-21 on the FIRST up of a new desktop instance
        # (--name desktop2 --index 7 --wayland): custom.conf said
        # WaylandEnable=true, this file said ubuntu-wayland, and the session
        # still came up Type=x11 with the file rewritten to ubuntu-xorg --
        # carrying the Icon= and [InputSource0] stanzas that only the daemon
        # writes, which is how the overwriter was identified. Restarting the
        # daemon here made the same flag take on the next try.
        #
        # The comment above already knew this setting outlives custom.conf.
        # What it missed is that the DAEMON outlives the FILE.
        sudo systemctl restart accounts-daemon 2>/dev/null || true
        # gdm3 inside the mediated PCI identity namespace: nvidia_drv.so
        # probes libpciaccess for a 10de device; our DRM node hangs off a
        # virtio device, so without this it reports "No devices detected".
        sudo mkdir -p /etc/systemd/system/gdm.service.d
        sudo tee /etc/systemd/system/gdm.service.d/10-lea-identity.conf >/dev/null <<EOC
[Service]
Environment=LEA_NVCFG=/home/'"$LEA_GUEST_USER"'/.lea-nvcfg.bin
ExecStart=
ExecStart=/home/'"$LEA_GUEST_USER"'/display-identity.sh /usr/sbin/gdm3
EOC
        sudo systemctl daemon-reload
        command -v xdotool >/dev/null || sudo apt-get install -y -q xdotool >/dev/null 2>&1 \
            || echo "  WARNING: xdotool did not install -- input probes will not run" >&2
        # Steam runs its games inside pressure-vessel, and bwrap needs an
        # unprivileged user namespace. Ubuntu 24.04 restricts those to
        # AppArmor-profiled binaries; a Steam started from SSH is unconfined
        # and CS2 reports "Failed to initialize Vulkan" (OPEN-QUESTIONS nr 11).
        # Lifting the restriction is a hardening trade-off, accepted for this
        # test guest.
        echo "kernel.apparmor_restrict_unprivileged_userns = 0" | \
            sudo tee /etc/sysctl.d/99-lea-userns.conf >/dev/null
        sudo sysctl -q -p /etc/sysctl.d/99-lea-userns.conf' || return 1
    echo "  guest state written"

    lea_ssh "$ip" "command -v sunshine >/dev/null" || {
        error "no sunshine binary in $name -- this image never streamed (build.sh bake --with-desktop)"; return 1; }

    if [[ $session == gnome ]]; then
        lea_head "$name: session: GNOME via gdm3 (the rig X on $disp makes way)"
        lea_ssh "$ip" "sudo pkill -x Xorg 2>/dev/null || true; sleep 2
                   pgrep -x Xorg >/dev/null && sudo pkill -9 -x Xorg || true; sleep 1
                   sudo rm -f /tmp/.X${disp#:}-lock /tmp/.X11-unix/X${disp#:}
                   sudo systemctl restart gdm3"
        local gs="" i
        for i in $(seq 1 30); do
            gs=$(lea_ssh "$ip" 'pgrep -x gnome-shell | head -1' 2>/dev/null | tr -d '[:space:]\r')
            [[ -n $gs ]] && break
            sleep 2
        done
        [[ -n $gs ]] || {
            error "gnome-shell did not come up"
            lea_ssh "$ip" 'sudo grep -E "\(EE\)" /var/log/Xorg.0.log | head -5' >&2 || true
            return 1; }
        sleep 5
        # Sunshine (and Steam) join the SESSION: display and Xauthority are
        # read from gnome-shell's environment rather than assumed.
        # BEFORE the daemon starts -- it reads all of this once (capture,
        # encoder, web login), and a Sunshine started without it picks NvFBC
        # and streams black.
        lea_sunshine_configure "$ip"
        # THE WAYLAND HALF OF THE SESSION TOO, and leaving it out is why
        # Sunshine refused every capture mode under a Wayland session:
        #
        #   Error: [wayland] Environment variable WAYLAND_DISPLAY has not
        #          been defined
        #   Error: Unable to initialize capture method
        #   Fatal: Unable to find display or encoder during startup
        #
        # and Moonlight then got 503 "Is a display connected and turned on?".
        # Measured 2026-08-21 with capture=kms AND with capture=portal -- both
        # need Wayland, because Sunshine enumerates outputs through the
        # compositor even for the KMS path.
        #
        # Under Wayland, gnome-shell's environ carries NEITHER DisplaY nor
        # XAUTHORITY (measured the same day, while chasing number 44), so the
        # two lines below find nothing and the `env` reduced to bare
        # `sunshine`. WAYLAND_DISPLAY and XDG_RUNTIME_DIR are read from the
        # compositor the same way, and DBUS_SESSION_BUS_ADDRESS goes with them
        # because the portal path needs the session bus.
        lea_ssh "$ip" 'GS=$(pgrep -x gnome-shell | head -1)
            e() { tr "\0" "\n" < /proc/$GS/environ | sed -n "s/^$1=//p" | head -1; }
            D=$(e DISPLAY); XA=$(e XAUTHORITY)
            WD=$(e WAYLAND_DISPLAY); XRD=$(e XDG_RUNTIME_DIR); DB=$(e DBUS_SESSION_BUS_ADDRESS)
            # A Wayland session that does not export WAYLAND_DISPLAY into the
            # compositor own environ still has the socket; fall back to it
            # rather than start a Sunshine that cannot capture.
            [ -n "$WD" ] || { for s in ${XRD:-/run/user/1000}/wayland-*; do
                    case "$s" in *.lock) continue ;; esac
                    [ -S "$s" ] && WD=${s##*/} && break; done; }
            set --
            [ -n "$D" ]   && set -- "$@" "DISPLAY=$D"
            [ -n "$XA" ]  && set -- "$@" "XAUTHORITY=$XA"
            [ -n "$WD" ]  && set -- "$@" "WAYLAND_DISPLAY=$WD"
            [ -n "$XRD" ] && set -- "$@" "XDG_RUNTIME_DIR=$XRD"
            [ -n "$DB" ]  && set -- "$@" "DBUS_SESSION_BUS_ADDRESS=$DB"
            echo "  sunshine env: $*"
            setsid nohup env "$@" sunshine >/tmp/lea-sunshine.out 2>&1 </dev/null &'
        if [[ $steam -eq 1 ]]; then
            lea_ssh "$ip" 'GS=$(pgrep -x gnome-shell | head -1)
                D=$(tr "\0" "\n" < /proc/$GS/environ | sed -n "s/^DISPLAY=//p")
                XA=$(tr "\0" "\n" < /proc/$GS/environ | sed -n "s/^XAUTHORITY=//p")
                sh -c "setsid nohup env DISPLAY=$D XAUTHORITY=$XA \
                    DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus steam \
                    >/tmp/lea-steam.log 2>&1 </dev/null &"'
        fi
        sleep 5
    else
        lea_head "$name: session (openbox) and Sunshine on $disp"
        lea_ssh "$ip" "sh -c 'export DISPLAY=$disp
            setsid nohup dbus-run-session -- sh -c \"openbox & sleep 3; sleep infinity\" \
                >/tmp/lea-session.log 2>&1 </dev/null &'"
        sleep 4
        if [[ $steam -eq 1 ]]; then
            lea_ssh "$ip" "sh -c 'setsid nohup env DISPLAY=$disp \
                DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus steam \
                >/tmp/lea-steam.log 2>&1 </dev/null &'"
        fi
        lea_sunshine_configure "$ip"
        lea_ssh "$ip" "sh -c 'setsid nohup env DISPLAY=$disp sunshine \
            >/tmp/lea-sunshine.out 2>&1 </dev/null &'"
        sleep 5
    fi

    # No success claim without a reader (the rule since 2026-08-07).
    lea_head "$name: read back"
    if [[ $session == gnome ]]; then
        lea_ssh "$ip" "pgrep -x gnome-shell >/dev/null" \
            || { error "gnome-shell died after coming up"; return 1; }
        # The vblank engine feeds the session, or GNOME is one frozen frame
        # with a moving pointer (OPEN-QUESTIONS nr 7). Read the counter twice.
        local v1 v2
        v1=$(lea_ssh "$ip" 'cat /sys/module/virtio_nvrm/parameters/stat_vblank_fired' | tr -d '[:space:]\r')
        sleep 3
        v2=$(lea_ssh "$ip" 'cat /sys/module/virtio_nvrm/parameters/stat_vblank_fired' | tr -d '[:space:]\r')
        if [[ -n $v1 && -n $v2 && $v2 -gt $v1 ]]; then
            echo "  vblank engine ticking: $v1 -> $v2"
        else
            error "stat_vblank_fired is not advancing ($v1 -> $v2) -- GNOME will freeze"
            return 1
        fi
    else
        lea_ssh "$ip" "pgrep -x openbox >/dev/null" || { error "openbox is not running"; return 1; }
        lea_ssh "$ip" "DISPLAY=$disp timeout 10 xwininfo -root | head -3" \
            || { error "the X server on $disp does not answer"; return 1; }
    fi
    lea_ssh "$ip" "pgrep -x sunshine >/dev/null" \
        || { error "sunshine is not running (log: /tmp/lea-sunshine.out)"; return 1; }
    lea_ssh "$ip" "ss -ltn | grep -qE ':(47984|47989|47990) '" \
        || { error "sunshine is up but not listening"; return 1; }
    # What it SETTLED on, read out of its own log. A black stream is decided
    # in these two lines, and a demonstration that does not print them makes
    # the next person guess.
    local sun_cap sun_enc
    sun_cap=$(lea_ssh "$ip" "grep -a 'Screencasting with' /tmp/lea-sunshine.out | tail -1" 2>/dev/null)
    sun_enc=$(lea_ssh "$ip" "grep -a 'Found H.264 encoder' /tmp/lea-sunshine.out | tail -1" 2>/dev/null)
    echo "  ${sun_cap:-<no capture line>}"
    echo "  ${sun_enc:-<no encoder line>}"
    case $sun_cap in
        *NvFBC*) warn "Sunshine chose NvFBC -- restricted on GeForce, the stream will be BLACK" ;;
    esac
    case $sun_enc in
        *nvenc*) ;;
        *libx264*) warn "software encoding: this card offered no NVENC to Sunshine" ;;
        *) warn "no encoder line yet -- if the stream stays black, read /tmp/lea-sunshine.out in the guest" ;;
    esac
    echo "  pair this host once:  scripts/showcase.sh pair --name $name"
    echo
    echo "desktop up ($session). Stream it with:"
    echo "  moonlight stream $ip Desktop --resolution $res --fps $hz --bitrate 40000"
}

# lea_sunshine_configure IP -- the three things Sunshine reads ONCE, at
# startup, and never again: what to capture, what to encode with, and the
# web-manager login `showcase.sh pair` hands the PIN to.
#
# Writing a capture setting at all is the point. Without one Sunshine picks
# NvFBC, which is restricted on GeForce and streams black (config.sh has the
# measurement). `auto` asks the guest which session it is running rather
# than assuming: a Wayland socket means the portal path, anything else the
# X root.
lea_sunshine_configure() {
    local ip=$1 cap=${LEA_SUN_CAPTURE:-auto}
    if [[ $cap == auto ]]; then
        if lea_ssh "$ip" "test -S /run/user/1000/wayland-0" 2>/dev/null; then
            cap=portal
            warn "$ip: the guest session is Wayland -- capture=portal."
            warn "  An unpatched Sunshine stops at the portal's permission dialog (measured);"
            warn "  bake with --desktop-session xorg for the path the numbers were taken on."
        else
            cap=x11
        fi
    fi
    echo "  sunshine: capture=$cap encoder=$LEA_SUN_ENCODER"
    lea_ssh "$ip" "mkdir -p ~/.config/sunshine
        cat > ~/.config/sunshine/sunshine.conf <<EOC
min_log_level = 1
capture = $cap
encoder = $LEA_SUN_ENCODER
EOC
        sunshine --creds $LEA_SUN_USER $LEA_SUN_PASS >/dev/null 2>&1 || true"
}

# lea_sunshine_restart IP [DISPLAY] -- stop Sunshine in the guest and start
# it again in the SESSION it has to capture.
#
# Only the pairing path needs this: `lea_desktop_up` starts Sunshine itself,
# with the web login already written, so a guest brought up by this tree
# never gets here. A guest that was up BEFORE that login existed does --
# Sunshine reads its credentials once, at startup, and answers every API
# call with a 307 to /welcome until it has some.
#
# DISPLAY and XAUTHORITY are read out of gnome-shell's own environment
# rather than assumed, exactly as `lea_desktop_up` does; the argument is the
# fallback for an openbox session, which has no gnome-shell to ask.
lea_sunshine_restart() {
    local ip=$1 disp=${2:-}
    lea_ssh "$ip" 'pkill -x sunshine 2>/dev/null || true
        sleep 2
        GS=$(pgrep -x gnome-shell | head -1)
        D=""; XA=""; WD=""; XRD=""; XCD=""
        if [ -n "$GS" ]; then
            e() { tr "\0" "\n" < /proc/$GS/environ | sed -n "s/^$1=//p" | head -1; }
            D=$(e DISPLAY); XA=$(e XAUTHORITY)
            WD=$(e WAYLAND_DISPLAY); XRD=$(e XDG_RUNTIME_DIR); XCD=$(e XDG_CURRENT_DESKTOP)
        fi
        [ -n "$D" ] || D='"'$disp'"'
        # gnome-shell IS the compositor under Wayland, so its own environment
        # carries no WAYLAND_DISPLAY -- it creates the socket rather than
        # using one. Look for the socket instead, and take Xwayland\047s
        # DISPLAY from the socket directory the same way.
        XRD=${XRD:-/run/user/$(id -u)}
        if [ -z "$WD" ]; then
            for c in "$XRD"/wayland-*; do
                case $c in *.lock) continue ;; esac
                [ -S "$c" ] && { WD=${c##*/}; break; }
            done
        fi
        if [ -z "$D" ] && [ -S /tmp/.X11-unix/X0 ]; then D=:0; fi
        # A Wayland session has no DISPLAY of its own, and Sunshine reaching
        # the desktop portal needs XDG_CURRENT_DESKTOP to pick GNOME\047s
        # backend -- so the environment is assembled from what the session
        # actually has rather than from one assumed variable.
        if [ -n "$WD" ]; then
            set -- XDG_RUNTIME_DIR="${XRD:-/run/user/$(id -u)}" WAYLAND_DISPLAY="$WD" \
                   XDG_CURRENT_DESKTOP="${XCD:-GNOME}"
            [ -n "$D" ] && set -- "$@" DISPLAY="$D"
        elif [ -n "$D" ]; then
            set -- DISPLAY="$D"
            [ -n "$XA" ] && set -- "$@" XAUTHORITY="$XA"
        else
            echo "no session environment to start sunshine in (no WAYLAND_DISPLAY, no DISPLAY)"
            exit 1
        fi
        setsid nohup env "$@" sunshine >/tmp/lea-sunshine.out 2>&1 </dev/null &
        for i in $(seq 1 30); do
            (exec 3<>/dev/tcp/127.0.0.1/47990) 2>/dev/null && break
            sleep 1
        done
        pgrep -x sunshine >/dev/null || { echo "sunshine did not come back"; exit 1; }
        (exec 3<>/dev/tcp/127.0.0.1/47990) 2>/dev/null || { echo "sunshine is up but 47990 is closed"; exit 1; }'
}

# lea_sunshine_pair NAME [PIN] -- pair THIS host's Moonlight with the
# guest's Sunshine, without opening a browser.
#
# Pairing is two halves that have to overlap: `moonlight pair --pin` opens
# the request and waits for the host to accept it, and Sunshine accepts only
# when the SAME PIN arrives on its REST API -- which is what the web UI does
# for a human. Four seconds between the two, and one at a time: two pair
# attempts at once answer "Incorrect PIN" while the API still reports
# success. That sequence is not invented here, it is the one `bench.sh
# stream` has paired with since 2026-08-18; this is it, lifted out so the
# showcase path can use it too.
#
# Idempotent: `moonlight list` answers only for a paired host, so a second
# call costs one round trip and does nothing.
lea_sunshine_pair() {
    local name=${1:-desktop} pin=${2:-$LEA_SUN_PIN} ip log body code
    command -v moonlight >/dev/null || { error "moonlight missing -- pacman -S moonlight-qt"; return 1; }
    command -v curl >/dev/null || { error "curl missing"; return 1; }
    [[ $pin =~ ^[0-9]{4}$ ]] || { error "the PIN is four digits, not '$pin'"; return 1; }
    ip=$(_lea_ip "$name") || return 1
    lea_vm_running "$name" || { error "$name is not running"; return 1; }

    if timeout 25 moonlight list "$ip" >/dev/null 2>&1; then
        info "$name ($ip): already paired with this host"
        return 0
    fi
    lea_ssh "$ip" "pgrep -x sunshine >/dev/null" || {
        error "$name: sunshine is not running -- bring the desktop up first:"
        error "  scripts/showcase.sh up --name $name --index <N> --session gnome"
        return 1; }

    log=$(lea_inst_dir "$name")/pair.log
    : > "$log"
    local try
    for try in 1 2; do
        info "$name ($ip): pairing, PIN $pin"
        ( timeout 40 moonlight pair "$ip" --pin "$pin" >>"$log" 2>&1 ) &
        local pairpid=$!
        sleep 4
        # The status, not just the body: a Sunshine with no web login answers
        # 307 (a redirect to its /welcome setup page) and a wrong one answers
        # 401, and both have an empty body -- indistinguishable from silence
        # unless the code is read.
        body=$(curl -sk -u "$LEA_SUN_USER:$LEA_SUN_PASS" -H 'Content-Type: application/json' \
            -d "{\"pin\":\"$pin\",\"name\":\"$(hostname)\"}" \
            -w '\n%{http_code}' "https://$ip:47990/api/pin" 2>>"$log")
        code=${body##*$'\n'}; body=${body%$'\n'*}
        printf 'api %s: %s\n' "$code" "$body" >>"$log"
        # Moonlight holds the request open until the PIN arrives or it times
        # out; either way it is finished with before anything else happens.
        wait $pairpid

        if timeout 25 moonlight list "$ip" >/dev/null 2>&1; then
            info "$name: paired. Stream it with:"
            info "  moonlight stream $ip Desktop --resolution $LEA_VDISPLAY_SIZE --fps $LEA_VDISPLAY_HZ --bitrate 40000"
            return 0
        fi
        # 307/401 is the one failure this can repair by itself, and only
        # once: Sunshine has no usable web login, which is the state every
        # Sunshine starts in. Write one and restart it -- it reads them at
        # startup and nowhere else.
        if [[ $try -eq 1 && ( $code == 307 || $code == 401 ) ]]; then
            info "$name: Sunshine has no usable web login (HTTP $code) -- setting it and restarting Sunshine"
            lea_ssh "$ip" "sunshine --creds $LEA_SUN_USER $LEA_SUN_PASS" >>"$log" 2>&1 \
                || { error "$name: sunshine --creds failed -- $log"; return 1; }
            lea_sunshine_restart "$ip" >>"$log" 2>&1 \
                || { error "$name: Sunshine did not come back -- $log"; return 1; }
            continue
        fi
        break
    done
    error "$name: pairing failed (API $code) -- $log"
    return 1
}

# lea_desktop_recycle NAME [--display :N] -- take a running desktop guest
# back to "modules can be reloaded": session, Sunshine, Steam and X stopped,
# the NVKMS modules unloaded. Whatever runs in that session dies with it --
# do not point this at a guest somebody is using.
#
# The NVKMS modules hold symbols of virtio_nvrm; a bare `rmmod virtio_nvrm`
# with nvidia_modeset still loaded fails SILENTLY and the following insmod
# dies on "File exists" (measured 2026-08-16, first recycle attempt).
lea_desktop_recycle() {
    local name=$1 disp=:7 ip; shift
    while [[ $# -gt 0 ]]; do
        case $1 in
            --display) disp=$2; shift 2 ;;
            *) die "lea_desktop_recycle: unknown option $1" ;;
        esac
    done
    ip=$(_lea_ip "$name") || return 1
    lea_head "$name: recycling the running guest -- stopping session, Sunshine, X"
    # -x, never -f: a -f pattern that appears in this very ssh command line
    # kills the shell carrying it (measured twice). And -x matches the COMM
    # name, which the kernel truncates to 15 characters -- "dbus-run-session"
    # never matches; its comm is "dbus-run-sessio".
    lea_ssh "$ip" 'pkill -x sunshine 2>/dev/null || true
               pkill -x steam 2>/dev/null || true
               pkill -x openbox 2>/dev/null || true
               pkill -x dbus-run-sessio 2>/dev/null || true
               sleep 2' || true
    lea_display_down "$name" --display "$disp"
    lea_ssh "$ip" 'sudo systemctl stop gdm3 2>/dev/null || true; sleep 2
               sudo pkill -9 -x gnome-shell 2>/dev/null || true; sleep 1
               sudo rmmod nvidia_drm 2>/dev/null || true
               sudo rmmod nvidia_modeset 2>/dev/null || true' || true
}
