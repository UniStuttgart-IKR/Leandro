#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# GLES: eglBindAPI(EGL_OPENGL_ES_API) and a GLES2 enumeration.
#
# The probe was written for libGLESv1_CM_nvidia / libGLESv2_nvidia, which
# were staged for 32-bit clients only. It does not reach them. Measured
# 2026-08-20 by reading the openat set out of this probe's own strace:
# `es2_info` opens libGLESv2.so.2 -- GLVND's dispatch -- and lands on
# libEGL_nvidia and from there on eglcore/glsi/gpucomp. The vendor
# libraries are named in no driver library's dlopen strings, so nothing
# with a GLVND stack under it loads them; they are the pre-GLVND
# direct-link ABI. They are staged 64-bit now anyway (provision.sh,
# `versioned`), and the honest catalogue row for them is "no probe":
# staging them was a fix, and it is not this measurement.
#
# What this probe therefore measures is the GLES entry point of the NVIDIA
# EGL stack, which is a surface of its own and is in no other probe here.
#
# It never blocks a run. If es2_info is absent the row carries the reason
# and no measurement.
#
# matrix-group:     gl
# matrix-libs:      libEGL_nvidia libnvidia-eglcore libnvidia-glsi
# matrix-entry:     eglBindAPI(EGL_OPENGL_ES_API) + GLES2 enumeration
# matrix-criterion: the GLES version string names NVIDIA and the driver
#                   version this tree targets
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
v=$(sed -n 's/^GL_VERSION: *//p' <<<"$out" | head -1)
# The version string and not the renderer string, for the reason gl-enum.sh
# states at length (number 54): the renderer string is the card's product
# name, which the identity mediation rewrites, and GL_VERSION is not.
# Measured 2026-08-20: 'OpenGL ES 3.2 NVIDIA 610.57.04' on both sides.
want=$(lea_want_driver)
grep -q "NVIDIA $want" <<<"$v" \
    || { error "GLES version is '$v', not NVIDIA $want"; exit 1; }
lea_matrix_criterion "GLES2 enumeration resolved to '$v' (renderer '$r')"
