#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# OpenCL: NVIDIA's second compute userspace on the same driver. Its escape
# surface is no more derivable from libcuda's than NVML's is, which is why
# nvidia-smi has a probe of its own.
#
# matrix-group:     compute
# matrix-libs:      libnvidia-opencl
# matrix-entry:     clGetPlatformIDs -> clBuildProgram -> clEnqueueNDRangeKernel
# matrix-criterion: all 4096 elements of a vector add come back correct --
#                   an absent vendor library gives a clean "no platform" and
#                   zero ioctls, which only a computed result tells apart
# matrix-status:    ready
set -uo pipefail
_LEA_LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib" && pwd) || exit 1
# shellcheck source=scripts/lib/config.sh
source "$_LEA_LIB/config.sh"
# shellcheck source=scripts/lib/common.sh
source "$_LEA_LIB/common.sh"
# shellcheck source=scripts/lib/matrix.sh
source "$_LEA_LIB/matrix.sh"

[[ -x $LEA_PROBE_BIN/oclprobe ]] || { error "no $LEA_PROBE_BIN/oclprobe -- make -C probe matrix-probes"; exit 1; }

out=$(lea_matrix_workload "$LEA_PROBE_BIN/oclprobe" 2>&1); rc=$?
echo "$out"
[[ $rc -eq 0 ]] || { error "oclprobe exited $rc"; exit 1; }
v=$(sed -n 's/^OCLVERIFIED=//p' <<<"$out" | tail -1)
[[ $v == 4096/4096 ]] || { error "OCLVERIFIED=$v"; exit 1; }
lea_matrix_criterion "OpenCL vector add on NVIDIA's platform: $v elements verified"
