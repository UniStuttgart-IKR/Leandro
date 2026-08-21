#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# From a fresh clone to a rig that can run showcase.sh and the gates: fetch,
# patch and build everything that is NOT in git -- which is everything
# heavy. A fresh clone is under 4 MB. And bake provisioned guest images.
#
#   scripts/build.sh [all] [--driver VERSION|auto] [--minimal|--full] [--jobs N]
#                          [--skip-checks] [--yes] [--dry-run]
#   scripts/build.sh preflight [--driver VERSION|auto] [--skip-checks]
#   scripts/build.sh vendor        open-gpu-kernel-modules @ DRIVER_VERSION (headers)
#   scripts/build.sh ch            cloud-hypervisor @ CH_VERSION + patches/, built
#   scripts/build.sh cargo         this workspace, release
#   scripts/build.sh probes        make -C probe all-probes
#   scripts/build.sh hostvenv      vendor/hostvenv: the gate's native torch reference
#   scripts/build.sh image         Ubuntu cloud image + kernel/initrd, checksummed
#   scripts/build.sh bake [--out PATH] [--with-cuda-toolkit] [--with-torch]
#                         [--with-desktop] [--desktop-session xorg|wayland]
#                         [--with-steam] [--index N] [--force] [--keep-vm]
#                         [--set-default]
#   scripts/build.sh bake --nixos [--out DIR] [--force] [--set-default] [--with-torch]
#   scripts/build.sh package [--out DIR] [--force]
#                                  the cluster artefact: an Apptainer image,
#                                  the NixOS guest and the probe binaries
#   scripts/build.sh check-driver  running driver == DRIVER_VERSION == vendor/ tag
#
# ALL runs preflight, then vendor, ch, cargo, probes, image, the host's
# native torch reference (vendor/hostvenv -- the gpu gate compares the guest
# against it), and bakes an image (--minimal skips the bake; --full also
# installs the torch venv in the GUEST, +~2.5 GB). It ends with the GPU-free check band. The host
# network is NOT part of a build (it is runtime state; `showcase.sh net up`
# -- or the NixOS module -- provides it).
#
#   --driver VERSION   build against this NVIDIA driver version instead of
#                      the pin in DRIVER_VERSION. `--driver auto` takes the
#                      version of the RUNNING host driver, which is what you
#                      want on someone else's machine.
#   --jobs N           parallel build jobs (default: nproc)
#   --skip-checks      do not stop on a failed preflight check
#   --yes              do not ask, just go
#   --dry-run          print the plan and what it will cost, change nothing
#
# WHAT IT COSTS (measured on the development machine; downloads dominate
# when the CPU is fast, the cloud-hypervisor build dominates when it is
# slow):
#   git clone                          4 MB
#   open-gpu-kernel-modules          170 MB    (headers only, blob:none)
#   cloud-hypervisor src + crates    ~300 MB   + a full release build
#   this workspace's crates          ~200 MB   + a full release build
#   Ubuntu cloud image + kernel      641 MB
#   guest apt (build tools, headers) ~300 MB   (bake)
#   host torch venv (hostvenv)      ~2500 MB   (the gate's native reference)
#   guest torch venv                ~2500 MB   (--full only)
#
# WARNING -- the one that bites on foreign hardware: the guest `libcuda` and
# the host's `nvidia.ko` must be the SAME version, because the ioctl structs
# have no ABI stability guarantee. `--driver auto` retargets the build at the
# running driver, but a version nobody has exercised is an experiment: the
# class table is verified mechanically (test.sh check, class-sizes step),
# yet only 610.43.03 on Turing has been run on real silicon.
#
# BAKE. Boots a throwaway guest, provisions it, syspreps it and publishes a
# compressed qcow2 plus a .manifest. The occasion: the first --fresh run
# failed with `make: command not found` -- the cloud image ships neither
# build tools nor kernel headers, and the long-lived dev VM had both only
# because someone installed them by hand. What goes in (stable, changes
# rarely): build tools and kernel headers, the render/video groups, the
# NVIDIA userspace matching DRIVER_VERSION, and above all the library SEARCH
# PATH (/etc/ld.so.conf.d/nvrm.conf -> /opt/nvrm/lib) -- NOT LD_LIBRARY_PATH,
# which is lost across sudo, su and systemd units. What stays OUT (rebuilt
# several times a day): virtio_nvrm.ko and nvrm_nodes.ko; they must match
# the host side and keep coming over the payload path (showcase.sh up).
# One image per DRIVER VERSION; nothing in it knows about a GPU generation.
# A bake ends by printing the `: "${LEA_BASE_IMAGE:=...}"` line that makes
# the result the default base; --set-default WRITES that line into
# local.env instead, replacing any earlier one (a second line for the same
# variable never takes effect -- config.sh's idiom lets the first win --
# while reading as if it did). It refuses where LEA_ROOT is read-only, which
# is every store install, and names the environment variable to use there.
# WARNING: reproducible only up to apt. The base image is pinned by sha256
# in GUEST_IMAGE, but the package versions come from whatever the Ubuntu
# archive serves on the day. The manifest records what was installed.
#
# BAKE --NIXOS is the answer to that warning, and it is a BUILD rather than a
# bake: nothing is booted and nothing is installed into. `nix build
# .#guest-image` evaluates nix/guest-image.nix -- a NixOS system with the
# 6.12 LTS, sshd, the guest user, and both guest modules from
# boot.extraModulePackages -- into kernel + initrd + qcow2, and this copies
# the four out of the store into LEA_NIXOS_DIR. Same inputs, same image, on
# anybody's machine. It boots by DIRECT KERNEL BOOT with no cloud-init seed;
# the per-instance identity travels on the kernel command line
# (showcase.sh up --guest nixos). NVIDIA's userspace is NOT in it and cannot
# be: the guest is handed the host's own libcuda at `up` time, exactly as the
# Ubuntu guests are.
set -uo pipefail
LEA_ROOT=${LEA_ROOT:-$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd -P)}
# shellcheck source=scripts/lib/rig.sh
source "$LEA_ROOT/scripts/lib/rig.sh"
lea_cd_root

usage() { lea_usage_from_header; exit "${1:-0}"; }

CMD=${1:-all}
case $CMD in
    all|preflight|vendor|ch|cargo|probes|hostvenv|image|bake|check-driver|package) shift ;;
    -h|--help) usage 0 ;;
    --*) CMD=all ;;
    *) error "unknown subcommand: $CMD"; usage 2 ;;
esac

if [[ -t 1 ]]; then
    B=$'\e[1m'; DIM=$'\e[2m'; GRN=$'\e[32m'; RED=$'\e[31m'; YEL=$'\e[33m'; R=$'\e[0m'
else B=""; DIM=""; GRN=""; RED=""; YEL=""; R=""; fi
say()  { echo; echo "${B}== $* ==${R}"; }
ok()   { echo "${GRN}  ok${R}   $*"; }
soft() { echo "${YEL}  warn${R} $*"; }
bad()  { echo "${RED}  FAIL${R} $*"; }
dim()  { echo "${DIM}       $*${R}"; }

# ---- vendor ---------------------------------------------------------------
# Pulls open-gpu-kernel-modules at exactly the version in DRIVER_VERSION.
# Not a submodule: a blob:none clone is faster and only headers are needed
# (the guest-side NVKMS build ships the full source from here, so it stays
# a git checkout rather than a headers-only fetch).
do_vendor() {
    local ver dst have
    ver=$(lea_want_driver)
    dst=$LEA_ROOT/vendor/open-gpu-kernel-modules
    if [[ -d $dst/.git ]]; then
        have=$(git -C "$dst" describe --tags --exact-match 2>/dev/null || echo none)
        if [[ $have == "$ver" ]]; then echo "vendor: $ver already present"; return 0; fi
        echo "vendor: $have != $ver, re-fetching"; rm -rf "$dst"
    fi
    mkdir -p "$LEA_ROOT/vendor"
    git clone --filter=blob:none --depth 1 --branch "$ver" \
        https://github.com/NVIDIA/open-gpu-kernel-modules.git "$dst" || return 1
    echo "vendor: $ver ok"
}

