#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# Vulkan enumeration: which ICD a client resolves to, and what the device
# reports before anything is allocated.
#
# matrix-group:     vulkan
# matrix-libs:      libGLX_nvidia libnvidia-glvkspirv
# matrix-entry:     vkEnumeratePhysicalDevices (vulkaninfo --summary)
# matrix-criterion: a device with NVIDIA's driver name is listed -- an
#                   unresolved ICD leaves only llvmpipe, which also answers
# matrix-status:    ready
set -uo pipefail
_LEA_LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib" && pwd) || exit 1
# shellcheck source=scripts/lib/config.sh
source "$_LEA_LIB/config.sh"
# shellcheck source=scripts/lib/common.sh
source "$_LEA_LIB/common.sh"
# shellcheck source=scripts/lib/matrix.sh
source "$_LEA_LIB/matrix.sh"

command -v vulkaninfo >/dev/null || { echo "declared-unsupported: workload not procurable in this environment -- no vulkaninfo"; exit 2; }

out=$(lea_matrix_workload vulkaninfo --summary 2>&1); rc=$?
echo "$out"
[[ $rc -eq 0 ]] || { error "vulkaninfo exited $rc"; exit 1; }
grep -qi 'driverName *= *nvidia' <<<"$out" || { error "no NVIDIA driverName in the summary"; exit 1; }
n=$(grep -ci 'driverName *= *nvidia' <<<"$out")
lea_matrix_criterion "vulkaninfo listed $n NVIDIA Vulkan device(s)"
