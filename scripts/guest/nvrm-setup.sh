#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# In the GUEST, once per boot: load and provision the nvrm_nodes kernel
# module.
#
# What used to be crutches is now done by the module: device nodes with the
# right majors (devtmpfs creates them), /proc/devices is correct through the
# real chrdev registration, and /proc/driver/nvidia/params comes from the
# host's file (params.txt).
#
# Afterwards CUDA runs in the guest as an ORDINARY USER -- no sudo needed,
# because the module takes over the GPA resolution (pagemap needs root).
#
#   ./nvrm-setup.sh [--persist]
#
# --persist makes the state survive a reboot. If virtio_nvrm.ko sits beside
# it, the COEXISTENCE is persisted, otherwise only nvrm_nodes -- in both
# cases a reboot afterwards suffices, without a single setup command.
# /usr/bin/env bash, not /bin/bash: a NixOS guest has /bin/sh and
# /usr/bin/env and NOTHING else in /bin -- measured 2026-08-18, where
# this script died as `/bin/bash: bad interpreter`.
set -e
cd "$(dirname "$0")"

MODDIR=${LEA_MODDIR:-$HOME/guest-module/nvrm_nodes}
NVRMDIR=${LEA_NVRMDIR:-$HOME/guest-module/virtio_nvrm}
PARAMS=${LEA_PARAMS:-$PWD/params.txt}
PERSIST=0
[[ ${1:-} == --persist ]] && PERSIST=1

[[ -f $PARAMS ]] || { echo "ERROR: $PARAMS missing (from the host's /proc/driver/nvidia/params)"; exit 1; }

# 0) A LOADED virtio_nvrm (from a persistent boot) owns the node majors --
#    it has to fall first, otherwise the full load below stays busy. The
#    coexistence is re-established afterwards (lea_guest_build_nvrm).
#
#    ONLY when nvrm_nodes is not up yet. With both modules loaded the
#    coexistence is already the state this script exists to produce, and
#    tearing it down to rebuild it fails on a guest that is USING the card:
#    on a running desktop nvidia_modeset holds virtio_nvrm, `rmmod` answers
#    "Module virtio_nvrm is in use by: nvidia_modeset", and a provisioning
#    run that had nothing to do aborts -- which is what `showcase.sh up
#    --with-torch` against a live desktop guest did (measured 2026-08-19).
if ! lsmod | grep -q '^nvrm_nodes ' && lsmod | grep -q '^virtio_nvrm '; then
    sudo rmmod virtio_nvrm || {
        echo "ERROR: virtio_nvrm is loaded and in use, so nvrm_nodes cannot take"
        echo "       the node majors. Something in this guest holds the GPU --"
        echo "       a desktop session, or a process with an open FD. Stop it, or"
        echo "       recycle the guest: showcase.sh up --name <n> --keep-vm ..."
        exit 1
    }
    echo "unloaded leftover virtio_nvrm (from a persistent boot)."
fi

# 1) Build the module if needed, and load it.
if ! lsmod | grep -q '^nvrm_nodes '; then
    if [[ ! -f $MODDIR/nvrm_nodes.ko ]]; then
        echo "building the module in $MODDIR ..."
        make -C "$MODDIR" >/dev/null
    fi
    sudo insmod "$MODDIR/nvrm_nodes.ko"
    echo "module loaded."
fi

# 2) Set /proc/driver/nvidia/params from the host's file.
sudo "$MODDIR/nvrm-nodes-tool" provision params "$PARAMS"

# 3) The DRM node belongs to the render group -- the transport runs over
#    it. That is the ordinary Linux convention, not a crutch.
if ! id -nG | grep -qw render; then
    sudo usermod -aG render,video "$USER"
    echo "NOTE: $USER added to render/video -- log in again (or 'newgrp render')."
fi

if [[ $PERSIST -eq 1 ]]; then
    KREL=$(uname -r)
    sudo mkdir -p /lib/modules/"$KREL"/extra
    sudo cp "$MODDIR/nvrm_nodes.ko" /lib/modules/"$KREL"/extra/
    sudo cp "$MODDIR/nvrm-nodes-tool" /usr/local/bin/nvrm-nodes-tool
    sudo cp "$PARAMS" /etc/nvrm-params.txt

    WITH_NVRM=0
    [[ -f $NVRMDIR/virtio_nvrm.ko ]] && WITH_NVRM=1
    [[ $WITH_NVRM -eq 1 ]] && sudo cp "$NVRMDIR/virtio_nvrm.ko" /lib/modules/"$KREL"/extra/
    sudo depmod -a

    # ORDER IS SEMANTICS, and that is exactly why it lives in ONE script
    # instead of in modules-load.d: nvrm_nodes.ko must come with
    # create_nodes=0 (otherwise it takes the nodes virtio_nvrm is meant to
    # own), params must be in place BEFORE the first CUDA start, and
    # virtio_nvrm.ko comes last. modules-load.d cannot order those three
    # steps -- it only knows "load this module".
    sudo tee /usr/local/sbin/nvrm-boot.sh >/dev/null <<'BOOT'
#!/bin/sh
# Called by the nvrm-boot.service unit. Idempotent: whatever is already
# loaded stays as it is.
set -e
lsmod | grep -q '^nvrm_nodes ' || modprobe nvrm_nodes create_nodes=0
/usr/local/bin/nvrm-nodes-tool provision params /etc/nvrm-params.txt
if [ -f /lib/modules/"$(uname -r)"/extra/virtio_nvrm.ko ]; then
    lsmod | grep -q '^virtio_nvrm ' || modprobe virtio_nvrm
fi
BOOT
    sudo chmod +x /usr/local/sbin/nvrm-boot.sh
    # nvrm_nodes is loaded by the script, not by modules-load.d: only that
    # way does the create_nodes=0 parameter reliably come BEFORE the
    # provisioning.
    sudo rm -f /etc/modules-load.d/nvrm.conf
    sudo tee /etc/systemd/system/nvrm-boot.service >/dev/null <<EOF
[Unit]
Description=nvrm_nodes: load the modules (coexistence) and provision params
After=systemd-modules-load.service
Requires=systemd-modules-load.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/sbin/nvrm-boot.sh

[Install]
WantedBy=multi-user.target
EOF
    sudo systemctl daemon-reload
    sudo systemctl enable nvrm-boot.service >/dev/null
    if [[ $WITH_NVRM -eq 1 ]]; then
        echo "persistent: nvrm_nodes (create_nodes=0) + params + virtio_nvrm survive the reboot."
    else
        echo "persistent: nvrm_nodes + params survive the reboot (virtio_nvrm.ko was not in $NVRMDIR)."
    fi
fi

echo "ready. CUDA now runs without sudo:"
echo "  ./nvprobe 3"
