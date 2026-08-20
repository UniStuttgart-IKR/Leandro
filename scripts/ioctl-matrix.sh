#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# The per-library ioctl coverage matrix: probe, trace, catalogue.
#
#   scripts/ioctl-matrix.sh discover     matrix/DISCOVERY.md -- what is here
#   scripts/ioctl-matrix.sh probes       matrix/PROBES.md    -- one row per probe
#   scripts/ioctl-matrix.sh trace [name] run the probes, natively, one at a time
#   scripts/ioctl-matrix.sh catalog      the catalogue, the matrix, the tasks
#   scripts/ioctl-matrix.sh all          all four, in that order
#   scripts/ioctl-matrix.sh guest [name] the same probes in a GUEST, against
#                                        the native traces the four produced
#   scripts/ioctl-matrix.sh verify       the ANSWER BYTES of the two runs,
#                                        control by control (needs `guest`)
#
# WHAT THIS IS FOR. "The guest can run CUDA" is a claim about one library.
# The guest is handed thirty, and for most of them nobody has ever looked at
# which ioctls they emit -- so nobody can say whether they would be carried,
# refused, or silently answered wrong. This walks every library that has a
# workload, records what it actually calls, resolves each call against the
# vendor headers, and says which of them the backend governs today.
#
# WHAT IT IS NOT. It implements nothing: it changes no mediation and edits
# no supported-command list. Its output is the input to that work.
#
# `discover`, `probes`, `trace` and `catalog` measure the HOST and predict
# what a guest would do. `guest` is the separate step that goes and looks --
# same probes, same tracer, inside a VM -- and turns each prediction into
# `guest-validated` or into a finding. It is not part of `all`, because it
# needs a rig and the other four do not.
#
# EVERYTHING HERE IS REGENERATED. There is no table in this repository that
# a human has to edit after a driver update: the probes are the files in
# probe/matrix/, the staged library set is read out of the arrays in
# scripts/lib/provision.sh, the host payload out of the package manager, the
# governed commands out of nvrm-genhdr, and every ioctl name, struct and
# size out of vendor/. If a run needs a hand edit afterwards, that is a bug
# in this script.
#
# THE GPU IS A SERIAL RESOURCE. `trace` takes an flock for the whole run and
# every probe goes through it one at a time. Two probes at once do not
# corrupt anything except the measurement, which is worse.
#
# Artefacts, all under matrix/:
#   DISCOVERY.md              what the pipeline found: tracer, governance
#                             list, headers, probes, both library inventories
#   PROBES.md                 probe x libraries x criterion x status
#   traces/<driver>/*.tsv     raw traces, one per probe, with provenance
#   traces/<driver>/*.strace  the counter-check run, without the tracer
#   catalog-<driver>.md/.json one row per (device, nr, sub) signature
#   MATRIX-<driver>.md        probes x status -- the compatibility statement
#   TASKS-<driver>.md         the implementation task list for follow-up
#   traces/<driver>/guest/    the guest's own traces, taken by the same tracer
#   guest-<driver>.json       per probe: guest-validated, or the findings
#   verified-<driver>.json    per signature: the answer bytes matched a
#                             native run, and over how many bytes
#
# Never overwrites another driver version's trace directory: the driver is
# part of the path, and every artefact carries the driver in its header.
set -uo pipefail

_LEA_LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")/lib" && pwd) || exit 1
# shellcheck source=scripts/lib/config.sh
source "$_LEA_LIB/config.sh"
# shellcheck source=scripts/lib/common.sh
source "$_LEA_LIB/common.sh"
# shellcheck source=scripts/lib/matrix.sh
source "$_LEA_LIB/matrix.sh"

usage() { lea_usage_from_header; exit "${1:-0}"; }

CMD=${1:-}
[[ -n $CMD ]] || usage 2
shift
case $CMD in -h|--help|help) usage 0 ;; esac

MDIR=$(lea_matrix_dir)
DRV=$(lea_matrix_driver)
TDIR=$(lea_matrix_traces)

