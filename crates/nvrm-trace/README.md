<!-- SPDX-License-Identifier: MIT -->
# `nvrm-trace` — an `LD_PRELOAD` tracer that changes nothing

Observes the ioctl surface without touching it. It interposes
`open`/`openat`/`close`/`dup*`/`ioctl`/`mmap`/`mmap64`/`read`/`poll` and
logs every call on `/dev/nvidia*`, plus every fd registered as an event
channel via `NV_ESC_ALLOC_OS_EVENT`. Nothing is rewritten: every call
reaches the real libc symbol with unmodified arguments.

This is a **measuring instrument, not a data path.** The production path
(`virtio_nvrm.ko` plus `vhost-user-nvrm`) runs without it. It exists so a
run can be compared against a native one call for call — which is how most
of the findings in [`../../docs/OPEN-QUESTIONS.md`](../../docs/OPEN-QUESTIONS.md) were
obtained.

| File | What it is |
|---|---|
| `src/lib.rs` | the interposed symbols, the `dlsym(RTLD_NEXT)` resolution, and the safety rules of the interposed path (no `std::io`, no unwinding panic, constructor-time symbol resolution) |
| `src/fdtable.rs` | which fd refers to which NVIDIA device node — and why the table takes a `Mutex`, never an `RwLock` (CUDA forks) |
| `src/log.rs` | logging through raw `write(2)`, the line formats, and the rule that DRM ioctls are never decoded past `nr`/`size`/`ret` |

The rules those files enforce are constraints, not style; each one is
stated on the code it constrains, with what it cost to learn.

## Use

    LEA_TRACE_FILE=out.tsv LD_PRELOAD=/path/to/libnvrm_trace.so <program>

The build product is `target/release/libnvrm_trace.so`; the scripts find
it through `LEA_TRACE_LIB` (`scripts/lib/config.sh`), and
`probe/run/trace.sh` is the end-to-end harness around it.

## Output

**Two formats, one record.** Every line is built once as a list of named
fields and rendered by both renderers, so the two cannot carry different
information. `LEA_TRACE_FORMAT` picks which are written — `tsv`, `jsonl`,
or `both`, the default — and the JSONL goes beside the TSV, with the
`.tsv` suffix replaced (`out.tsv` → `out.jsonl`).

    open      ctl 9                        {"t":"open","dev":"ctl","fd":9}
    ioctl     ctl 0xd6 - 8 - 0 - 9         {"t":"ioctl","dev":"ctl",...,"fd":9}

The record kinds are listed at the top of `src/log.rs`. Nothing parses
either format directly: `lea_trace_stream` (`scripts/lib/common.sh`, awk,
because it also runs inside the guest) and `probe/python/traceread.py` are
the two readers, and `traceread.py --check` is the gate that asserts a
run's JSONL carries exactly the records its TSV does.

Why JSONL at all, since TSV counts and greps fine: the answer dumps for
allocations and UVM are per-command, variable-length payloads, and a
positional format with a fixed 32-byte tail cannot carry one without being
parsed by position *and* by convention.
