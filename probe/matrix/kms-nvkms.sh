#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# nvidia_modeset and nvidia_drm: a separate axis, not a userspace column.
#
# Their RM traffic never crosses a userspace ioctl boundary -- NVKMS talks to
# RM from inside the kernel, so no LD_PRELOAD interposer can see any of it,
# whatever client is running. And the NVKMS command numbers are a namespace
# of their own: they are not RM_CONTROL commands and do not resolve against
# ctrl*.h at all.
#
# What CAN be measured natively is the DRM ioctl surface a client presents to
# nvidia_drm, and this tree already measures it: probe/run/drmtrace.sh, which
# decodes private and core DRM numbers out of the headers that apply because
# strace names NVIDIA's private numbers after other vendors' drivers. That is
# a different boundary from this catalogue's, so it stays a separate tool and
# a separate report rather than being folded into these columns.
#
# matrix-group:     kms
# matrix-libs:      nvidia-modeset.ko nvidia-drm.ko
# matrix-entry:     NVKMS ioctls (in-kernel RM client)
# matrix-criterion: none -- not observable from userspace
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