# ===========================================================================
# discover -- Phase 0, written down
# ===========================================================================
# Everything in DISCOVERY.md is READ at the moment the file is written. It is
# not a description of the repository, it is a measurement of it, and a
# reader can tell the difference by the fact that it regenerates.
do_discover() {
    mkdir -p "$MDIR" || die "cannot write $MDIR"
    lea_matrix_staged_check || return 1

    local out=$MDIR/DISCOVERY.md
    local vendor=$LEA_ROOT/vendor/open-gpu-kernel-modules
    local libdir libdir32 tables
    libdir=$(lea_matrix_libdir) || libdir="(not found)"
    libdir32=$(lea_matrix_libdir32)
    tables=$MDIR/.tables.txt

    # The governance list, dumped from its own writer rather than read out of
    # the source: this is the stream the guest module is handed at startup.
    if ! (cd "$LEA_ROOT" && cargo run --release --quiet --bin nvrm-genhdr -- \
            --expect-dump "$tables" >/dev/null 2>&1); then
        error "cannot dump the descriptor tables -- ./scripts/build.sh cargo"
        return 1
    fi

    {
        echo "<!-- SPDX-License-Identifier: MIT -->"
        echo "<!-- GENERATED by scripts/ioctl-matrix.sh discover -- do not edit. -->"
        echo "# Discovery: what the coverage matrix is built on"
        echo
        echo '```'
        lea_matrix_provenance
        echo '```'
        cat <<'PROSE'

Regenerate with `scripts/ioctl-matrix.sh discover`. Every number below was
read at the moment this file was written; nothing here is transcribed.

## 1. The tracer

PROSE
        printf '| what | where |\n|---|---|\n'
        printf '| interposer | `crates/nvrm-trace` -> `%s` |\n' "${LEA_TRACE_LIB#"$LEA_ROOT"/}"
        printf '| line format | `crates/nvrm-trace/src/log.rs` (the header is the authoritative list) |\n'
        printf '| existing runner | `probe/run/trace.sh` (nvprobe / torch / smi / analyse) |\n'
        printf '| DRM surface | `probe/run/drmtrace.sh` -- a different boundary, kept separate |\n'
        printf '| guest suites | `probe/run/suites.sh` -- needs a VM, out of scope here |\n'
        cat <<'PROSE'

The `ioctl` line carries nine tab-separated fields:

    ioctl  <dev>  <nr>  <sub>  <size>  <psize>  <ret>  <status>  <fd>

`sub` is the second dispatch level: NVOS54.cmd for RM_CONTROL, hClass for
RM_ALLOC, `-` otherwise. The signature this catalogue is keyed on is
`(dev, nr, sub)`, which is the same key `probe/run/trace.sh analyse` counts
-- deliberately, so the two reports are comparable.

Confirmed by running one, not by reading log.rs:

PROSE
        echo '```'
        echo "$_LEA_DISCOVER_TRACE_SAMPLE"
        echo '```'
        cat <<'PROSE'

There is no kernel-side trace point on the host. `guest-module/` holds
`virtio_nvrm` and `nvrm_nodes`, both guest-side. That is what makes the
32-bit set and the NVKMS axis unmeasurable here rather than merely
unmeasured -- see PROBES.md.

## 2. The supported-commands list

There is no separate allow-list file. The governance list IS the descriptor
table that `crates/nvrm-abi/src/xlate.rs` defines and `table.rs` serialises;
the guest module carries no NVIDIA constant of its own and interprets that
stream. The machine-readable form is what this pipeline reads:

    cargo run --release --bin nvrm-genhdr -- --expect-dump <file>

PROSE
        printf 'Rows in that dump today:\n\n'
        printf '| row | count | what it governs |\n|---|---:|---|\n'
        printf '| `ioctl <dev> <nr> ...` | %s | escapes that can be carried at all; a call with no row cannot cross |\n' \
            "$(awk '$1=="ioctl"' "$tables" | wc -l)"
        printf '| `class <hclass> <size> ...` | %s | RM_ALLOC classes; alloc params are not self-describing, so no row means no size |\n' \
            "$(awk '$1=="class"' "$tables" | wc -l)"
        printf '| `ctrl <cmd> ...` | %s | RM_CONTROL commands that are MEDIATED (nested pointers, fd fields) or BLOCKED |\n' \
            "$(awk '$1=="ctrl"' "$tables" | wc -l)"
        printf '| `nested ...` | %s | second-level pointers inside control params |\n' \
            "$(awk '$1=="nested"' "$tables" | wc -l)"
        cat <<'PROSE'

The asymmetry between the three matters for how a signature is classified.
RM_CONTROL is **self-describing** -- the params pointer and its length are
in NVOS54 -- so a control with no `ctrl` row is forwarded verbatim, which is
`passthrough` and not `missing`. RM_ALLOC and the UVM commands are not
self-describing: without a row there is no size, and the honest answer is
ENOTSUP. That is why a missing class row is a `missing` signature and a
missing ctrl row is not.

## 3. The vendor headers

PROSE
        printf '| what | header |\n|---|---|\n'
        printf '| frontend escapes | `%s` (%s defines) |\n' \
            "src/nvidia/arch/nvalloc/unix/include/nv_escape.h" \
            "$(grep -c '^#define NV_ESC_' "$vendor/src/nvidia/arch/nvalloc/unix/include/nv_escape.h")"
        printf '| numbered escapes | `%s` (%s defines, `NV_IOCTL_BASE + n`) |\n' \
            "kernel-open/common/inc/nv-ioctl-numbers.h" \
            "$(grep -c '^#define NV_ESC_' "$vendor/kernel-open/common/inc/nv-ioctl-numbers.h")"
        printf '| RM_ALLOC classes | `%s` (%s class defines) |\n' \
            "src/nvidia/generated/g_allclasses.h" \
            "$(grep -cE '^#define +[A-Z0-9_]+ +\(?0x' "$vendor/src/nvidia/generated/g_allclasses.h")"
        printf '| RM_CONTROL commands | `%s` (%s headers) |\n' \
            "src/common/sdk/nvidia/inc/ctrl/" \
            "$(find "$vendor/src/common/sdk/nvidia/inc/ctrl" -name '*.h' | wc -l)"
        printf '| UVM commands | `%s` -- RAW numbers, `UVM_IOCTL_BASE(i) = i`, no `_IOC` encoding |\n' \
            "kernel-open/nvidia-uvm/uvm_ioctl.h"
        printf '| parameter blocks | `%s` (NVOS02/32/33/46/54/64) |\n' \
            "src/common/sdk/nvidia/inc/nvos.h"
        cat <<'PROSE'

Struct sizes are not parsed out of the headers. They are **compiled**: the
resolver generates a C file that includes exactly the headers the observed
commands come from and prints `sizeof` for each params struct, built with
the same include set `crates/nvrm-sys/build.rs` uses.

WARNING, and it cost a compile to find: `src/common/sdk/nvidia/inc` must
come BEFORE `kernel-open/common/inc` on the include path. Both directories
contain an `rs_access.h`, they are not the same file, and with the other
order the two definitions collide.

## 4. Existing probes and gates, reused

PROSE
        printf '| reused | as | where |\n|---|---|---|\n'
        printf '| `nvprobe` stages 2/3/4 | cuda-core, cuda-jit, cuda-launch | `probe/c/nvprobe.c` |\n'
        printf '| `managedprobe` | cuda-managed (stage 2 -- stage 3 oversubscribes the card) | `probe/c/managedprobe.c` |\n'
        printf '| `hostregprobe` | cuda-hostreg | `probe/c/hostregprobe.c` |\n'
        printf '| `torchprobe.py` | cuda-torch (`NVTORCH_CONV=1`) | `probe/python/torchprobe.py` |\n'
        printf '| `nvidia-smi -q` | nvml | the NVML path, not libcuda |\n'
        printf '| the strace counter-check | every probe gate | the rule from `probe/run/trace.sh` |\n'
        printf '\nNew, because no probe in this tree reached these libraries:\n\n'
        printf '| new | covers |\n|---|---|\n'
        printf '| `probe/c/oclprobe.c` | libnvidia-opencl |\n'
        printf '| `probe/c/eglplat.c` | the five EGL external-platform modules |\n'
        printf '| `probe/c/vkrt.c` | libnvidia-rtcore |\n'
        printf '| `probe/matrix/cuda-gdb.sh` | libcudadebugger |\n'
        cat <<'PROSE'

## 5. The two library inventories

Neither is transcribed. The staged set is read out of the six arrays that
define it in `scripts/lib/provision.sh` (`libs`, `optional` in
`lea_payload_stage`; `versioned`, `loose`, `versioned32`, `optional32` in
`lea_gl_stage`) -- referring to them by name is a code reference, copying
their contents here would be the hand-maintained table this pipeline
refuses to have. The host payload is read from the driver package manifest.

PROSE
        printf '| inventory | source | count |\n|---|---|---:|\n'
        printf '| guest-staged, 64-bit | `scripts/lib/provision.sh` arrays | %s |\n' "$(lea_matrix_staged 64 | wc -l)"
        printf '| guest-staged, 32-bit | `scripts/lib/provision.sh` arrays | %s |\n' "$(lea_matrix_staged 32 | wc -l)"
        printf '| host payload, 64-bit | %s | %s |\n' "$(lea_matrix_host_payload_source 64)" "$(lea_matrix_host_payload 64 | wc -l)"
        printf '| host payload, 32-bit | %s | %s |\n' "$(lea_matrix_host_payload_source 32)" "$(lea_matrix_host_payload 32 | wc -l)"
        printf '| library directory | discovered (`lea_nvidia_libdir`) | `%s` |\n' "$libdir"
        printf '| 32-bit directory | `LEA_NVIDIA_LIB32_DIR` | `%s` |\n' "$libdir32"
        printf '\n### The difference: not staged\n\n'
        printf 'Host payload minus staged set. This is the most important list in the\n'
        printf 'file: for a feature guarantee, what is ABSENT is the line that decides\n'
        printf 'it, and it has to be data rather than an omission.\n\n'
        printf '| library | where it is | consequence for a 64-bit guest client |\n|---|---|---|\n'
        lea_matrix_not_staged | while IFS=$'\t' read -r lib where; do
            printf '| `%s` | %s | %s |\n' "$lib" "$where" \
                "$([[ $where == 'staged 32-bit only' ]] \
                    && echo 'the name does not resolve; a dlopen of it is a silently absent capability' \
                    || echo 'absent from the guest entirely')"
        done
        cat <<'PROSE'

Two readings of "the host driver payload" disagree, and the difference is
itself worth recording. Reading the loader directory for files ending in
`.so.<DRIVER_VERSION>` finds seven of these. The package manifest finds
more, because three of them are not named that way: two live in
subdirectories of the library directory (the X server's GLX module and the
VDPAU backend) and one carries a bare SONAME version rather than the driver
version. The manifest is the reading used above -- it is what the word
"ships" means -- and the filesystem rule is the fallback where there is no
package manager.

## 6. Answer-verification evidence

PROSE
        if [[ -f $MDIR/verified-$DRV.json ]]; then
            printf 'FOUND: `matrix/verified-%s.json`.\n' "$DRV"
        else
            cat <<'PROSE'
**Absent.** Searched for a generated file mapping signature ->
verified-against-native under `matrix/` and in the gate output paths; there
is none, and nothing in the tree produces one. The gpu gate compares
STATUS codes and workload results between a guest run and a native run; no
tool anywhere compares the answer BYTES of a forwarded control against the
bytes the same call returns natively.

That is a real gap and not a missing file: the failure class it would catch
is "an answer that looks valid and is wrong", which is exactly what numbers
32 and 44 turned out to be -- a status comparison cannot see it, and only
the bytes can.

The consequence for this catalogue is deliberate and stated plainly: the
`implemented-verified` class is **empty**, and every governed signature
lands in `implemented-unverified`. That emptiness is a correct output, not
a defect of the pipeline. TASKS carries the standing task that would fill
it.

This file is never written by hand. When a differential harness exists and
emits `matrix/verified-<driver>.json`, this pipeline reads it and the class
fills itself.
PROSE
        fi
    } > "$out"
    info "wrote $out"
}

