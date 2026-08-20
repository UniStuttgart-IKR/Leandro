#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# The raytracing branch: a logical device created with
# VK_KHR_acceleration_structure. This is the ONLY moment the driver dlopens
# libnvidia-rtcore -- no enumerating client reaches it, and the day the
# library was missing from the guest the only evidence anywhere was twelve
# ENOENTs in strace (number 11).
#
# matrix-group:     vulkan
# matrix-libs:      libnvidia-rtcore libGLX_nvidia libnvidia-glvkspirv
# matrix-entry:     vkCreateDevice with VK_KHR_acceleration_structure
# matrix-criterion: the device is created AND hands back a non-zero device
#                   address for a buffer -- device creation alone does not
#                   prove the address space behind it
# matrix-status:    ready
set -uo pipefail
_LEA_LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib" && pwd) || exit 1
# shellcheck source=scripts/lib/config.sh
source "$_LEA_LIB/config.sh"
# shellcheck source=scripts/lib/common.sh
source "$_LEA_LIB/common.sh"
# shellcheck source=scripts/lib/matrix.sh
source "$_LEA_LIB/matrix.sh"

[[ -x $LEA_PROBE_BIN/vkrt ]] || { error "no $LEA_PROBE_BIN/vkrt -- make -C probe matrix-probes"; exit 1; }

out=$(lea_matrix_workload "$LEA_PROBE_BIN/vkrt" 2>&1); rc=$?
echo "$out"
[[ $rc -eq 0 ]] || { error "vkrt exited $rc ($(sed -n 's/^VKRT=//p' <<<"$out" | tail -1))"; exit 1; }
grep -q '^VKRT=ok' <<<"$out" || { error "vkrt did not report VKRT=ok"; exit 1; }
lea_matrix_criterion "Vulkan device created with VK_KHR_acceleration_structure and a non-zero buffer device address"
