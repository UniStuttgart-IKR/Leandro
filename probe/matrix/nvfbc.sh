#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# NvFBC, NVIDIA's own frame capture. Two reasons in one row, and only the
# first one blocks: the API headers come from the licensed Capture SDK, and
# NvFBC is restricted on GeForce hardware anyway.
#
# It is staged deliberately even so, because Sunshine looks for it BY NAME
# and logs its absence -- an absent library and a refused one are different
# findings and only one of them would be ours.
#
# matrix-group:     video
# matrix-libs:      libnvidia-fbc
# matrix-entry:     NvFBCCreateInstance (Capture SDK)
# matrix-criterion: none -- no workload
# matrix-status:    declared-unsupported: workload not procurable in this environment
set -uo pipefail
_LEA_LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib" && pwd) || exit 1
# shellcheck source=scripts/lib/config.sh
source "$_LEA_LIB/config.sh"
# shellcheck source=scripts/lib/common.sh
source "$_LEA_LIB/common.sh"
# shellcheck source=scripts/lib/matrix.sh
source "$_LEA_LIB/matrix.sh"

lea_matrix_unsupported