# The trace sample quoted in DISCOVERY.md: taken, not remembered.
_LEA_DISCOVER_TRACE_SAMPLE=""
lea_matrix_sample_trace() {
    local tmp
    tmp=$(mktemp) || return 1
    lea_on_exit "rm -f $(printf '%q' "$tmp")"
    [[ -x $LEA_PROBE_BIN/nvprobe && -f $LEA_TRACE_LIB ]] || {
        _LEA_DISCOVER_TRACE_SAMPLE="(no nvprobe or no tracer built -- ./scripts/build.sh cargo probes)"
        return 0
    }
    LEA_TRACE_FILE="$tmp" LD_PRELOAD="$LEA_TRACE_LIB" \
        "$LEA_PROBE_BIN/nvprobe" 0 >/dev/null 2>&1
    _LEA_DISCOVER_TRACE_SAMPLE=$(awk -F'\t' '$1=="ioctl"' "$tmp" | head -4 | cat -A \
        | sed 's/\^I/\\t/g; s/\$$//')
    [[ -n $_LEA_DISCOVER_TRACE_SAMPLE ]] \
        || _LEA_DISCOVER_TRACE_SAMPLE="(the sample run produced no ioctl lines)"
}

# ===========================================================================
# probes -- Phase 1, generated from the probe files themselves
# ===========================================================================
do_probes() {
    mkdir -p "$MDIR" || die "cannot write $MDIR"
    lea_matrix_staged_check || return 1
    local out=$MDIR/PROBES.md p f group libs entry crit status
    local pdir; pdir=$(lea_matrix_probes_dir)

    {
        echo "<!-- SPDX-License-Identifier: MIT -->"
        echo "<!-- GENERATED by scripts/ioctl-matrix.sh probes -- do not edit. -->"
        echo "# Probes: one row per feature path"
        echo
        echo '```'
        lea_matrix_provenance
        echo '```'
        cat <<'PROSE'

Regenerated by `scripts/ioctl-matrix.sh probes`, by reading the headers of
the files in `probe/matrix/`. There is no registry: the set of probes is the
set of files, and adding a probe is adding one file.

A probe is one feature path, minimal and deterministic. One probe may cover
several libraries (a GL probe covers glcore, glsi, tls and the allocator at
once); one library may need several probes (EGL has a separate external
platform module per windowing system, and they are different code paths).

**Every probe has a criterion beyond its exit code.** The reason is not
rigour for its own sake: a missing library fails with ENOENT before any
ioctl is issued, so the tracer sees nothing at all and an empty trace looks
like a feature nobody used. "Denied", "absent" and "silently degraded" are
three different findings and only a checked result tells them apart.

PROSE
        for group in compute nvml video gl egl vulkan compat kms; do
            local any=0
            for p in $(lea_matrix_probe_list); do
                f=$pdir/$p.sh
                [[ $(lea_matrix_meta "$f" group) == "$group" ]] || continue
                [[ $any -eq 0 ]] && {
                    printf '## %s\n\n' "$group"
                    printf '| probe | libraries covered | entry API | functional criterion | status |\n'
                    printf '|---|---|---|---|---|\n'
                    any=1
                }
                libs=$(lea_matrix_meta "$f" libs)
                entry=$(lea_matrix_meta "$f" entry)
                crit=$(lea_matrix_meta "$f" criterion)
                status=$(lea_matrix_meta "$f" status)
                printf '| `%s` | %s | `%s` | %s | %s |\n' \
                    "$p" "$(sed 's/ /<br>/g' <<<"$libs")" "$entry" "$crit" "$status"
            done
            [[ $any -eq 1 ]] && echo
        done

        cat <<'PROSE'
## The 32-bit set and the NVKMS axis

Both rows above are `blocked: needs kernel-side trace point`, and the reason
is the same in two different shapes.

The tracer is a 64-bit LD_PRELOAD interposer and cannot be preloaded into a
32-bit client. A 32-bit tracer is explicitly not the answer: it would be a
second instrument measuring the same boundary, with its own offsets to keep
correct. The instrument that covers both bit widths at once sits on the
kernel side, and this tree has none on the host. If one appears, ONE 32-bit
probe -- glxgears or equivalent -- validates the trace path for the whole
20-library set.

nvidia_modeset and nvidia_drm are not a userspace column at all -- as KERNEL
MODULES. NVKMS is an in-kernel RM client, so its RM traffic never crosses a
userspace ioctl boundary and no interposer can see it whatever the client
does. That is what this row is blocked on, and it is NOT the same statement
as "NVKMS is invisible": `/dev/nvidia-modeset` is a userspace node the GL
and Vulkan libraries open themselves, it is traced and gated since
2026-08-20, and the catalogue has a section for it. What those calls still
lack is names -- NVKMS command numbers are a namespace of their own and do
not resolve against `ctrl*.h`. The DRM surface a client presents to
`nvidia_drm` IS measurable
natively and this tree already measures it -- `probe/run/drmtrace.sh`, which
decodes the numbers out of the headers that apply because strace names
NVIDIA's private DRM numbers after other vendors' drivers. That is a
different boundary and stays a separate report.

## Libraries with no probe

PROSE
        printf 'Every library in the staged set, and the probe that covers it. A library\n'
        printf 'with no probe is a row in the catalogue with no measurement behind it,\n'
        printf 'which is a finding rather than a gap in this table.\n\n'
        printf '| library | staged | covered by |\n|---|---|---|\n'
        {
            lea_matrix_staged 64
            lea_matrix_host_payload 64
        } | sort -u | while read -r lib; do
            [[ -n $lib ]] || continue
            local cover="" st
            for p in $(lea_matrix_probe_list); do
                grep -qw -- "$lib" <<<"$(lea_matrix_meta "$pdir/$p.sh" libs)" && cover="$cover $p"
            done
            if grep -qxF "$lib" <<<"$(lea_matrix_staged 64)"; then st="64-bit"
            elif grep -qxF "$lib" <<<"$(lea_matrix_staged 32)"; then st="32-bit only"
            else st="**not staged**"; fi
            printf '| `%s` | %s | %s |\n' "$lib" "$st" "${cover:- -- no probe}"
        done
    } > "$out"
    info "wrote $out"
}

