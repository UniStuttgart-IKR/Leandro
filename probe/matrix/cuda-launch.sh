#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# The submission and completion path: many launches with a BLOCKING wait, so
# the completion goes through the event/FD path rather than a spin.
#
# matrix-group:     compute
# matrix-libs:      libcuda
# matrix-entry:     cuLaunchKernel x N, cuCtxSynchronize (CU_CTX_SCHED_BLOCKING_SYNC)
# matrix-criterion: nvprobe reports "stage 4 ok" and a non-zero iteration
#                   count in its own ITERS= line
# matrix-status:    ready
set -uo pipefail
_LEA_LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib" && pwd) || exit 1
# shellcheck source=scripts/lib/config.sh
source "$_LEA_LIB/config.sh"
# shellcheck source=scripts/lib/common.sh
source "$_LEA_LIB/common.sh"
# shellcheck source=scripts/lib/matrix.sh
source "$_LEA_LIB/matrix.sh"

export NVPROBE_PTX="${NVPROBE_PTX:-$LEA_ROOT/probe/kernels/kernels.ptx}"
export NVPROBE_SCHED=blocking

out=$(lea_matrix_workload "$LEA_PROBE_BIN/nvprobe" 4 2>&1); rc=$?
echo "$out"
[[ $rc -eq 0 ]] || { error "nvprobe 4 exited $rc"; exit 1; }
grep -q 'stage 4 ok' <<<"$out" || { error "no 'stage 4 ok' in the output"; exit 1; }
iters=$(sed -n 's/^ITERS=//p' <<<"$out" | tail -1)
[[ ${iters:-0} -gt 0 ]] || { error "ITERS=${iters:-<none>} -- nothing was launched"; exit 1; }
lea_matrix_criterion "nvprobe 4 (blocking): $iters kernel launches completed and verified"
