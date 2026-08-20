#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# The 32-bit GL/EGL/Vulkan set. Twenty libraries, staged, and not one of
# them can be traced by anything in this tree.
#
# WHY: the tracer is an LD_PRELOAD interposer built as a 64-bit object, so it
# cannot be preloaded into a 32-bit client. Building a 32-bit tracer is
# explicitly not the answer -- the same ioctls seen from a second, parallel
# instrument is a second thing to keep correct. The trace point that WOULD
# cover both bit widths at once is on the kernel side (the guest module, or
# the backend), and this tree has none on the host.
#
# Checked when this file was written: guest-module/ holds virtio_nvrm and
# nvrm_nodes, both GUEST-side, and there is no host kernel trace module. If
# one appears, one 32-bit probe (glxgears or equivalent) validates the trace
# path for the whole set and this row becomes measurable. It is the same
# instrument OPEN-QUESTIONS numbers 48 and this probe's gate both want.
#
# matrix-group:     compat
# matrix-libs:      lib32:libGLX_nvidia lib32:libEGL_nvidia lib32:libnvidia-glcore lib32:libnvidia-eglcore lib32:libnvidia-glsi lib32:libnvidia-tls lib32:libnvidia-gpucomp lib32:libnvidia-allocator lib32:libnvidia-glvkspirv lib32:libcuda lib32:libnvidia-nvvm lib32:libnvidia-ptxjitcompiler lib32:libnvidia-tileiras
# matrix-entry:     any 32-bit client (glxgears)
# matrix-criterion: none -- not traceable here
# matrix-status:    blocked: needs kernel-side trace point
set -uo pipefail
_LEA_LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib" && pwd) || exit 1
# shellcheck source=scripts/lib/config.sh
source "$_LEA_LIB/config.sh"
# shellcheck source=scripts/lib/common.sh
source "$_LEA_LIB/common.sh"
# shellcheck source=scripts/lib/matrix.sh
source "$_LEA_LIB/matrix.sh"

lea_matrix_unsupported
