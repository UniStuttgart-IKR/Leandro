#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# Run the Python suites in a guest VM and print one result table.
#
#   probe/run/suites.sh [--name INSTANCE | --ip A.B.C.D] [--only NAME] [--list] [--keep]
#
# The suites are breadth tests over real CUDA APIs through PyTorch, CuPy and
# RAPIDS. They are NOT an acceptance criterion -- see probe/suites/README.md
# for how they relate to the gates and the probes. Two of them are
# CONDITIONAL on a host and a guest knob (LEA_MANAGED_COMPAT; the pin cap):
# the runner derives the expectation from the running rig, and with both
# knobs set all eight pass. This runner exists mainly to keep an expected
# refusal distinguishable from everything else that could go wrong.
#
# Exit code:
#   0  every suite matched its expectation (including expected refusals)
#   1  a deviation -- something regressed, or a known failure started passing
#   2  the run itself could not be carried out (VM unreachable, no venv)
#
# WARNING: needs torch, cupy, cudf and cuml in the guest venv. None of them
# are in the base image; a member that lacks them reports SKIP rather than
# FAIL, because a missing dependency says nothing about the boundary.
# `--install-deps` puts the three missing ones there (gigabytes, opt-in).
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1

source ./scripts/lib/config.sh
source ./scripts/lib/rig.sh

usage() {
    # Skip the shebang and the SPDX block: licence and copyright are
    # metadata for a tool, not the first three lines of help.
    awk 'NR>1 { if ($0 !~ /^#/) exit; sub(/^# ?/, ""); if ($0 ~ /^SPDX-/) next; print }' "$0"
    exit "${1:-0}"
}

IP=$(lea_ip 0); ONLY=""; LIST=0; KEEP=0; DEPS=0; NAME=vm0
while [[ $# -gt 0 ]]; do
    case $1 in
        --ip)   IP=$2; shift 2 ;;
        --name) lea_inst "$2"; IP=$INST_IP; NAME=$2; shift 2 ;;
        --only) ONLY=$2; shift 2 ;;
        --list) LIST=1; shift ;;
        --keep) KEEP=1; shift ;;
        --install-deps) DEPS=1; shift ;;
        -h|--help) usage 0 ;;
        *) error "unknown option: $1"; usage 2 ;;
    esac
done

# HOW TO WAIT FOR IT: this holds a pidfile for its lifetime. Wait on the
# FILE, never on `pgrep -f` -- that pattern stands in the waiting shell's
# own command line and the loop then waits for itself (lea_hold_pidfile
# in scripts/lib/common.sh has the long version):
#   until ! lea_running vm/suites.pid; do sleep 15; done
lea_hold_pidfile "$LEA_VM_DIR/suites.pid"

# Managed memory is not a property of the boundary but of how the HOST
# backend was started: LEA_MANAGED_COMPAT=1 (read in session.rs) answers the
# managed-only UVM commands instead of forwarding them to a 0x1e refusal.
# Measured both ways on the same rig: with the switch test_uvm_migration
# passes and reads 52.0 back; without it cudaMallocManaged returns
# cudaErrorInvalidValue.
#
# So the expectation is DERIVED from the running backend rather than written
# down. Hard-coding it is what produced a recorded "architecture limit" that
# was really a missing environment variable.
backend_has_managed_compat() {
    local pid
    for pid in $(pgrep -x vhost-user-nvr 2>/dev/null) $(pgrep -x vhost-user-nvrm 2>/dev/null); do
        tr '\0' '\n' < "/proc/$pid/environ" 2>/dev/null \
            | grep -qx 'LEA_MANAGED_COMPAT=1' && return 0
    done
    return 1
}
if backend_has_managed_compat; then
    MANAGED_EXPECT="pass"; MANAGED_NOTE=""
    MANAGED_WHY="backend runs with LEA_MANAGED_COMPAT=1"
else
    MANAGED_EXPECT="xfail"
    MANAGED_NOTE="no LEA_MANAGED_COMPAT=1 on the backend -- cudaMallocManaged returns cudaErrorInvalidValue"
    MANAGED_WHY="backend runs WITHOUT LEA_MANAGED_COMPAT"
fi