# ---- ch -------------------------------------------------------------------
# Fetch cloud-hypervisor @ CH_VERSION into vendor/, apply the patch series,
# build release. Idempotent; adopts a manually created clone as long as the
# tag matches.
#
# Why the patch: CH's generic vhost-user device (upstream since v53) cannot
# do a SHARED MEMORY REGION -- it does not negotiate
# VHOST_USER_PROTOCOL_F_SHMEM, asks no GET_SHMEM_CONFIG and answers no
# SHMEM_MAP/UNMAP. But that is exactly what every RM mapping into the guest
# runs over (the host-visible window, 8 GiB today). A candidate for an
# upstream PR.
# Why the second patch: the device-specific feature bits (0-23) are masked
# off by DEFAULT_VIRTIO_FEATURES. virtio-nvrm does not care -- it advertises
# 28, 30 and 32 only -- but virtio-input behind the same generic device does.
do_ch() {
    local want dir bin have i
    want=$(tr -d '[:space:]' < "$LEA_ROOT/CH_VERSION")
    dir=$LEA_ROOT/vendor/cloud-hypervisor
    bin=$dir/target/release/cloud-hypervisor
    # The whole series in numeric order, not a single path: the order has to
    # be fixed as soon as a second patch touches the same area.
    local -a patches abs=()
    mapfile -t patches < <(ls "$LEA_ROOT"/patches/[0-9][0-9][0-9][0-9]-*.patch 2>/dev/null | sort)
    [[ ${#patches[@]} -gt 0 ]] || { error "no patches in patches/"; return 1; }
    if [[ ! -d $dir/.git ]]; then
        git clone --depth 1 --branch "$want" \
            https://github.com/cloud-hypervisor/cloud-hypervisor.git "$dir" || return 1
    fi
    have=$(git -C "$dir" describe --tags --always 2>/dev/null || echo none)
    [[ $have == "$want" ]] || { error "$dir is at '$have', expected '$want'. Fix: rm -rf $dir && $0 ch"; return 1; }
    for i in "${patches[@]}"; do abs+=("$i"); done

    # The tree is derived, not edited: either it is clean (then apply the
    # series) or it carries exactly our series (then do nothing). Anything
    # else is handwork that this must not throw away.
    #
    # "Exactly our series" is NOT 'git apply --reverse --check': that checks
    # several patches independently against the working tree, not
    # cumulatively -- as soon as a later patch touches the context of an
    # earlier one, it fails in every order. Instead: apply the series to a
    # throwaway index and compare the blobs of the affected files.
    # series_applied N -- is the working tree exactly the first N patches?
    #
    # N is a PREFIX length, and it is what makes adding a patch to the series
    # survivable. A checkout that built before carries the OLD series, so once
    # a patch is added it is neither clean nor congruent with the full series,
    # and the honest-looking answer ("save your own changes") is wrong: that
    # tree holds no handwork at all, only an earlier prefix of this same
    # series. Measured 2026-08-21 on a second machine, which hit exactly that
    # on the first pull after 0003 was added.
    series_applied() {
        local n=$1 idx rc=0 f a b i
        local -a pre=("${abs[@]:0:$n}")
        [[ ${#pre[@]} -gt 0 ]] || return 1
        idx=$(mktemp)
        GIT_INDEX_FILE="$idx" git -C "$dir" read-tree HEAD 2>/dev/null || { rm -f "$idx"; return 1; }
        for i in "${pre[@]}"; do
            GIT_INDEX_FILE="$idx" git -C "$dir" apply --cached "$i" 2>/dev/null \
                || { rm -f "$idx"; return 1; }
        done
        [[ "$(git -C "$dir" diff --name-only | sort -u)" \
           == "$(git -C "$dir" apply --numstat "${pre[@]}" | cut -f3 | sort -u)" ]] || rc=1
        while read -r f; do
            [[ -n $f ]] || continue
            a=$(git -C "$dir" hash-object "$f" 2>/dev/null || true)
            b=$(GIT_INDEX_FILE="$idx" git -C "$dir" rev-parse ":$f" 2>/dev/null || true)
            [[ -n $a && $a == "$b" ]] || rc=1
        done < <(git -C "$dir" apply --numstat "${pre[@]}" | cut -f3 | sort -u)
        rm -f "$idx"
        return $rc
    }
    apply_series() {
        local i
        for i in "${!abs[@]}"; do
            git -C "$dir" apply "${abs[$i]}" \
                || { error "$(basename "${patches[$i]}") does not apply to $want."; return 1; }
            echo "applied: $(basename "${patches[$i]}")"
        done
    }
    if git -C "$dir" diff --quiet && git -C "$dir" diff --cached --quiet; then
        apply_series || return 1
    elif series_applied "${#abs[@]}"; then
        echo "series already fully applied (${#patches[@]} patches)."
    else
        # An OLDER prefix of this same series is not handwork. Prove it, then
        # heal it -- longest prefix first, so the message names what was there.
        local k found=0
        for (( k=${#abs[@]}-1; k>=1; k-- )); do
            if series_applied "$k"; then
                echo "tree carries the first $k of ${#abs[@]} patches -- reapplying the series."
                git -C "$dir" checkout -- . || return 1
                apply_series || return 1
                found=1
                break
            fi
        done
        if [[ $found -eq 0 ]]; then
            error "$dir is modified, and the changes are not this patch series
(neither the whole of it nor any earlier part of it), so they are somebody's
own work and this will not throw them away. Either save them, or discard:
  git -C $dir checkout -- . && $0 ch"
            return 1
        fi
    fi
    cargo build --release --manifest-path "$dir/Cargo.toml" --bin cloud-hypervisor || return 1
    # Counter-check on the built tree: the patch brings exactly ONE
    # capability, and without it there is no host-visible window (no CUDA).
    grep -q 'get_shmem_config' "$dir/virtio-devices/src/vhost_user/generic_vhost_user.rs" \
        || { error "patch marker (SHMEM) missing from the source."; return 1; }
    "$bin" --version
    echo "OK  $bin"
}

# ---- cargo / probes -----------------------------------------------------------
# The workspace binaries, and then their mtimes brought up to now.
#
# WHY THE TOUCH IS NOT CHEATING. lea_require_built compares the OLDEST binary
# against the NEWEST source anywhere in the workspace, deliberately coarse so
# that a gate cannot measure yesterday's backend. But cargo relinks only what
# actually changed: nvrm-genhdr lives in nvrm-abi, so an edit to
# vhost-user-nvrm never touches it, its mtime stays where it was, and the
# gate skips with "run: scripts/build.sh cargo" -- which is THIS command, and
# which could not fix it. Measured 2026-08-21: two consecutive runs of the
# gpu gate skipped for a binary that was already correct.
#
# A check whose prescribed remedy does not work is the one shape that teaches
# people to defeat it, and the comment on lea_require_built says exactly that
# about `touch`. So the remedy is made to work, and only where it is TRUE: a
# successful `cargo build --release` means every one of these is up to date
# against its real dependency graph, which is finer than any mtime sweep. The
# check keeps its whole point -- edit and do not build, and the mtimes still
# flag it.
do_cargo()  {
    cargo build --release || return 1
    local b
    for b in "$LEA_ROOT"/target/release/*; do
        [[ -f $b && -x $b ]] && touch "$b"
    done
    return 0
}
do_probes() { make -C "$LEA_ROOT/probe" all-probes; }

# do_hostvenv -- the NATIVE REFERENCE the gpu gate's torch stage measures
# against (vendor/hostvenv, LEA_HOSTVENV).
#
# WHY IT IS PART OF A BUILD AT ALL. This script says it takes a fresh clone to
# "a rig that can run showcase.sh AND THE GATES", and the torch stage compares
# the guest against a native host run from this venv -- so without it that
# stage cannot measure. It used to be "made by hand once" (config.sh), with
# the recipe living in an error string inside test.sh, which is not a place
# anybody looks before running a gate. Reported 2026-08-21 by someone who ran
# `build.sh all` and correctly asked where the venv was.
#
# THE SAME WHEELS AS THE GUEST, which is the whole point: the gate compares
# two TRANSPORT PATHS, and a host running nixpkgs' torch against a guest
# running the pip wheel would be comparing two libraries instead. `uv` when
# it is there because it is much faster, plain venv otherwise -- both end at
# the same wheels from the same index.
do_hostvenv() {
    local v=$LEA_HOSTVENV
    if [[ -x $v/bin/python ]] && "$v/bin/python" -c 'import torch, numpy' 2>/dev/null; then
        echo "  reference venv present: $("$v/bin/python" -c 'import torch,numpy;print(torch.__version__, numpy.__version__)')"
        return 0
    fi
    rm -rf "$v"
    if command -v uv >/dev/null 2>&1; then
        uv venv --python 3.12 "$v" || return 1
        VIRTUAL_ENV=$v uv pip install --quiet torch numpy || return 1
    else
        python3 -m venv "$v" || return 1
        "$v/bin/pip" install --quiet --upgrade pip >/dev/null 2>&1 || true
        "$v/bin/pip" install --quiet torch numpy || return 1
    fi
    "$v/bin/python" -c 'import torch,numpy;print("  reference venv:", torch.__version__, numpy.__version__)'
}

# ---- image ----------------------------------------------------------------
# Fetch the Ubuntu cloud image (the base of every guest) together with
# kernel/initrd into LEA_IMAGE_DIR and verify them against the pins in
# GUEST_IMAGE. The image itself stays untouched (used read-only as a base);
# instance disks are derived from it per run.
do_image() {
    # shellcheck disable=SC2034  # RELEASE is part of GUEST_IMAGE's vocabulary
    local RELEASE SERIAL BASE IMG IMG_SHA256 KERNEL KERNEL_SHA256 INITRD INITRD_SHA256
    # shellcheck source=/dev/null
    source "$LEA_ROOT/GUEST_IMAGE"
    mkdir -p "$LEA_IMAGE_DIR"
    fetch() { # name url sha256
        local name=$1 url=$2 sha=$3
        if [[ ! -f "$LEA_IMAGE_DIR/$name" ]]; then
            echo "fetching $name ..."
            curl -fL -o "$LEA_IMAGE_DIR/$name.part" "$url" && mv "$LEA_IMAGE_DIR/$name.part" "$LEA_IMAGE_DIR/$name" || return 1
        fi
        if ! echo "$sha  $LEA_IMAGE_DIR/$name" | sha256sum -c - >/dev/null 2>&1; then
            error "checksum of $name does not match GUEST_IMAGE.
Fix: rm $LEA_IMAGE_DIR/$name && $0 image
(On a new upstream serial: update the pins in GUEST_IMAGE.)"
            return 1
        fi
        echo "ok  $name"
    }
    fetch "$IMG"   "$BASE/$IMG"    "$IMG_SHA256"  || return 1
    fetch vmlinuz  "$BASE/$KERNEL" "$KERNEL_SHA256" || return 1
    fetch initrd   "$BASE/$INITRD" "$INITRD_SHA256" || return 1
    echo "OK  $LEA_IMAGE_DIR (serial $SERIAL)"
}

# ---- check-driver -------------------------------------------------------------
do_check_driver() {
    local want have vend
    want=$(lea_want_driver)
    have=$(lea_driver_version); have=${have:-none}
    vend=$(git -C "$LEA_ROOT/vendor/open-gpu-kernel-modules" describe --tags --exact-match 2>/dev/null || echo none)
    printf 'want=%s  running=%s  vendor=%s\n' "$want" "$have" "$vend"
    [[ $have == "$want" ]] || { error "host driver differs"; return 1; }
    [[ $vend == "$want" ]] || { error "vendor/ differs (scripts/build.sh vendor)"; return 1; }
    echo OK
}

# ---- preflight ------------------------------------------------------------
# Checked BEFORE anything is downloaded. Finding out after a 600 MB image
# that the driver does not match is the failure mode this exists to avoid.
# Sets PINNED (the driver version this build targets).
PINNED=""
do_preflight() {
    local driver="" skip=0 problems=0 running pm smi libdir
    local -a missing=()
    while [[ $# -gt 0 ]]; do
        case $1 in
            --driver) driver=$2; shift 2 ;;
            --skip-checks) skip=1; shift ;;
            *) die "preflight: unknown option $1" ;;
        esac
    done
    say "preflight"
    need() {  # need <tool> <where it comes from>
        if command -v "$1" >/dev/null 2>&1; then ok "$1"
        else bad "$1 missing  ($2)"; missing+=("$1"); problems=$((problems+1)); fi
    }
    need git        "git"
    need cargo      "rustup / nixpkgs rustc+cargo"
    need cc         "gcc or clang"
    need make       "make"
    need qemu-img   "qemu / qemu-img"
    need mkfs.vfat  "dosfstools"
    need mcopy      "mtools"
    need curl       "curl"
    need iptables   "iptables"
    need ip         "iproute2"
    need pkg-config "pkg-config (cloud-hypervisor build)"
    # NixOS has no FHS: a stray non-nix toolchain usually cannot link.
    if [[ -e /etc/NIXOS ]] || grep -qi nixos /etc/os-release 2>/dev/null; then
        soft "NixOS detected."
        dim "Use the dev shell so the build finds its dependencies:  nix develop"
        dim "or import the flake's module (nix/module.nix) and skip building here."
    fi
    # GPU and driver: the check that decides whether any of this can work.
    running=$(lea_driver_version)
    if [[ -n $running ]]; then ok "NVIDIA driver running: $running"
    else bad "no /proc/driver/nvidia/version -- is the NVIDIA driver loaded?"; problems=$((problems+1)); fi
    PINNED=$(lea_want_driver)
    if [[ $driver == auto ]]; then
        [[ -n $running ]] || { bad "--driver auto needs a running NVIDIA driver"; exit 1; }
        driver=$running
    fi
    if [[ -n $driver && $driver != "$PINNED" ]]; then
        soft "retargeting the build: $PINNED -> $driver"
        dim "DRIVER_VERSION is the single source of truth; bindings, class table"
        dim "and the guest payload all follow it."
        if [[ $DRY -eq 0 ]]; then
            echo "$driver" > "$LEA_ROOT/DRIVER_VERSION"
            # A stale vendor tree at the old tag would silently produce
            # bindings for the wrong version, so it goes.
            rm -rf "$LEA_ROOT/vendor/open-gpu-kernel-modules"
        fi
        PINNED=$driver
    elif [[ -n $running && $running != "$PINNED" ]]; then
        bad "driver mismatch: running $running, this tree targets $PINNED"
        dim "Struct offsets are version specific -- a mismatch is not a warning,"
        dim "it is misread memory. Retarget with:   scripts/build.sh --driver auto"
        problems=$((problems+1))
    fi
    # The NVIDIA userspace half has to be present on the HOST: it is not
    # redistributable, so the guest gets the host's copy (LICENSES.md).
    if libdir=$(lea_nvidia_libdir "$PINNED"); then
        ok "libcuda.so.$PINNED in $libdir"
    else
        bad "no libcuda.so.$PINNED found on this host"
        dim "Looked at: LEA_NVIDIA_LIB_DIR, ldconfig, /usr/lib, /usr/lib64,"
        dim "           /usr/lib/x86_64-linux-gnu, /run/opengl-driver/lib."
        dim "The guest is given the host's libcuda; it cannot be shipped."
        dim "Install the matching driver userspace, or set LEA_NVIDIA_LIB_DIR."
        problems=$((problems+1))
    fi
    if smi=$(lea_nvidia_bin nvidia-smi); then ok "nvidia-smi ($smi)"
    else bad "nvidia-smi missing"; problems=$((problems+1)); fi
    # Persistence mode is not cosmetic: the gate refuses without it.
    if command -v nvidia-smi >/dev/null 2>&1; then
        pm=$(nvidia-smi --query-gpu=persistence_mode --format=csv,noheader 2>/dev/null | head -1)
        if [[ $pm == Enabled ]]; then ok "persistence mode on"
        else soft "persistence mode is off -- the GPU gate will refuse to run."; dim "    sudo nvidia-smi -pm 1"; fi
    fi
    # sudo is needed for the bridge, the taps and the firewall rules.
    if sudo -n true 2>/dev/null; then ok "sudo (passwordless)"
    else soft "sudo will prompt -- needed for bridge/taps/iptables"; fi
    echo
    if [[ $problems -gt 0 ]]; then
        bad "$problems blocking problem(s)."
        [[ ${#missing[@]} -gt 0 ]] && dim "missing tools: ${missing[*]}"
        [[ $skip -eq 0 ]] && { echo "Refusing to continue. Override with --skip-checks."; return 1; }
        soft "continuing anyway (--skip-checks)"
    else
        ok "preflight clean"
    fi
    return 0
}

# ---- bake --nixos ---------------------------------------------------------
# The NixOS guest, and it is a BUILD rather than a bake: no VM is booted, no
# apt runs, nothing is syspreped. `nix build .#guest-image` evaluates
# nix/guest-image.nix into kernel + initrd + qcow2 + image.env, and this
# copies the four out of the store into LEA_NIXOS_DIR.
#
# WHY COPY rather than point LEA_NIXOS_DIR at the store path. The qcow2 is
# the BACKING FILE of every instance overlay, and a backing file that the
# garbage collector may remove is an overlay that stops reading halfway
# through a measurement. A copy under the artefact store is also where every
# other heavy thing lives, and it moves with LEA_VM_DIR.
#
# WHAT IS *NOT* IN IT, and cannot be: NVIDIA's userspace. `nix build` fetches
# nothing from NVIDIA here and ships nothing of theirs (LICENSES.md); the
# guest gets the HOST's own libcuda over the payload path at `up` time, the
# same way the Ubuntu guests do.
# _lea_nixos_torch DIR -- add the torch venv to a published NixOS image.
#
# WHY IT IS A BOOT AND NOT A DERIVATION. The gate's torch stage compares the
# guest against the HOST's reference venv (vendor/hostvenv, torch
# 2.13.0+cu130), so it has to be the same pip wheels rather than nixpkgs'
# torch -- and pip cannot run inside a nix build, which has no network.
#
# WHY IT HAS TO BE IN THE IMAGE AT ALL. On a cluster the guest is reached over
# vsock and has NO network device, so it cannot fetch anything itself; and the
# node may have no outbound route either. Everything the run needs is in the
# image or in the package, or it does not happen.
#
# WHY NOT JUST SHIP A PROVISIONED DISK, which would be one line: a provisioned
# disk carries the HOST's NVIDIA userspace in /opt/nvrm/lib, and that is the
# one thing this project may not redistribute (LICENSES.md). This boots the
# BARE image, adds only the venv, and never stages the payload -- so what ends
# up in the image is PyTorch and the NVIDIA *redistributable* CUDA wheels it
# depends on, and no driver library. Check that claim before publishing an
# image anywhere: `find … -name 'libcuda*'` must come back empty.
_lea_nixos_torch() {
    local dir=$1 name=nixtorch idx=${LEA_TORCH_BAKE_INDEX:-1} ip rootfs
    info "== torch venv (boots the image once; downloads ~2.5 GiB) =="
    lea_inst "$name" "$idx"
    INST_GUEST=nixos; INST_TRANSPORT=ip; _lea_inst_save
    ip=$INST_IP; rootfs=$INST_DIR/rootfs.qcow2
    lea_rig_down "$name" >/dev/null 2>&1
    rm -f "$rootfs" "$INST_DIR/seed.img"
    lea_net_ready "$((idx + 1))"
    # No backend: pip needs a network and a disk, not a GPU.
    lea_vm_start "$name" --index "$idx" --guest nixos --transport ip --fresh \
        --base "$dir/rootfs.qcow2" >"$INST_DIR/up.log" 2>&1 \
        || { error "the torch VM did not start -- $INST_DIR/up.log"; tail -20 "$INST_DIR/up.log" >&2; return 1; }
    lea_on_exit "lea_vm_stop $(printf '%q' "$name") >/dev/null 2>&1"
    # LD_LIBRARY_PATH for the CHECK only. A manylinux wheel's .so files need a
    # C++ and OpenMP runtime that NixOS supplies to nobody, and the image
    # declares one at /opt/nvrm/wheel-runtime for exactly this (see
    # nix/guest-image.nix). At RUN time lea_guest_setup links it into the
    # directory the gate names, so nothing has to remember this variable; here
    # there is no payload staged yet, so the import needs it spelled out.
    lea_ssh "$ip" 'set -e
        mkdir -p ~/gpu
        python3 -m venv ~/gpu/venv
        ~/gpu/venv/bin/pip install --quiet torch numpy
        LD_LIBRARY_PATH=/opt/nvrm/wheel-runtime \
            ~/gpu/venv/bin/python -c "import torch,numpy; print(\"venv:\", torch.__version__, numpy.__version__)"' \
        || { error "creating the venv in the guest failed"; return 1; }
    # No NVIDIA DRIVER library may end up in a published image.
    local blob
    blob=$(lea_ssh "$ip" 'find ~/gpu/venv /opt/nvrm -name "libcuda.so*" -o -name "libnvidia-ml.so*" 2>/dev/null | head -5' | tr -d '\r')
    [[ -z $blob ]] || { error "the venv image would contain driver libraries and must not be published:
$blob"; return 1; }
    info "  venv built, and no driver library in the image"
    lea_ssh "$ip" 'sudo bash -c "rm -f /etc/ssh/ssh_host_*; truncate -s 0 /etc/machine-id; sync; systemctl poweroff"' \
        >/dev/null 2>&1 || true
    local _
    for _ in $(seq 150); do lea_vm_running "$name" || break; sleep 0.2; done
    lea_vm_running "$name" && lea_vm_stop "$name"
    rm -f "$INST_DIR/ch.pid"
    info "  flattening the overlay into the published image"
    qemu-img convert -q -O qcow2 -c "$rootfs" "$dir/rootfs-torch.qcow2" \
        || { error "qemu-img convert failed"; return 1; }
    rm -f "$rootfs" "$INST_DIR/seed.img"
    info "  $dir/rootfs-torch.qcow2  ($(du -shL "$dir/rootfs-torch.qcow2" | cut -f1))"
}

do_bake_nixos() {
    local out=$1 force=$2 setdefault=$3 unsupported=$4 torch=${5:-0}
    local driver stamp link env_user
    driver=$(lea_want_driver)
    stamp=$(date +%Y%m%d-%H%M%S)
    [[ -n $out ]] || out="$LEA_VM_DIR/guest-nixos-$driver-$stamp"
    # The flags that only mean something to the Ubuntu bake. Refused rather
    # than ignored: a run that silently drops --with-torch is a run whose
    # image is missing the thing the caller asked for.
    [[ $unsupported == "000" ]] || die "bake --nixos takes none of --with-cuda-toolkit,
       --with-desktop, --with-steam. The NixOS image is declared, not
       installed into: what goes in is nix/guest-image.nix. (--with-torch IS
       supported -- it boots the image once to add the pip venv the gate's
       torch stage needs; see _lea_nixos_torch.)"
    [[ -e $out && $force -eq 0 ]] && die "$out exists (use --force to overwrite)"
    lea_require_tools nix qemu-img
    # WARNING: `-o` and never a bare `nix build`. `result` at the repository
    # root is a TRACKED symlink (it names the vendored NVIDIA headers), and a
    # bare build overwrites it.
    link=$LEA_VM_DIR/.nixos-result
    info "== nix build .#guest-image =="
    info "  (first run builds the 6.12 kernel and both guest modules; minutes)"
    nix build "$LEA_ROOT#guest-image" -o "$link" --print-build-logs \
        || die "nix build .#guest-image failed"

    info "== publishing to $out =="
    rm -rf "$out"; mkdir -p "$out"
    # -L: everything in the result is a symlink INTO the store, and the guest
    # has no store to follow it into. cp --no-preserve=mode: store files are
    # r--r--r-- and an unwritable qcow2 cannot be resized.
    local f
    for f in kernel initrd rootfs.qcow2 image.env; do
        cp -L --no-preserve=mode "$link/$f" "$out/$f" || die "cannot copy $f out of the store"
    done
    rm -f "$link"

    # The manifest lea_vm_start reads before every boot -- same name and same
    # `driver userspace:` key as the Ubuntu bake's, so the same check covers
    # both images rather than a second one covering this one.
    # shellcheck source=/dev/null
    env_user=$(sed -n 's/^LEA_NIXOS_USER=//p' "$out/image.env")
    {
        echo "# Leandro guest image (NixOS)"
        echo "built:           $stamp"
        echo "built by:        nix build .#guest-image"
        echo "driver userspace: $driver"
        echo "guest user:      $env_user"
        echo
        echo "# what the image states about itself"
        cat "$out/image.env"
        echo
        echo "# flake"
        git -C "$LEA_ROOT" rev-parse HEAD 2>/dev/null | sed 's/^/commit: /'
        nix flake metadata "$LEA_ROOT" --json 2>/dev/null \
            | sed -n 's/.*"lastModified":\([0-9]*\).*/nixpkgs lastModified: \1/p' | head -1
    } > "$out/rootfs.manifest"

    # The image was built for ONE guest user; the scripts address the guest as
    # LEA_GUEST_USER. Checked HERE as well as at boot, because finding it out
    # at boot means a VM that came up perfectly and refuses every login.
    [[ $env_user == "$LEA_GUEST_USER" ]] \
        || warn "the image was built for user '$env_user', this rig uses LEA_GUEST_USER='$LEA_GUEST_USER'.
       showcase.sh up --guest nixos will refuse until they agree: either
       export LEA_GUEST_USER=$env_user, or set guestUser in flake.nix and rebuild."

    [[ $torch -eq 1 ]] && { _lea_nixos_torch "$out" || die "the torch venv step failed"; }

    info ""
    info "image:    $out  ($(du -shL "$out/rootfs.qcow2" | cut -f1) rootfs, kernel $(sed -n 's/^LEA_NIXOS_KERNEL_VERSION=//p' "$out/image.env"))"
    info "manifest: $out/rootfs.manifest"
    info ""
    info "To make it the NixOS guest of this checkout, put this in local.env:"
    info "  : \"\${LEA_NIXOS_DIR:=$out}\""
    if [[ $setdefault -eq 1 ]]; then
        if lea_local_env_set LEA_NIXOS_DIR "$out"; then
            info "--set-default: written to $LEA_ROOT/local.env"
        else
            die "--set-default: could not write $LEA_ROOT/local.env -- the line above goes there by hand"
        fi
    fi
    info "then: scripts/showcase.sh up --guest nixos"
}

do_bake() {
    local out="" toolkit=0 torch=0 index=1 force=0 keepvm=0 desktop=0 session=xorg steam=0
    local setdefault=0 nixos=0
    while [[ $# -gt 0 ]]; do
        case $1 in
            --nixos)             nixos=1; shift ;;
            --out)               out=$(readlink -m "$2"); shift 2 ;;
            --set-default)       setdefault=1; shift ;;
            --with-cuda-toolkit) toolkit=1; shift ;;
            --with-torch)        torch=1; shift ;;
            --with-desktop)      desktop=1; shift ;;
            --with-steam)        steam=1; desktop=1; shift ;;
            --desktop-session)   session=$2; shift 2 ;;
            --index)             index=$2; shift 2 ;;
            --force)             force=1; shift ;;
            --keep-vm)           keepvm=1; shift ;;
            -h|--help)           usage 0 ;;
            *) error "bake: unknown option $1"; usage 2 ;;
        esac
    done
    # HOW TO WAIT FOR IT: this holds a pidfile for its lifetime. Wait on the
    # FILE, never on `pgrep -f` (lea_hold_pidfile has the long version):
    #   until ! lea_running vm/build.pid; do sleep 15; done
    lea_hold_pidfile "$LEA_VM_DIR/build.pid"
    [[ $nixos -eq 1 ]] && { do_bake_nixos "$out" "$force" "$setdefault" \
        "$toolkit$desktop$steam" "$torch"; return $?; }
    local driver serial stamp name=bake ip rootfs manifest
    driver=$(lea_want_driver)
    # shellcheck source=/dev/null
    serial=$(source "$LEA_ROOT/GUEST_IMAGE"; echo "$SERIAL")
    stamp=$(date +%Y%m%d-%H%M%S)
    [[ -n $out ]] || out="$LEA_VM_DIR/guest-baked-$serial-$driver-$stamp.qcow2"
    [[ -e $out && $force -eq 0 ]] && die "$out exists (use --force to overwrite)"
    # The bake runs as its own instance so it cannot collide with the dev VM
    # or a running gate: own name, own IP, own tap.
    lea_inst "$name" "$index"; _lea_inst_save
    ip=$INST_IP; rootfs=$INST_DIR/rootfs.qcow2
    # Sunshine for the guest must be the version the HOST runs, or the two
    # sides of a stream measurement are two different programs. Asked of the
    # binary rather than of a package manager; LEA_SUNSHINE_VER overrides.
    if [[ -z ${LEA_SUNSHINE_VER:-} ]] && command -v sunshine >/dev/null 2>&1; then
        LEA_SUNSHINE_VER=$(sunshine --version 2>&1 | sed -n 's/.*[Vv]ersion:* *\([0-9][0-9.]*\).*/\1/p' | head -1)
    fi
    local sunshine_ver=${LEA_SUNSHINE_VER:-}
    if [[ $desktop -eq 1 && -z $sunshine_ver ]]; then
        die "cannot tell which Sunshine this host runs.
       Install it here so guest and host are comparable, or name it:
         LEA_SUNSHINE_VER=2026.516.143833 $0 bake --with-desktop"
    fi
    case $session in xorg|wayland) ;; *) die "--desktop-session wants xorg or wayland" ;; esac

    info "== preparing =="
    [[ -f $LEA_BASE_IMAGE ]] || do_image || die "no base image"
    # A STANDALONE disk, not an overlay: the result is meant to become a
    # base image itself, and a base whose contents live in someone else's
    # backing file is a base that breaks the moment that file moves.
    info "  copying base image to $rootfs (standalone, $LEA_DISK_SIZE)"
    lea_rig_down "$name" >/dev/null 2>&1
    rm -f "$rootfs" "$INST_DIR/seed.img"
    qemu-img convert -q -O qcow2 "$LEA_BASE_IMAGE" "$rootfs" || die "qemu-img convert failed"
    qemu-img resize -q "$rootfs" "$LEA_DISK_SIZE"
    [[ $keepvm -eq 1 ]] || lea_on_exit "lea_vm_stop $name >/dev/null 2>&1"

    info "== booting the build VM ($ip on tap$index) =="
    lea_net_ready "$((index + 1))"
    lea_vm_start "$name" --index "$index" >"$INST_DIR/up.log" 2>&1 \
        || { error "build VM did not start -- $INST_DIR/up.log"; tail -20 "$INST_DIR/up.log" >&2; exit 1; }

    # Every step below is the same shape: run something in the build VM, put
    # the output in its own log, and stop with the last twenty lines if it
    # fails. Written six times it drifts six ways.
    #
    # DETACHED in the guest and polled from here, not run inside one SSH
    # session: an apt run that upgrades systemd restarts networkd and drops
    # the session -- measured 2026-08-18, systemd 255.4-1ubuntu8.17 arriving
    # in the middle of ubuntu-desktop-minimal, "Read from remote host:
    # Connection timed out" -- and a step that lives in the session dies
    # with it, half-installed. The step runs under setsid, writes its own
    # exit code, and this side waits for that file, tolerating the drop.
    bake_step() {   # bake_step <short name> <log suffix> <remote script>
        local label=$1 id=$2 log="$INST_DIR/bake-$2.log" script=$3 rc="" t0
        printf 'set -e\nexport DEBIAN_FRONTEND=noninteractive\n%s\n' "$script" \
            | lea_ssh "$ip" "cat > /tmp/bake-$id.sh && chmod +x /tmp/bake-$id.sh && rm -f /tmp/bake-$id.rc" \
            || { error "$label: could not ship the step to the guest"; exit 1; }
        lea_ssh "$ip" "setsid nohup sh -c '/tmp/bake-$id.sh >/tmp/bake-$id.log 2>&1; echo \$? >/tmp/bake-$id.rc' </dev/null >/dev/null 2>&1 &" \
            || { error "$label: could not start the step in the guest"; exit 1; }
        t0=$(date +%s); local unreachable=0
        while :; do
            if rc=$(lea_ssh "$ip" "cat /tmp/bake-$id.rc 2>/dev/null" 2>/dev/null); then
                unreachable=0
                rc=$(tr -dc '0-9' <<<"$rc"); [[ -n $rc ]] && break
            else
                # A guest that dropped off the bridge for five minutes is not
                # coming back on its own; say so rather than waiting the hour.
                unreachable=$((unreachable + 1))
                [[ $unreachable -lt 60 ]] || { error "$label: $name unreachable for 5 minutes -- console: $INST_DIR/serial.log"; exit 1; }
            fi
            [[ $(( $(date +%s) - t0 )) -lt 5400 ]] || { error "$label: no result after 90 minutes -- see the guest's /tmp/bake-$id.log"; exit 1; }
            sleep 5
        done
        lea_ssh "$ip" "cat /tmp/bake-$id.log" > "$log" 2>/dev/null
        [[ $rc -eq 0 ]] && return 0
        error "$label failed (exit $rc) -- $log"; tail -20 "$log" >&2; exit 1
    }

    # No service restarts from dpkg while the image is being built:
    # policy-rc.d answering 101 is what every container image build does,
    # and here it is what keeps the guest ON THE BRIDGE. Measured 2026-08-18,
    # twice: the archive's systemd (255.4-1ubuntu8.17) was newer than the
    # image's, its postinst restarted systemd-networkd, and the guest lost
    # its address in the middle of ubuntu-desktop-minimal -- "No route to
    # host" for the rest of the bake. Removed again before the sysprep, so
    # the image itself behaves normally.
    lea_ssh "$ip" "printf '#!/bin/sh\nexit 101\n' | sudo tee /usr/sbin/policy-rc.d >/dev/null && sudo chmod +x /usr/sbin/policy-rc.d" \
        || { error "could not install policy-rc.d in the guest"; exit 1; }
    # And a keeper for the address itself, because policy-rc.d was not
    # enough: with it in place the guest STILL lost its IPv4 address while
    # dpkg configured the systemd 255.4-1ubuntu8.17 family (watched from the
    # serial console, 2026-08-18: eth0 UP with only its link-local address
    # from that moment on). Every five seconds: no address on eth0 -> netplan
    # apply, which restores the static one from /etc/netplan. Dies with the
    # bake VM; the image boots fresh through cloud-init.
    lea_ssh "$ip" "cat > /tmp/lea-netkeep.sh <<'KEEP'
