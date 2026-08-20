#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# Vulkan SC (safety critical). libnvidia-vksc-core is staged -- it turned up
# as a dlopen name of libcudadebugger during the payload audit -- and there
# is nothing to run against it.
#
# THE REASON IS "no workload exists" AND NOT "not procurable", and the
# difference is the whole point of recording it: Vulkan SC needs a
# conformant SC loader and an SC application, neither of which exists on
# any general-purpose desktop. Measured on this host: an SC ICD manifest is
# installed (/usr/share/vulkansc/icd.d/nvidia_icd_vksc.json) and there is no
# libvulkansc loader to read it. That is a standing decision, not an
# invitation to procure something later.
#
# matrix-group:     vulkan
# matrix-libs:      libnvidia-vksc-core
# matrix-entry:     vkscCreateInstance (Vulkan SC loader)
# matrix-criterion: none -- no workload
# matrix-status:    declared-unsupported: no workload exists
set -uo pipefail
_LEA_LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib" && pwd) || exit 1
# shellcheck source=scripts/lib/config.sh
source "$_LEA_LIB/config.sh"
# shellcheck source=scripts/lib/common.sh
source "$_LEA_LIB/common.sh"
# shellcheck source=scripts/lib/matrix.sh
source "$_LEA_LIB/matrix.sh"

lea_matrix_unsupported