# Pinned host memory is the same kind of story as managed memory: not a
# property of the boundary but of a knob. The guest module caps concurrently
# pinned memory at `max_pin_mib` (default 1024). test_async_streams.py wants
# four streams x two 256 MiB buffers = ~2 GiB and fails at the cap, with the
# same 304 a genuine refusal would give.
#
# There are TWO limits and the suite can hit either:
#   guest module, cumulative:  max_pin_mib      default 1024 MiB
#   host backend, per alloc:   LEA_MAX_PIN_MIB  default  256 MiB
# The suite allocates 256 MiB buffers, so it sits exactly ON the host's
# per-allocation default, and ~2 GiB in total, well over the guest's.
# Both refuse with CUDA error 304; the backend log distinguishes them
# ("over the pin limit ... (LEA_MAX_PIN_MIB)").
#
# Measured on one rig: with both raised it runs through, and its NaN
# checksums appear IDENTICALLY in the native host run -- they come from the
# test's own fp16 arithmetic, not from the crossing.
#
# The expectation is therefore read from the guest (below, once it is known
# to be reachable) rather than written down here.
PIN_NEEDED=2048          # ~2 GiB of buffers plus headroom (guest, CUMULATIVE)
# One buffer: 8192x8192 fp32 = EXACTLY 256 MiB, and the host cap is compared
# STRICTLY. Measured 2026-08-21: LEA_MAX_PIN_MIB=256 against a 256 MiB buffer
# is REFUSED (cudaErrorOperatingSystem, the 304 this file documents), and 512
# passes. A cap equal to the allocation is not a cap that admits it.
PIN_BUF=256              # one buffer (host, PER ALLOCATION -- LEA_MAX_PIN_MIB)
PIN_EXPECT="xfail"       # assume the cap until the guest says otherwise
PIN_NOTE="max_pin_mib below $PIN_NEEDED -- pinned memory hits the cap, error 304"
PIN_WHY="max_pin_mib not read yet"

# name | expectation | what a failure means
#
# `xfail` entries are limitations measured on this boundary. They carry the
# error they are expected to produce, so that a DIFFERENT failure in the same
# suite still shows up as a deviation instead of hiding behind the label.
SUITES=(
    "test_async_streams.py|$PIN_EXPECT|$PIN_NOTE"
    "test_cuda_graphs.py|pass|"
    "test_high_freq_event_polling.py|pass|"
    "test_multi_process_cuda.py|pass|"
    "test_nvrtc.py|pass|"
    "test_uvm_migration.py|$MANAGED_EXPECT|$MANAGED_NOTE"
    "test_vram_churn.py|pass|"
    "rapids_cuml_cudf.py|pass|"
)

if [[ $LIST -eq 1 ]]; then
    printf '%-36s %-7s %s\n' SUITE EXPECT NOTE
    for entry in "${SUITES[@]}"; do
        IFS='|' read -r n e note <<<"$entry"
        printf '%-36s %-7s %s\n' "$n" "$e" "$note"
    done
    exit 0
fi

GUESTDIR=gpu/suites
OUT=$LEA_VM_DIR/out-suites
mkdir -p "$OUT"

lea_ssh "$IP" true 2>/dev/null \
    || { error "guest $IP not reachable -- is the VM up (scripts/showcase.sh up)?"; exit 2; }
