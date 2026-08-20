#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# NVENC: encode a generated clip on the card.
#
# The source is lavfi's testsrc2 rather than a file, so the probe carries no
# pinned binary input and produces the same frames on every host.
#
# matrix-group:     video
# matrix-libs:      libnvidia-encode libcuda
# matrix-entry:     h264_nvenc (NVENCODE API)
# matrix-criterion: an H.264 elementary stream large enough to hold the
#                   frames comes back, and ffmpeg reports no encoder error
#                   -- a refused NVENC session exits non-zero with an empty
#                   file, which is the failure this distinguishes
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
        -f lavfi -i 'testsrc2=size=640x480:rate=25:duration=2' \
        -c:v h264_nvenc -f h264 -y "$work/enc.h264" 2>&1); rc=$?
echo "$out"
[[ $rc -eq 0 ]] || { error "ffmpeg h264_nvenc exited $rc"; exit 1; }
sz=$(stat -c%s "$work/enc.h264" 2>/dev/null || echo 0)
# 50 frames of 640x480. Anything under 10 KiB is a header and no pictures.
[[ ${sz:-0} -gt 10240 ]] || { error "encoded stream is $sz bytes -- no frames"; exit 1; }
lea_matrix_criterion "NVENC produced a $sz byte H.264 stream from 50 generated frames"
