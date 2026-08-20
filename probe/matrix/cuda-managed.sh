#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# Managed memory: cuMemAllocManaged, touched from both sides.
#
# It is in the matrix although the project's stance on UVM migration is
# pinning-only: the UVM commands it emits are real, they appear in a real
# client's trace, and a catalogue that left them out would describe a
# smaller surface than the one that exists.
#
# matrix-group:     compute
# matrix-libs:      libcuda
# matrix-entry:     cuMemAllocManaged, UVM_*
# matrix-criterion: managedprobe's own correctness check on the managed
#                   buffer (it compares what the GPU wrote with what the CPU
#                   reads)
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
# Stage 2, not the default 3. Stage 3 deliberately OVERSUBSCRIBES the card
# (9216 MiB against 8 GiB here) and this probe shares the GPU with whatever
# else is on it -- a matrix run must not evict a neighbour's working set to
# take a measurement. Stage 2 covers the managed surface: allocation,
# prefetch and advise, which is where the UVM commands are.
export NVMG_STAGE="${NVMG_STAGE:-2}"

out=$(lea_matrix_workload "$LEA_PROBE_BIN/managedprobe" 2>&1); rc=$?
echo "$out"
[[ $rc -eq 0 ]] || { error "managedprobe exited $rc"; exit 1; }
grep -q 'stage2 ok, result correct' <<<"$out" \
    || { error "managedprobe did not report a correct stage-2 result"; exit 1; }
lea_matrix_criterion "managedprobe stage 2: managed buffer written by the GPU, prefetched, and compared on the CPU (bad=0)"
