#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# OpenGL that actually renders, off-screen: no window, no compositor, no
# presentation. glmark2 --off-screen draws into an FBO, which is render cost
# with nothing else in the path -- and it keeps this probe off the
# operator's desktop, which a windowed client would not.
#
# matrix-group:     gl
# matrix-libs:      libGLX_nvidia libnvidia-glcore libnvidia-glsi libnvidia-tls libnvidia-gpucomp libnvidia-allocator
# matrix-entry:     GLX context + FBO rendering
# matrix-criterion: a non-zero glmark2 score over a FIXED 100 frames, i.e.
#                   frames were drawn -- an enumerating client would produce
#                   ioctls without ever rendering anything
# matrix-status:    ready
set -uo pipefail
_LEA_LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib" && pwd) || exit 1
# shellcheck source=scripts/lib/config.sh
source "$_LEA_LIB/config.sh"
# shellcheck source=scripts/lib/common.sh
source "$_LEA_LIB/common.sh"
# shellcheck source=scripts/lib/matrix.sh
source "$_LEA_LIB/matrix.sh"

command -v glmark2 >/dev/null || { echo "declared-unsupported: workload not procurable in this environment -- no glmark2"; exit 2; }

# nframes, NOT duration. A duration-based benchmark renders however many
# frames the machine manages in the time, so the traced run and the
# counter-check run are not the same workload -- measured here first: with
# duration=2 the two runs differed by 441 ioctls, which reads exactly like a
# tracer that misses calls and is nothing of the kind.
out=$(lea_matrix_workload glmark2 --off-screen --size 640x480 -b build:nframes=100 2>&1); rc=$?
echo "$out"
[[ $rc -eq 0 ]] || { error "glmark2 exited $rc"; exit 1; }
score=$(sed -n 's/.*glmark2 Score: *\([0-9][0-9]*\).*/\1/p' <<<"$out" | tail -1)
[[ ${score:-0} -gt 0 ]] || { error "glmark2 score is ${score:-<none>}"; exit 1; }
lea_matrix_criterion "glmark2 --off-screen rendered the build scene, score $score"