#!/bin/sh
while :; do
    ip -4 -br addr show eth0 | grep -q '$ip/' || netplan apply >/dev/null 2>&1
    sleep 5
done
KEEP
        chmod +x /tmp/lea-netkeep.sh
        sudo sh -c 'setsid /tmp/lea-netkeep.sh >/dev/null 2>&1 </dev/null &'" \
        || { error "could not start the network keeper in the guest"; exit 1; }
    info "== packages, kernel headers, build tools =="
    bake_step "package installation" packages '
        sudo apt-get update -q
        sudo apt-get install -y -q build-essential "linux-headers-$(uname -r)" \
            python3-venv python3-pip pkg-config git curl ca-certificates kmod pciutils'
    info "  installed"

    # render and video: the groups that own the DRM/GPU device nodes. Without
    # them CUDA in the guest needs root, and needing root is what the guest
    # helper module exists to avoid.
    info "== guest user groups =="
    lea_ssh "$ip" "sudo usermod -aG render,video $LEA_GUEST_USER && id -nG $LEA_GUEST_USER"

    info "== library search path (/opt/nvrm/lib) =="
    bake_step "search path" ldso '
        sudo mkdir -p /opt/nvrm/lib
        echo /opt/nvrm/lib | sudo tee /etc/ld.so.conf.d/nvrm.conf >/dev/null
        sudo ldconfig
        ldconfig -v 2>/dev/null | grep -q "^/opt/nvrm/lib:" \
            && echo "  /opt/nvrm/lib is in the search path" \
            || { echo "ERROR: /opt/nvrm/lib is NOT searched"; exit 1; }'

    info "== NVIDIA userspace $driver -> /opt/nvrm/lib =="
    local stage; stage=$(mktemp -d)
    lea_on_exit "rm -rf $(printf '%q' "$stage")"
    lea_payload_stage "$stage" >/dev/null || exit 1
    tar -C "$stage/nv" -cf - . | lea_ssh "$ip" 'sudo mkdir -p /opt/nvrm/stage && sudo tar -C /opt/nvrm/stage -xf -'
    bake_step "userspace install" userspace '
        sudo cp -a /opt/nvrm/stage/lib/. /opt/nvrm/lib/
        sudo mkdir -p /opt/nvrm/bin && sudo cp -a /opt/nvrm/stage/bin/. /opt/nvrm/bin/
        sudo rm -rf /opt/nvrm/stage
        sudo ldconfig
        ldconfig -p | grep -c libcuda'
    info "  libcuda resolvable through ldconfig"

    # ---- optional: a real desktop -- for DEMONSTRATING, not for measuring
    # the boundary. The compute image stays what it was; this is a second
    # artifact with its own reason to exist. The desktop runs on NVIDIA's
    # virtual display (showcase.sh up --display --session gnome): gdm3 starts
    # X on the NVIDIA driver inside the mediated PCI identity, so `xorg` is
    # the session that has streamed (2026-08-15). `wayland` is kept for
    # trying the portal capture path; an unpatched Sunshine captures a GNOME
    # Wayland session through the desktop portal, which asks for permission
    # ONCE in a dialog nobody can click -- measured here.
    if [[ $desktop -eq 1 ]]; then
        info "== desktop (ubuntu-desktop-minimal, GDM session: $session) =="
        # WARNING: xdg-desktop-portal-gtk is NOT pulled by ubuntu-desktop-
        # minimal, and without it the portal frontend waits 50 s for
        # org.freedesktop.impl.portal.desktop.gtk and dies in a timeout.
        bake_step "desktop install" desktop '
            sudo apt-get install -y -q ubuntu-desktop-minimal \
                glmark2-x11 glmark2-wayland mesa-utils vulkan-tools vkmark \
                xvfb x11-utils x11-xserver-utils imagemagick xinput xdotool \
                xserver-xorg-input-libinput \
                pipewire pipewire-pulse wireplumber \
                xdg-desktop-portal xdg-desktop-portal-gnome xdg-desktop-portal-gtk \
                openbox'
        info "  ubuntu-desktop-minimal installed"
        bake_step "autologin" autologin "
            sudo install -d /etc/gdm3 /var/lib/AccountsService/users
            sudo tee /etc/gdm3/custom.conf >/dev/null <<CONF
