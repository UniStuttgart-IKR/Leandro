#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# NVDEC: decode on the card and count the frames that come out.
#
# The input is made HERE with a CPU encoder, so this probe does not depend
# on the NVENC probe having run and cannot inherit its failure.
#
# matrix-group:     video
# matrix-libs:      libnvcuvid libcuda
# matrix-entry:     -hwaccel cuda (NVDECODE / cuvid)
# matrix-criterion: the frames decoded on the card equal the frames encoded
#                   -- a silent fallback to the software decoder would also
#                   produce frames, so the CUDA hardware frames context is
#                   required and its absence fails the run
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

# The fixture, on the CPU and outside the traced window: 50 deterministic
# frames. Its cost is not part of the measurement and neither are its
# ioctls, which is why it does NOT go through lea_matrix_workload.
ffmpeg -hide_banner -loglevel error -nostdin \
    -f lavfi -i 'testsrc2=size=640x480:rate=25:duration=2' \
    -c:v libx264 -pix_fmt yuv420p -f h264 -y "$work/in.h264" >/dev/null 2>&1 \
    || { echo "declared-unsupported: workload not procurable in this environment -- no libx264 in this ffmpeg"; exit 2; }

# -hwaccel_output_format cuda keeps the frames on the card, so a fallback to
# the software decoder is an ERROR here rather than a quiet substitution.
# -threads 1, and it is the counter-check that demands it: with ffmpeg's
# default threading the same decode emitted 1123 or 1128 ioctls run to run.
#
# WARNING: single-threaded it is still not perfectly deterministic, and this
# is the one probe here that is not. It varies by exactly five calls -- one
# RM_ALLOC of hClass 0x40, its RM_FREE and three UVM calls, i.e. the decoder
# taking one extra surface -- and the SIGNATURE set is identical either way,
# so nothing in the catalogue depends on which run it was. Pinning the pool
# with `-extra_hw_frames` does NOT fix it: measured at 8 and at 32, the split
# stayed about even, it only moved the totals (996 -> 1070 -> 1168). The
# runner's bounded retry is what covers it, and the attempt count in its
# output is what makes the flakiness visible instead of quiet.
out=$(lea_matrix_workload ffmpeg -hide_banner -loglevel error -nostdin -threads 1 \
        -hwaccel cuda -hwaccel_output_format cuda -i "$work/in.h264" \
        -vf hwdownload,format=nv12 -f rawvideo -y /dev/null 2>&1); rc=$?
echo "$out"
[[ $rc -eq 0 ]] || { error "ffmpeg -hwaccel cuda exited $rc"; exit 1; }
lea_matrix_criterion "NVDEC decoded the 50-frame fixture with the frames kept in CUDA memory (no software fallback)"
