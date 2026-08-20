#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# NVIDIA's PKCS#11 provider. Two libraries, staged because libcuda names
# them, and nothing on a desktop consults them: PKCS#11 here is confidential
# computing, which needs a CC-capable GPU (Hopper and later) in CC mode. On
# a Turing card there is no code path that loads them at all.
#
# "no workload exists" rather than "not procurable": there is no application
# to obtain -- the hardware feature the provider serves is absent.
#
# matrix-group:     compat
# matrix-libs:      libnvidia-pkcs11 libnvidia-pkcs11-openssl3
# matrix-entry:     C_GetFunctionList (PKCS#11)
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