[daemon]
AutomaticLoginEnable=true
AutomaticLogin=$LEA_GUEST_USER
WaylandEnable=$([[ $session == wayland ]] && echo true || echo false)
CONF
            sudo tee /var/lib/AccountsService/users/$LEA_GUEST_USER >/dev/null <<ACC
[User]
Session=$([[ $session == wayland ]] && echo ubuntu || echo ubuntu-xorg)
XSession=$([[ $session == wayland ]] && echo ubuntu || echo ubuntu-xorg)
SystemAccount=false
ACC
            printf 'allowed_users=anybody\nneeds_root_rights=yes\n' \
                | sudo tee /etc/X11/Xwrapper.config >/dev/null
            sudo systemctl set-default graphical.target"
        # NEVER BLANK. A streaming VM that turns its screen off after five
        # minutes streams a black rectangle, and every other check still
        # passes -- measured: mean 0, stddev 0 over the whole root window
        # while session, compositor, encoder and port were all fine. As a
        # dconf SYSTEM default: at bake time there is no session to set them in.
        bake_step "no blanking" noblank "
            sudo install -d /etc/dconf/db/local.d /etc/dconf/profile
            printf 'user-db:user\nsystem-db:local\n' | sudo tee /etc/dconf/profile/user >/dev/null
            sudo tee /etc/dconf/db/local.d/00-leandro-nostandby >/dev/null <<DCONF
