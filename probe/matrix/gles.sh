#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# GLES through NVIDIA's own libGLESv1_CM_nvidia / libGLESv2_nvidia.
#
# These are in the host driver payload and are staged for 32-bit clients
# ONLY, so a 64-bit guest client that asks for them finds nothing. The row
# is therefore a not-staged row -- and this probe exists because tracing it
# natively is what prices the staging: it says what the surface would cost
# if the libraries were added.
#
# It never blocks a run. If es2_info is absent the row stays a plain
# not-staged row with no measurement attached.
#
# matrix-group:     gl
# matrix-libs:      libGLESv2_nvidia libGLESv1_CM_nvidia libEGL_nvidia
# matrix-entry:     eglBindAPI(EGL_OPENGL_ES_API) + GLES2 enumeration
# matrix-criterion: the GLES renderer string names NVIDIA
# matrix-status:    ready
set -uo pipefail
_LEA_LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib" && pwd) || exit 1
# shellcheck source=scripts/lib/config.sh
source "$_LEA_LIB/config.sh"
# shellcheck source=scripts/lib/common.sh
source "$_LEA_LIB/common.sh"
# shellcheck source=scripts/lib/matrix.sh
source "$_LEA_LIB/matrix.sh"

command -v es2_info >/dev/null || { echo "declared-unsupported: workload not procurable in this environment -- no es2_info (mesa-utils)"; exit 2; }

out=$(lea_matrix_workload es2_info 2>&1); rc=$?
echo "$out"
[[ $rc -eq 0 ]] || { error "es2_info exited $rc"; exit 1; }
r=$(sed -n 's/^GL_RENDERER: *//p' <<<"$out" | head -1)
grep -q NVIDIA <<<"$r" || { error "GLES renderer is '$r', not NVIDIA"; exit 1; }
lea_matrix_criterion "GLES2 enumeration resolved to '$r'"
