#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# NVML, through nvidia-smi. It does NOT go through libcuda, and its escape
# surface can contain escapes and controls that appear in no libcuda and no
# torch trace at all -- forwarding it correctly cannot be inferred from
# those traces, it has to be measured on its own (probe/run/trace.sh smi
# makes the same argument).
#
# -q rather than the bare invocation: the query form pulls considerably more
# RM_CONTROL.
#
# matrix-group:     nvml
# matrix-libs:      libnvidia-ml libnvidia-cfg
# matrix-entry:     nvidia-smi -q (NVML)
# matrix-criterion: the report names this card's product and its driver
#                   version -- a degraded NVML still prints a table
# matrix-status:    ready
set -uo pipefail
_LEA_LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib" && pwd) || exit 1
# shellcheck source=scripts/lib/config.sh
source "$_LEA_LIB/config.sh"
# shellcheck source=scripts/lib/common.sh
source "$_LEA_LIB/common.sh"
# shellcheck source=scripts/lib/matrix.sh
source "$_LEA_LIB/matrix.sh"

smi=${NVIDIA_SMI:-$(command -v nvidia-smi)}
[[ -x $smi ]] || { error "no nvidia-smi (NVIDIA_SMI)"; exit 1; }

out=$(lea_matrix_workload "$smi" -q 2>&1); rc=$?
echo "$out"
[[ $rc -eq 0 ]] || { error "nvidia-smi -q exited $rc"; exit 1; }
name=$(awk -F': ' '/^ *Product Name/{print $2; exit}' <<<"$out")
drv=$(awk -F': ' '/^ *Driver Version/{print $2; exit}' <<<"$out")
[[ -n $name && -n $drv ]] || { error "no Product Name / Driver Version in the report"; exit 1; }
lea_matrix_criterion "nvidia-smi -q enumerated '$name' at driver $drv"