[org/gnome/desktop/session]
idle-delay=uint32 0

[org/gnome/desktop/screensaver]
lock-enabled=false
idle-activation-enabled=false

[org/gnome/settings-daemon/plugins/power]
sleep-inactive-ac-type='nothing'
sleep-inactive-battery-type='nothing'
idle-dim=false
DCONF
            sudo dconf update"
        info "  screen blanking, locking and idle suspend disabled; autologin as $LEA_GUEST_USER"
        # Sunshine, the SAME VERSION as the host runs.
        bake_step "sunshine install" sunshine "
            curl -fsSL -o /tmp/sunshine.deb \
              'https://github.com/LizardByte/Sunshine/releases/download/v$sunshine_ver/sunshine-ubuntu-24.04-amd64.deb'
            sudo apt-get install -y -q /tmp/sunshine.deb
            rm -f /tmp/sunshine.deb
            sunshine --version 2>&1 | head -1"
        info "  sunshine $sunshine_ver installed"
        # Input injection. Sunshine types and moves the mouse through
        # /dev/uinput; without it the stream is something to watch, not to
        # use. The .deb ships a rule, but the GROUP membership is ours to make.
        bake_step "uinput" uinput "
            sudo groupadd -f input
            sudo usermod -aG input,render,video $LEA_GUEST_USER
            echo 'KERNEL==\"uinput\", GROUP=\"input\", MODE=\"0660\", OPTIONS+=\"static_node=uinput\"' \
                | sudo tee /etc/udev/rules.d/60-sunshine-uinput.rules >/dev/null
            echo uinput | sudo tee /etc/modules-load.d/uinput.conf >/dev/null"
        info "  /dev/uinput wired for $LEA_GUEST_USER"
        # NVIDIA's GL/EGL, system-wide and in BOTH word sizes. 32-bit is not
        # a nicety: most Steam titles are 32-bit or drag 32-bit dependencies,
        # and a missing 32-bit libGLX_nvidia is a silent fallback to llvmpipe.
        info "== NVIDIA GL/EGL $driver -> system (64 + 32 bit) =="
        lea_gl_stage "$name" --system --with-32bit >"$INST_DIR/bake-gl.log" 2>&1 \
            || { error "GL staging failed -- $INST_DIR/bake-gl.log"; tail -20 "$INST_DIR/bake-gl.log" >&2; exit 1; }
        info "  nvidia-run <program> available"
    fi

    # ---- optional: Steam. Implies --with-desktop, because a game needs a
    # session to appear in. WHY i386 IS NOT OPTIONAL: see above.
    if [[ $steam -eq 1 ]]; then
        info "== steam (multiverse + i386) =="
        bake_step "steam install" steam "
            sudo dpkg --add-architecture i386
            sudo add-apt-repository -y multiverse >/dev/null 2>&1 || true
            sudo apt-get update -q
            echo steam steam/question select 'I AGREE' | sudo debconf-set-selections
            echo steam steam/license note '' | sudo debconf-set-selections
            sudo apt-get install -y -q steam-installer \
                libgl1:i386 libglx-mesa0:i386 libc6:i386 libstdc++6:i386 \
                mesa-vulkan-drivers:i386 libvulkan1:i386"
        info "  steam installed -- start it with: nvidia-run steam"
    fi

    # ---- optional: CUDA toolkit. Off by default: several GB, copied per
    # fleet member, and only needed to COMPILE CUDA inside the guest.
    if [[ $toolkit -eq 1 ]]; then
        info "== CUDA toolkit (nvcc) =="
        bake_step "toolkit installation" toolkit 'sudo apt-get install -y -q nvidia-cuda-toolkit'
        info "  nvcc: $(lea_ssh "$ip" 'nvcc --version | tail -2 | head -1')"
    fi
    if [[ $torch -eq 1 ]]; then
        info "== torch venv (~2.5 GiB) =="
        bake_step "torch venv" torch '
            python3 -m venv ~/gpu/venv 2>/dev/null || { mkdir -p ~/gpu && python3 -m venv ~/gpu/venv; }
            ~/gpu/venv/bin/pip install --quiet torch numpy'
        info "  installed"
    fi

    info "== manifest =="
    manifest="${out%.qcow2}.manifest"
    {
        echo "# Leandro guest image"
        echo "built:           $stamp"
        echo "base serial:     $serial"
        echo "base image:      $LEA_BASE_IMAGE"
        echo "driver userspace: $driver"
        echo "cuda toolkit:    $([[ $toolkit -eq 1 ]] && echo yes || echo no)"
        echo "torch venv:      $([[ $torch -eq 1 ]] && echo yes || echo no)"
        echo "steam:           $([[ $steam -eq 1 ]] && echo yes || echo no)"
        # The GL half is versioned separately because it is the part that
        # has to be rebuilt when the HOST driver changes.
        echo "nvidia gl:       $([[ $desktop -eq 1 ]] && echo "$driver, 64 + 32 bit" || echo no)"
        echo "host gpu:        $(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1)"
        echo "desktop:         $([[ $desktop -eq 1 ]] && echo "ubuntu-desktop-minimal, $session, sunshine $sunshine_ver" || echo no)"
        echo "guest user:      $LEA_GUEST_USER"
        echo
        echo "# guest kernel"
        lea_ssh "$ip" 'uname -r'
        echo
        echo "# installed packages (relevant)"
        lea_ssh "$ip" "dpkg-query -W -f='\${Package} \${Version}\n' build-essential linux-headers-\$(uname -r) python3-venv git curl 2>/dev/null"
        echo
        echo "# /opt/nvrm/lib"
        lea_ssh "$ip" 'ls -1 /opt/nvrm/lib'
    } > "$manifest"
    info "  $manifest"

    # Make it a TEMPLATE, not a used disk: drop the identity that must be
    # unique per instance. Without this every VM cloned from the image would
    # share a machine-id and the same SSH host keys, and cloud-init would
    # consider itself already run and never apply the new seed. ONE ssh
    # invocation together with the poweroff: after the host keys are gone a
    # NEW connection cannot be established.
    info "== sysprep + shutdown =="
    lea_ssh "$ip" 'sudo rm -f /usr/sbin/policy-rc.d'
    lea_ssh "$ip" 'sudo bash -c "
        cloud-init clean --logs --seed 2>/dev/null || rm -rf /var/lib/cloud
        rm -f /etc/ssh/ssh_host_*
        truncate -s 0 /etc/machine-id
        rm -f /var/lib/dbus/machine-id
        apt-get clean
        rm -rf /var/lib/apt/lists/*
        find /var/log -type f -exec truncate -s 0 {} + 2>/dev/null
        rm -rf /root/.bash_history /home/*/.bash_history
        sync
        systemctl poweroff" ' >/dev/null 2>&1 || true
    for _ in $(seq 150); do lea_vm_running "$name" || break; sleep 0.2; done
    if lea_vm_running "$name"; then
        warn "guest did not power off in time -- stopping it"
        lea_vm_stop "$name"
    fi
    rm -f "$INST_DIR/ch.pid"

    # Compressed convert rather than a copy: it drops the freed blocks the
    # apt cache left behind, and the result is a clean single-file image.
    info "== writing $out =="
    rm -f "$out"
    qemu-img convert -q -O qcow2 -c "$rootfs" "$out" || die "qemu-img convert failed"
    rm -f "$rootfs" "$INST_DIR/seed.img"
    info ""
    info "image:    $out  ($(du -h "$out" | cut -f1))"
    info "manifest: $manifest"
    info ""
    info "To use it as the base for every VM, put this in local.env:"
    info "  : \"\${LEA_BASE_IMAGE:=$out}\""
    if [[ $setdefault -eq 1 ]]; then
        # Written rather than printed, and REPLACING any earlier entry: a
        # second `: "${LEA_BASE_IMAGE:=...}"` line would never take effect
        # (the first assignment wins) while reading as if it had.
        if lea_local_env_set LEA_BASE_IMAGE "$out"; then
            info "--set-default: written to $LEA_ROOT/local.env"
        else
            die "--set-default: could not write $LEA_ROOT/local.env -- the line above goes there by hand"
        fi
    fi
    info "then: scripts/showcase.sh up --fresh   (or export LEA_BASE_IMAGE for one run)"
}

