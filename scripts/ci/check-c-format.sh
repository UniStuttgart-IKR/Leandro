#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
set -euo pipefail

LEA_ROOT=${LEA_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)}
cd "$LEA_ROOT"
formatter=${CLANG_FORMAT:-clang-format}
expected=22.1.8
version=$("$formatter" --version)
if [[ ! $version =~ version[[:space:]]22\.1\.8([[:space:]]|$) ]]; then
    echo "Expected clang-format $expected; found: $version" >&2
    exit 1
fi

mapfile -d '' -t sources < <(git ls-files -z -- '*.c' '*.h' \
    ':!:vendor/**' ':!:guest-module/virtio_nvrm/nvrm_wire.h')
if ((${#sources[@]} == 0)); then
    echo "No tracked C sources found." >&2
    exit 1
fi
"$formatter" --dry-run --Werror --style=file "${sources[@]}"