lea_ssh "$IP" 'test -x ~/gpu/venv/bin/python' 2>/dev/null \
    || { error "no python venv in the guest (~/gpu/venv). Provision with:
       ./scripts/showcase.sh up --with-torch"; exit 2; }

# --install-deps: cupy, cudf and cuml into the guest venv.
#
# OPT-IN, because it is a download measured in gigabytes and because the three
# of them say nothing about the boundary -- a suite that cannot import cudf
# reports SKIP for exactly that reason. But "SKIP: missing module" with no way
# to fix it is a dead end, and this runner knew the names all along (the
# WARNING at the top of this file lists them).
#
# THE CUDA MAJOR IS ASKED, NOT ASSUMED. cupy and RAPIDS ship one wheel per
# CUDA major -- cupy-cuda12x/cupy-cuda13x, cudf-cu12/cudf-cu13 -- and the
# right one is whatever the GUEST's driver stack answers, not whatever this
# host has. Read from nvidia-smi in the guest, which is the same place the
# gate reads the UMD version from.
if [[ $DEPS -eq 1 ]]; then
    CU=$(lea_ssh "$IP" 'cd ~/gpu && LD_LIBRARY_PATH=$PWD/nv/lib ./nv/bin/nvidia-smi 2>/dev/null \
            | sed -n "s/.*CUDA UMD Version: *\([0-9]*\)\..*/\1/p" | head -1' | tr -dc '0-9')
    [[ -n $CU ]] || { error "cannot read the guest's CUDA major -- is the module loaded?"; exit 2; }
    info "installing cupy-cuda${CU}x, cudf-cu$CU and cuml-cu$CU into the guest venv (GBs) ..."
    lea_ssh "$IP" "~/gpu/venv/bin/pip install --quiet cupy-cuda${CU}x" \
        || warn "cupy did not install -- test_nvrtc and test_uvm_migration will still SKIP"
    # RAPIDS is not on PyPI proper; NVIDIA's index is where cudf/cuml live.
    lea_ssh "$IP" "~/gpu/venv/bin/pip install --quiet \
        --extra-index-url=https://pypi.nvidia.com cudf-cu$CU cuml-cu$CU" \
        || warn "cudf/cuml did not install -- rapids_cuml_cudf will still SKIP"
    lea_ssh "$IP" '~/gpu/venv/bin/python -c "
import importlib
for m in (\"cupy\",\"cudf\",\"cuml\"):
    try:
        importlib.import_module(m); print(\"  ok     \", m)
    except Exception as e: print(\"  MISSING\", m, type(e).__name__)"' || true
fi

PIN_MIB=$(lea_ssh "$IP" 'cat /sys/module/virtio_nvrm/parameters/max_pin_mib 2>/dev/null' 2>/dev/null | tr -dc '0-9')
# THE HOST CAP TOO, and reading only the guest's was a real defect: the
# comment above says "There are TWO limits and the suite can hit either" and
# then only one of them was asked. Raising the guest's max_pin_mib flipped
# this expectation to `pass` while the BACKEND still refused every 256 MiB
# buffer at its own default -- so the suite reported FAIL "expected to pass"
# for a knob nobody had touched. llm.md measured exactly this in 2026-08-16:
# "Raising the guest limit alone changes nothing."
#
# Read from the RUNNING BACKEND's environment rather than this shell's: the
# backend was started by an earlier `showcase.sh up`, possibly with a
# different LEA_MAX_PIN_MIB than whoever is running the suites now has set.
# Its own environ is what it is actually enforcing. Empty or unset means the
# 256 MiB default (host_pool.rs).
HOST_PIN=""
_pidf="$(lea_inst_dir "$NAME" 2>/dev/null)/nvrm.pid"
if [[ -r $_pidf ]] && _bp=$(cat "$_pidf" 2>/dev/null) && [[ -r /proc/$_bp/environ ]]; then
    HOST_PIN=$(tr '\0' '\n' < "/proc/$_bp/environ" | sed -n 's/^LEA_MAX_PIN_MIB=//p' | tr -dc '0-9')
fi
[[ -n $HOST_PIN ]] || HOST_PIN=${LEA_MAX_PIN_MIB:-}
[[ -n $HOST_PIN ]] || HOST_PIN=256      # host_pool.rs default, per ALLOCATION
# STRICTLY GREATER, and that is measured rather than reasoned. The suite pins
# 4 streams x 2 x 256 MiB = exactly 2048 MiB, and a cumulative cap of exactly
# 2048 REFUSES it -- there is no room left for anything else the process has
# pinned. Measured 2026-08-21 on one rig, host cap at its 256 MiB default
# throughout:
#
#     guest max_pin_mib   1024 -> refused    2048 -> refused
#                         2560 -> runs       4096 -> runs
#
# THE HOST CAP IS NOT THE BINDING ONE, which is worth writing down because
# the comment above this block reasons that it should be: the buffers are
# exactly 256 MiB and LEA_MAX_PIN_MIB defaults to 256 "per allocation", so
# they ought to sit exactly on it. They do not -- 2560/256 runs through. The
# host cap is reported below for diagnosis and is not part of the
# expectation, because no measurement supports making it one.
if [[ -n ${PIN_MIB:-} && ${PIN_MIB:-0} -gt $PIN_NEEDED ]]; then
    PIN_EXPECT="pass"; PIN_NOTE=""
    PIN_WHY="max_pin_mib=$PIN_MIB (> $PIN_NEEDED needed), host LEA_MAX_PIN_MIB=$HOST_PIN"
else
    PIN_EXPECT="xfail"
    PIN_NOTE="max_pin_mib=${PIN_MIB:-?} does not exceed $PIN_NEEDED -- pinned memory hits the cap, error 304 (raise it: showcase.sh up --max-pin-mib 4096)"
    PIN_WHY="max_pin_mib=${PIN_MIB:-?} (needs > $PIN_NEEDED), host LEA_MAX_PIN_MIB=$HOST_PIN"
fi
SUITES[0]="test_async_streams.py|$PIN_EXPECT|$PIN_NOTE"

info "copying suites to $IP:~/$GUESTDIR"
lea_ssh "$IP" "mkdir -p ~/$GUESTDIR"
tar -C probe/suites -cf - . | lea_ssh "$IP" "tar -C ~/$GUESTDIR -xf -" \
    || { error "could not copy the suites"; exit 2; }

declare -a ROWS=()
deviation=0

for entry in "${SUITES[@]}"; do
    IFS='|' read -r name expect note <<<"$entry"
    [[ -n $ONLY && $name != *"$ONLY"* ]] && continue

    log="$OUT/${name%.py}.log"
    info "-- $name"
    # LD_LIBRARY_PATH is not enough on its own for everything, but it is what
    # the rest of the rig uses; keep it identical so a difference here cannot
    # be the explanation for a differing result.
    lea_ssh "$IP" "cd ~/$GUESTDIR && export LD_LIBRARY_PATH=\$HOME/gpu/nv/lib
        timeout 1200 \$HOME/gpu/venv/bin/python $name" >"$log" 2>&1
    rc=$?

    # A missing module is a rig statement, not a boundary statement.
    if grep -qE "^ModuleNotFoundError|No module named" "$log"; then
        missing=$(grep -oP "No module named '\K[^']+" "$log" | head -1)
        ROWS+=("SKIP|$name|missing module: ${missing:-?}")
        continue
    fi

    if [[ $rc -eq 124 ]]; then
        ROWS+=("FAIL|$name|timed out after 1200s")
        deviation=1
        continue
    fi

    case "$expect" in
    pass)
        if [[ $rc -eq 0 ]]; then
            ROWS+=("PASS|$name|")
        else
            ROWS+=("FAIL|$name|rc=$rc, expected to pass -- see $log")
            deviation=1
        fi
        ;;
    xfail)
        if [[ $rc -ne 0 ]]; then
            ROWS+=("XFAIL|$name|$note")
        else
            # Deliberately a deviation: a known limitation that starts
            # passing is news, and silently swallowing it would leave the
            # table lying for weeks.
            ROWS+=("XPASS|$name|expected to fail ($note) but passed")
            deviation=1
        fi
        ;;
    esac
done

[[ $KEEP -eq 0 ]] && lea_ssh "$IP" "rm -rf ~/$GUESTDIR" 2>/dev/null

echo
echo "== suites ($IP) =="
echo "   ($MANAGED_WHY; $PIN_WHY)"
printf '%-6s %-36s %s\n' RESULT SUITE NOTE
for row in "${ROWS[@]}"; do
    IFS='|' read -r r n note <<<"$row"
    printf '%-6s %-36s %s\n' "$r" "$n" "$note"
done
echo
echo "logs: $OUT/"

skipped=$(printf '%s\n' "${ROWS[@]}" | grep -c '^SKIP|') || true
if [[ ${skipped:-0} -gt 0 ]]; then
    echo "SUITES: $skipped suite(s) could not run (missing dependencies)."
    echo "  install them into the guest venv:  $0 --install-deps${IP:+ --ip $IP}"
    echo "  (gigabytes, and none of them says anything about the boundary --"
    echo "   which is why a missing one is SKIP and never FAIL.)"
    exit 2
fi
if [[ $deviation -ne 0 ]]; then
    echo "SUITES: DEVIATION -- see the table above."
    exit 1
fi
echo "SUITES: as expected (including any expected refusals)."
exit 0
