#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# Vulkan that computes: upload frames into device memory, run a compute
# shader over them, read them back. No window, no swapchain, no surface.
#
# matrix-group:     vulkan
# matrix-libs:      libGLX_nvidia libnvidia-glvkspirv libnvidia-gpucomp libnvidia-allocator
# matrix-entry:     Vulkan compute + hwupload/hwdownload
# matrix-criterion: raw frames come back out of the filter chain at the
#                   expected byte count -- an initialisation-only run would
#                   produce ioctls and no pixels
# matrix-status:    ready
set -uo pipefail
_LEA_LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib" && pwd) || exit 1
# shellcheck source=scripts/lib/config.sh
source "$_LEA_LIB/config.sh"
# shellcheck source=scripts/lib/common.sh
source "$_LEA_LIB/common.sh"
# shellcheck source=scripts/lib/matrix.sh
source "$_LEA_LIB/matrix.sh"

lea_require_tools ffmpeg
work=$(mktemp -d) || exit 1
lea_on_exit "rm -rf $(printf '%q' "$work")"

out=$(lea_matrix_workload ffmpeg -hide_banner -loglevel error -nostdin \
        -init_hw_device vulkan=vk -filter_hw_device vk \
        -f lavfi -i 'testsrc2=size=640x480:rate=25:duration=1' \
        -vf format=nv12,hwupload,gblur_vulkan,hwdownload,format=nv12 \
        -f rawvideo -y "$work/out.raw" 2>&1); rc=$?
echo "$out"
[[ $rc -eq 0 ]] || { error "ffmpeg vulkan filter exited $rc"; exit 1; }
# 25 frames of 640x480 NV12 = 25 * 640*480*3/2.
want=$((25 * 640 * 480 * 3 / 2))
sz=$(stat -c%s "$work/out.raw" 2>/dev/null || echo 0)
[[ ${sz:-0} -eq $want ]] || { error "read back $sz bytes, expected $want"; exit 1; }
lea_matrix_criterion "Vulkan compute filter produced $sz bytes of NV12, exactly 25 frames"
