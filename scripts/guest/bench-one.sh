#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# ONE measurement: one variant, one load. Prints lines the host pours into
# the CSV -- everything else is scripts/bench.sh transport.
#
#   ./bench-one.sh <modul|nativ-host> <load>
#
# Runs in the GUEST (modul) and on the HOST (nativ-host); only the paths
# differ, the measurement is the same. That is why the script lives in
# scripts/guest/ and is also invoked natively from there.
#
# Why exactly one load per invocation: the host reads the CPU time of ITS
# backend around every invocation. Only that way can the work that moves to
# the other side of the boundary while forwarding be attributed -- measuring
# only the guest wall clock makes the transport look cheaper than it is.
#
# Measured ON SITE, not over ssh: the connection latency does not belong in
# the number.
# /usr/bin/env bash, not /bin/bash: a NixOS guest has /bin/sh and
# /usr/bin/env and NOTHING else in /bin -- measured 2026-08-18, where
# this script died as `/bin/bash: bad interpreter`.
set -u

V=${1:?variant: modul|nativ-host}
W=${2:?load}
OUT=/tmp/bench-out-$V.txt
TICK=$(getconf CLK_TCK)

case $V in
    modul)
        # In the guest: NVIDIA userspace and probes live in ~/gpu.
        # modul = real nodes, virtio_nvrm.ko: no LD_PRELOAD, no wrapper.
        cd "$(dirname "$0")" || exit 1
        SMI=./nv/bin/nvidia-smi; NVPROBE=./nvprobe; PING=./ioctlping; MMPING=./mmapping; PY=venv/bin/python; PYDIR=.
        export LD_LIBRARY_PATH="$PWD/nv/lib" NVPROBE_PTX="$PWD/kernels.ptx"
        ;;
    nativ-host)
        # On the host: real driver, system-wide libcuda, probes in probe/.
        cd "$(dirname "$0")/../.." || exit 1
        SMI=nvidia-smi; NVPROBE=./probe/bin/nvprobe; PING=./probe/bin/ioctlping; MMPING=./target/release/mmapping; PYDIR=probe/python
        export NVPROBE_PTX="$PWD/probe/kernels/kernels.ptx"
        # torch comes from vendor/hostvenv: the system Python is 3.14 and
        # there are no wheels for it, and the native reference must run the
        # SAME torch version as the guest (2.13.0+cu130) -- otherwise one
        # compares two libraries instead of two transport paths.
        PY=vendor/hostvenv/bin/python
        [ -x "$PY" ] || PY=python3
        ;;
    *)  echo "ERR unknown variant $V"; exit 2 ;;
esac

# Wall clock in ns (integer -- bc is not installed in the cloud image) and
# the CPU time of all finished children of this shell (cutime+cstime,
# /proc/self/stat fields 16/17, 0-based 15/16).
declare -a F
run() {
    local t0 t1 c0 c1 rc
    read -r -a F < /proc/self/stat; c0=$(( F[15] + F[16] ))
    t0=$(date +%s%N)
    "$@" > "$OUT" 2>&1; rc=$?
    t1=$(date +%s%N)
    read -r -a F < /proc/self/stat; c1=$(( F[15] + F[16] ))
    echo "MEAS wall_ms=$(( (t1 - t0) / 1000000 )) guest_cpu_ms=$(( (c1 - c0) * 1000 / TICK )) rc=$rc"
}

val() { echo "VAL $1 $2"; }

# Pull one number out of the output. If it is missing, NOTHING is printed
# -- a missing line is visible in the CSV, an invented 0 would not be.
grab() { local re=$1 name=$2 v; v=$(grep -oP "$re" "$OUT" | head -1); [ -n "$v" ] && val "$name" "$v"; }

