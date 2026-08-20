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
# matrix-criterion: the OpenGL version string names NVIDIA and the driver
#                   version this tree targets, and direct rendering is on --
#                   llvmpipe answers this call too
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
v=$(sed -n 's/^OpenGL version string: *//p' <<<"$out" | head -1)
# THE VERSION STRING, NOT THE RENDERER STRING, and that is number 54. The
# renderer string carries the card's PRODUCT NAME, which the identity
# mediation rewrites on purpose -- a guest reads 'Leandro RTX 2070/PCIe/SSE2'
# where the host reads 'NVIDIA GeForce RTX 2070/PCIe/SSE2', so this probe
# failed in a guest for the mediation working. The version string is
# untouched by it and identical on both sides (measured 2026-08-20:
# '4.6.0 NVIDIA 610.57.04' natively and in the guest), and it is the
# stronger claim anyway: it names the driver, and llvmpipe's says Mesa.
# Checked against DRIVER_VERSION rather than against the word NVIDIA, so a
# guest answered by a userspace of the wrong version is a failure and not a
# pass. The renderer string is still REPORTED -- it is the mediated identity,
# which is worth having in the record and is not a pass criterion.
want=$(lea_want_driver)
grep -q "NVIDIA $want" <<<"$v" \
    || { error "OpenGL version is '$v', not NVIDIA $want"; exit 1; }
lea_matrix_criterion "glxinfo resolved to '$v' with direct rendering (renderer '$r')"
