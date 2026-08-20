#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# libcuda's core: initialise, make a context, move memory both ways.
#
# matrix-group:     compute
# matrix-libs:      libcuda
# matrix-entry:     cuInit, cuCtxCreate, cuMemAlloc/cuMemcpyHtoD/DtoH
# matrix-criterion: nvprobe reports "stage 2 ok", which it prints only after
#                   reading its buffer back off the card and comparing it
# matrix-status:    ready
set -uo pipefail
_LEA_LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib" && pwd) || exit 1
# shellcheck source=scripts/lib/config.sh
source "$_LEA_LIB/config.sh"
# shellcheck source=scripts/lib/common.sh
source "$_LEA_LIB/common.sh"
# shellcheck source=scripts/lib/matrix.sh
source "$_LEA_LIB/matrix.sh"

out=$(lea_matrix_workload "$LEA_PROBE_BIN/nvprobe" 2 2>&1); rc=$?
echo "$out"
[[ $rc -eq 0 ]] || { error "nvprobe 2 exited $rc"; exit 1; }
grep -q 'stage 2 ok' <<<"$out" || { error "no 'stage 2 ok' in the output"; exit 1; }
lea_matrix_criterion "nvprobe 2: device memory written, read back and compared"
