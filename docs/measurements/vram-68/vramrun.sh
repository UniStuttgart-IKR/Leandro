#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# The acceptance run for OPEN-QUESTIONS 68: two guests on one card, the
# same load in both, once per policy.
#
#   vramrun.sh profile|limit MIB OUTDIR [SECONDS] [--fill PCT|--max]
#
# Holds the serial lock for the WHOLE run -- up, load, readings, down --
# because the VMs it starts outlive any shorter holder (matrix.sh).
set -uo pipefail
LEA_ROOT=${LEA_ROOT:-/home/silas/git/Leandro}
source "$LEA_ROOT/scripts/lib/rig.sh"
source "$LEA_ROOT/scripts/lib/matrix.sh"

POLICY=$1; MIB=$2; OUT=$3; SECONDS_LOAD=${4:-600}; MODE=${5:---fill}; PCT=${6:-85}
mkdir -p "$OUT"
case $POLICY in
    profile) FLAG=(--vram-profile "$MIB") ;;
    limit)   FLAG=(--vram-limit "$MIB") ;;
    *) echo "policy must be profile or limit"; exit 2 ;;
esac
NAMES=(desktop desktop2)
IDX=(5 7)

say() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }

lea_matrix_serial || { echo "no lock"; exit 1; }
say "lock held, policy=$POLICY ${MIB} MiB, load ${SECONDS_LOAD}s $MODE $PCT"

# --- up -------------------------------------------------------------------
"$LEA_ROOT/scripts/showcase.sh" net up >"$OUT/net.txt" 2>&1
for i in 0 1; do
    n=${NAMES[$i]}
    say "up $n"
    LEA_SUN_CAPTURE=kms "$LEA_ROOT/scripts/showcase.sh" up --name "$n" --index "${IDX[$i]}" \
        --session gnome --wayland "${FLAG[@]}" --cpus 6 --mem 8192 \
        >"$OUT/up-$n.txt" 2>&1 || { say "up $n FAILED -- see $OUT/up-$n.txt"; tail -20 "$OUT/up-$n.txt"; }
done
for i in 0 1; do
    n=${NAMES[$i]}
    grep -iE "VRAM (cap|profile)" "$LEA_VM_DIR/$n/nvrm.log" | head -3 | sed "s/^/$n: /" | tee -a "$OUT/policy.txt"
done

ip_of() { lea_inst "$1" >/dev/null; echo "$INST_IP"; }
IP0=$(ip_of desktop); IP1=$(ip_of desktop2)
say "guests at $IP0 $IP1"

# --- the host sampler, 1 Hz ----------------------------------------------
PID0=$(cat "$LEA_VM_DIR/desktop/nvrm.pid" 2>/dev/null)
PID1=$(cat "$LEA_VM_DIR/desktop2/nvrm.pid" 2>/dev/null)
say "backend pids $PID0 $PID1"
(
  echo "ts,used_mib,free_mib,util,enc_sessions,enc_fps,desktop_mib,desktop2_mib"
  while :; do
    g=$(nvidia-smi --query-gpu=memory.used,memory.free,utilization.gpu,encoder.stats.sessionCount,encoder.stats.averageFps \
        --format=csv,noheader,nounits 2>/dev/null | head -1 | tr -d ' ')
    a=$(nvidia-smi --query-compute-apps=pid,used_memory --format=csv,noheader,nounits 2>/dev/null | tr -d ' ')
    d0=$(awk -F, -v p="$PID0" '$1==p{print $2}' <<<"$a"); d1=$(awk -F, -v p="$PID1" '$1==p{print $2}' <<<"$a")
    printf '%s,%s,%s,%s\n' "$(date +%H:%M:%S)" "$g" "${d0:-0}" "${d1:-0}"
    sleep 1
  done
) > "$OUT/host-1hz.csv" &
SAMPLER=$!
lea_on_exit "kill $SAMPLER 2>/dev/null; true"

# --- the guests: fbprobe, an animation, then the load ---------------------
for ip in $IP0 $IP1; do
    lea_ssh "$ip" 'mkdir -p ~/vram' >/dev/null 2>&1
    scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -i "$LEA_SSH_KEY" \
        "$LEA_ROOT/scripts/guest/fbprobe.c" "$LEA_GUEST_USER@$ip:~/vram/fbprobe.c" 2>/dev/null
    lea_ssh "$ip" 'cd ~/vram && gcc -O2 -o fbprobe fbprobe.c -ldl -lEGL 2>&1 | tail -3; ls -l fbprobe' \
        >>"$OUT/fbprobe-build.txt" 2>&1
done

