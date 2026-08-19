# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# Central configuration for the host-side scripts. Not a script -- source it
# (scripts/lib/common.sh does so when nobody has):
#   source "$LEA_ROOT/scripts/lib/config.sh"
#
# Everything that more than one script needs to agree on lives here and
# NOWHERE else: network numbers, bridge and tap names, image and binary
# paths, guest identity. A value hard-coded in a second place is a value
# that will drift, and network numbers drift silently -- a VM simply does
# not answer, and the error looks like a boot problem.
#
# Every name follows the LEA_* prefix (docs/NAMING.md rule 1). Each one may
# be overridden from the environment, which is what makes a second rig, a
# throw-away experiment, or a Nix store install (nix/packages/
# leandro-scripts.nix sets LEA_ROOT, LEA_BIN_DIR, LEA_CH, LEA_VM_DIR and
# LEA_TRACE_LIB)
# possible without editing this file.
#
# Every path here is ABSOLUTE once this file has run: relative overrides
# are resolved against LEA_ROOT. The scripts therefore never depend on the
# working directory, and nothing is ever written under LEA_ROOT itself --
# all state goes below LEA_VM_DIR (see there).
#
# WHERE THE ARTEFACTS GO is decided in ONE place: LEA_VM_DIR. Everything
# heavy and semi-permanent lives below it -- guest instances, base and baked
# images, the upstream cloud image, measurement outputs -- so pointing it at
# a big disk (a network block store, an NVMe pool) moves all of it at once.
# The per-checkout answer belongs in local.env next to this repository's
# root (git-ignored, read below); local.env.example shows the shape.

# ---- root -----------------------------------------------------------------
# The repository root (or $out/share/leandro in the store), resolved from
# THIS file rather than from $0: a script invoked through a symlink, a
# wrapper or from a subdirectory gets the same answer.
: "${LEA_ROOT:=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)}"

# ---- local.env: this checkout's answers ------------------------------------
# Read BEFORE the defaults below, so a value in it becomes this checkout's
# default. Written with the same `: "${VAR:=value}"` idiom, so a variable
# set in the environment still wins -- an experiment can override the file
# without editing it. Absent in a store install (the wrapper sets LEA_*).
if [[ -f $LEA_ROOT/local.env ]]; then
    # shellcheck source=/dev/null
    source "$LEA_ROOT/local.env"
fi