case $W in
    smi)
        run timeout 120 $SMI
        grep -q "NVIDIA-SMI" "$OUT" && val ok 1 || val ok 0
        ;;
    ioctlping)
        # The transport figure: the same cheap ioctl, N times, us per call.
        # No CUDA involved -- the percentiles come from the probe itself.
        run timeout 120 $PING "${NVIP_ITERS:-10000}"
        grab '(?<=p50_us=)[0-9.]+'  p50_us
        grab '(?<=p99_us=)[0-9.]+'  p99_us
        grab '(?<=mean_us=)[0-9.]+' mean_us
        grep -q "fails=0" "$OUT" && val ok 1 || val ok 0
        ;;
    mmapping)
        # The second transport figure, and the only one that measures the
        # MAPPING path instead of the ioctl path: create N window mappings
        # and release them again, that time only. It settles the 40 ms
        # question -- same number of ioctls, faster transport, and cuInit
        # still slower.
        run timeout 180 $MMPING "${NVMM_MAPS:-200}" 20 "${NVMM_BYTES:-65536}"
        grab '(?<=p10_us=)[0-9.]+'  p10_us
        grab '(?<=p50_us=)[0-9.]+'  p50_us
        grab '(?<=p90_us=)[0-9.]+'  p90_us
        grab '(?<=mean_us=)[0-9.]+' mean_us
        grep -q "fails=0" "$OUT" && val ok 1 || val ok 0
        ;;
    smibusy)
        # nvidia-smi is not on any hot path -- but this is the case where
        # the VM's process list actually has work: with a CUDA process
        # holding memory, the host rewrites a PID table and one info entry
        # per process on the way back. `smi` alone measures the empty list
        # and would miss exactly the cost that was added.
        # WARNING: truncate FIRST. Left over from the previous round the
        # file still says "ready", so the wait below returns at once and the
        # measurement is nvidia-smi with NO process holding memory -- the
        # `smi` load, measured a second time under another name. Seen: 32 ms
        # per "busy" run, which is exactly the empty list.
        : > /tmp/bench-hold.txt
        $PY -c "
import torch, time, os, sys
t = torch.empty(64*1024*1024//4, dtype=torch.float32, device='cuda'); t.fill_(1.0)
torch.cuda.synchronize(); print('ready', flush=True); time.sleep(25)" > /tmp/bench-hold.txt 2>&1 &
        HOLD=$!
        for _ in $(seq 120); do grep -q ready /tmp/bench-hold.txt && break; sleep 0.5; done
        grep -q ready /tmp/bench-hold.txt || echo "ERR smibusy: holder never became ready"
        run timeout 120 $SMI
        kill $HOLD 2>/dev/null; wait $HOLD 2>/dev/null
        grep -q "NVIDIA-SMI" "$OUT" && val ok 1 || val ok 0
        ;;
    cuinit|ctx|kernel)
        case $W in cuinit) S=0 ;; ctx) S=1 ;; kernel) S=3 ;; esac
        run timeout 180 $NVPROBE "$S"
        grep -q "stage $S ok" "$OUT" && val ok 1 || val ok 0
        ;;
    convburn)
        run timeout 600 env NVCB_ITERS="${NVCB_ITERS:-100}" $PY "$PYDIR/convburn.py"
        grab '(?<=time=)[0-9.]+'   time_s
        grab '[0-9.]+(?=ms/it)'    ms_per_it
        grab '(?<=acc=)[0-9.e+-]+' acc
        grep -q "ms/it" "$OUT" && val ok 1 || val ok 0
        ;;
    rl)
        run timeout 600 env NVRL_EPISODES="${NVRL_EPISODES:-50}" $PY "$PYDIR/rlprobe.py"
        grab '(?<=mean10=)[0-9.]+' mean10
        grab '(?<=~)[0-9]+(?=/s)'  transfers_per_s
        grep -q "stage4:" "$OUT" && val ok 1 || val ok 0
        ;;
    mm)
        run timeout 600 env NVMM_SIZES="${NVMM_SIZES:-2048,4096}" NVMM_ITERS="${NVMM_ITERS:-10}" \
            $PY "$PYDIR/mmsweep.py"
        # Per size GFLOP/s AND a checksum: the number says how fast, the
        # checksum whether it is the same result.
        while read -r n gf chk; do
            [ -n "${chk:-}" ] && { val "gflops_n$n" "$gf"; val "chk_n$n" "$chk"; }
        done < <(grep -oP 'N=\K[0-9]+|[0-9.]+(?= GFLOP/s)|(?<=chk=)[-0-9.e+]+' "$OUT" | paste - - -)
        grep -q "GFLOP/s" "$OUT" && val ok 1 || val ok 0
        ;;
    *)  echo "ERR unknown load $W"; exit 2 ;;
esac

# The last line of the output travels into the log as evidence.
echo "TAIL $(tail -1 "$OUT" | tr -d '\r' | cut -c1-160)"