# ---- package ---------------------------------------------------------------
# The artefact an unprivileged user drops on a cluster.
#
#   scripts/build.sh package [--out DIR] [--force]
#
# WHAT IT PRODUCES, and why it is one directory rather than one file:
#
#   sif/leandro-<driver>.sif   everything that must RUN: the scripts, their
#                              library, vhost-user-nvrm, vsockconnect, the
#                              patched cloud-hypervisor, and the ssh, qemu-img,
#                              python3 and coreutils they call. One Apptainer
#                              image, built from the Nix closure.
#   guest/                     the NixOS guest: kernel, initrd, rootfs.qcow2,
#                              image.env. DATA, and deliberately outside the
#                              image -- a job stages it to node-local scratch
#                              (see `bench.sh slurm`), and a 4.7 GiB qcow2
#                              inside a read-only squashfs could not be
#                              overlaid anyway.
#   probe-bin/                 the probe binaries, built HERE from the sources
#                              in this same tree. They cannot be built on a
#                              compute node: probe/Makefile needs the CUDA
#                              headers and links libcuda, and LEA_ROOT inside
#                              the image is read-only. probe/Makefile's warning
#                              about stale prebuilts is kept by MANIFEST
#                              recording the commit they were built from.
#   MANIFEST                   driver version, CH version, guest kernel, the
#                              git commit, and a sha256 of every part.
#
# WHY APPTAINER AND NOT A NIX CLOSURE. A closure needs Nix on the node, or
# root to unpack into /nix; clusters have neither. Apptainer/Singularity is
# the one unprivileged runtime that HPC sites do have, and it needs no daemon
# and no setuid helper on a kernel with user namespaces. Measured 2026-08-19
# on this host: `apptainer build` from a directory completes as an ordinary
# user, no --fakeroot, no root.
#
# WHY NOT A .deb: there is no root on a cluster to install one with.
#
# WHAT THE NODE STILL HAS TO PROVIDE -- and it is a short list, which is the
# point: /dev/kvm, the assigned /dev/nvidia*, a writable scratch directory,
# and the host's own NVIDIA userspace (apptainer --nv injects it; the guest is
# handed the HOST's libcuda and it must match the host driver exactly).
# NOT needed: root, a bridge, taps, NAT, or any nested-virtualisation trick
# beyond KVM itself.
do_package() {
    local out="" force=0
    while [[ $# -gt 0 ]]; do
        case $1 in
            --out)    out=$(readlink -m "$2"); shift 2 ;;
            --force)  force=1; shift ;;
            -h|--help) usage 0 ;;
            *) error "package: unknown option $1"; usage 2 ;;
        esac
    done
    lea_hold_pidfile "$LEA_VM_DIR/build.pid"
    local driver stamp
    driver=$(lea_want_driver)
    stamp=$(date +%Y%m%d-%H%M%S)
    [[ -n $out ]] || out="$LEA_VM_DIR/leandro-pkg-$driver-$stamp"
    [[ -e $out && $force -eq 0 ]] && die "$out exists (use --force to overwrite)"
    lea_require_tools nix qemu-img make sha256sum

    # The guest image is a precondition, not something this builds: it has its
    # own subcommand and its own hours.
    lea_nixos_image || die "no NixOS guest image -- run: scripts/build.sh bake --nixos"

    info "== probes (built here, from this tree) =="
    do_probes >/dev/null || die "make -C probe all-probes failed -- the probes need the CUDA headers (CUDA_HOME=${CUDA_HOME:-/opt/cuda})"
    [[ -x $LEA_ROOT/probe/bin/nvprobe ]] || die "probe/bin/nvprobe missing after the build"

    info "== nix closure (scripts, binaries, cloud-hypervisor, their tools) =="
    local link=$LEA_VM_DIR/.pkg-result
    # WARNING: -o, never a bare `nix build`. `result` at the repository root is
    # a TRACKED symlink and a bare build overwrites it.
    nix build "$LEA_ROOT#leandro-scripts" -o "$link" --print-build-logs \
        || die "nix build .#leandro-scripts failed"
    local store; store=$(readlink -f "$link")

    rm -rf "$out"; mkdir -p "$out/sif" "$out/guest" "$out/probe-bin"

