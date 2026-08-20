#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# The GLX enumeration path on its own. Separate from gl-render because it is
# a different question: which driver a client RESOLVES to, before anything
# is drawn. FBConfig 0 and "no available drivers" were once read as a GLX
# bug and were in fact a dead channel underneath (number 8), so the
# resolution step is worth measuring by itself.
#
# matrix-group:     gl
# matrix-libs:      libGLX_nvidia libnvidia-glcore
# matrix-entry:     glXQueryServerString / glXChooseFBConfig (glxinfo -B)
# matrix-criterion: the OpenGL renderer string names NVIDIA and direct
#                   rendering is on -- llvmpipe answers this call too
# matrix-status:    ready
set -uo pipefail
_LEA_LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib" && pwd) || exit 1
# shellcheck source=scripts/lib/config.sh
source "$_LEA_LIB/config.sh"
# shellcheck source=scripts/lib/common.sh
source "$_LEA_LIB/common.sh"
# shellcheck source=scripts/lib/matrix.sh
source "$_LEA_LIB/matrix.sh"

command -v glxinfo >/dev/null || { echo "declared-unsupported: workload not procurable in this environment -- no glxinfo"; exit 2; }

out=$(lea_matrix_workload glxinfo -B 2>&1); rc=$?
echo "$out"
[[ $rc -eq 0 ]] || { error "glxinfo exited $rc"; exit 1; }
grep -q 'direct rendering: Yes' <<<"$out" || { error "direct rendering is not on"; exit 1; }
r=$(sed -n 's/^OpenGL renderer string: *//p' <<<"$out" | head -1)
grep -q NVIDIA <<<"$r" || { error "renderer is '$r', not NVIDIA"; exit 1; }
lea_matrix_criterion "glxinfo resolved to '$r' with direct rendering"