# SOMETHING HAS TO ANIMATE, or fbprobe reads STATIC on a guest that is
# perfectly healthy -- which is the frozen signature. Measured in the first
# run of this script: with nothing drawing, all three readers said STATIC
# 0/9 and the verdict was AMBER, on two guests that were fine. glxgears is
# not installed in this image and the session is Wayland; vkcube-wayland is
# there and is what the frame limiter was measured with.
gears() {
    lea_ssh "$1" "export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0
        pgrep -x vkcube-wayland >/dev/null || (setsid nohup vkcube-wayland >/tmp/vkcube.log 2>&1 </dev/null &)
        sleep 3; pgrep -x vkcube-wayland >/dev/null && echo 'vkcube up' || { echo 'VKCUBE DID NOT START'; tail -3 /tmp/vkcube.log; }"
}
for ip in $IP0 $IP1; do gears "$ip" | tee -a "$OUT/gears.txt"; done

fbread() {   # fbread <ip> <name> <tag>
    lea_ssh "$1" "export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 DISPLAY=:0
        cd ~/vram && sudo -E timeout 120 ./fbprobe 2>&1 | tail -30" >"$OUT/fbprobe-$2-$3.txt" 2>&1
    say "fbprobe $2 $3: $(grep -E 'READER|STATIC|MOVING' "$OUT/fbprobe-$2-$3.txt" | tr '\n' ' ' | cut -c1-160)"
}
for i in 0 1; do fbread "$([[ $i == 0 ]] && echo "$IP0" || echo "$IP1")" "${NAMES[$i]}" before; done
# ... and the baseline is taken AFTER the animation is up, or it is not a
# baseline: it is the same STATIC the freeze produces.

# guest-side 5 s samples: what the guest believes it has
declare -a GSAMP
for i in 0 1; do
    ip=$([[ $i == 0 ]] && echo "$IP0" || echo "$IP1")
    ( lea_ssh "$ip" 'for i in $(seq 1 400); do
          printf "%s," "$(date -u +%H:%M:%S)"
          nvidia-smi --query-gpu=memory.total,memory.used,memory.free --format=csv,noheader,nounits | tr -d " " | head -1
          sleep 5
      done' > "$OUT/guest-${NAMES[$i]}.csv" 2>&1 ) &
    GSAMP[$i]=$!
done

declare -a LOADPID
say "starting the load in both guests"
for i in 0 1; do
    ip=$([[ $i == 0 ]] && echo "$IP0" || echo "$IP1")
    if [[ $MODE == --max ]]; then
        ( lea_ssh "$ip" "cd ~/gpu && ./vrampress --max --seconds $SECONDS_LOAD" \
            > "$OUT/vrampress-${NAMES[$i]}.csv" 2>&1 ) &
    else
        ( lea_ssh "$ip" "cd ~/gpu && ./vrampress --fill $PCT --seconds $SECONDS_LOAD" \
            > "$OUT/vrampress-${NAMES[$i]}.csv" 2>&1 ) &
    fi
    LOADPID[$i]=$!
done

# --- readings DURING the load --------------------------------------------
sleep 120
for i in 0 1; do fbread "$([[ $i == 0 ]] && echo "$IP0" || echo "$IP1")" "${NAMES[$i]}" during; done
HALF=$(( SECONDS_LOAD > 400 ? SECONDS_LOAD - 300 : 60 ))
sleep "$HALF"
for i in 0 1; do fbread "$([[ $i == 0 ]] && echo "$IP0" || echo "$IP1")" "${NAMES[$i]}" late; done

wait "${LOADPID[0]}" "${LOADPID[1]}" 2>/dev/null
sleep 30
say "load done"
for i in 0 1; do fbread "$([[ $i == 0 ]] && echo "$IP0" || echo "$IP1")" "${NAMES[$i]}" after; done

# --- collect --------------------------------------------------------------
# By the PID that was recorded when it started -- never by pattern:
# pkill -f matches the killing command's own line (docs/llm.md).
kill $SAMPLER "${GSAMP[0]}" "${GSAMP[1]}" 2>/dev/null
for i in 0 1; do
    n=${NAMES[$i]}; ip=$([[ $i == 0 ]] && echo "$IP0" || echo "$IP1")
    cp "$LEA_VM_DIR/$n/nvrm.log" "$OUT/backend-$n.txt" 2>/dev/null
    lea_ssh "$ip" 'sudo dmesg | tail -60' > "$OUT/dmesg-$n.txt" 2>&1
    lea_ssh "$ip" 'sudo dmesg | grep -c "Failed to allocate NVKMS memory"' > "$OUT/nvkms-fail-$n.txt" 2>&1
    lea_ssh "$ip" 'nvidia-smi --query-gpu=name,memory.total,memory.used,memory.free --format=csv' \
        > "$OUT/guest-smi-$n.txt" 2>&1
done
nvidia-smi > "$OUT/host-smi-final.txt" 2>&1

say "taking the rig down"
for n in "${NAMES[@]}"; do "$LEA_ROOT/scripts/showcase.sh" down --name "$n" --force >>"$OUT/down.txt" 2>&1; done
say "done -- $OUT"
