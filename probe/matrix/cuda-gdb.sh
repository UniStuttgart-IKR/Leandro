#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# libcudadebugger, the one library in the staged set that only a debugger
# reaches. cuda-gdb LAUNCHES the probe rather than attaching to a running
# one: attaching to a foreign process needs ptrace_scope 0, and a
# measurement must not change the machine it is taken on. A launched
# inferior is a child, which ptrace_scope 1 permits.
#
# matrix-group:     compute
# matrix-libs:      libcudadebugger libcuda
# matrix-entry:     cuda-gdb --batch -ex run (CUDA debugger back end)
# matrix-criterion: the inferior ran to completion UNDER the debugger and
#                   still reported "stage 3 ok" -- the kernel produced the
#                   right numbers with the debugger back end attached
# matrix-status:    ready
# matrix-gate:      none -- strace cannot follow a process that is itself
#                   ptracing. Measured: under `strace -f` cuda-gdb records
#                   zero ioctls, because its own ptrace of the inferior is
#                   the one strace has already taken. There is no second
#                   instrument here that could count the same calls, so this
#                   probe's CRITERION stands and its signatures do NOT enter
#                   the catalogue. The instrument that would gate it is the
#                   same kernel-side trace point the 32-bit set needs.
set -uo pipefail
_LEA_LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib" && pwd) || exit 1
# shellcheck source=scripts/lib/config.sh
source "$_LEA_LIB/config.sh"
# shellcheck source=scripts/lib/common.sh
source "$_LEA_LIB/common.sh"
# shellcheck source=scripts/lib/matrix.sh
source "$_LEA_LIB/matrix.sh"

gdb=${CUDA_GDB:-$(command -v cuda-gdb || echo /opt/cuda/bin/cuda-gdb)}
[[ -x $gdb ]] || {
    echo "declared-unsupported: workload not procurable in this environment -- no cuda-gdb (CUDA_GDB)"
    exit 2
}
export NVPROBE_PTX="${NVPROBE_PTX:-$LEA_ROOT/probe/kernels/kernels.ptx}"

out=$(lea_matrix_workload "$gdb" --batch -ex run --args "$LEA_PROBE_BIN/nvprobe" 3 2>&1); rc=$?
echo "$out"
[[ $rc -eq 0 ]] || { error "cuda-gdb exited $rc"; exit 1; }
# cuda-gdb's own exit code says nothing about the inferior -- it exits 0
# after reporting an inferior that died. Both facts have to be read out of
# the transcript.
grep -q 'stage 3 ok' <<<"$out" || { error "the inferior did not reach 'stage 3 ok'"; exit 1; }
grep -q 'exited normally' <<<"$out" || { error "the inferior did not exit normally under cuda-gdb"; exit 1; }
lea_matrix_criterion "nvprobe 3 ran to completion under cuda-gdb and its kernel result was still verified"