info "== apptainer image =="
    # BOTH the sandbox and apptainer's own scratch go below LEA_VM_DIR,
    # not /tmp: the image root is the whole 850 MiB closure and apptainer
    # unpacks a second copy of it while packing, and /tmp is a 16 GiB
    # tmpfs on this host and smaller on plenty of others. LEA_VM_DIR is
    # the artefact store and already sized for qcow2 images.
    local pkgtmp=$LEA_VM_DIR/.pkg-tmp
    mkdir -p "$pkgtmp"
    local sbx; sbx=$(mktemp -d "$pkgtmp/leandro-sbx.XXXXXX")
    export TMPDIR=$pkgtmp APPTAINER_TMPDIR=$pkgtmp
    # lea_on_exit, NOT `trap ... RETURN`. A RETURN trap set inside a
    # function fires when the NEXT function completes, not when this one
    # does -- measured 2026-08-19: the very next `info` call ran the
    # handler and deleted the sandbox out from under the build, which
    # then failed on a missing bin/. lea_on_exit is the repo's one exit
    # stack and runs at shell exit, which is when this is actually done
    # with.
    # ITS OWN SANDBOX ONLY, never the shared parent. $pkgtmp is a fixed
    # path under LEA_VM_DIR, so removing it takes any CONCURRENT package
    # build's sandbox with it -- measured 2026-08-19 with two builds back
    # to back: the first one's exit handler was still deleting 850 MiB
    # when the second created its sandbox inside the same parent, and the
    # second died on `cannot copy <store path> into the image root`. The
    # parent is left behind empty, which costs nothing and races with
    # nobody.
    lea_on_exit "chmod -R u+w $(printf '%q' "$sbx") 2>/dev/null; rm -rf $(printf '%q' "$sbx")"
    mkdir -p "$sbx/nix/store" "$sbx/bin" "$sbx/etc"
    local p n=0
    while read -r p; do
        cp -a "$p" "$sbx/nix/store/" || die "cannot copy $p into the image root"
        n=$((n + 1))
    done < <(nix path-info -r "$store")
    info "  $n store paths"
    # THE STORE'S OWN MODES BLOCK THE PACKER. Every store directory is
    # r-xr-xr-x and `cp -a` keeps that, so apptainer's unpack of the
    # sandbox fails on the first `mkdir` inside one -- measured
    # 2026-08-19: "permission denied ... rootfs/nix/store/<p>/bin". The
    # copy is ours and throwaway, so make it writable; what ends up in
    # the read-only squashfs is decided by the image format, not by these
    # bits.
    chmod -R u+w "$sbx"
    # /bin/sh IS REQUIRED, and not only for `apptainer shell`: ssh runs a
    # ProxyCommand through /bin/sh, and the vsock transport is a
    # ProxyCommand. Without it every guest is unreachable inside the
    # image.
    #
    # Resolved out of the SANDBOX, not from `nix eval nixpkgs#bash`.
    # Measured 2026-08-19: the latter answers with the unstable channel's
    # bash, which is a different store path from the one in this flake's
    # pinned closure, so the symlink pointed outside the image and
    # apptainer said `stat /bin/sh: no such file or directory`. Globbing
    # what was actually copied cannot miss.
    local _sh
    _sh=$(ls -d "$sbx"/nix/store/*-bash-*/bin/sh 2>/dev/null | head -1) \
        || die "no bash in the closure -- cannot provide /bin/sh"
    [[ -n $_sh ]] || die "no bash in the closure -- cannot provide /bin/sh"
    ln -sfn "${_sh#"$sbx"}" "$sbx/bin/sh"
    # /bin/bash TOO, and this one is not cosmetic. OpenSSH runs a
    # ProxyCommand through $SHELL, not through /bin/sh -- and $SHELL is
    # inherited from whoever launched the container, which on an ordinary
    # login is /bin/bash. Measured 2026-08-19 inside this image: ssh
    # printed `/bin/bash: No such file or directory` and the connection
    # closed, which surfaces three layers up as "SSH did not come up
    # within 120s" and reads like a broken guest. The vsock transport IS
    # a ProxyCommand, so without this no guest is reachable from a
    # container at all.
    ln -sfn "${_sh%/sh}/bash" "$sbx/bin/bash"

    # /lib64/ld-linux-x86-64.so.2 -- THE FHS LOADER, and the image needs
    # it for the same reason the NixOS guest does. Everything Leandro
    # builds is a nix binary with an absolute interpreter, but the
    # binaries apptainer's --nv injects from the HOST are ordinary glibc
    # ELF and ask for the loader at that path. Measured 2026-08-19 without
    # it: `apptainer exec --nv <sif> nvidia-smi` fails with "a shared
    # library is likely missing in the image", and lea_rig_state then
    # reports driver=unknown and the gate skips on a perfectly good node.
    local _ld
    _ld=$(ls "$sbx"/nix/store/*-glibc-*/lib/ld-linux-x86-64.so.2 2>/dev/null | head -1)
    [[ -n $_ld ]] || die "no glibc loader in the closure"
    mkdir -p "$sbx/lib64"
    ln -sfn "${_ld#"$sbx"}" "$sbx/lib64/ld-linux-x86-64.so.2"
    printf 'root:x:0:0:root:/root:/bin/sh\n' > "$sbx/etc/passwd"
    printf 'root:x:0:\n'                     > "$sbx/etc/group"
    nix run nixpkgs#apptainer -- build --force "$out/sif/leandro-$driver.sif" "$sbx" 2>&1 \
        | sed -n 's/^INFO: */  /p' || die "apptainer build failed"

    info "== guest image =="
    local f
    for f in kernel initrd image.env rootfs.manifest; do
        [[ -f $LEA_NIXOS_DIR/$f ]] && cp -L --no-preserve=mode "$LEA_NIXOS_DIR/$f" "$out/guest/$f"
    done
    # THE TORCH-CARRYING ROOTFS WHEN THERE IS ONE, and it is not a
    # preference. The gate's torch stage needs a venv the guest cannot build
    # for itself: over vsock it has no network device, and a compute node may
    # have no outbound route either. `bake --nixos --with-torch` puts the venv
    # in the image (and refuses to publish one that picked up a driver
    # library on the way). It is copied in under the plain name, so nothing
    # downstream has to know which of the two it got.
    if [[ -f $LEA_NIXOS_DIR/rootfs-torch.qcow2 ]]; then
        cp -L --no-preserve=mode "$LEA_NIXOS_DIR/rootfs-torch.qcow2" "$out/guest/rootfs.qcow2"
        info "  guest carries the torch venv"
    else
        cp -L --no-preserve=mode "$LEA_NIXOS_DIR/rootfs.qcow2" "$out/guest/rootfs.qcow2"
        warn "this guest image has NO torch venv -- the gate's torch stage will fail on a
       node with no outbound route. Rebuild it: build.sh bake --nixos --with-torch"
    fi
    info "  $(du -shL "$out/guest" | cut -f1)"
    # NOTHING OF NVIDIA'S DRIVER MAY LEAVE THIS MACHINE IN THE PACKAGE. The
    # guest is handed the host's libcuda at RUN time and it is not
    # redistributable (LICENSES.md); an image that acquired one during
    # provisioning would carry it into the tarball silently.
    local leak
    leak=$(find "$out/guest" "$out/probe-bin" -name 'libcuda.so*' -o -name 'libnvidia-ml.so*' 2>/dev/null | head -5)
    [[ -z $leak ]] || die "the package would contain NVIDIA driver libraries and must not be published:
