# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# The per-library ioctl coverage matrix: everything that is shared between
# the entry point (scripts/ioctl-matrix.sh) and the probes in probe/matrix/.
# Not a script -- source it:
#   source "$LEA_ROOT/scripts/lib/matrix.sh"
#
# THE RULE THIS FILE EXISTS TO ENFORCE: nothing here is a list of NVIDIA
# facts. Every number, every library name and every command name is READ --
# from the running driver, from the host's package manager, from
# scripts/lib/provision.sh's own staging arrays, or from the vendor headers.
# A human must never have to edit a table here after a driver update. Where
# a rule cannot be read it is derived, and where it cannot be derived the
# artefact says "unknown" rather than guessing.
#
# The three readers below all SELF-CHECK, for the reason drmtrace.sh's
# decoder does: a parser pointed at a file it does not understand produces a
# plausible, empty, wrong answer instead of an error.

# ---- provenance -------------------------------------------------------------
# Every generated artefact carries the same header. If two artefacts disagree
# about the driver they were measured on, that is visible in the file rather
# than in somebody's memory.

# lea_matrix_gpu_field FIELD -- one field out of `nvidia-smi -q`, or "unknown".
lea_matrix_gpu_field() {
    local v
    v=$(nvidia-smi -q 2>/dev/null | awk -F': ' -v f="$1" '
        $0 ~ "^ *" f " *:" { sub(/^[ \t]+/, "", $2); print $2; exit }')
    echo "${v:-unknown}"
}

# lea_matrix_driver -- the driver the measurement was taken against.
#
# The RUNNING driver, not DRIVER_VERSION: the artefacts describe what was
# measured. lea_matrix_check_driver is what refuses a mismatch.
lea_matrix_driver() {
    local v
    v=$(lea_driver_version) || v=""
    [[ -n $v ]] || v=$(nvidia-smi --query-gpu=driver_version --format=csv,noheader 2>/dev/null | head -1)
    echo "${v:-unknown}"
}

# lea_matrix_check_driver -- the lockstep every binary in probe/ also does.
# A trace taken under a driver other than the one the tree targets resolves
# against the wrong headers, and every struct size in the catalogue is then
# a guess wearing a citation.
lea_matrix_check_driver() {
    local running want
    running=$(lea_matrix_driver)
    want=$(lea_want_driver)
    [[ $running == "$want" ]] && return 0
    error "driver lockstep: running $running, this tree targets $want.
The catalogue resolves signatures against vendor/ headers at $want, so a
trace taken under $running would be resolved against the wrong structs.
Fix: boot the pinned driver, or move DRIVER_VERSION and re-fetch
     (./scripts/build.sh vendor)."
    return 1
}

# lea_matrix_provenance [PREFIX] -- the header, one key per line, each line
# prefixed (e.g. "# " for markdown-adjacent files, "#" for TSV).
lea_matrix_provenance() {
    local p=${1-}
    local commit dirty=""
    commit=$(git -C "$LEA_ROOT" rev-parse --short HEAD 2>/dev/null) || commit=unknown
    git -C "$LEA_ROOT" diff --quiet 2>/dev/null || dirty=" (working tree modified)"
    printf '%sdriver:  %s\n' "$p" "$(lea_matrix_driver)"
    printf '%sgpu:     %s\n' "$p" "$(lea_matrix_gpu_field 'Product Name')"
    printf '%sarch:    %s (compute %s)\n' "$p" \
        "$(lea_matrix_gpu_field 'Product Architecture')" \
        "$(nvidia-smi --query-gpu=compute_cap --format=csv,noheader 2>/dev/null | head -1)"
    printf '%skernel:  %s\n' "$p" "$(uname -r)"
    printf '%sdate:    %s\n' "$p" "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf '%scommit:  %s%s\n' "$p" "$commit" "$dirty"
}

# lea_matrix_provenance_json [KEY VALUE]... -- the same header as ONE JSONL
# `meta` record, for the JSONL side of a trace.
#
# A `#` comment line is not JSON, and a trace file that is JSONL except for
# its first six lines is a format with an exception in it -- which is how a
# reader ends up with a special case that later grows. The header is a
# record instead, and `traceread.meta()` reads it from either format.
_lea_json_escape() { printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'; }

lea_matrix_provenance_json() {
    local commit dirty="" extra=""
    commit=$(git -C "$LEA_ROOT" rev-parse --short HEAD 2>/dev/null) || commit=unknown
    git -C "$LEA_ROOT" diff --quiet 2>/dev/null || dirty=" (working tree modified)"
    while (($#)); do
        extra+=$(printf ',"%s":"%s"' "$(_lea_json_escape "$1")" "$(_lea_json_escape "${2-}")")
        shift 2 2>/dev/null || shift
    done
    printf '{"t":"meta","driver":"%s","gpu":"%s","arch":"%s (compute %s)","kernel":"%s","date":"%s","commit":"%s"%s}\n' \
        "$(_lea_json_escape "$(lea_matrix_driver)")" \
        "$(_lea_json_escape "$(lea_matrix_gpu_field 'Product Name')")" \
        "$(_lea_json_escape "$(lea_matrix_gpu_field 'Product Architecture')")" \
        "$(_lea_json_escape "$(nvidia-smi --query-gpu=compute_cap --format=csv,noheader 2>/dev/null | head -1)")" \
        "$(_lea_json_escape "$(uname -r)")" \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        "$(_lea_json_escape "$commit$dirty")" \
        "$extra"
}

# lea_trace_place RAW DEST_BASE [KEY VALUE]... -- put a finished trace where
# it belongs, in every format the tracer wrote, each with its own header.
#
# The tracer writes to a scratch path because it truncates its output file
# in its constructor; this is the step that gives the result its provenance
# and its name. It removes the scratch files in both formats, so a probe
# that is retried does not leave the previous attempt's JSONL behind for the
# next one to be judged on.
lea_trace_place() {
    local raw=$1 base=$2; shift 2
    # Both headers are built from the SAME key/value pairs, so keep them:
    # the loop below would otherwise consume "$@" before the JSONL header
    # ever saw it, and the difference is a header, which nothing gates on.
    local -a kv=("$@") i
    { lea_matrix_provenance '#'
      for ((i = 0; i < ${#kv[@]}; i += 2)); do
          printf '#%s: %s\n' "${kv[i]}" "${kv[i+1]-}"
      done
      cat "$raw"; } > "$base.tsv"
    if [[ -f $raw.jsonl ]]; then
        { lea_matrix_provenance_json "${kv[@]}"
          cat "$raw.jsonl"; } > "$base.jsonl"
    else
        # LEA_TRACE_FORMAT=tsv, or a tracer from before the migration. Do
        # not leave a stale JSONL from an earlier attempt beside a TSV from
        # this one -- lea_trace_file prefers the JSONL and would hand every
        # consumer the wrong run.
        rm -f "$base.jsonl"
    fi
    rm -f "$raw" "$raw.jsonl"
}

# ---- where things go --------------------------------------------------------
# One place, so no consumer computes a path of its own.
lea_matrix_dir()    { echo "$LEA_ROOT/matrix"; }
lea_matrix_traces() { echo "$LEA_ROOT/matrix/traces/$(lea_matrix_driver)"; }
lea_matrix_probes_dir() { echo "$LEA_ROOT/probe/matrix"; }

# ---- probe metadata ---------------------------------------------------------
# A probe declares itself in its own header. There is no registry file: the
# set of probes IS the set of scripts in probe/matrix/, and PROBES.md is
# generated by reading their headers. Adding a probe is adding one file.
#
#   # matrix-group:     compute | video | gl | vulkan | egl | compat | kms
#   # matrix-libs:      libcuda libnvidia-nvvm ...   (space separated, no .so)
#   # matrix-entry:     the entry API this probe exercises
#   # matrix-criterion: what is verified BEYOND the exit code
#   # matrix-status:    ready
#                     | declared-unsupported: <reason>
#                     | blocked: <reason>
#
# lea_matrix_meta FILE KEY -- the value, or empty.
#
# A value may wrap: a continuation is a following comment line that is
# INDENTED and does not open a key of its own. Without that, a criterion
# written on two lines lands in PROBES.md cut in half at the first line
# break -- which reads like a criterion that stops mid-sentence.
lea_matrix_meta() {
    awk -v k="# matrix-$2:" '
        index($0, k) == 1 { sub(/^[^:]*:[[:space:]]*/, ""); v = $0; on = 1; next }
        on {
            if ($0 !~ /^#[[:space:]][[:space:]]+/ || $0 ~ /^# matrix-/) { on = 0; next }
            line = $0; sub(/^#[[:space:]]+/, "", line); v = v " " line
        }
        END { if (v != "") print v }
    ' "$1"
}

# lea_matrix_probe_list -- every probe, in filename order.
lea_matrix_probe_list() {
    local d f
    d=$(lea_matrix_probes_dir)
    for f in "$d"/*.sh; do [[ -e $f ]] || continue; basename "$f" .sh; done
}

# ---- the counting rule ------------------------------------------------------
# What "how many ioctls did this workload make" means, in one place. It is
# the rule OPEN-QUESTIONS number 47 is about: both instruments were counted
# wrongly once, in opposite directions, and each wrong count looked like a
# finding about the other instrument. Anything that compares the two -- the
# native run, the guest run -- asks these five functions and never writes an
# awk line of its own.
#
# Three namespaces, never added together: the RM nodes, the NVKMS node, DRM.
# The strace patterns are disjoint by construction -- `/dev/nvidia-modeset`
# does not match the RM alternation, which is `ctl`, `-uvm` or a digit.

# The three tracer counters take a trace in EITHER format and read it
# through lea_trace_stream, so the selector below is the same awk
# expression it has always been -- the format changed underneath it and
# what "how many ioctls" means did not. That is the property the format
# migration had to preserve: the counter-check against strace is the trust
# anchor of every probe in the matrix, and an instrument that counts
# differently is a new instrument, not a new format.

# lea_matrix_n_tracer TRACE -- ioctls the tracer recorded on the RM nodes.
lea_matrix_n_tracer() {
    lea_trace_stream "$1" \
        | awk -F'\t' '$1=="ioctl" && $2!="drm" && $2!="render" && $2!="modeset"' | wc -l
}
# lea_matrix_n_tracer_kms TRACE -- ... on /dev/nvidia-modeset.
lea_matrix_n_tracer_kms() {
    lea_trace_stream "$1" | awk -F'\t' '$1=="ioctl" && $2=="modeset"' | wc -l
}
# lea_matrix_n_tracer_drm TRACE -- ... on the DRM nodes. Counted, never gated.
lea_matrix_n_tracer_drm() {
    lea_trace_stream "$1" | awk -F'\t' '$1=="ioctl" && ($2=="drm" || $2=="render")' | wc -l
}
# lea_matrix_n_strace STRACE -- the same count from strace -y.
#
# `_IOC(` and not the substring `_IOC`: strace prints a NAMED request for
# every ioctl it knows, and under an interpreter every isatty is a TCGETS2 --
# counting those made a tracer that missed nothing look 1062 calls short.
lea_matrix_n_strace() {
    grep '_IOC(' "$1" 2>/dev/null | grep -cE '/dev/nvidia(ctl|-uvm|[0-9])' || true
}
# lea_matrix_n_strace_kms STRACE -- ... on /dev/nvidia-modeset.
lea_matrix_n_strace_kms() {
    grep '_IOC(' "$1" 2>/dev/null | grep -c '/dev/nvidia-modeset' || true
}

# lea_trace_sig TRACE... -- the signature SET of one or more traces, sorted.
#
# The key is (device, nr, sub), and `sub` is the second DISPATCH level: the
# cmd for RM_CONTROL, the hClass for RM_ALLOC. For NV_ESC_RM_MAP_MEMORY
# (0x4e) it is NOT -- log.rs deliberately puts hMemory there so a mapping can
# be matched to its allocation, and a handle is an INSTANCE, not a dimension
# of the surface. Keyed on it, one escape becomes as many rows as the
# workload made mappings.
#
# Measured 2026-08-21 on the traces OPEN-QUESTIONS number 47 names, raw key
# against this one: lvl3 135 -> 107, lvl4blocking 135 -> 107, torch5conv
# 134 -> 106, i.e. 28 of the 135 "signatures" were one escape wearing 29
# handles. It is worse where more is mapped: nvenc 250 -> 135, vk-enum
# 161 -> 123. A saturation curve built on the raw key therefore starts too
# high and, on a mapping-heavy workload, never flattens.
#
# ioctlmatrix.collect_signatures collapses it the same way and cites the same
# number; this is that rule for the shell consumers.
# Several traces may be given; their signature sets are unioned, which is
# what the "new versus libcuda" comparison asks for. lea_trace_stream takes
# ONE file, so the loop is the caller's job and not an oversight.
lea_trace_sig() {
    local f
    for f in "$@"; do
        lea_trace_stream "$f"
    done | awk -F'\t' '
        $1=="ioctl" { s = ($3=="0x4e") ? "-" : ($4=="" ? "-" : $4)
                      print $2"\t"$3"\t"s }' | sort -u
}

# ---- the serial queue -------------------------------------------------------
# The GPU is a serial resource and these probes measure it. Two at once do not
# corrupt anything, they corrupt the MEASUREMENT -- a trace taken while
# another probe holds a channel is a different trace. The lock is held for
# the whole `trace` run, not per probe, so an operator who starts a second
# run waits instead of interleaving with the first.
#
# WARNING: `flock` on a file descriptor, never a pidfile check. A pidfile
# race has a window; flock does not.
#
# WARNING, and this one cost a run: the lock is held by a CHILD PROCESS and
# not by a descriptor of this shell. A descriptor this shell holds is
# INHERITED by everything it starts, and the guest phase starts three
# daemons that outlive it deliberately -- the two backends and
# cloud-hypervisor. Measured 2026-08-20: a guest run that had exited half an
# hour earlier still held this lock through its VM, and the next run sat in
# `flock` waiting for a lock whose owner was a virtual machine. Nothing in
# the trace phase could have shown it, because that phase starts no daemons.
#
# The holder takes the lock, says so through a file, and then waits for THIS
# shell to disappear. It watches with `kill -0` rather than through a pipe,
# because a pipe would be a descriptor again -- inherited by every daemon,
# which is the bug.
lea_matrix_serial() {
    local lock="$LEA_VM_DIR/ioctl-matrix.lock" ready i
    mkdir -p "$LEA_VM_DIR" || return 1
    ready=$(mktemp) || return 1
    (
        exec {f}>"$lock" || exit 1
        if ! flock -n "$f"; then
            echo waiting > "$ready"
            flock "$f" || exit 1
        fi
        echo held > "$ready"
        while kill -0 "$$" 2>/dev/null; do sleep 2; done
    ) &
    _LEA_MATRIX_LOCK_PID=$!
    lea_on_exit "kill $_LEA_MATRIX_LOCK_PID 2>/dev/null; rm -f $(printf '%q' "$ready"); true"
    for ((i = 0; i < 3600; i++)); do
        grep -q held "$ready" 2>/dev/null && return 0
        if [[ $i -eq 0 ]] && grep -q waiting "$ready" 2>/dev/null; then
            info "another ioctl-matrix run holds $lock -- waiting for it"
        fi
        kill -0 "$_LEA_MATRIX_LOCK_PID" 2>/dev/null || { error "the lock holder died"; return 1; }
        sleep 1
    done
    error "waited an hour for $lock"
    return 1
}

# ---- the probe contract -----------------------------------------------------
# A probe runs its ONE workload through lea_matrix_workload. The runner puts
# the tracer (or strace) around exactly that call and around nothing else.
#
# WHY THE WRAPPER IS HERE AND NOT AROUND THE SCRIPT: what goes inside it is
# what gets MEASURED. Preloaded into the probe script itself, the trace would
# carry every `mkdir` and every `awk` the script runs, and the counter-check
# against strace would be comparing two instruments over two different sets
# of processes. So the shell stays outside the wrapper and only the workload
# goes in.
#
# This used to be a much sharper rule, because the tracer opened
# LEA_TRACE_FILE with O_TRUNC and any preloaded child WIPED the run in
# progress -- silently, because a truncated file is a valid file. It appends
# now (log.rs open_out) and the owner of the path truncates, so a stray child
# costs precision rather than the whole measurement.
#
# Run without the runner (probe/matrix/<name>.sh on its own) the wrapper is
# empty and the probe is just a PASS/FAIL check of the feature path.
lea_matrix_workload() {
    if [[ -n ${LEA_MATRIX_WRAP:-} ]]; then
        # Deliberately word-split: LEA_MATRIX_WRAP is a command prefix.
        # shellcheck disable=SC2086
        $LEA_MATRIX_WRAP "$@"
    else
        "$@"
    fi
}

# lea_matrix_criterion TEXT -- what this run actually verified. The runner
# reads this line back out of the probe's stdout; a probe that exits 0
# without printing it has not stated what it proved and does not pass.
lea_matrix_criterion() { echo "CRITERION: $*"; }

# lea_matrix_unsupported -- a probe that cannot run here says so from its own
# header, so the reason exists in exactly one place. Exit 2, never 1: a
# declared reason is not a failure.
lea_matrix_unsupported() {
    local reason
    reason=$(lea_matrix_meta "$0" status)
    echo "${reason:-declared-unsupported: no reason recorded in the header}"
    exit 2
}

# ---- library inventories ----------------------------------------------------
# Two inventories and their difference. What is ABSENT from the guest is the
# most important line in a feature guarantee, so it has to be data.

# lea_matrix_libdir / lea_matrix_libdir32 -- the same discovery the staging
# uses, so the inventory describes the directory the guest is actually fed
# from and not a directory that merely exists.
lea_matrix_libdir()   { lea_nvidia_libdir "$(lea_want_driver)"; }
lea_matrix_libdir32() { echo "${LEA_NVIDIA_LIB32_DIR:-/usr/lib32}"; }

# lea_matrix_stage_array FILE NAME -- one `local -a NAME=(...)` array out of
# a shell source file, comments stripped, one entry per line.
#
# This is how the guest-staged set is READ rather than transcribed: the
# arrays in scripts/lib/provision.sh are the definition of what the guest
# gets, and a second copy of them here would be exactly the hand-maintained
# table this whole pipeline refuses to have. Referring to them BY NAME is a
# code reference; copying their contents would be a table.
lea_matrix_stage_array() {
    awk -v n="$2" '
        index($0, "local -a " n "=(") { on = 1; sub(/^.*=\(/, "") }
        on {
            line = $0
            sub(/[[:space:]]*#.*$/, "", line)       # a trailing or whole-line comment
            fin = (index(line, ")") > 0)
            sub(/\).*$/, "", line)
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", line)
            if (line != "") print line
            if (fin) exit
        }
    ' "$1" | tr ' ' '\n' | sed '/^$/d'
}

# The six arrays that define the payload, named where they are defined.
# lea_payload_stage: the compute set. lea_gl_stage: GL/EGL/Vulkan and the
# 32-bit half.
_LEA_MATRIX_ARRAYS_64="libs optional versioned loose"
_LEA_MATRIX_ARRAYS_32="versioned32 optional32"

# lea_matrix_staged BITS -- the staged set, one bare library name per line
# (no .so suffix, no version). BITS is 64 or 32.
lea_matrix_staged() {
    local a names
    [[ $1 == 32 ]] && names=$_LEA_MATRIX_ARRAYS_32 || names=$_LEA_MATRIX_ARRAYS_64
    for a in $names; do
        lea_matrix_stage_array "$LEA_ROOT/scripts/lib/provision.sh" "$a"
    done | sed 's/\.so.*$//' | sort -u
}

# lea_matrix_staged_check -- the self-check. An awk parser aimed at a file
# whose shape has moved returns nothing and looks like "the guest stages
# nothing", which would turn every library into a not-staged row and read
# like a catastrophic finding. Empty is therefore an error, loudly.
lea_matrix_staged_check() {
    local a n bad=0
    for a in $_LEA_MATRIX_ARRAYS_64 $_LEA_MATRIX_ARRAYS_32; do
        n=$(lea_matrix_stage_array "$LEA_ROOT/scripts/lib/provision.sh" "$a" | wc -l)
        [[ $n -gt 0 ]] || { error "staging array '$a' read as empty from scripts/lib/provision.sh"; bad=1; }
    done
    [[ $bad -eq 0 ]] || {
        error "the staged-set reader no longer understands provision.sh.
Every library would be reported as not-staged, which is a wrong answer that
looks like a finding. Fix the reader (lea_matrix_stage_array) before
trusting any artefact from this run."
        return 1
    }
    return 0
}

# lea_matrix_driver_packages -- every installed package AT the driver
# version, one name per line.
#
# NOT a list of package names. NVIDIA's userspace is split differently by
# every distribution -- on this host `libnvidia-opencl` is in `opencl-nvidia`
# and not in `nvidia-utils`, and asking `nvidia-utils` alone reported OpenCL
# as a library the driver does not ship at all, while a probe was computing
# with it. The version IS the driver package: anything installed at exactly
# DRIVER_VERSION came out of the same release.
lea_matrix_driver_packages() {
    command -v pacman >/dev/null 2>&1 || return 1
    pacman -Q 2>/dev/null | awk -v v="$(lea_want_driver)" '$2 ~ "^" v "-" { print $1 }'
}

# lea_matrix_host_payload BITS -- every library the INSTALLED host driver
# ships, one bare name per line.
#
# Primary source is the package manager's manifest, because that is what the
# word "ships" means. Where there is none, the fallback is the version
# suffix: a file called <name>.so.<DRIVER_VERSION> in the driver's library
# directory came out of the driver package by construction. Which of the two
# answered is recorded in DISCOVERY.md -- the inventories are only as good as
# their source and the artefact has to say which one it was.
lea_matrix_host_payload() {
    local bits=${1:-64} want dir pkg
    want=$(lea_want_driver)
    if [[ $bits == 32 ]]; then dir=$(lea_matrix_libdir32)
    else dir=$(lea_matrix_libdir) || return 1
    fi
    if [[ -n $(lea_matrix_driver_packages) ]]; then
        for pkg in $(lea_matrix_driver_packages); do
            pacman -Ql "$pkg" 2>/dev/null | awk -v d="$dir/" '{ if (index($2, d) == 1) print $2 }'
        done | xargs -r -n1 basename \
            | grep -E '\.so\.[0-9]' | sed 's/\.so\..*$//' | sort -u
        return 0
    fi
    if command -v dpkg-query >/dev/null 2>&1 \
       && dpkg-query -L "libnvidia-compute-${want%%.*}" >/dev/null 2>&1; then
        dpkg-query -L "libnvidia-compute-${want%%.*}" 2>/dev/null \
            | grep -E '\.so\.[0-9]' | xargs -r -n1 basename \
            | sed 's/\.so\..*$//' | sort -u
        return 0
    fi
    # A glob and not `ls | grep`: a file name is not a line, and ls mangles
    # the ones that contain a newline or a backslash. The `-e` guard is what
    # makes an unmatched glob produce nothing instead of its own pattern --
    # `nullglob` is deliberately not set here, because setting a shell option
    # in a sourced library changes every caller's globbing too.
    local f
    for f in "$dir"/*".so.$want"*; do
        [[ -e $f ]] || continue
        printf '%s\n' "${f##*/}"
    done | sed 's/\.so\..*$//' | sort -u
}

# lea_matrix_host_payload_source BITS -- which of the three answered.
lea_matrix_host_payload_source() {
    local pkgs
    pkgs=$(lea_matrix_driver_packages | tr '\n' ' ')
    if [[ -n ${pkgs// /} ]]; then
        echo "pacman -Ql over every package at $(lea_want_driver): ${pkgs% }"
    elif command -v dpkg-query >/dev/null 2>&1; then
        echo "dpkg-query -L (Debian/Ubuntu driver package)"
    else
        echo "filesystem: *.so.$(lea_want_driver) in the driver library directory"
    fi
}

# lea_matrix_not_staged -- host payload minus staged set, per bit width.
# Prints "<name> <where-it-is-staged-instead>" so that "not staged at all"
# and "staged only for the other bit width" stay different findings.
lea_matrix_not_staged() {
    local s64 s32 h64 n
    s64=$(lea_matrix_staged 64); s32=$(lea_matrix_staged 32)
    h64=$(lea_matrix_host_payload 64)
    while read -r n; do
        [[ -n $n ]] || continue
        grep -qxF "$n" <<<"$s64" && continue
        if grep -qxF "$n" <<<"$s32"; then
            echo "$n	staged 32-bit only"
        else
            echo "$n	not staged at all"
        fi
    done <<<"$h64"
}
