#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# Pinned host memory: cuMemHostRegister, the hClass 0x71 (OS_DESCRIPTOR)
# path. A different allocation door from cuMemAlloc, and the one the VRAM
# ledger had to learn about separately (number 12).
#
# matrix-group:     compute
# matrix-libs:      libcuda
# matrix-entry:     cuMemHostRegister / cuMemHostGetDevicePointer
# matrix-criterion: hostregprobe's own check that the registered pages carry
#                   data both ways
# matrix-status:    ready
set -uo pipefail
_LEA_LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib" && pwd) || exit 1
# shellcheck source=scripts/lib/config.sh
source "$_LEA_LIB/config.sh"
# shellcheck source=scripts/lib/common.sh
source "$_LEA_LIB/common.sh"
# shellcheck source=scripts/lib/matrix.sh
source "$_LEA_LIB/matrix.sh"

out=$(lea_matrix_workload "$LEA_PROBE_BIN/hostregprobe" 2>&1); rc=$?
echo "$out"
[[ $rc -eq 0 ]] || { error "hostregprobe exited $rc"; exit 1; }
# Two roundtrips, one per door: HostRegister on pages the process already
# owns, and HostAlloc where the driver supplies them. Both have to come back
# correct -- one of them passing alone is how a half-working pinned path
# looks.
n=$(grep -c 'bad=0 correct' <<<"$out")
[[ ${n:-0} -ge 2 ]] || { error "hostregprobe: $n of 2 roundtrips reported 'bad=0 correct'"; exit 1; }
lea_matrix_criterion "hostregprobe: $n pinned-memory roundtrips (HostRegister and HostAlloc) compared byte for byte"