$leak"
    info "  no NVIDIA driver library in the package"

    info "== probe binaries =="
    cp -a "$LEA_ROOT"/probe/bin/. "$out/probe-bin/"
    cp -aL "$LEA_ROOT"/probe/kernels/kernels.ptx "$out/probe-bin/" 2>/dev/null || true
    info "  $(find "$out/probe-bin" -type f | wc -l) files"

    # ---- the native reference ------------------------------------------------
    # THE GATE COMPARES THE GUEST AGAINST A NATIVE RUN, and on a cluster node
    # there is nothing native to run: no checkout, no vendor/hostvenv, and no
    # way to build one on a node that may have no outbound route. So the
    # package carries the reference venv, and it is created INSIDE the image
    # -- a venv records the interpreter it was made with, and the only python
    # that will exist at run time is the image's.
    #
    # Same wheels as the guest's venv, deliberately: the torch stage's claim
    # is that the numbers are bit-identical across the boundary, and that
    # claim is empty if the two sides run different builds of torch.
info "== native reference venv (torch, inside the image) =="
    rm -rf "$out/hostvenv"
    nix run nixpkgs#apptainer -- exec --bind "$out" "$out/sif/leandro-$driver.sif" \
        "$store/bin/leandro-build" --version >/dev/null 2>&1 || true
    nix run nixpkgs#apptainer -- exec --bind "$out" "$out/sif/leandro-$driver.sif" \
        /bin/sh -c "set -e
            for d in /nix/store/*-python3-*/bin; do PATH=\$d:\$PATH; done
            export PATH
            python3 -m venv $out/hostvenv
            $out/hostvenv/bin/pip install --quiet torch numpy
            # The wheels' C++/OpenMP runtime, exactly as in the guest: a
            # manylinux wheel expects a system libstdc++ and a pure Nix
            # image has none on any default path.
            for g in /nix/store/*-gcc-*-lib/lib /nix/store/*-zlib-*/lib; do LD_LIBRARY_PATH=\$g:\${LD_LIBRARY_PATH:-}; done
            export LD_LIBRARY_PATH
            $out/hostvenv/bin/python -c 'import torch,numpy; print(\"  reference venv:\", torch.__version__, numpy.__version__)'" \
        || die "could not build the native reference venv in the image"

    info "== manifest =="
    {
        echo "# Leandro cluster package"
        echo "built:            $stamp"
        echo "driver:           $driver"
        echo "ch:               $(tr -d '[:space:]' < "$LEA_ROOT/CH_VERSION")"
        echo "guest kernel:     $(sed -n 's/^LEA_NIXOS_KERNEL_VERSION=//p' "$out/guest/image.env")"
        echo "guest user:       $(sed -n 's/^LEA_NIXOS_USER=//p' "$out/guest/image.env")"
        echo "commit:           $(git -C "$LEA_ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"
        echo "commit dirty:     $(git -C "$LEA_ROOT" diff --quiet 2>/dev/null && echo no || echo YES)"
        echo "closure:          $store"
        echo "host gpu:         $(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1)"
        echo
        echo "# WHAT THE NODE MUST PROVIDE"
        echo "#   /dev/kvm, the assigned /dev/nvidia*, a writable scratch dir,"
        echo "#   apptainer (singularity is untested here), and the host NVIDIA userspace of"
        echo "#   EXACTLY driver $driver (apptainer --nv injects it)."
        echo "# NOT needed: root, a bridge, taps, NAT, nested virt beyond KVM."
        echo
        echo "# sha256"
        ( cd "$out" && find . -type f ! -name MANIFEST -print0 | sort -z \
            | xargs -0 sha256sum ) 2>/dev/null
    } > "$out/MANIFEST"

    info ""
    info "package: $out  ($(du -sh "$out" | cut -f1))"
    info ""
    info "Run the compute gate out of it (this host, or a cluster node):"
    info "  scripts/bench.sh slurm --package $out --run gate-1"
    info ""
    info "Generate sbatch scripts for a cluster:"
    info "  scripts/bench.sh slurm --package $out --out ./jobs --counts '1 2 4'"
}

# ---- all ------------------------------------------------------------------
DRY=0
do_all() {
    local mode=default jobs driver="" skip=0 yes=0 t0 pinned
    jobs=$(nproc 2>/dev/null || echo 4)
    while [[ $# -gt 0 ]]; do
        case $1 in
            --driver)      driver=$2; shift 2 ;;
            --minimal)     mode=minimal; shift ;;
            --full)        mode=full; shift ;;
            --jobs)        jobs=$2; shift 2 ;;
            --skip-checks) skip=1; shift ;;
            --yes|-y)      yes=1; shift ;;
            --dry-run)     DRY=1; shift ;;
            -h|--help)     usage 0 ;;
            *) error "unknown option: $1"; usage 2 ;;
        esac
    done
    t0=$(date +%s)
    step() {  # step <label> <command...>
        local label=$1 s0; shift
        s0=$(date +%s)
        echo; echo "${B}--> $label${R}"
        if [[ $DRY -eq 1 ]]; then dim "(dry-run) $*"; return 0; fi
        if "$@"; then ok "$label ($(( $(date +%s) - s0 ))s)"
        else bad "$label failed"; return 1; fi
    }
    local -a popt=(); [[ -n $driver ]] && popt+=(--driver "$driver"); [[ $skip -eq 1 ]] && popt+=(--skip-checks)
    do_preflight "${popt[@]}" || exit 1
    pinned=$PINNED

    say "plan  (mode: $mode, driver: $pinned, jobs: $jobs)"
    cat <<PLAN
  1  vendor    open-gpu-kernel-modules @ $pinned      ~170 MB
  2  ch        cloud-hypervisor @ $(tr -d '[:space:]' < "$LEA_ROOT/CH_VERSION") + patches/, built
  3  cargo     this workspace, release
  4  probes    the C probes and the PTX
  5  image     Ubuntu cloud image + kernel/initrd      ~641 MB
  6  hostvenv  the gate's native torch reference      ~2.5 GB
PLAN
    case $mode in
      minimal) echo "  7  (skipped: image bake -- --minimal)" ;;
      full)    echo "  7  bake --with-torch   provisioned image + torch   ~2.8 GB" ;;
      *)       echo "  7  bake                provisioned image           ~300 MB" ;;
    esac
    echo
    dim "Downloads dominate on a fast CPU; the cloud-hypervisor build dominates otherwise."
    dim "The host network is runtime state: 'showcase.sh net up' after this (sudo)."
    if [[ $DRY -eq 1 ]]; then echo; ok "dry run -- nothing changed"; exit 0; fi
    if [[ $yes -eq 0 && -t 0 ]]; then
        echo; read -rp "proceed? [Y/n] " a; [[ ${a:-y} =~ ^[Yy]?$ ]] || { echo "aborted."; exit 1; }
    fi
    export CARGO_BUILD_JOBS=$jobs
    step "1/7  NVIDIA headers @ $pinned"   do_vendor   || exit 1
    step "2/7  cloud-hypervisor + patches" do_ch       || exit 1
    step "3/7  build the workspace"        do_cargo    || exit 1
    step "4/7  build the probes"           do_probes   || exit 1
    step "5/7  guest image"                do_image    || exit 1
    # Not behind --full. The gpu gate's torch stage measures the guest
    # against this and can do nothing without it, so a build that leaves it
    # out does not deliver what this script's own header promises: "a rig
    # that can run showcase.sh AND THE GATES". It was --full for one commit
    # and that was the wrong call -- the operator's words were "there are no
    # extras required".
    step "6/7  native torch reference"     do_hostvenv || exit 1
    case $mode in
      minimal) echo; dim "7/7  image bake skipped (--minimal)" ;;
      full)    step "7/7  bake image (+torch)" do_bake --with-torch || exit 1 ;;
      *)       step "7/7  bake image"          do_bake              || exit 1 ;;
    esac
    # The baked image is only useful if the scripts actually pick it up. It
    # may also be one from an earlier run -- say which.
    local baked why="just baked"
    baked=$(ls -t "$LEA_VM_DIR"/guest-baked-*.qcow2 2>/dev/null | head -1)
    [[ $mode == minimal ]] && why="from an earlier run"

    say "ready  ($(( ($(date +%s) - t0) / 60 ))m $(( ($(date +%s) - t0) % 60 ))s)"
    "$LEA_ROOT/scripts/test.sh" check 2>&1 | tail -10
    echo
    echo "${B}Next:${R}"
    [[ -n $baked ]] && cat <<DONE
  ${DIM}# baked image ($why) -- use it as the base for every VM${R}
  export LEA_BASE_IMAGE=$baked

DONE
    cat <<DONE
  ${DIM}# the host network (runtime state, sudo; the NixOS module does this declaratively)${R}
  ./scripts/showcase.sh net up

  ${DIM}# the guided demonstration -- start here${R}
  ./scripts/showcase.sh demo --fast --pause

  ${DIM}# the acceptance gate (~70 s, needs the GPU exclusively)${R}
  ./scripts/test.sh gpu

  ${DIM}# one VM, by hand${R}
  ./scripts/showcase.sh up && ./scripts/showcase.sh ssh

  ${DIM}# several VMs on the one GPU${R}
  ./scripts/showcase.sh up --count 2

  ${DIM}# all eight Python suites green needs both knobs${R}
  LEA_MANAGED_COMPAT=1 ./scripts/showcase.sh up --max-pin-mib 3072
  ./probe/run/suites.sh

Documentation: DEVELOPMENT.md
DONE
}

case $CMD in
    all)          do_all "$@" ;;
    preflight)    DRY=0; do_preflight "$@" ;;
    vendor)       do_vendor ;;
    ch)           do_ch ;;
    cargo)        do_cargo ;;
    probes)       do_probes ;;
    hostvenv)     do_hostvenv ;;
    image)        do_image ;;
    bake)         do_bake "$@" ;;
    package)      do_package "$@" ;;
    check-driver) do_check_driver ;;
esac
