#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# The JIT chain: load PTX, let the driver compile it, launch the kernel and
# check the numbers it produced.
#
# PTX rather than a cubin on purpose -- the JIT is part of what has to run
# unchanged behind the boundary, and pre-compiling it would stop measuring
# it (probe/Makefile says the same about kernels.ptx).
#
# matrix-group:     compute
# matrix-libs:      libcuda libnvidia-nvvm libnvidia-ptxjitcompiler libnvidia-tileiras libnvidia-nvvm70
# matrix-entry:     cuModuleLoadData (PTX), cuLaunchKernel
# matrix-criterion: nvprobe reports "stage 3 ok" -- the kernel's result is
#                   compared against the value it must produce
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
[[ -f $NVPROBE_PTX ]] || { error "no $NVPROBE_PTX -- make -C probe"; exit 1; }

out=$(lea_matrix_workload "$LEA_PROBE_BIN/nvprobe" 3 2>&1); rc=$?
echo "$out"
[[ $rc -eq 0 ]] || { error "nvprobe 3 exited $rc"; exit 1; }
grep -q 'stage 3 ok' <<<"$out" || { error "no 'stage 3 ok' in the output"; exit 1; }
lea_matrix_criterion "nvprobe 3: PTX compiled by the driver, kernel launched, result verified"
