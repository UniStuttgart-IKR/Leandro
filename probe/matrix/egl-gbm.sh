#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# EGL on the gbm platform.
#
# One probe per platform because NVIDIA ships one external-platform library
# per windowing system and they are five different paths through
# libEGL_nvidia. A single eglinfo run would collapse them into one row and
# hide exactly the difference this matrix exists to show.
#
# matrix-group:     egl
# matrix-libs:      libEGL_nvidia libnvidia-eglcore libnvidia-egl-gbm libnvidia-glsi libnvidia-tls libnvidia-glvkspirv
# matrix-entry:     eglGetPlatformDisplayEXT(gbm) + pbuffer + GLES2
# matrix-criterion: a pixel cleared to 3377bb comes back as 3377bb from a
#                   64x64 pbuffer -- eglInitialize alone would pass while
#                   the platform module underneath fell back silently
# matrix-status:    ready
set -uo pipefail
_LEA_LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib" && pwd) || exit 1
# shellcheck source=scripts/lib/config.sh
source "$_LEA_LIB/config.sh"
# shellcheck source=scripts/lib/common.sh
source "$_LEA_LIB/common.sh"
# shellcheck source=scripts/lib/matrix.sh
source "$_LEA_LIB/matrix.sh"

[[ -x $LEA_PROBE_BIN/eglplat ]] || { error "no $LEA_PROBE_BIN/eglplat -- make -C probe matrix-probes"; exit 1; }

out=$(lea_matrix_workload "$LEA_PROBE_BIN/eglplat" gbm 2>&1); rc=$?
echo "$out"
# Exit 2 from eglplat means it was built without this platform's headers, or
# there is no a DRM render node (/dev/dri/renderD128) here. That is a declared reason, not a failure.
[[ $rc -ne 2 ]] || { echo "declared-unsupported: workload not procurable in this environment -- a DRM render node (/dev/dri/renderD128) is not available"; exit 2; }
[[ $rc -eq 0 ]] || { error "eglplat gbm exited $rc"; exit 1; }
px=$(sed -n 's/^PIXEL=//p' <<<"$out" | tail -1)
vendor=$(sed -n 's/^EGLVENDOR=//p' <<<"$out" | tail -1)
lea_matrix_criterion "EGL/gbm: vendor $vendor, pbuffer pixel read back as $px"
