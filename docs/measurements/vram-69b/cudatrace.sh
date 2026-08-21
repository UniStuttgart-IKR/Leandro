#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# OPEN-QUESTIONS 69(a): what does libcuda check after the mode answer?
#
# One guest, twice: with the VIRTUALIZATION_MODE answer ON (where cuInit
# returns 100) and with it OFF (where the same binary works). Both runs are
# traced in the guest by crates/nvrm-trace, so the question stops being
# "the objection is inside libcuda" and becomes a call with a number.
set -uo pipefail
LEA_ROOT=${LEA_ROOT:-/home/silas/git/Leandro}
source "$LEA_ROOT/scripts/lib/rig.sh"
source "$LEA_ROOT/scripts/lib/matrix.sh"
OUT=${1:?outdir}
mkdir -p "$OUT"
say() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$OUT/run.txt"; }

lea_matrix_serial || { echo "no lock"; exit 1; }
export LEA_CPUS=2 LEA_MEM=2048

down_now() { "$LEA_ROOT/scripts/showcase.sh" down --all --force >> "$OUT/down.txt" 2>&1; }

run_one() {   # run_one TAG MEDIATE
    local tag=$1 med=$2 ip
    say "=== $tag: LEA_VGPU_MEDIATE=$med ==="
    LEA_VGPU_MEDIATE="$med" LEA_VGPU_TYPE=2Q \
        "$LEA_ROOT/scripts/showcase.sh" up --name vm0 --index 0 --vgpu-type 2Q \
        > "$OUT/$tag-up.txt" 2>&1 || {
            say "  up FAILED: $(grep -m1 -iE 'error|refus|full|taken' "$OUT/$tag-up.txt" | cut -c1-140)"
            down_now; return 1; }
    ip=$(lea_ip 0)
    say "  smi: $(lea_ssh "$ip" 'nvidia-smi --query-gpu=name,memory.total --format=csv,noheader' 2>&1 | tr -d '\r')"
    say "  mode: $(lea_ssh "$ip" 'nvidia-smi -q | grep -A2 "Virtualization Mode" | head -3' 2>&1 | tr -d '\r' | tr '\n' ' ')"
    # The tracer lives beside the staged probes; ask the guest rather than
    # assuming a layout.
    # THE COMPUTE GUEST CARRIES NO TRACER. provision.sh stages the probes
    # and the driver libraries; the .so only reaches a guest through the
    # matrix's own staging, which is a path this run must not take (it
    # would be `ioctl-matrix.sh guest <one-probe>`). So push the built one
    # in directly -- same ELF the matrix would have staged.
    local lib=/tmp/libnvrm_trace.so
    if ! lea_scp "$LEA_TRACE_LIB" "$ip:$lib"; then
        say "  could not push the tracer"; down_now; return 1
    fi
    say "  tracer pushed to $lib ($(stat -c%s "$LEA_TRACE_LIB") bytes)"
    # A CUDA program that does the least a CUDA program can do: cuInit and
    # one small allocation. Whatever libcuda checks, it checks before this
    # returns.
    lea_ssh "$ip" "cd ~/gpu && rm -f /tmp/t.jsonl /tmp/t && \
        LEA_TRACE_FILE=/tmp/t LD_PRELOAD=$lib ./vrampress --fill 1 --seconds 2" \
        > "$OUT/$tag-vrampress.txt" 2>&1
    say "  vrampress: $(grep -m1 -iE 'cuInit|cuCtx|MiB' "$OUT/$tag-vrampress.txt" | cut -c1-100)"
    lea_ssh "$ip" 'cat /tmp/t 2>/dev/null'      > "$OUT/$tag.tsv"   2>/dev/null
    lea_ssh "$ip" 'cat /tmp/t.jsonl 2>/dev/null' > "$OUT/$tag.jsonl" 2>/dev/null
    say "  trace: $(wc -l < "$OUT/$tag.tsv") tsv lines, $(wc -l < "$OUT/$tag.jsonl") jsonl"
    cp "$LEA_VM_DIR/vm0/nvrm.log" "$OUT/$tag-backend.txt" 2>/dev/null
    down_now
}

"$LEA_ROOT/scripts/showcase.sh" net up --count 1 > "$OUT/net.txt" 2>&1
run_one mode-on  mode
run_one mode-off none
say "=== cudatrace done ==="
