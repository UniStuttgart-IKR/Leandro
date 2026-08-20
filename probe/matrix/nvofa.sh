#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# NvOFA, NVIDIA's optical flow accelerator. The only workload is the sample
# in the Video Codec SDK, whose headers (nvOpticalFlowCuda.h) are behind a
# licensed manual download -- and this run must not fetch anything.
#
# "workload not procurable in this environment" rather than "no workload
# exists", and the difference matters: the sample DOES exist and a human
# with an SDK download can turn this row green. It is an invitation, not a
# decision.
#
# Note the library is not staged 64-bit at all today (it is in the 32-bit
# optional set only), so this row would price the staging as well as the
# forwarding.
#
# matrix-group:     video
# matrix-libs:      libnvidia-opticalflow
# matrix-entry:     NvOFAPICreateInstanceCuda (Video Codec SDK)
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
