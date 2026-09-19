#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# Compatibility entrypoint; hardware tooling lives in Leandro-Test.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
exec env LEA_ROOT="${LEA_ROOT:-$root}" "${LEA_TEST_ROOT:-$root/../Leandro-Test}/scripts/ioctl-matrix.sh" "$@"
