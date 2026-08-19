#!/bin/sh
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# The mediated PCI identity, in a private mount namespace, and then run a
# command inside it.
#
#   display-identity.sh <command...>
#
# WHY THIS EXISTS, in one paragraph. Everything about the card already works
# through virtio-nvrm -- CUDA, NVML, NVKMS, the DRM node. What does NOT work
# is the X driver's own probe: nvidia_drv.so carries a PCI id list and asks
# libpciaccess for a device with vendor 0x10de. Our DRM node hangs off a
# virtio device, so it never finds one. This script gives it one, WITHOUT
# touching the VMM, by bind-mounting a doctored copy of /sys/bus/pci/devices
# into a namespace that dies with the command.
#
# ONE device is corrected, and the correction was measured: NVIDIA --
# 10de:1f02 with the card's real 64-byte config header, at the address the
# GUEST gave the mediating virtio device. Not at the address RM reports:
# that one is the HOST's, and no such bus exists in here. The BDF is read
# from the virtio device's PCI parent, exactly the way virtio_nvrm's BDF
# mediation reads it, so the two can never disagree. Class 0x030200 (3D
# controller), boot_vga=0 -- the Optimus shape. With 0x030000 X makes it the
# PRIMARY screen and the display GPU goes unused. NVIDIA is the sole DRM
# device in the guest; there is no second display adapter to correct.
#
# TWO views, and they are not the same files. /sys/bus/pci/devices/<bdf> is a
# SYMLINK into /sys/devices/pci0000:00/<bdf>, and libdrm reaches the second
# one by a different road entirely: /sys/dev/char/<major>:<minor>/device.
# Bind-mounting only the first therefore corrects the X driver's probe
# (libpciaccess) and NOT libdrm's -- measured 2026-08-14 as NVIDIA's EGL
# declining the GBM platform with EGL_NO_DISPLAY, and glamor falling back to
# ShadowFB, i.e. software rendering on the path whose whole point is the GPU.
#
# So the identity files are ALSO bound one by one over the real device
# directory. One by one, and not the directory: /sys/devices/.../<bdf>/drm/
# lives there too, and covering it would break /sys/dev/char/<maj>:<min>
# altogether -- a worse answer than the wrong vendor id.
set -eu

[ $# -gt 0 ] || { echo "usage: display-identity.sh <command...>" >&2; exit 2; }

CFG=${LEA_NVCFG:-$HOME/.lea-nvcfg.bin}
[ -r "$CFG" ] || { echo "no config-space blob at $CFG -- the display stage (showcase.sh up --display) ships it" >&2; exit 1; }

NODE=$(readlink -f /sys/bus/virtio/drivers/virtio_nvrm/virtio*/ 2>/dev/null | head -1)
[ -n "$NODE" ] || { echo "virtio_nvrm is not bound -- load the guest module first" >&2; exit 1; }
NVBDF=$(basename "$(dirname "$NODE")")
case "$NVBDF" in
    0000:*) ;;
    *) echo "virtio_nvrm has no PCI parent ($NVBDF)" >&2; exit 1 ;;
esac

FAKE=/run/lea-pci-identity
rm -rf "$FAKE"; mkdir -p "$FAKE"
# --no-dereference keeps the entries as symlinks. Their targets are relative
# (../../../devices/...) and resolve correctly only AFTER the bind mount puts
# them back at /sys/bus/pci/devices -- which is why this is a copy and not a
# separate tree somewhere else.
cp -a --no-dereference /sys/bus/pci/devices/. "$FAKE/"

write_dev() {
    d=$1; vendor=$2; device=$3; class=$4; bootvga=$5
    rm -f "$d"; mkdir -p "$d"
    printf '%s\n' "$vendor" > "$d/vendor"
    printf '%s\n' "$device" > "$d/device"
    printf '%s\n' "$class"  > "$d/class"
    printf '%s\n' "$bootvga" > "$d/boot_vga"
    printf '0\n' > "$d/irq"
    printf '1\n' > "$d/enable"
    # No BARs. Declaring windows with nothing behind them is the variant
    # where a mistake surfaces far from its cause; without them the probe
    # came through anyway, measured three times.
    : > "$d/resource"
    i=0
    while [ $i -lt 7 ]; do
        printf '0x0000000000000000 0x0000000000000000 0x0000000000000000\n' >> "$d/resource"
        i=$((i + 1))
    done
}

D="$FAKE/$NVBDF"
write_dev "$D" 0x10de 0x1f02 0x030200 0
cp "$CFG" "$D/config"
printf '0x1458\n' > "$D/subsystem_vendor"
printf '0x4011\n' > "$D/subsystem_device"
printf '0xa1\n'   > "$D/revision"
MSG="nvidia -> $NVBDF"

# The REAL directory, resolved BEFORE the bind mount -- afterwards
# /sys/bus/pci/devices/<bdf> points into the copy and this would be circular.
REALNV=$(readlink -f "/sys/bus/pci/devices/$NVBDF")

# Only the attributes that carry the IDENTITY. `resource`, `enable` and `irq`
# describe real hardware state and stay real; the doctored ones in $FAKE are
# for the probe that reads them through the other view.
ID_FILES="vendor device subsystem_vendor subsystem_device revision class config"

echo "display-identity: $MSG" >&2
export FAKE D REALNV ID_FILES
exec unshare -m sh -c '
    mount --bind "$FAKE" /sys/bus/pci/devices
    bind_ids() {
        src=$1; dst=$2
        [ -n "$dst" ] && [ -d "$dst" ] || return 0
        for f in $ID_FILES; do
            [ -e "$src/$f" ] && [ -e "$dst/$f" ] || continue
            mount --bind "$src/$f" "$dst/$f" || \
                echo "display-identity: could not bind $f onto $dst" >&2
        done
    }
    bind_ids "$D" "$REALNV"
    exec "$@"' sh "$@"
