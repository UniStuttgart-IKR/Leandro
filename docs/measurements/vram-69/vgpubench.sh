#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# One row of the density benchmark for OPEN-QUESTIONS 68/69: N compute VMs
# on one card under one policy, all of them pressing on VRAM at once.
#
#   vgpubench.sh POLICY COUNT OUTDIR [SECONDS] [MEM_MIB]
#     POLICY   off | limit:<MiB> | profile:<MiB> | grid:<type>
#
# THE HOST IS A TENANT TOO and this refuses to start a run that would take
# the machine down with it: RAM for the guests plus a floor for the host,
# and vCPUs no more than 2x the threads there are.
set -uo pipefail
LEA_ROOT=${LEA_ROOT:-/home/silas/git/Leandro}
source "$LEA_ROOT/scripts/lib/rig.sh"
source "$LEA_ROOT/scripts/lib/matrix.sh"

POLICY=$1; COUNT=$2; OUT=$3; SECS=${4:-120}; MEM=${5:-2048}
CPUS=${LEA_BENCH_CPUS:-2}
mkdir -p "$OUT"
say() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$OUT/run.txt"; }

# ---- host guard rails ----------------------------------------------------
avail=$(free -m | awk '/^Mem:/{print $7}')
need=$((COUNT * MEM))
if (( need + 4096 > avail )); then
    echo "REFUSED: $COUNT x $MEM MiB = $need MiB, host has $avail MiB available and needs a 4096 MiB floor"
    exit 1
fi
threads=$(nproc)
if (( COUNT * CPUS > 2 * threads )); then
    echo "REFUSED: $COUNT x $CPUS vCPU against $threads threads is past 2x"
    exit 1
fi
host_vram=$(nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits | head -1)

lea_matrix_serial || { echo "no lock"; exit 1; }
say "policy=$POLICY count=$COUNT mem=${MEM} cpus=$CPUS load=${SECS}s host already holds ${host_vram} MiB of VRAM and ${avail} MiB of RAM free"

# ---- the policy ----------------------------------------------------------
export LEA_VRAM_LIMIT_MIB="" LEA_VRAM_PROFILE_MIB="" LEA_VGPU_TYPE=""
case $POLICY in
    off)        ;;
    limit:*)    export LEA_VRAM_LIMIT_MIB="${POLICY#limit:}" ;;
    profile:*)  export LEA_VRAM_PROFILE_MIB="${POLICY#profile:}" ;;
    grid:*)     export LEA_VGPU_TYPE="${POLICY#grid:}" ;;
    *) echo "unknown policy $POLICY"; exit 2 ;;
esac
"$LEA_BIN_DIR/vgpuprofile" > "$OUT/catalogue.txt" 2>&1

# ---- up ------------------------------------------------------------------
export LEA_CPUS=$CPUS LEA_MEM=$MEM
"$LEA_ROOT/scripts/showcase.sh" net up --count "$COUNT" >"$OUT/net.txt" 2>&1
# The FLEET path: thin overlays on the frozen base, which is what makes N
# guests affordable at all. `up --count N` is lea_fleet_up, and the policy
# reaches each member's backend through the environment exported above.
if "$LEA_ROOT/scripts/showcase.sh" up --count "$COUNT" --mem "$MEM" >"$OUT/up.txt" 2>&1; then
    up_ok=$COUNT
else
    up_ok=$(grep -c ": up$" "$OUT/up.txt" 2>/dev/null || echo 0)
    say "fleet incomplete -- $(grep -m1 -iE 'INSUFFICIENT|refused|homogeneous|failed' "$OUT/up.txt" | cut -c1-160)"
fi
say "up: $up_ok of $COUNT"
(( up_ok > 0 )) || { say "nothing came up"; exit 1; }

for i in $(seq 0 $((COUNT - 1))); do
    [[ -f $LEA_VM_DIR/vm$i/nvrm.log ]] && grep -m1 -iE "VRAM (cap|profile)|vGPU-shaped" \
        "$LEA_VM_DIR/vm$i/nvrm.log" | sed "s/^/vm$i: /" >> "$OUT/policy.txt"
done

# ---- the host sampler ----------------------------------------------------
pids=""
for i in $(seq 0 $((COUNT - 1))); do
    p=$(cat "$LEA_VM_DIR/vm$i/nvrm.pid" 2>/dev/null || echo 0)
    pids="$pids $p"
done
say "backend pids:$pids"
(
  echo "ts,used_mib,free_mib,util,ram_avail_mib,$(for i in $(seq 0 $((COUNT-1))); do printf 'vm%s,' $i; done)"
  while :; do
    g=$(nvidia-smi --query-gpu=memory.used,memory.free,utilization.gpu --format=csv,noheader,nounits | head -1 | tr -d ' ')
    a=$(nvidia-smi --query-compute-apps=pid,used_memory --format=csv,noheader,nounits | tr -d ' ')
    r=$(free -m | awk '/^Mem:/{print $7}')
    line="$(date +%H:%M:%S),$g,$r"
    for p in $pids; do
        v=$(awk -F, -v p="$p" '$1==p{print $2}' <<<"$a")
        line="$line,${v:-0}"
    done
    echo "$line"
    sleep 1
  done
) > "$OUT/host-1hz.csv" &
SAMPLER=$!
lea_on_exit "kill $SAMPLER 2>/dev/null; true"

# ---- the load ------------------------------------------------------------
say "load: vrampress --max in every guest"
# WAIT ON THE LOADS BY PID, never on a bare `wait`: the 1 Hz sampler is a
# background job of this same shell and never exits, so `wait` sat there
# forever with the load long finished and the rig still up. Measured the
# hard way at 21:46 on 2026-08-21.
declare -a LOADPID=()
for i in $(seq 0 $((COUNT - 1))); do
    lea_running "$LEA_VM_DIR/vm$i/ch.pid" || continue
    ( lea_ssh "$(lea_ip "$i")" "cd ~/gpu && ./vrampress --max --seconds $SECS" \
        > "$OUT/vrampress-vm$i.csv" 2>&1 ) &
    LOADPID+=("$!")
done
for p in "${LOADPID[@]}"; do wait "$p"; done
sleep 5
kill $SAMPLER 2>/dev/null

# ---- collect -------------------------------------------------------------
for i in $(seq 0 $((COUNT - 1))); do
    [[ -f $LEA_VM_DIR/vm$i/nvrm.log ]] && cp "$LEA_VM_DIR/vm$i/nvrm.log" "$OUT/backend-vm$i.txt"
done
nvidia-smi > "$OUT/host-smi-final.txt" 2>&1
free -m > "$OUT/host-ram-final.txt" 2>&1

say "down"
"$LEA_ROOT/scripts/showcase.sh" down --all --force >>"$OUT/down.txt" 2>&1
say "done -- $OUT"
