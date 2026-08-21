#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# The commands the guest's graphics stack never asks, asked directly.
#
# OPEN-QUESTIONS number 52. Every graphics probe issues the same handful of
# commands natively and none of them in a guest, so those signatures are
# predicted to be carried and exercised by nothing on the other side --
# `predicted-green` for them stays untested however many guest sweeps pass.
# No workload can settle that, because the workloads are what stopped
# asking. This probe asks, through raw RM calls and no driver userspace at
# all: `probe/c/rmdirect.c` opens the control node, builds the object
# hierarchy by hand and issues each command once.
#
# It is therefore the one probe here that measures the BOUNDARY rather than
# a library's use of it. Everything else in this directory is a workload
# whose ioctls are a side effect; this one has no side effects, only ioctls.
#
# matrix-group:     direct
# matrix-libs:
# matrix-entry:     raw RM ioctls (probe/c/rmdirect.c), no driver userspace
# It also allocates AMPERE_SMC_MONITOR_SESSION (class 0xc640), which is
# number 65 and not one of number 52's commands. That allocation is NOT a
# pass criterion and never fails the probe: without the MIG monitor
# capability -- a node the host's nvidia.ko makes and this project's guest
# module does not -- RM refuses it with NV_ERR_INSUFFICIENT_PERMISSIONS on
# BOTH sides, which is the measurement rather than a fault. It is issued so
# that the class is exercised by something at all; nothing else in a guest
# ever asks for it.
#
# matrix-criterion: five of number 52's six commands answer NV_OK through
#                   whatever boundary this side has -- the two GPU id
#                   controls, the attach/detach pair and the two that need
#                   an object hierarchy
# matrix-status:    ready
set -uo pipefail
_LEA_LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib" && pwd) || exit 1
# shellcheck source=scripts/lib/config.sh
source "$_LEA_LIB/config.sh"
# shellcheck source=scripts/lib/common.sh
source "$_LEA_LIB/common.sh"
# shellcheck source=scripts/lib/matrix.sh
source "$_LEA_LIB/matrix.sh"

bin=$LEA_PROBE_BIN/rmdirect
[[ -x $bin ]] || { echo "declared-unsupported: workload not procurable in this environment -- no probe/bin/rmdirect"; exit 2; }

out=$(lea_matrix_workload "$bin" 2>&1); rc=$?
echo "$out"

# The count the binary prints, not the exit code alone: a probe that says
# "one command was refused" is a finding, and which one it was is in the
# output above.
failed=$(sed -n 's/^COMMANDS_FAILED=//p' <<<"$out" | tail -1)
[[ -n $failed ]] || { error "rmdirect printed no COMMANDS_FAILED line (exited $rc)"; exit 1; }
[[ $failed -eq 0 ]] || { error "$failed of number 52's commands were refused here"; exit 1; }

ids=$(sed -n 's/^ *probed ids: //p' <<<"$out" | tail -1)
[[ ${ids:-0} -ge 1 ]] || { error "GPU_GET_PROBED_IDS answered with no GPU"; exit 1; }

lea_matrix_criterion "five of number 52's six commands answered NV_OK on \
$ids probed GPU: GET_PROBED_IDS, ATTACH_IDS, DETACH_IDS, TIMER_GET_TIME and \
SYSTEM_GET_CAPS_V2 (IDLE_CHANNELS needs a channel and is not covered)"