# _lea_abs VAR -- make a path variable absolute (relative to LEA_ROOT).
_lea_abs() {
    local -n _lea_ref=$1
    [[ $_lea_ref == /* ]] || _lea_ref=$LEA_ROOT/$_lea_ref
}

# ---- network --------------------------------------------------------------
# The /24 the guests live on. LEA_NET_PREFIX is the first three octets; the
# bridge itself takes .LEA_NET_GW, the guests count up from .LEA_IP_FIRST.
: "${LEA_NET_PREFIX:=192.168.100}"
: "${LEA_NET_GW:=1}"
: "${LEA_IP_FIRST:=10}"
: "${LEA_NETMASK:=24}"

# Bridge and taps. tap0..tap<LEA_MAX_VMS-1> hang off the bridge; instance
# index i uses tap i and IP .<LEA_IP_FIRST + i>.
: "${LEA_BRIDGE:=br-poco}"
: "${LEA_TAP_PREFIX:=tap}"
: "${LEA_MAX_VMS:=8}"

# ---- guest identity -------------------------------------------------------
: "${LEA_GUEST_USER:=leandro}"
: "${LEA_HOSTNAME_PREFIX:=leandro-dev}"

# ---- paths ----------------------------------------------------------------
# THE artefact store: instance directories (<name>/), the SSH key, base and
# baked images, the upstream guest image (guest-image/), output directories,
# script pidfiles. In a checkout it defaults to vm/; a store install puts it
# under ~/.local/state/leandro/vm; local.env moves it wherever the space is.
: "${LEA_VM_DIR:=vm}";                       _lea_abs LEA_VM_DIR
: "${LEA_SSH_KEY:=$LEA_VM_DIR/id_leandro}";  _lea_abs LEA_SSH_KEY
# Frozen copy of a fully provisioned disk. Fleet members 1..N are thin
# qcow2 overlays on it, which is why it must never be written again.
: "${LEA_FLEET_BASE:=$LEA_VM_DIR/base-torch.qcow2}"; _lea_abs LEA_FLEET_BASE
: "${LEA_DISK_SIZE:=40G}"

# The host-side binaries. LEA_BIN_DIR holds what `cargo build --release`
# produces (vhost-user-nvrm, vhost-user-input, nvrm-genhdr, mmapping,
# smipids, vsockconnect); the tracer library sits beside them in a checkout and under
# lib/ in the store, hence its own variable.
: "${LEA_BIN_DIR:=target/release}";          _lea_abs LEA_BIN_DIR
# The PROBE binaries. Normally probe/bin in the checkout, which
# lea_guest_setup rebuilds from source before every run -- probe/Makefile's
# warning says why: a checked-in prebuilt once made a gate grep for output
# strings no source produced any more, and it went green against text nothing
# printed. That invariant is "the binaries match the sources beside them",
# not "make runs every time", so a PACKAGE may carry them: build.sh package
# builds them from the same tree it packages and records the commit, and then
# points this at them. Where it does, LEA_ROOT is read-only (a store install)
# and make could not run anyway.
: "${LEA_PROBE_BIN:=$LEA_ROOT/probe/bin}";   _lea_abs LEA_PROBE_BIN
# The NATIVE REFERENCE the gpu gate's torch stage measures against: the same
# pip wheels the guest runs, on the host, so that the comparison is two
# transport paths and not two libraries (DEVELOPMENT.md section 3). In a
# checkout that is vendor/hostvenv, made by hand once. A package carries its
# own, because a cluster node has neither the checkout nor a way to build one.
: "${LEA_HOSTVENV:=$LEA_ROOT/vendor/hostvenv}"; _lea_abs LEA_HOSTVENV
: "${LEA_TRACE_LIB:=$LEA_BIN_DIR/libnvrm_trace.so}"; _lea_abs LEA_TRACE_LIB
# The ssh ProxyCommand helper for the vsock transport. Its own variable for
# the same reason the tracer has one: a store install puts the binaries
# somewhere LEA_BIN_DIR names and this has to follow them.
: "${LEA_VSOCK_CONNECT:=$LEA_BIN_DIR/vsockconnect}"; _lea_abs LEA_VSOCK_CONNECT
: "${LEA_CH:=vendor/cloud-hypervisor/target/release/cloud-hypervisor}"; _lea_abs LEA_CH

# The upstream guest image (Ubuntu cloud image, kernel, initrd -- pins in
# GUEST_IMAGE), below the artefact store like everything heavy. The BASE
# image every instance overlays defaults to that cloud image; after
# `build.sh bake` point it at the baked one (local.env is the place).
: "${LEA_IMAGE_DIR:=$LEA_VM_DIR/guest-image}"; _lea_abs LEA_IMAGE_DIR
: "${LEA_BASE_IMAGE:=$LEA_IMAGE_DIR/ubuntu-24.04-server-cloudimg-amd64.img}"; _lea_abs LEA_BASE_IMAGE
: "${LEA_KERNEL:=$LEA_IMAGE_DIR/vmlinuz}";   _lea_abs LEA_KERNEL
: "${LEA_INITRD:=$LEA_IMAGE_DIR/initrd}";    _lea_abs LEA_INITRD

# The NIXOS guest image (build.sh bake --nixos), and it is a DIRECTORY rather
# than a file: direct kernel boot needs three artefacts -- kernel, initrd,
# rootfs -- plus one fact, which `init=` the image's system generation is, and
# an image that lets any of the four be supplied separately is an image that
# can be booted in an inconsistent combination. They are published together
# and read together (image.env, lea_nixos_image in rig.sh).
: "${LEA_NIXOS_DIR:=$LEA_VM_DIR/guest-nixos}"; _lea_abs LEA_NIXOS_DIR
# The frozen base a NIXOS fleet member overlays, and it exists for exactly
# one reason: the torch venv. Everything else a NixOS guest needs is in the
# image, so a member could overlay that directly -- but the gate and the
# fleet bench measure against the HOST's reference venv (vendor/hostvenv,
# torch 2.13.0+cu130), which means pip wheels rather than nixpkgs' torch, and
# a 2.5 GiB download per member. One provisioned disk, frozen, spares three
# of them. ABSENT IS NOT AN ERROR here (unlike LEA_FLEET_BASE): the image
# alone works, each member just pays for its own venv.
: "${LEA_NIXOS_FLEET_BASE:=$LEA_VM_DIR/base-nixos-torch.qcow2}"; _lea_abs LEA_NIXOS_FLEET_BASE

# Where the host driver's 32-bit userspace lives (Arch: lib32-nvidia-utils).
# The 64-bit half is DISCOVERED (lea_nvidia_libdir in common.sh); the 32-bit
# half has no ldconfig entry to ask, so it is a setting.
: "${LEA_NVIDIA_LIB32_DIR:=/usr/lib32}"

# ---- VM defaults ----------------------------------------------------------
: "${LEA_CPUS:=4}"
: "${LEA_MEM:=4096}"
# The desktop rig: GNOME plus a game wants more of both (measured 2026-08-15).
: "${LEA_DESKTOP_CPUS:=8}"
: "${LEA_DESKTOP_MEM:=16384}"
# The virtual display NVKMS is asked to invent.
: "${LEA_VDISPLAY_SIZE:=1920x1080}"
: "${LEA_VDISPLAY_HZ:=60}"

# ---- derived --------------------------------------------------------------
# The bridge address, i.e. the guests' default gateway.
#
# Read by the scripts that source this file, never inside it -- and the
# inside is all a linter can see. It is also the value
# scripts/lib/common.sh probes to decide whether this file still needs
# sourcing, so it must stay derived rather than defaulted.
#
# WARNING: do not start a comment line here with the word that follows the
# hash on the next line. ShellCheck reads any such line as a directive and
# reports SC1072/SC1073 on prose -- which then makes every script that
# sources this one report SC1094 as well.
# shellcheck disable=SC2034
LEA_GATEWAY="$LEA_NET_PREFIX.$LEA_NET_GW"

# lea_ip <index>  -> IP of instance <index>
lea_ip() { echo "$LEA_NET_PREFIX.$((LEA_IP_FIRST + $1))"; }

# lea_tap <index> -> tap device of instance <index>
lea_tap() { echo "$LEA_TAP_PREFIX$1"; }

# ---- the vsock transport --------------------------------------------------
# The SECOND transport: instead of an address on a bridge, one unix socket per
# instance speaking cloud-hypervisor's hybrid vsock. Slot i's two host-side
# endpoints are named by FORMULA here, exactly as lea_ip and lea_tap name its
# address and its tap -- so that an index is all anybody needs to find them,
# and there is no table to keep in step with anything.
#
# WHY THE CID IS NOT A SHARED RESOURCE, and this is the finding the cluster
# story rests on. Measured 2026-08-19 with cloud-hypervisor v53.0.0: its vsock
# is implemented in USERSPACE. It never opens /dev/vhost-vsock -- the module
# was not loaded before the run and was still not loaded after it, and the VM
# process held no file descriptor on the device. So there is no host kernel
# vsock address space for two VMs to collide in: the guest CID is a number
# that VM sees, and everything host-side is reached through the SOCKET PATH.
# Two guests may hold the same CID as long as their sockets differ.
#
# WARNING: AF_UNIX sun_path is 108 bytes including the terminator, and the
# error for crossing it names nothing. That is why the socket sits directly
# under LEA_VM_DIR with a short name rather than inside the instance
# directory: it keeps the longest component out of the path on a cluster
# scratch directory, which is where the limit is actually reachable.
# lea_vsock_connect checks the length and says so.
: "${LEA_VSOCK_CID_BASE:=3}"

# lea_vsock_cid <index> -> guest CID of instance <index>. CIDs 0, 1 and 2 are
# reserved by the vsock specification, hence the base of 3.
lea_vsock_cid() { echo "$((LEA_VSOCK_CID_BASE + $1))"; }

# lea_vsock_sock <index> -> host-side hybrid vsock socket of instance <index>
lea_vsock_sock() { echo "$LEA_VM_DIR/vsock$1.sock"; }

# lea_inst_dir <name> -> the directory holding everything of one instance
lea_inst_dir() { echo "$LEA_VM_DIR/$1"; }
