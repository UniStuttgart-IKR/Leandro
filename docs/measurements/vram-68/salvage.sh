#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# Finish the profile run by hand after the runner died on an edit-in-flight.
set -uo pipefail
LEA_ROOT=/home/silas/git/Leandro
source "$LEA_ROOT/scripts/lib/rig.sh"
OUT=$1; SAMPLER=$2
say() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }
# Wait for the load to end -- by asking the guest, not by a pattern here.
for i in $(seq 1 60); do
    a=$(lea_ssh 192.168.100.15 'pgrep -x vrampress >/dev/null && echo y || echo n' 2>/dev/null)
    b=$(lea_ssh 192.168.100.17 'pgrep -x vrampress >/dev/null && echo y || echo n' 2>/dev/null)
    [[ $a == n && $b == n ]] && break
    sleep 10
done
say "load finished"
sleep 20
kill "$SAMPLER" 2>/dev/null
for pair in 15:desktop 17:desktop2; do
    ip=192.168.100.${pair%%:*}; n=${pair##*:}
    lea_ssh "$ip" "export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 DISPLAY=:0
        cd ~/vram && sudo -E timeout 120 ./fbprobe 2>&1 | tail -14" > "$OUT/fbprobe-$n-after.txt" 2>&1
    say "fbprobe $n after: $(grep READER "$OUT/fbprobe-$n-after.txt")"
    cp "$LEA_VM_DIR/$n/nvrm.log" "$OUT/backend-$n.txt" 2>/dev/null
    lea_ssh "$ip" 'sudo dmesg | tail -80' > "$OUT/dmesg-$n.txt" 2>&1
    lea_ssh "$ip" 'sudo dmesg | grep -c "Failed to allocate NVKMS memory"' > "$OUT/nvkms-fail-$n.txt" 2>&1
    lea_ssh "$ip" 'nvidia-smi --query-gpu=name,memory.total,memory.used,memory.free --format=csv' \
        > "$OUT/guest-smi-$n.txt" 2>&1
done
nvidia-smi > "$OUT/host-smi-final.txt" 2>&1
say "taking the rig down"
for n in desktop desktop2; do "$LEA_ROOT/scripts/showcase.sh" down --name "$n" --force >>"$OUT/down.txt" 2>&1; done
say "salvage done"
