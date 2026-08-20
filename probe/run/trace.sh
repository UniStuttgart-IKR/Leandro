#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# The LD_PRELOAD tracer, end to end: run a workload under it, write one
# trace per stage, cross-check every stage against strace, evaluate.
#
#   probe/run/trace.sh nvprobe [--out DIR]   raw CUDA driver API, lvl0..lvl4blocking
#   probe/run/trace.sh torch   [--out DIR]   the PyTorch stack, torch0..torch5conv
#   probe/run/trace.sh smi     [--out DIR]   nvidia-smi -- the NVML path, not libcuda
#   probe/run/trace.sh analyse [DIR]         evaluate a trace directory
#
# Traces land in probe/traces/ (gitignored) unless --out says otherwise, and
# `analyse` reads the same default. Every stage leaves three files:
#   <tag>.tsv     the tracer's own record -- the measurement
#   <tag>.jsonl   the same records in the new format (LEA_TRACE_FORMAT)
#   <tag>.strace  the SAME run without the tracer, under strace
#   <tag>.out     the workload's own output (analyse reads ITERS=/MSPERLAUNCH=)
#
# THE COUNTER-CHECK IS THE POINT. The tracer counts what IT saw; strace
# counts what the KERNEL saw. The delta column must be 0. A delta other than
# 0 means somebody bypasses the PLT, and from that moment on every number
# derived from the trace is decoration.
#
# The delta counts strace lines carrying `_IOC`, not every `ioctl(` line.
# Measured 2026-08-18 on the development rig: for nvprobe the two rules agree
# exactly (430 against 430 on lvl1), because strace has no NVIDIA table and
# prints every one of those NVIDIA ioctls in the raw _IOC form. Under `torch`
# they do not come close. Python's own import machinery asks isatty on every
# stream it opens, and each of those is a TCGETS2 that strace NAMES and the
# tracer rightly ignores -- it is not an NVIDIA fd. Counting every `ioctl(`
# line reported, in the same run whose _IOC delta is 0:
#
#   torch0       tracer 431   _IOC 431   ioctl( 1493   -> false delta 1062
#   torch5conv   tracer 517   _IOC 517   ioctl( 2457   -> false delta 1940
#
# So `_IOC` is the rule everywhere here. (A predecessor counted `ioctl(`
# and got away with it only because a C probe opens no tty.)
#
# The library is LEA_TRACE_LIB from scripts/lib/config.sh: absolute, and
# derived from LEA_ROOT like every other path. (The scripts this replaced
# computed the path relative to their own directory and got it wrong --
# an absolute, config-owned path is the lesson, not a convenience.)
#
# What the probes themselves read: NVPROBE_CYCLES, NVPROBE_ITERS,
# NVPROBE_SCHED, NVPROBE_PTX, NVPROBE_NOCLEANUP; NVTORCH_PY (an interpreter
# with PyTorch in it), NVTORCH_ITERS, NVTORCH_CONV, NVTORCH_NOCLEANUP;
# NVIDIA_SMI (the binary to trace).
set -uo pipefail

# No `cd`. Every path below is absolute and derived from LEA_ROOT, so this
# runs from anywhere -- and `$0` stays resolvable, which is what
# lea_usage_from_header reads.
_LEA_LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib" && pwd) || exit 1
# shellcheck source=scripts/lib/config.sh
source "$_LEA_LIB/config.sh"
# shellcheck source=scripts/lib/common.sh
source "$_LEA_LIB/common.sh"

usage() { lea_usage_from_header; exit "${1:-0}"; }

CMD=${1:-}
[[ -n $CMD ]] || usage 2
shift
case $CMD in -h|--help|help) usage 0 ;; esac

D=$LEA_ROOT/probe/traces
while [[ $# -gt 0 ]]; do
    case $1 in
        --out) D=$2; shift 2 ;;
        -h|--help) usage 0 ;;
        -*) error "unknown option: $1"; usage 2 ;;
        *)  # `analyse` takes the directory positionally, nothing else does.
            [[ $CMD == analyse ]] || { error "unexpected argument: $1"; usage 2; }
            D=$1; shift ;;
    esac
