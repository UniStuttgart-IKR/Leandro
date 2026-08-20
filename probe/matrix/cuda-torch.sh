#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# The full PyTorch stack with a convolution, which is the one stage that is
# not just more of the stage below it: cuDNN enters and brings its own
# allocations with it.
#
# matrix-group:     compute
# matrix-libs:      libcuda libnvidia-nvvm libnvidia-ptxjitcompiler
# matrix-entry:     torch.cuda + a cuDNN convolution
# matrix-criterion: torchprobe reports "stage 5 ok" after comparing its own
#                   tensor result
# matrix-status:    ready
set -uo pipefail
_LEA_LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib" && pwd) || exit 1
# shellcheck source=scripts/lib/config.sh
source "$_LEA_LIB/config.sh"
# shellcheck source=scripts/lib/common.sh
source "$_LEA_LIB/common.sh"
# shellcheck source=scripts/lib/matrix.sh
source "$_LEA_LIB/matrix.sh"

# The native reference interpreter the gpu gate also measures against, so
# this row is the same torch the guest runs (DEVELOPMENT.md section 3).
py=${NVTORCH_PY:-$LEA_HOSTVENV/bin/python}
[[ -x $py ]] || py=$(command -v python3)
"$py" -c 'import torch' 2>/dev/null || {
    echo "declared-unsupported: workload not procurable in this environment -- no PyTorch in '$py' (NVTORCH_PY)"
    exit 2
}
export NVTORCH_CONV=1

out=$(lea_matrix_workload "$py" "$LEA_ROOT/probe/python/torchprobe.py" 5 2>&1); rc=$?
echo "$out"
[[ $rc -eq 0 ]] || { error "torchprobe 5 exited $rc"; exit 1; }
grep -q 'stage 5 ok' <<<"$out" || { error "no 'stage 5 ok' in the output"; exit 1; }
lea_matrix_criterion "torchprobe stage 5 with NVTORCH_CONV=1: cuDNN convolution result verified by the probe"
