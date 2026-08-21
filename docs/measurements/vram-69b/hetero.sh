#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# OPEN-QUESTIONS 69: the admission demonstration and the heterogeneous run.
#   P1  four 2Q, then a fifth refused at maxInstance
#   P2  one 4Q beside two 2Q -- what the homogeneous rule could not express
#       -- then a fourth refused because the card is full, then load.
set -uo pipefail
LEA_ROOT=${LEA_ROOT:-/home/silas/git/Leandro}
source "$LEA_ROOT/scripts/lib/rig.sh"
source "$LEA_ROOT/scripts/lib/matrix.sh"
OUT=${1:?outdir}; SECS=${2:-120}
mkdir -p "$OUT"
say() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$OUT/run.txt"; }

avail=$(free -m | awk '/^Mem:/{print $7}')
(( 4 * 2048 + 4096 < avail )) || { echo "REFUSED: host has only ${avail} MiB free"; exit 1; }
lea_matrix_serial || { echo "no lock"; exit 1; }
export LEA_CPUS=2 LEA_MEM=2048
"$LEA_BIN_DIR/vgpuprofile" > "$OUT/catalogue.txt" 2>&1
say "catalogue: $(grep -c '^RTX' "$OUT/catalogue.txt") types; available $(sed -n 's/.*\([0-9]\{4\}\) MiB total.*/\1/p' "$OUT/catalogue.txt" | head -1) MiB"
"$LEA_ROOT/scripts/showcase.sh" net up --count 4 >"$OUT/net.txt" 2>&1

sample_start() {   # sample_start TAG PID...
    local tag=$1; shift
    ( echo "ts,used_mib,free_mib,util,$(for p in "$@"; do printf 'pid%s,' "$p"; done)"
      while :; do
        g=$(nvidia-smi --query-gpu=memory.used,memory.free,utilization.gpu \
              --format=csv,noheader,nounits | head -1 | tr -d ' ')
        a=$(nvidia-smi --query-compute-apps=pid,used_memory --format=csv,noheader,nounits | tr -d ' ')
        line="$(date +%H:%M:%S),$g"
        for p in "$@"; do line="$line,$(awk -F, -v p="$p" '$1==p{print $2}' <<<"$a")"; done
        echo "$line"; sleep 1
      done ) > "$OUT/$tag.csv" &
    SAMPLER=$!
}

# ---- P1: homogeneous density and the maxInstance refusal ------------------
say "=== P1: four 2Q, then a fifth ==="
export LEA_VGPU_TYPE=2Q
"$LEA_ROOT/scripts/showcase.sh" up --count 4 >"$OUT/p1-up.txt" 2>&1
n=$(grep -c ": up$" "$OUT/p1-up.txt" || true)
say "P1 up: $n of 4"
for i in 0 1 2 3; do
    lea_running "$LEA_VM_DIR/vm$i/ch.pid" || continue
    say "  vm$i: $(lea_ssh "$(lea_ip "$i")" \
        'nvidia-smi --query-gpu=name,memory.total --format=csv,noheader' 2>&1 | tr -d '\r')"
done
say "--- the fifth (expect NV_ERR_INSUFFICIENT_RESOURCES at maxInstance 4) ---"
"$LEA_ROOT/scripts/showcase.sh" up --name vm4 --index 4 --vgpu-type 2Q \
    >"$OUT/p1-fifth.txt" 2>&1
rc=$?
say "  fifth exit=$rc :: $(grep -m1 -iE 'maxInstance|INSUFFICIENT|card is full' "$OUT/p1-fifth.txt" | cut -c1-140)"
"$LEA_ROOT/scripts/showcase.sh" down --all --force >>"$OUT/p1-down.txt" 2>&1

# ---- P2: the mixed card ---------------------------------------------------
say "=== P2: one 4Q beside two 2Q ==="
unset LEA_VGPU_TYPE
declare -A WANT=([vm0]=4Q [vm1]=2Q [vm2]=2Q)
for i in 0 1 2; do
    t=${WANT[vm$i]}
    say "  up vm$i as $t"
    "$LEA_ROOT/scripts/showcase.sh" up --name "vm$i" --index "$i" --vgpu-type "$t" \
        >"$OUT/p2-up-vm$i.txt" 2>&1 \
        || say "  vm$i FAILED: $(grep -m1 -iE 'error|refus|full' "$OUT/p2-up-vm$i.txt" | cut -c1-140)"
    grep -m1 "admitted" "$OUT/p2-up-vm$i.txt" | sed 's/^/    /' | tee -a "$OUT/run.txt"
done
for i in 0 1 2; do
    lea_running "$LEA_VM_DIR/vm$i/ch.pid" || { say "  vm$i is not running"; continue; }
    say "  vm$i (${WANT[vm$i]}): $(lea_ssh "$(lea_ip "$i")" \
        'nvidia-smi --query-gpu=name,memory.total --format=csv,noheader' 2>&1 | tr -d '\r')"
done
say "--- a fourth, 1Q, on a card that is already exactly full ---"
"$LEA_ROOT/scripts/showcase.sh" up --name vm3 --index 3 --vgpu-type 1Q \
    >"$OUT/p2-fourth.txt" 2>&1
rc=$?
say "  fourth exit=$rc :: $(grep -m1 -iE 'card is full|maxInstance|INSUFFICIENT' "$OUT/p2-fourth.txt" | cut -c1-160)"

bpids=""; for i in 0 1 2; do bpids="$bpids $(cat "$LEA_VM_DIR/vm$i/nvrm.pid" 2>/dev/null || echo 0)"; done
say "backend pids:$bpids"
# shellcheck disable=SC2086
sample_start p2-host-1hz $bpids
say "load: vrampress --max in all three for ${SECS}s"
declare -a LP=()
for i in 0 1 2; do
    lea_running "$LEA_VM_DIR/vm$i/ch.pid" || continue
    ( lea_ssh "$(lea_ip "$i")" "cd ~/gpu && ./vrampress --max --seconds $SECS" \
        > "$OUT/p2-vrampress-vm$i.csv" 2>&1 ) &
    LP+=("$!")
done
for p in "${LP[@]}"; do wait "$p"; done
sleep 5; kill $SAMPLER 2>/dev/null
for i in 0 1 2; do
    [[ -f $LEA_VM_DIR/vm$i/nvrm.log ]] && cp "$LEA_VM_DIR/vm$i/nvrm.log" "$OUT/p2-backend-vm$i.txt"
done
nvidia-smi > "$OUT/p2-smi-final.txt" 2>&1
say "down"
"$LEA_ROOT/scripts/showcase.sh" down --all --force >>"$OUT/p2-down.txt" 2>&1
say "=== hetero done ==="
