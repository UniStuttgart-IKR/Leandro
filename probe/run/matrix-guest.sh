#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# Run ONE matrix probe inside a guest, the way the native run runs it.
#
#   probe/run/matrix-guest.sh PROBE [--out DIR] [--display :N]
#
# This is the guest half of `scripts/ioctl-matrix.sh guest`. It is not run
# by hand on the host: the host stages a small tree into the guest
# (scripts/lib, probe/matrix, probe/bin, the tracer) and calls this over
# ssh, once per probe.
#
# WHY A SCRIPT IN THE GUEST AND NOT A LONG ssh COMMAND LINE. The native
# runner wraps the workload in two instruments and compares their counts,
# and every one of those quoting levels would have to survive ssh, sudo and
# a shell that is not this one. It is the same run either way; only the
# escaping differs, and escaping is where a run silently stops tracing.
#
# WHAT IT DOES, per probe, and it is deliberately the same shape as the
# native phase in scripts/ioctl-matrix.sh:
#   1. the probe under the tracer  (LD_PRELOAD, LEA_TRACE_FILE)
#   2. the SAME probe under strace, without the tracer
#   3. the counter-check: tracer count == strace count, on the RM nodes and
#      on /dev/nvidia-modeset separately (lea_matrix_n_* in matrix.sh -- the
#      counting rule is defined once and both phases ask it)
#
# It prints one summary line and writes <out>/<probe>.{tsv,strace,out}. The
# comparison against the native reference happens on the HOST, in
# probe/python/guestdiff.py: this side measures and does not judge.
#
# The tracer is a userspace interposer and works in a guest for the same
# reason it works on the host -- the guest runs the same NVIDIA libraries
# against nodes that carry the same ABI. That is the point: the two traces
# are comparable because they were taken by the same instrument, not by two
# instruments that agree about what they mean.
set -uo pipefail

_LEA_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd) || exit 1
export LEA_ROOT=$_LEA_ROOT
# shellcheck source=scripts/lib/config.sh
source "$LEA_ROOT/scripts/lib/config.sh"
# shellcheck source=scripts/lib/common.sh
source "$LEA_ROOT/scripts/lib/common.sh"
# shellcheck source=scripts/lib/matrix.sh
source "$LEA_ROOT/scripts/lib/matrix.sh"

usage() { lea_usage_from_header; exit "${1:-0}"; }

PROBE=""; OUT=$LEA_ROOT/out; DISP=""
while [[ $# -gt 0 ]]; do
    case $1 in
        --out)     OUT=$2; shift 2 ;;
        --display) DISP=$2; shift 2 ;;
        -h|--help) usage 0 ;;
        -*) error "unknown option: $1"; usage 2 ;;
        *)  PROBE=$1; shift ;;
    esac
done
[[ -n $PROBE ]] || usage 2

f=$LEA_ROOT/probe/matrix/$PROBE.sh
[[ -f $f ]] || die "no such probe: $PROBE"
mkdir -p "$OUT" || die "cannot write $OUT"

# The guest's own payload, in the layout the host staged it in. LEA_PROBE_BIN
# is where the probes look for nvprobe, oclprobe and eglplat.
export LEA_PROBE_BIN=${LEA_PROBE_BIN:-$LEA_ROOT/probe/bin}
# config.sh has already answered where the tracer is, and its answer is a
# build directory this tree has no build in: the staged tree carries the .so
# beside the probes instead. The configured path still wins when it exists,
# so running this out of a full checkout behaves as it reads.
[[ -f ${LEA_TRACE_LIB:-} ]] || LEA_TRACE_LIB=$LEA_ROOT/lib/libnvrm_trace.so
export LEA_TRACE_LIB
[[ -f $LEA_TRACE_LIB ]] || die "no tracer at $LEA_TRACE_LIB"
[[ -n $DISP ]] && export DISPLAY=$DISP

tsv=$OUT/$PROBE.tsv; strc=$OUT/$PROBE.strace; outf=$OUT/$PROBE.out
raw=$(mktemp) || die "mktemp"
trap 'rm -f "$raw"' EXIT

# Run 1: under the tracer. The wrapper goes around the WORKLOAD only, never
# around this script: the tracer opens LEA_TRACE_FILE with O_TRUNC in its
# constructor, so a preloaded shell would have every child wipe the trace of
# the run in progress -- silently, because a truncated file is a valid file.
LEA_MATRIX_WRAP="env LEA_TRACE_FILE=$raw LD_PRELOAD=$LEA_TRACE_LIB" \
    timeout 600 "$f" > "$outf" 2>&1
rc=$?

# Run 2: the same probe under strace, without the tracer. Two runs and not
# one: strace under LD_PRELOAD would measure the tracer too.
if command -v strace >/dev/null; then
    LEA_MATRIX_WRAP="strace -f -y -e trace=ioctl,openat -o $strc" \
        timeout 600 "$f" >/dev/null 2>&1
else
    rm -f "$strc"
fi

{ lea_matrix_provenance '#'
  printf '#probe: %s\n#side: guest\n#host: %s\n' "$PROBE" "$(hostname)"
  cat "$raw"; } > "$tsv"

a=$(lea_matrix_n_tracer "$tsv")
am=$(lea_matrix_n_tracer_kms "$tsv")
if [[ -f $strc ]]; then
    b=$(lea_matrix_n_strace "$strc"); mset=$(lea_matrix_n_strace_kms "$strc")
else
    b=-1; mset=-1
fi
crit=$(sed -n 's/^CRITERION: //p' "$outf" | tail -1)

# The verdict of THIS side only: did the probe meet its own criterion, and
# is the trace complete. Whether the guest matches the host is not a question
# this side can answer -- it has never seen the host's trace.
if [[ $rc -eq 2 ]]; then
    result=$(grep -m1 -E '^(declared-unsupported|blocked):' "$outf" \
             || echo "declared-unsupported: no reason printed")
elif [[ $rc -ne 0 ]]; then
    # The probe's OWN last word, carried into the result. Without it the
    # artefact says "exited 1" and the reason stays in a file on a guest
    # that the next run deletes.
    why=$(grep -m1 -E '^ERROR: ' "$outf" | tail -1)
    result="FAIL: probe exited $rc${why:+ -- ${why#ERROR: }}"
elif [[ -z $crit ]]; then
    result="FAIL: no CRITERION line -- the probe did not say what it proved"
elif [[ $b -lt 0 ]]; then
    # No strace in the guest is a BLOCKED row, never a pass: an ungated
    # trace could be incomplete and nothing here can say it is not.
    result="blocked: no strace in the guest -- the trace cannot be gated"
elif [[ $((b - a)) -ne 0 ]]; then
    result="FAIL: strace gate, delta $((b - a))"
elif [[ $((mset - am)) -ne 0 ]]; then
    result="FAIL: NVKMS gate, delta $((mset - am))"
else
    result=PASS
fi

printf 'GUESTRESULT\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$PROBE" "$result" "$a" "$b" "$am" "$mset" "$crit"
[[ $result == PASS ]]