done
[[ $D == /* ]] || D=$PWD/$D

# ---------------------------------------------------------------------------
# running a stage
# ---------------------------------------------------------------------------
TAGS=()

# lea_trace_stage <tag> <command...> -- one traced run plus its counter-check.
#
# The traced run and the strace run are the SAME command twice, deliberately:
# strace under LD_PRELOAD would measure the tracer as well, and a single run
# cannot answer "did the tracer see everything" at all.
lea_trace_stage() {
    local tag=$1; shift
    echo "=== $tag ==="
    LEA_TRACE_FILE="$D/$tag.tsv" LD_PRELOAD="$LEA_TRACE_LIB" \
        "$@" > "$D/$tag.out" 2>&1
    tail -2 "$D/$tag.out"
    strace -f -e trace=ioctl -o "$D/$tag.strace" "$@" >/dev/null 2>&1
    TAGS+=("$tag")
    return 0
}

# _lea_stream TAG -- this stage's trace, whatever format it is in,
# projected once to the canonical columns and cached for the run.
#
# Everything below selects on `$1=="ioctl"` and reads columns by number, and
# that is still exactly what it does: the projection is where the format is
# known, and it is `lea_trace_stream` in scripts/lib/common.sh -- one reader,
# the same one the matrix pipeline and the guest half ask.
# The directory is created ONCE, here, at the top level. It deliberately is
# not created on demand inside `_lea_stream`: almost every caller is a
# `$(_lea_stream ...)` command substitution, which is a SUBSHELL, so the
# assignment would not survive the call and -- much worse -- the `lea_on_exit`
# cleanup would fire when that subshell exited and delete the directory
# before the caller could read the path it had just been handed.
_LEA_STREAMS=$(mktemp -d) || die "cannot create a temporary directory"
lea_on_exit "rm -rf $(printf '%q' "$_LEA_STREAMS")"

_lea_stream() {
    local tag=$1 src
    if [[ ! -s $_LEA_STREAMS/$tag ]]; then
        src=$(lea_trace_file "$D" "$tag")
        lea_trace_stream "$src" > "$_LEA_STREAMS/$tag"
    fi
    printf '%s' "$_LEA_STREAMS/$tag"
}

# _lea_stages -- every stage that left a trace, in either format.
#
# The TSV glob is walked FIRST and the order it comes back in is kept. That
# is not arbitrary: the saturation curve's `new` column is cumulative, so it
# depends on the order the stages are visited in, and `sort -u` over the
# extensionless names put `smi` before `smi-q` where the glob puts `smi-q`
# first. A format change that reorders a measurement is not a format change.
_lea_stages() {
    local f t
    local -A seen=()
    for f in "$D"/*.tsv "$D"/*.jsonl; do
        [[ -f $f ]] || continue
        t=$(basename "$f"); t=${t%.tsv}; t=${t%.jsonl}
        [[ -n ${seen[$t]:-} ]] && continue
        seen[$t]=1
        printf '%s\n' "$t"
    done
}

# lea_trace_table -- the delta table over everything lea_trace_stage ran.
lea_trace_table() {
    local t a b nf
    echo
    printf '%-14s %8s %8s %7s %6s\n' stage tracer strace delta NF
    for t in "${TAGS[@]}"; do
        a=$(grep -c '^ioctl' "$(_lea_stream "$t")" 2>/dev/null) || a=0
        b=$(grep -c '_IOC'   "$D/$t.strace" 2>/dev/null) || b=0
        nf=$(awk -F'\t' '$1=="ioctl"{print NF; exit}' "$(_lea_stream "$t")" 2>/dev/null)
        printf '%-14s %8s %8s %7s %6s\n' "$t" "$a" "$b" "$((b - a))" "${nf:-0}"
    done
    echo
    echo "delta must be 0. NF must be >=9 (the fd field); below that, an old"
    echo "log.rs is in the tree."
}

lea_trace_prepare() {
    lea_require_tools strace
    [[ -f $LEA_TRACE_LIB ]] \
        || die "no $LEA_TRACE_LIB -- ./scripts/build.sh cargo"
    mkdir -p "$D" || die "cannot write $D"
}

# ---------------------------------------------------------------------------
# nvprobe -- the raw CUDA driver API, stage by stage
# ---------------------------------------------------------------------------
do_nvprobe() {
    lea_trace_prepare
    [[ -x $LEA_ROOT/probe/bin/nvprobe ]] \
        || die "no probe/bin/nvprobe -- ./scripts/build.sh probes"
    export NVPROBE_PTX="${NVPROBE_PTX:-$LEA_ROOT/probe/kernels/kernels.ptx}"

    # HOW TO WAIT FOR IT: this holds a pidfile for its lifetime. Wait on the
    # FILE, never on `pgrep -f` -- that pattern stands in the waiting shell's
    # own command line (lea_hold_pidfile in scripts/lib/common.sh):
    #   until ! lea_running vm/trace-nvprobe.pid; do sleep 15; done
    lea_hold_pidfile "$LEA_VM_DIR/trace-nvprobe.pid"

    local lvl
    for lvl in 0 1 2 3; do
        lea_trace_stage "lvl$lvl" "$LEA_ROOT/probe/bin/nvprobe" "$lvl"
    done
    # Stage 4 twice: the scheduling mode decides whether the completion goes
    # through a spin or through a blocking wait, and that is exactly the
    # difference the wait-path section of `analyse` reads.
    NVPROBE_SCHED=auto     lea_trace_stage lvl4auto     "$LEA_ROOT/probe/bin/nvprobe" 4
    NVPROBE_SCHED=blocking lea_trace_stage lvl4blocking "$LEA_ROOT/probe/bin/nvprobe" 4
    lea_trace_table
}

# ---------------------------------------------------------------------------
# torch -- the same question through the PyTorch stack
# ---------------------------------------------------------------------------
do_torch() {
    lea_trace_prepare
    local py=${NVTORCH_PY:-python3}
    command -v "$py" >/dev/null || die "no interpreter '$py' (NVTORCH_PY)"
    "$py" -c 'import torch' 2>/dev/null \
        || die "no PyTorch in $py -- set NVTORCH_PY to an interpreter that has it"

    #   until ! lea_running vm/trace-torch.pid; do sleep 15; done
    lea_hold_pidfile "$LEA_VM_DIR/trace-torch.pid"

    local lvl
    for lvl in 0 1 2 3 4 5; do
        lea_trace_stage "torch$lvl" "$py" "$LEA_ROOT/probe/python/torchprobe.py" "$lvl"
    done
    # cuDNN only enters the picture with a convolution, and it brings its own
    # allocations -- the one stage that is not just "more of stage 5".
    NVTORCH_CONV=1 lea_trace_stage torch5conv "$py" "$LEA_ROOT/probe/python/torchprobe.py" 5
    lea_trace_table
}

# ---------------------------------------------------------------------------
# smi -- nvidia-smi, which goes through NVML and not through libcuda
#
# Why separately: its escape surface can contain escapes and controls that
# appear in no libcuda and no torch trace at all. Forwarding it correctly
# cannot be inferred from those traces, it has to be measured on its own.
# ---------------------------------------------------------------------------
do_smi() {
    lea_trace_prepare
    local smi=${NVIDIA_SMI:-nvidia-smi}
    command -v "$smi" >/dev/null || die "no $smi in PATH"

    #   until ! lea_running vm/trace-smi.pid; do sleep 15; done
    lea_hold_pidfile "$LEA_VM_DIR/trace-smi.pid"

    lea_trace_stage smi   "$smi"          # the default invocation (the table)
    lea_trace_stage smi-q "$smi" -q       # -q pulls considerably more RM_CONTROL
    lea_trace_table

    echo
    echo "--- escapes nvidia-smi uses (nr) ---"
    awk -F'\t' '$1=="ioctl"{print $2, $3}' \
        "$(_lea_stream smi)" "$(_lea_stream smi-q)" | sort -u

    echo
    echo "--- signatures new versus libcuda (needs lvl4blocking.tsv) ---"
    if [[ -f $D/lvl4blocking.tsv || -f $D/lvl4blocking.jsonl ]]; then
        sig() { awk -F'\t' '$1=="ioctl"{print $2"\t"$3"\t"$4}' "$@" | sort -u; }
        comm -13 <(sig "$(_lea_stream lvl4blocking)") \
                 <(sig "$(_lea_stream smi)" "$(_lea_stream smi-q)")
    else
        echo "  (lvl4blocking.tsv missing -- probe/run/trace.sh nvprobe first)"
    fi
}

# ---------------------------------------------------------------------------
# analyse -- evaluation of the tracer runs
#
# Line formats:
#   ioctl     dev  nr  sub  size  psize  ret  status    (8 fields + fd = 9)
#   mmap      dev  fd  len  off  addr                   (6)
#   open      dev  fd                                   (3)
#   read      dev  fd  ret                              (4)
#   poll      dev  fd  revents                          (4)
#   eventreg  fd   previous                             (3)
#
# poll: one line per FD in the pollfd array, NOT per syscall. The line count
# is calls x array size -- keep that in mind when reading it.
#
# Additional .tsv files (e.g. torch-fwd.tsv) are appended automatically --
# the saturation curve then runs through to them.
# ---------------------------------------------------------------------------
hdr() { printf '\n\033[1m--- %s ---\033[0m\n' "$*"; }

do_analyse() {
    local t f nf L LN prev calc sigs new

    [[ -d $D ]] || die "no directory $D -- probe/run/trace.sh nvprobe"
    [[ -f $D/lvl3.tsv || -f $D/lvl3.jsonl ]] \
        || die "no traces in $D/ -- probe/run/trace.sh nvprobe"

    # A stale log.rs in the tree writes an ioctl line without the fd field.
    # The projection keeps a short legacy line short and spells a missing
    # JSON key as `-`, so both shapes are caught here.
    nf=$(awk -F'\t' '$1=="ioctl"{print NF; exit}' "$(_lea_stream lvl3)")
    [[ ${nf:-0} -ge 9 ]] || die "NF=${nf:-0}, expected >=9. Stale log.rs in the tree."
    [[ $(awk -F'\t' '$1=="ioctl"{print $9; exit}' "$(_lea_stream lvl3)") != "-" ]] \
        || die "the ioctl records carry no fd. Stale log.rs in the tree."

    # Known stages in order, then everything else alphabetically.
    local -a ORDER=(lvl0 lvl1 lvl2 lvl3 lvl4auto lvl4blocking) SEEN=()
    for t in "${ORDER[@]}"; do
        [[ -f $D/$t.tsv || -f $D/$t.jsonl ]] && SEEN+=("$t")
    done
    while read -r t; do
        [[ -n $t ]] || continue
        [[ " ${ORDER[*]} " == *" $t "* ]] || SEEN+=("$t")
    done < <(_lea_stages)

    # The heading names the FILE the report was made from, as it always
    # has -- which now also says which of the two formats that was.
    L="$(_lea_stream "${SEEN[-1]}")"
    LN=$(basename "$(lea_trace_file "$D" "${SEEN[-1]}")")
    echo "reference run for the detailed evaluation: $LN"

    # Signature sets are compared pairwise, so they have to be kept. A fixed
    # /tmp/sig.<tag> was what this did before: a world-writable name that two
    # concurrent runs -- or two users -- silently share.
    local sigdir
    sigdir=$(mktemp -d) || die "cannot create a temporary directory"
    lea_on_exit "rm -rf $(printf '%q' "$sigdir")"

    # -----------------------------------------------------------------------
    # 1  saturation curve
    #    The measurement that carries the argument against API remoting: does
    #    the surface grow with CUDA features, or is it finite?
    # -----------------------------------------------------------------------
    hdr "saturation curve (signature = device + nr + sub)"
    printf '%-16s %8s %11s %6s\n' run calls signatures new
    prev=""
    for t in "${SEEN[@]}"; do
        f="$(_lea_stream "$t")"
        calc=$(awk -F'\t' '$1=="ioctl"' "$f" | wc -l)
        awk -F'\t' '$1=="ioctl"{print $2"\t"$3"\t"$4}' "$f" | sort -u > "$sigdir/$t"
        sigs=$(wc -l < "$sigdir/$t")
        if [[ -n $prev ]]; then new=$(comm -13 "$prev" "$sigdir/$t" | wc -l); else new=$sigs; fi
        printf '%-16s %8s %11s %6s\n' "$t" "$calc" "$sigs" "$new"
        prev="$sigdir/$t"
    done
    echo "  Once 'new' flattens, the surface is finite -- measured, not claimed."

    # -----------------------------------------------------------------------
    # 2  surface per device
    # -----------------------------------------------------------------------
    hdr "surface per device ($LN)"
    printf '%-10s %9s %11s\n' device calls signatures
    local dev c s
    for dev in ctl gpu uvm uvmtools event; do
        c=$(awk -F'\t' -v D="$dev" '$1=="ioctl" && $2==D' "$L" | wc -l)
        [[ $c -gt 0 ]] || continue
        s=$(awk -F'\t' -v D="$dev" '$1=="ioctl" && $2==D{print $3"\t"$4}' "$L" | sort -u | wc -l)
        printf '%-10s %9s %11s\n' "$dev" "$c" "$s"
    done

    # -----------------------------------------------------------------------
    # 3  UVM
    #    Its own set of ioctls with its own structs, none of them NVOS*.
    #    UVM uses NO _IOC encoding (UVM_IOCTL_BASE(i) = i), so these are raw
    #    numbers -- including 0x30000001 (UVM_INITIALIZE), which with _IOC
    #    masking would wrongly have appeared as 0x1.
    # -----------------------------------------------------------------------
    hdr "UVM numbers ($LN)"
    local u
    u=$(awk -F'\t' '$1=="ioctl" && $2 ~ /^uvm/{print $3}' "$L" | sort -u | wc -l)
    echo "  distinct UVM ioctls: $u"
    awk -F'\t' '$1=="ioctl" && $2 ~ /^uvm/{print $3}' "$L" | sort | uniq -c | sort -rn | head -25

    # -----------------------------------------------------------------------
    # 4  RM_CONTROL
    # -----------------------------------------------------------------------
    hdr "RM_CONTROL commands ($LN)"
    local nc
    nc=$(awk -F'\t' '$1=="ioctl" && $3=="0x2a" && $4!="-"{print $4}' "$L" | sort -u | wc -l)
    echo "  distinct commands: $nc     <- size of the dispatch table"
    awk -F'\t' '$1=="ioctl" && $3=="0x2a" && $4!="-"{print $4}' "$L" | sort | uniq -c | sort -rn | head -20
    echo
    echo "  paramsSize:"
    awk -F'\t' '$1=="ioctl" && $3=="0x2a"{
        print ($6=="0x0") ? "    zero" : (($6=="-") ? "    not decoded" : "    set")
    }' "$L" | sort | uniq -c
    echo "  commands with paramsSize == 0 (special cases of forwarding):"
    awk -F'\t' '$1=="ioctl" && $3=="0x2a" && $6=="0x0"{print "    "$4}' "$L" | sort | uniq -c

    # -----------------------------------------------------------------------
    # 5  RM_ALLOC
    #    Measured: always _IOC_SIZE 48 (NVOS64, never NVOS21), paramsSize
    #    always 0. The driver derives the size from hClass -> a hand-written
    #    table, as in nvproxy. The line count below is the size of that table.
    # -----------------------------------------------------------------------
    hdr "RM_ALLOC classes ($LN)"
    local na ch tsg
    na=$(awk -F'\t' '$1=="ioctl" && $3=="0x2b" && $4!="-"{print $4}' "$L" | sort -u | wc -l)
    echo "  distinct classes: $na     <- size of the hClass->size table"
    awk -F'\t' '$1=="ioctl" && $3=="0x2b" && $4!="-"{print $4}' "$L" | sort | uniq -c | sort -rn
    echo
    echo "  _IOC_SIZE / paramsSize:"
    awk -F'\t' '$1=="ioctl" && $3=="0x2b"{print "    "$5"  "$6}' "$L" | sort | uniq -c
    echo "  (48 = NVOS64; paramsSize 0x0 = not self-describing)"

    # Channel budget: relevant for multi-tenant. Density presumably hangs off
    # channels per runlist, not off VRAM.
    ch=$(awk -F'\t' '$1=="ioctl" && $3=="0x2b" && $4=="0xc46f"' "$L" | wc -l)
    tsg=$(awk -F'\t' '$1=="ioctl" && $3=="0x2b" && $4=="0xa06c"' "$L" | wc -l)
    echo "  channel budget: $ch GPFIFO channels (0xc46f), $tsg channel groups (0xa06c)"

    # -----------------------------------------------------------------------
    # 6  RM status
    #    ret == 0 with status != NV_OK is the normal case. Building the
    #    forwarding on the ioctl return value alone suppresses errors the
    #    guest expects.
    # -----------------------------------------------------------------------
    hdr "RM status != NV_OK ($LN)"
    local n
    n=$(awk -F'\t' '$1=="ioctl" && $8!="-" && $8!="0x0"' "$L" | wc -l)
    if [[ $n -gt 0 ]]; then
        awk -F'\t' '$1=="ioctl" && $8!="-" && $8!="0x0"{printf "  nr=%s sub=%s status=%s\n",$3,$4,$8}' "$L" \
            | sort | uniq -c | sort -rn | head -15
    else
        echo "  none -- every call went through cleanly"
    fi

    # -----------------------------------------------------------------------
    # 7  mmap
    #    Every mapping becomes a mapping across the boundary. mmap64 is a
    #    separate symbol on glibc and must be hooked too, otherwise this
    #    section undercounts.
    # -----------------------------------------------------------------------
    hdr "mmap profile ($LN)"
    local m mm o
    m=$(awk -F'\t' '$1=="mmap"' "$L" | wc -l)
    mm=$(awk -F'\t' '$1=="ioctl" && ($3=="0x4e" || $3=="0x2e")' "$L" | wc -l)
    echo "  mappings: $m"
    echo "  for comparison: $mm x RM_MAP_MEMORY in the same run"
    [[ $m -lt $mm ]] && echo "  WARNING: fewer mmaps than MAP_MEMORY -- is mmap64 hooked?"
    awk -F'\t' '$1=="mmap"{print "    "$2}' "$L" | sort | uniq -c | sort -rn
    echo "  sizes:"
    awk -F'\t' '$1=="mmap"{
        n=$4+0
        b = (n<4096) ? "   <4K" : (n<65536) ? "4K-64K" : (n<1048576) ? "64K-1M" :
            (n<16777216) ? " 1M-16M" : "  >16M"
        c[b]++
    } END { for (k in c) printf "    %-8s %6d\n", k, c[k] }' "$L" | sort -k2 -rn
    echo "  non-zero offsets on RM devices (the driver demands 0 there):"
    o=$(awk -F'\t' '$1=="mmap" && $5!="0" && $2!~/^uvm/' "$L" | wc -l)
    if [[ $o -gt 0 ]]; then
        awk -F'\t' '$1=="mmap" && $5!="0" && $2!~/^uvm/{print "    "$0}' "$L" | head -5
    else
        echo "    none (expected; UVM encodes the VA range there and is exempt)"
    fi

    # -----------------------------------------------------------------------
    # 8  wait path
    #    Measured: 1 read per launch, 0 ioctls. Completion runs over FDs. What
    #    matters is WHICH: a /dev/nvidia* FD registered via
    #    NV_ESC_ALLOC_OS_EVENT means the guest side only has to make poll() on
    #    its own chardev wakeable -- no event pipe.
    # -----------------------------------------------------------------------
    hdr "wait path ($LN)"
    local nw ne
    nw=$(awk -F'\t' '$1=="read"||$1=="poll"' "$L" | wc -l)
    if [[ $nw -gt 0 ]]; then
        echo "  reads per FD:"
        awk -F'\t' '$1=="read"{print "    "$2" fd="$3}' "$L" | sort | uniq -c | sort -rn | head -8
        echo "  polls per FD (lines = calls x array size):"
        awk -F'\t' '$1=="poll"{print "    "$2" fd="$3}' "$L" | sort | uniq -c | sort -rn | head -8
        echo "  distinct polled FDs: $(awk -F'\t' '$1=="poll"{print $3}' "$L" | sort -u | wc -l)"
    else
        echo "  no read/poll lines -- hooks not built in?"
        echo "  counter-check: strace -c -e trace=read,poll probe/bin/nvprobe 4"
    fi

    hdr "event registration ($LN)"
    ne=$(grep -c '^eventreg' "$L") || ne=0
    if [[ $ne -gt 0 ]]; then
        echo "  $ne registrations, where the FDs come from:"
        awk -F'\t' '$1=="eventreg"{print "    "$3}' "$L" | sort | uniq -c
        echo "  'gpu'/'ctl' = a known device FD -> NO event pipe needed,"
        echo "  only a wakeable poll() on the faked chardev."
        echo "  'new'       = an eventfd -> completions have to be pumped."
        echo
        echo "  registered vs. polled:"
        comm -3 <(awk -F'\t' '$1=="eventreg"{print $2}' "$L" | sort -n | uniq) \
                <(awk -F'\t' '$1=="poll"{print $3}' "$L" | sort -n | uniq) \
            | sed 's/^/    /' | head -20
        echo "  (left column = registered only, right = polled only;"
        echo "   a systematic offset means: two FDs per event)"
    else
        echo "  none -- note_event_fd() inactive or the field offset is wrong"
    fi

    # -----------------------------------------------------------------------
    # 9  cost per launch
    # -----------------------------------------------------------------------
    hdr "cost per kernel launch (difference against lvl3)"
    local b_i b_r b_p it ms n_i n_r n_p
    b_i=$(awk -F'\t' '$1=="ioctl"' "$(_lea_stream lvl3)" | wc -l)
    b_r=$(awk -F'\t' '$1=="read"'  "$(_lea_stream lvl3)" | wc -l)
    b_p=$(awk -F'\t' '$1=="poll"'  "$(_lea_stream lvl3)" | wc -l)
    printf '%-16s %7s %9s %8s %8s   %s\n' run iters ioctl/L read/L poll/L ms/launch
    for t in lvl4auto lvl4blocking; do
        [[ -f $D/$t.tsv || -f $D/$t.jsonl ]] || continue
        f="$(_lea_stream "$t")"
        it=$(sed -n 's/^ITERS=//p'       "$D/$t.out" 2>/dev/null | tail -1); it=${it:-1}
        ms=$(sed -n 's/^MSPERLAUNCH=//p' "$D/$t.out" 2>/dev/null | tail -1); ms=${ms:-?}
        n_i=$(awk -F'\t' '$1=="ioctl"' "$f" | wc -l)
        n_r=$(awk -F'\t' '$1=="read"'  "$f" | wc -l)
        n_p=$(awk -F'\t' '$1=="poll"'  "$f" | wc -l)
        awk -v t="$t" -v i="$it" -v ms="$ms" \
            -v di="$((n_i-b_i))" -v dr="$((n_r-b_r))" -v dp="$((n_p-b_p))" \
            'BEGIN{printf "%-16s %7d %9.3f %8.3f %8.2f   %s\n", t, i, di/i, dr/i, dp/i, ms}'
    done
    echo
    echo "  ioctl/L == 0: the submission path does not cross the ioctl boundary."
    echo "  read/L ~ 1:   the completion, over an FD instead of a round trip."
}

case $CMD in
    nvprobe) do_nvprobe ;;
    torch)   do_torch ;;
    smi)     do_smi ;;
    analyse) do_analyse ;;
    *) error "unknown subcommand: $CMD"; usage 2 ;;
esac