# ===========================================================================
# trace -- Phase 3, through the serial queue
# ===========================================================================
# One probe at a time, natively, on the host. Each probe runs TWICE: once
# under the tracer, once under strace without it. The tracer counts what IT
# saw and strace counts what the KERNEL saw, and the delta must be 0 -- the
# rule probe/run/trace.sh established and the reason it exists: a delta
# other than 0 means somebody bypasses the PLT, and from that point every
# number derived from the trace is decoration.
#
# THE COUNTING RULE IS NARROWER THAN trace.sh's, and it had to be. That
# script counts strace lines carrying the substring `_IOC`, which is right
# for a CUDA workload and wrong for a graphics one:
#
#   * `DRM_IOCTL_VERSION` CONTAINS `_IOC`. Every DRM call a GL, EGL or
#     Vulkan client makes was counted as an NVIDIA ioctl the tracer had
#     missed. Measured while writing this: 441 phantom calls on one EGL run.
#     The token is `_IOC(`, which is the form strace prints when it has no
#     name for a request -- and NVIDIA's escapes have no name in its table.
#   * strace is run with `-y`, so every fd carries its path, and the count
#     is restricted to the nodes the tracer actually covers
#     (/dev/nvidiactl, /dev/nvidiaN, /dev/nvidia-uvm*). Without that, calls
#     on OTHER character devices land in the delta.
#
# Under an interpreter every isatty is a TCGETS2, which strace NAMES and the
# tracer rightly ignores; counting those made a tracer that missed nothing
# look 1062 calls short. Both traps are the same trap: count what the rule
# means, not what the substring matches (OPEN-QUESTIONS number 47).
#
# /dev/nvidia-modeset is counted and gated SEPARATELY, in its own pair of
# columns. Separately because it is a different namespace -- NVKMS commands
# resolve against no `ctrl*.h` -- and gated because it is a userspace
# boundary like the others: the GL and Vulkan libraries open that node
# themselves. It was ungated until 2026-08-20 for the only reason that ever
# justified it, that the tracer had no tag for the node and could see
# nothing there (OPEN-QUESTIONS number 48); it has one now, so the same
# counter-check applies and the same rule holds -- a delta other than 0
# means an instrument is blind and every number derived from it is
# decoration.
do_trace() {
    local only=("$@")
    lea_matrix_check_driver || return 1
    lea_require_tools strace
    [[ -f $LEA_TRACE_LIB ]] || die "no $LEA_TRACE_LIB -- ./scripts/build.sh cargo"
    mkdir -p "$TDIR" || die "cannot write $TDIR"
    lea_matrix_serial || die "cannot take the serial lock"

    # HOW TO WAIT FOR IT: this holds a pidfile for its lifetime. Wait on the
    # FILE, never on `pgrep -f` -- that pattern stands in the waiting shell's
    # own command line (lea_hold_pidfile in scripts/lib/common.sh):
    #   until ! lea_running vm/ioctl-matrix.pid; do sleep 15; done
    lea_hold_pidfile "$LEA_VM_DIR/ioctl-matrix.pid"

    # The governance list and the inventories are captured INTO the trace
    # directory, so a catalogue can be rebuilt from that directory alone and
    # describes the tree as it was when the traces were taken.
    (cd "$LEA_ROOT" && cargo run --release --quiet --bin nvrm-genhdr -- \
        --expect-dump "$TDIR/tables.txt" >/dev/null 2>&1) \
        || die "cannot dump the descriptor tables -- ./scripts/build.sh cargo"
    # The FIELD MAP, beside the table stream and for the same reason: every
    # consumer downstream can then say `biosInfoList, an NvP64` where it used
    # to say `word 2 (offset 8)`. Sizes and offsets are COMPILED out of the
    # pinned vendor headers, one translation unit per header -- nothing here
    # is parsed, which is the property the catalogue's sizeof cross-check
    # already rests on, one level finer.
    #
    # It needs no traces, so it is written HERE rather than in `catalog`:
    # `verify` runs answerdiff BEFORE it regenerates the catalogue, and a
    # field map that only appeared afterwards would be missing on exactly
    # the first run that wanted it.
    python3 "$LEA_ROOT/probe/python/ioctlmatrix.py" --fieldmap-only \
        --fieldmap "$TDIR/fields.json" --traces "$TDIR" --out "$MDIR" \
        --driver "$DRV" --vendor "$LEA_ROOT/vendor/open-gpu-kernel-modules" \
        --xlate "$LEA_ROOT/crates/nvrm-abi/src/xlate.rs" \
        --provenance "$(lea_matrix_provenance | tr '\n' '|')" \
        || die "cannot generate the field map"
    {
        lea_matrix_staged 64 | sed 's/^/staged64\t/'
        lea_matrix_staged 32 | sed 's/^/staged32\t/'
        lea_matrix_host_payload 64 | sed 's/^/host64\t/'
        lea_matrix_host_payload 32 | sed 's/^/host32\t/'
        lea_matrix_not_staged | sed 's/^/notstaged\t/'
    } > "$TDIR/inventory.tsv"

    # The index is written to a scratch file and merged at the end. A run
    # that names single probes must not truncate the rows of the ones it did
    # not run: a partial index reads as "the other probes were never
    # measured", which is a different and much worse statement than "they
    # were measured earlier".
    local index=$TDIR/probes.tsv rows
    rows=$(mktemp) || die "mktemp"
    lea_on_exit "rm -f $(printf '%q' "$rows")"

    local pdir p f status gate tsv strc outf raw a b nsig rc crit seen am mset drm short
    pdir=$(lea_matrix_probes_dir)
    printf '%-16s %8s %8s %7s %7s %7s %7s  %s\n' \
        probe tracer strace delta sigs nvkms nvkmsd result
    for p in $(lea_matrix_probe_list); do
        if [[ ${#only[@]} -gt 0 ]]; then
            local hit=0 o
            for o in "${only[@]}"; do [[ $o == "$p" ]] && hit=1; done
            [[ $hit -eq 1 ]] || continue
        fi
        f=$pdir/$p.sh
        status=$(lea_matrix_meta "$f" status)
        if [[ $status != ready ]]; then
            printf '%-16s %8s %8s %7s %7s %7s %7s  %s\n' "$p" - - - - - - "$status"
            printf '%s\t%s\t\t\t\t\t\t\t\t%s\t%s\t\t%s\n' "$p" "$status" \
                "$(lea_matrix_meta "$f" group)" "$(lea_matrix_meta "$f" libs)" \
                "$(lea_matrix_meta "$f" criterion)" >> "$rows"
            continue
        fi
        gate=$(lea_matrix_meta "$f" gate)
        short=

        tsv=$TDIR/$p.tsv; strc=$TDIR/$p.strace; outf=$TDIR/$p.out

        # UP TO FIVE ATTEMPTS, and the reason is not flakiness tolerance.
        # The gate's claim is "the tracer sees every call this workload
        # makes", and it is tested by running the workload twice under two
        # instruments -- which only means anything if the workload makes the
        # same calls twice. Not all of them do: the NVDEC probe allocated one
        # extra decode surface in roughly half its runs -- pinning the frame
        # pool does not stop it -- and counted strictly that jitter is
        # indistinguishable from a tracer that missed five calls, which is
        # the opposite conclusion. Five attempts at an even split leaves a
        # 3 % chance of a spurious FAIL; three left 12 %, which was too
        # often to be useful.
        #
        # The first answer is always to make the workload deterministic, and
        # every probe here is. The retry is the backstop for the case that is
        # not: one matching pair falsifies "the tracer is blind here", so the
        # gate passes on the first exact match and says how many attempts it
        # took -- a row that never says "attempt 1" is a workload worth
        # pinning down. A probe that never matches has not passed.
        local try
        for try in 1 2 3 4 5; do
            raw=$(mktemp) || die "mktemp"

            # Run 1: under the tracer. The wrapper goes around the WORKLOAD
            # only (lea_matrix_workload) -- the tracer truncates its output
            # file in its constructor, so a preloaded shell would have every
            # child wipe the trace of the run in progress, silently.
            LEA_MATRIX_WRAP="env LEA_TRACE_FILE=$raw LD_PRELOAD=$LEA_TRACE_LIB" \
                timeout 600 "$f" > "$outf" 2>&1
            rc=$?

            # Run 2: the same command without the tracer, under strace. Two
            # runs and not one, deliberately: strace under LD_PRELOAD would
            # measure the tracer too, and a single run cannot answer "did the
            # tracer see everything" at all. openat rides along in the same
            # run because a missing library is an ENOENT and never an ioctl
            # -- there is no other way to tell a denied library from an
            # absent one.
            # -y so every fd carries its path: that is what makes the count
            # restrictable to the nodes the tracer covers.
            LEA_MATRIX_WRAP="strace -f -y -e trace=ioctl,openat -o $strc" \
                timeout 600 "$f" >/dev/null 2>&1

            # Provenance first, then the tracer's own bytes. The tracer
            # cannot write the header itself: it opens its file with O_TRUNC.
            { lea_matrix_provenance '#'; printf '#probe: %s\n#attempt: %s\n' "$p" "$try"
              cat "$raw"; } > "$tsv"
            rm -f "$raw"

            # Like for like: the tracer's NVIDIA-node lines against strace's
            # unnamed requests on those same nodes. DRM is excluded on both
            # sides -- it is a different namespace and a different report.
            # The counting rule lives in matrix.sh and nowhere else, so the
            # guest phase counts exactly what this one counts. Two pairs,
            # never added together: the RM nodes, and the NVKMS node.
            a=$(lea_matrix_n_tracer "$tsv")
            b=$(lea_matrix_n_strace "$strc")
            am=$(lea_matrix_n_tracer_kms "$tsv")
            mset=$(lea_matrix_n_strace_kms "$strc")
            [[ -n $gate ]] && break
            [[ $rc -ne 0 ]] && break
            [[ $((b - a)) -eq 0 && $((mset - am)) -eq 0 ]] && break
        done
        drm=$(lea_matrix_n_tracer_drm "$tsv")
        nsig=$(awk -F'\t' '$1=="ioctl"{print $2"\t"$3"\t"$4}' "$tsv" | sort -u | wc -l)
        crit=$(sed -n 's/^CRITERION: //p' "$outf" | tail -1)
        seen=$(_lea_matrix_libs_seen "$strc")

        local result
        if [[ $rc -eq 2 ]]; then
            result=$(grep -m1 -E '^(declared-unsupported|blocked):' "$outf" || echo "declared-unsupported: no reason printed")
        elif [[ $rc -ne 0 ]]; then
            result="FAIL: probe exited $rc"
        elif [[ -z $crit ]]; then
            result="FAIL: no CRITERION line -- the probe did not say what it proved"
        elif [[ -n $gate ]]; then
            # A probe whose header declares that the counter-check cannot be
            # run states the reason there. Its criterion still stands; its
            # signatures do NOT enter the catalogue, because an ungated
            # trace could be incomplete and nothing here can say it is not.
            # The full reason goes to probes.tsv and PROBES.md; the console
            # gets its first clause, or the table stops being a table.
            result="ungated: $gate"
            short="ungated: ${gate%%.*}"
            b=          # no counter-check ran, so there is no delta to print
            mset=       # ... on either node
        elif [[ $((b - a)) -ne 0 ]]; then
            # The strace gate. A probe that fails it does not enter the
            # matrix: its trace is incomplete and every signature derived
            # from it would understate the surface.
            result="FAIL: strace gate, delta $((b - a))"
        elif [[ $((mset - am)) -ne 0 ]]; then
            # The NVKMS node has its own gate for the same reason the RM
            # nodes have one, and it fails the probe for the same reason: a
            # trace that is short on one node understates the surface just as
            # badly as one that is short on another.
            result="FAIL: NVKMS gate, delta $((mset - am))"
        else
            result=PASS
            [[ $try -gt 1 ]] && result="PASS (matched on attempt $try)"
        fi
        printf '%-16s %8s %8s %7s %7s %7s %7s  %s\n' "$p" "$a" "${b:--}" \
            "$([[ -n $b ]] && echo $((b - a)) || echo -)" "$nsig" "$am" \
            "$([[ -n $mset ]] && echo $((mset - am)) || echo -)" "${short:-$result}"
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
            "$p" "$result" "$a" "${b:--}" "$([[ -n $b ]] && echo $((b - a)) || echo -)" \
            "$a" "$nsig" "$am" "$drm" \
            "$(lea_matrix_meta "$f" group)" "$(lea_matrix_meta "$f" libs)" \
            "$seen" "$crit" >> "$rows"
    done
    # Merge: rows from this run win, rows from an earlier run survive.
    {
        lea_matrix_provenance '#'
        printf '#\n#probe\tresult\ttracer\tstrace\tdelta\tioctls\tsigs\tmodeset\tdrm\tgroup\tlibs\tlibs_seen\tcriterion\n'
        {
            if [[ ${#only[@]} -gt 0 && -f $index ]]; then
                awk -F'\t' 'NR==FNR { seen[$1]=1; next } !/^#/ && !($1 in seen)' \
                    "$rows" "$index"
            fi
            cat "$rows"
        } | sort -t$'\t' -k1,1
    } > "$index.new"
    mv "$index.new" "$index"

    echo
    echo "traces: $TDIR"
    echo "delta must be 0 in every row. A non-zero delta means the tracer missed"
    echo "calls, and a catalogue built on it would understate the surface."
    echo "nvkms counts ioctls on /dev/nvidia-modeset, nvkmsd is that node's own"
    echo "delta against strace. It must be 0 for the same reason: NVKMS is a second"
    echo "userspace boundary, not a second opinion about the first."
}

# _lea_matrix_libs_seen STRACE -- which NVIDIA libraries this run actually
# LOADED, space separated, 32-bit ones prefixed lib32:.
#
# Two things it must get right, and the first version got both wrong.
#
# SUCCESSFUL opens only. The loader tries a library in every directory on its
# path and gets ENOENT for all but one; counting attempts would report a
# library as loaded from a directory it is not in -- and would report a
# genuinely MISSING library as present, which is the exact confusion this
# column exists to prevent.
#
# The name set is the host payload UNION the staged set, not the payload
# alone. The five external EGL platform libraries come from egl-gbm and
# egl-wayland rather than from the driver package, so they are in no driver
# manifest at all; with the payload alone, every EGL probe reported that it
# had not loaded the platform library it had just rendered through.
_lea_matrix_libs_seen() {
    local strc=$1 tmp names
    [[ -f $strc ]] || return 0
    tmp=$(mktemp) || return 0
    names=$(mktemp) || { rm -f "$tmp"; return 0; }
    grep 'openat' "$strc" 2>/dev/null | grep -v 'ENOENT' \
        | grep -o 'openat([^)]*"[^"]*\.so[^"]*"' \
        | grep -o '"[^"]*"' | tr -d '"' | sort -u > "$tmp"
    { lea_matrix_host_payload 64; lea_matrix_staged 64; } | sort -u > "$names"
    {
        while read -r l; do
            [[ -n $l ]] || continue
            grep -q "/$l\.so" "$tmp" && echo "$l"
        done < "$names"
        { lea_matrix_host_payload 32; lea_matrix_staged 32; } | sort -u | while read -r l; do
            [[ -n $l ]] || continue
            grep -q "lib32/$l\.so" "$tmp" && echo "lib32:$l"
        done
    } | sort -u | tr '\n' ' ' | sed 's/ $//'
    rm -f "$tmp" "$names"
}

# ===========================================================================
# guest -- the same probes, in a guest, against the native reference
# ===========================================================================
# Everything before this phase is a PREDICTION. The catalogue says a probe's
# every signature is governed or passthrough, and therefore that a guest
# could carry it; nothing had run in a guest. This is where a prediction
# becomes evidence, and the only interesting outcome is a deviation.
#
# THE INSTRUMENT IS THE SAME INSTRUMENT. The guest runs the same NVIDIA
# libraries over nodes with the same ABI, so the tracer is preloaded there
# exactly as it is here and writes the same nine columns. That is what makes
# the two traces comparable: not two instruments that agree about what they
# mean, but one instrument on both sides of the boundary. The guest half is
# `probe/run/matrix-guest.sh`, called once per probe over ssh; the
# comparison is `probe/python/guestdiff.py` and runs HERE, because only this
# side has both traces.
#
# WHAT IS COMPARED, and the second one is the point:
#   * the SIGNATURE SET -- a call the guest never makes is as much a finding
#     as one it makes and the host does not.
#   * the rm_status FINGERPRINT per signature -- including the deliberate
#     non-zero ones. A guest that answers 0x0 where the host answers 0x56
#     (NOT_SUPPORTED, on the ECC and InfoROM paths) has not carried the
#     call, it has invented an answer, and a status-code gate that only
#     asks "did anything fail" cannot see it.
# Call COUNTS are not compared: a workload may allocate one surface more in
# one run than in another, which the native phase already has to retry
# around. What must not move is WHICH statuses a signature can return.
do_guest() {
    local vm=${LEA_MATRIX_VM:-vdisplay} display=1 keep=0
    local -a only=()
    while [[ $# -gt 0 ]]; do
        case $1 in
            --vm)         vm=$2; shift 2 ;;
            --no-display) display=0; shift ;;
            --keep)       keep=1; shift ;;
            -*) error "unknown option: $1"; usage 2 ;;
            *)  only+=("$1"); shift ;;
        esac
    done

    local index=$TDIR/probes.tsv
    [[ -f $index ]] || die "no native reference in $TDIR -- scripts/ioctl-matrix.sh trace"
    lea_matrix_check_driver || return 1
    lea_require_tools python3 tar
    # rig.sh is sourced HERE and not at the top: every other subcommand runs
    # without a VM, and pulling the whole rig library into them would make a
    # catalogue run depend on a cloud-hypervisor binary it never calls.
    # shellcheck source=scripts/lib/rig.sh
    source "$_LEA_LIB/rig.sh"

    lea_matrix_serial || die "cannot take the serial lock"
    # HOW TO WAIT FOR IT: wait on the FILE, never on `pgrep -f` -- that
    # pattern stands in the waiting shell's own command line:
    #   until ! lea_running vm/ioctl-matrix-guest.pid; do sleep 15; done
    lea_hold_pidfile "$LEA_VM_DIR/ioctl-matrix-guest.pid"

    # BUILD FIRST, and this is not a convenience. The guest gets its answer
    # from two places that are updated by two different mechanisms: the
    # MODULE is rebuilt inside the guest from sources this run ships, and the
    # DESCRIPTOR TABLE is serialised at startup by the backend binary in
    # LEA_BIN_DIR. Nothing tied the second one to the tree.
    #
    # Measured 2026-08-20, and it cost a validation run: a change that added
    # a gpuId row (module side) and a nested-pointer row (table side) came
    # back half applied -- the gpuId was translated, the pointer was not, and
    # the artefact reported the second half as "still broken" when it was
    # "never shipped". A half-new rig is worse than an old one, because its
    # output looks like a measurement.
    info "== building, so the backend serialises THIS tree's tables =="
    (cd "$LEA_ROOT" && cargo build --release) >/dev/null 2>&1 \
        || { error "cargo build --release failed -- the rig would run a stale backend"; return 1; }

    # ---- the rig ---------------------------------------------------------
    # REUSED if it is already up. A guest that is running is a guest whose
    # modules are loaded and whose display is already built, and rebuilding
    # it would cost minutes and change nothing that is measured here.
    local ip
    if lea_vm_running "$vm"; then
        info "== reusing the running rig $vm =="
        warn "a reused rig keeps what it was STARTED with: the backend process
      that serialised its descriptor table, and the kernel command line the
      guest booted with. A change to xlate.rs or to the module reaches it
      only after a restart -- take the rig down first when validating one."
    else
        info "== bringing up $vm ${display:+(with display)} =="
        # ALLOCATOR DEBUGGING ON, for a sweep and not for a measurement. The
        # sweep runs twenty workloads through a boundary one after another,
        # and if one of them corrupts guest memory the symptom without this
        # is a hang somewhere later with no attribution at all -- which is
        # what number 38's double free looked like for a session.
        # `slub_debug=FZPU page_poison=1` turns that into a splat naming the
        # allocation and the free.
        #
        # `panic_on_warn` is deliberately NOT set. It would abort the sweep
        # on the first warning and take the remaining probes with it; a
        # sweep exists to produce twenty rows, and a warning that stops it
        # produces one. That trade belongs to the concurrency work, where a
        # single reproducer IS the deliverable.
        export LEA_GUEST_CMDLINE_EXTRA=${LEA_GUEST_CMDLINE_EXTRA:-"slub_debug=FZPU page_poison=1"}
        # The COMPUTE rig only. The display half is brought up separately
        # below, because it must not be able to fail the whole sweep: X is
        # needed by seven of these probes and by none of the other thirteen,
        # and a rig that refuses to come up because Xorg did not start would
        # take the thirteen down with it.
        lea_rig_up "$vm" || { error "$vm did not come up"; return 1; }
        [[ $keep -eq 1 ]] || lea_rig_down_on_exit "$vm"
    fi
    lea_inst "$vm"; ip=$INST_IP
    lea_ssh "$ip" true || { error "$vm ($ip) does not answer over ssh"; return 1; }

    # ---- what the guest is given -----------------------------------------
    # The GL/EGL/Vulkan userspace, staged by the SAME function whose arrays
    # the inventory reads (lea_gl_stage in scripts/lib/provision.sh). That is
    # what makes this sweep a test of the staging decision and not of a
    # second, hand-made payload: what the guest gets here is by construction
    # what DISCOVERY.md calls the staged set. Skipped when it is already
    # there -- a baked image stages it once.
    if ! lea_ssh "$ip" 'test -f /opt/nvrm-gl/env.sh'; then
        info "== staging the GL/EGL/Vulkan userspace into $vm =="
        lea_gl_stage "$vm" --system >/dev/null || { error "GL staging failed"; return 1; }
    fi

    # The WORKLOADS. A probe whose workload is absent declares itself
    # unsupported and measures nothing, so the packages that carry them are
    # installed once, here, the way the encode gate installs ffmpeg. This
    # adds no NVIDIA userspace: mesa-utils brings glxinfo and es2_info,
    # vulkan-tools brings vulkaninfo, xserver-xorg-core brings the X server
    # the seven display probes need -- all of them clients, and which
    # DRIVER answers them is the thing being measured.
    local need=""
    lea_ssh "$ip" 'command -v glxinfo   >/dev/null' || need+=" mesa-utils"
    lea_ssh "$ip" 'command -v vulkaninfo >/dev/null' || need+=" vulkan-tools"
    lea_ssh "$ip" 'command -v ffmpeg    >/dev/null' || need+=" ffmpeg"
    lea_ssh "$ip" 'command -v strace    >/dev/null' || need+=" strace"
    [[ $display -eq 1 ]] && { lea_ssh "$ip" 'command -v Xorg >/dev/null'         || need+=" xserver-xorg-core xauth"; }
    if [[ -n $need ]]; then
        info "installing the guest's workload tools:$need"
        lea_ssh "$ip" "sudo apt-get install -y -q $need >/dev/null 2>&1"             || warn "apt-get failed -- the probes whose workload is missing will
      declare themselves unsupported, which is a row with a reason and not a pass"
    fi

    # ---- the display half, allowed to fail --------------------------------
    local dispopt=""
    if [[ $display -eq 1 ]]; then
        if lea_ssh "$ip" 'pgrep -x Xorg >/dev/null'; then
            info "== X is already up in $vm =="
            dispopt="--display :7"
        elif lea_display_up "$vm" >/dev/null 2>&1 && lea_ssh "$ip" 'pgrep -x Xorg >/dev/null'; then
            info "== virtual display up in $vm (X on :7) =="
            dispopt="--display :7"
        else
            warn "no X in $vm -- the GL and EGL probes will declare themselves
      unsupported for want of a display. That is a row with a reason; the
      thirteen probes that need no display are unaffected."
        fi
    fi

    # ---- the payload -----------------------------------------------------
    # A MINIATURE OF THIS TREE, not a copy of it: the probes address each
    # other by relative path (probe/matrix/x.sh reaches ../../scripts/lib),
    # so the guest gets that shape and nothing else. Nothing is rewritten on
    # the way in -- a probe that ran differently in the guest because it was
    # edited on the way there would measure the edit.
    local stage; stage=$(mktemp -d) || die "mktemp"
    lea_on_exit "rm -rf $(printf '%q' "$stage")"
    mkdir -p "$stage/scripts/lib" "$stage/probe/matrix" "$stage/probe/run" \
             "$stage/probe/bin" "$stage/probe/kernels" "$stage/lib"
    cp "$LEA_ROOT"/scripts/lib/{config.sh,common.sh,matrix.sh} "$stage/scripts/lib/"
    cp "$LEA_ROOT"/probe/matrix/*.sh "$stage/probe/matrix/"
    cp "$LEA_ROOT"/probe/run/matrix-guest.sh "$stage/probe/run/"
    cp "$LEA_ROOT"/probe/kernels/*.ptx "$stage/probe/kernels/" 2>/dev/null
    cp "$LEA_PROBE_BIN"/* "$stage/probe/bin/" 2>/dev/null
    cp "$LEA_TRACE_LIB" "$stage/lib/" || die "no tracer at $LEA_TRACE_LIB"
    cp "$LEA_ROOT/DRIVER_VERSION" "$stage/"
    info "staging $(du -sh "$stage" | cut -f1) into $vm:~/matrix"
    lea_guest_tar "$vm" "$stage" '$HOME/matrix' || { error "staging failed"; return 1; }
    lea_ssh "$ip" 'chmod +x ~/matrix/probe/matrix/*.sh ~/matrix/probe/run/*.sh ~/matrix/probe/bin/* 2>/dev/null; true'

    # strace is the counter-check, and without it a guest trace cannot be
    # gated -- so its absence is a BLOCKED row per probe, never a silent
    # pass. Installed if the guest can, reported if it cannot.
    if ! lea_ssh "$ip" 'command -v strace >/dev/null'; then
        info "installing strace in the guest (the counter-check needs it)"
        lea_ssh "$ip" 'sudo apt-get install -y -q strace >/dev/null 2>&1' \
            || warn "no strace in the guest -- every probe will be blocked, not passed"
    fi

    # ---- the sweep -------------------------------------------------------
    # CHEAPEST FIRST, read from the native reference rather than ordered by
    # hand: the `ioctls` column is what each probe cost there, so nvml (179)
    # runs first and vk-offscreen (5568) last. A failure early is then a
    # failure that cost a minute.
    #
    # Which probes: the ones that PASSED natively. A probe whose native trace
    # did not pass has no reference to compare against, and one that is
    # declared-unsupported has nothing to run.
    local -a list=()
    mapfile -t list < <(awk -F'\t' '!/^#/ && $2 ~ /^PASS/ { print $6"\t"$1 }' "$index" \
                        | sort -n | cut -f2)
    if [[ ${#only[@]} -gt 0 ]]; then
        local -a want=() p o
        for p in "${list[@]}"; do
            for o in "${only[@]}"; do [[ $o == "$p" ]] && want+=("$p"); done
        done
        [[ ${#want[@]} -gt 0 ]] || die "none of the named probes passed natively: ${only[*]}"
        list=("${want[@]}")
    fi

    local gdir=$TDIR/guest
    rm -rf "$gdir"; mkdir -p "$gdir" || die "cannot write $gdir"
    local rows; rows=$(mktemp) || die "mktemp"
    lea_on_exit "rm -f $(printf '%q' "$rows")"

    printf '%-16s %8s %8s %7s %7s  %s\n' probe tracer strace nvkms nvkmsd result
    local p line
    for p in "${list[@]}"; do
        # One probe at a time through the serial queue, and a FAIL does not
        # stop the sweep: a run that stops at the first deviation reports one
        # finding and hides the rest.
        # stderr is KEPT, per probe. A runner that dies before it can print
        # a result line is the case that most needs a reason, and sending it
        # to /dev/null leaves "no result line" as the whole diagnosis.
        line=$(lea_ssh "$ip" "cd ~/matrix && ./probe/run/matrix-guest.sh $p --out ~/matrix/out $dispopt" \
               2>"$gdir/$p.runner.err" | grep -m1 '^GUESTRESULT') || true
        if [[ -z $line ]]; then
            local why
            why=$(grep -m1 . "$gdir/$p.runner.err" 2>/dev/null)
            line=$(printf 'GUESTRESULT\t%s\t%s\t-\t-\t-\t-\t' \
                   "$p" "FAIL: the guest runner said nothing: ${why:-no output at all}")
        else
            rm -f "$gdir/$p.runner.err"
        fi
        local gp gres ga gb gam gmset gcrit
        IFS=$'\t' read -r _ gp gres ga gb gam gmset gcrit <<<"$line"
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
            "$gp" "$gres" "$ga" "$gb" "$gam" "$gmset" "$gcrit" >> "$rows"
        printf '%-16s %8s %8s %7s %7s  %s\n' "$gp" "$ga" "$gb" "$gam" \
            "$([[ ${gmset:--} =~ ^-?[0-9]+$ && ${gam:--} =~ ^-?[0-9]+$ ]] \
               && echo $((gmset - gam)) || echo -)" "$gres"
    done

    # The guest's traces come back whole. They are artefacts in their own
    # right: the comparison below is derived from them, and a derived answer
    # nobody can re-derive is an assertion.
    lea_ssh "$ip" 'cd ~/matrix/out 2>/dev/null && tar -cf - .' | tar -C "$gdir" -xf - \
        || warn "could not fetch the guest traces from $vm"
    {
        lea_matrix_provenance '#'
        printf '#vm: %s (%s)\n' "$vm" "$ip"
        printf '#\n#probe\tresult\ttracer\tstrace\tmodeset\tmodeset_strace\tcriterion\n'
        sort -t$'\t' -k1,1 "$rows"
    } > "$gdir/probes.tsv"

    # ---- the comparison --------------------------------------------------
    python3 "$LEA_ROOT/probe/python/guestdiff.py" \
        --native "$TDIR" --guest "$gdir" --out "$MDIR" --driver "$DRV" \
        --vendor "$LEA_ROOT/vendor/open-gpu-kernel-modules" \
        --provenance "$(lea_matrix_provenance | tr '\n' '|')" \
        || die "the guest comparison failed"
    info "re-generating the catalogue so MATRIX carries the guest verdicts"
    do_catalog
}

# ===========================================================================
# verify -- the answer bytes, guest against native
# ===========================================================================
# The first slice of the differential harness OPEN-QUESTIONS number 50 asks
# for, and the ONLY thing in this tree that compares what a forwarded control
# ANSWERED rather than whether it failed.
#
# It logs nothing new. The tracer has dumped the first 32 bytes of the params
# buffer after the call since the enumeration work (`ctrlout` in log.rs,
# written for exactly this diff), so both phases have been recording the
# evidence all along -- this reads the traces that exist. That is also its
# limit, and the evidence file states it per signature: 32 bytes of a
# 384-byte answer is 32 bytes, and `implemented-verified` must not be read as
# more than what was compared.
#
# Runs after `guest`, and needs its traces. The mask is derived from those
# same traces rather than declared (probe/python/answerdiff.py says how).
do_verify() {
    local -a only=("$@")
    local gdir=$TDIR/guest
    [[ -d $gdir ]] || die "no guest traces in $gdir -- scripts/ioctl-matrix.sh guest"
    lea_require_tools python3
    local -a list=()
    if [[ ${#only[@]} -gt 0 ]]; then
        list=("${only[@]}")
    else
        # Every probe that has both traces. A probe whose guest run failed
        # left a trace too, and comparing it is not wrong -- what it answered
        # before it failed is still what it answered.
        local f
        for f in "$gdir"/*.tsv; do
            [[ -e $f ]] || continue
            f=$(basename "$f" .tsv)
            # probes.tsv is the run's index, not a probe. It has a .tsv name
            # because every artefact here does.
            [[ $f == probes ]] && continue
            [[ -f $TDIR/$f.tsv ]] && list+=("$f")
        done
    fi
    [[ ${#list[@]} -gt 0 ]] || die "no probe has both a native and a guest trace"
    python3 "$LEA_ROOT/probe/python/answerdiff.py" \
        --native "$TDIR" --guest "$gdir" --out "$MDIR" --driver "$DRV" \
        --provenance "$(lea_matrix_provenance | tr '\n' '|')" \
        --probes "${list[@]}"
    local rc=$?
    info "re-generating the catalogue so it reads the evidence file"
    do_catalog
    return $rc
}

# ===========================================================================
# catalog -- Phases 4, 5 and 6
# ===========================================================================
do_catalog() {
    [[ -f $TDIR/probes.tsv ]] || die "no traces in $TDIR -- scripts/ioctl-matrix.sh trace"
    lea_require_tools python3 gcc
    mkdir -p "$MDIR" || die "cannot write $MDIR"
    python3 "$LEA_ROOT/probe/python/ioctlmatrix.py" \
        --traces "$TDIR" --out "$MDIR" --driver "$DRV" \
        --vendor "$LEA_ROOT/vendor/open-gpu-kernel-modules" \
        --xlate "$LEA_ROOT/crates/nvrm-abi/src/xlate.rs" \
        --provenance "$(lea_matrix_provenance | tr '\n' '|')" \
        || die "the catalogue generator failed"
}

case $CMD in
    discover) lea_matrix_sample_trace; do_discover ;;
    probes)   do_probes ;;
    trace)    do_trace "$@" ;;
    catalog)  do_catalog ;;
    guest)    do_guest "$@" ;;
    verify)   do_verify "$@" ;;
    all)
        lea_matrix_sample_trace
        do_discover && do_probes && do_trace && do_catalog
        ;;
    *) error "unknown subcommand: $CMD"; usage 2 ;;
esac
