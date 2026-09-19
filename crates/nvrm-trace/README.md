<!-- SPDX-License-Identifier: MIT -->
# nvrm-trace

- `LD_PRELOAD` observer for NVIDIA device calls, event FDs and selected DRM traffic.
- Interposes open, close, dup, ioctl, mmap, read and poll functions while forwarding
  their arguments to libc. Tracing adds overhead; it is not the production data path.
- Native/guest comparison evidence is recorded in
  [OPEN-QUESTIONS](../../docs/OPEN-QUESTIONS.md).

## Source map

| File | Responsibility |
|---|---|
| `src/lib.rs` | Interposed symbols and `RTLD_NEXT` resolution |
| `src/fdtable.rs` | Device/FD tracking, including fork-related locking constraints |
| `src/log.rs` | Raw-write logging, record fields and output formats |

- Avoid `std::io` on interposed paths: it can re-enter the hooks.
- Preserve constructor-time symbol resolution and C ABI panic constraints.
- DRM calls are logged without interpreting their payloads as RM structures.
- Logging preserves the libc call's `errno`, including failed trace writes.
- The FD table uses atomics; full hooks allocate and are not async-signal-safe.
- Payload decoding assumes readable caller buffers and a matching build/driver ABI.
  Invalid pointers or an ABI mismatch can fault in the tracer.

## Use

```sh
cargo build --release -p nvrm-trace
LEA_TRACE_FILE=out.tsv LD_PRELOAD=/path/to/libnvrm_trace.so <program>
```

- Build output: `target/release/libnvrm_trace.so`.
- Optional trace runners and comparison tools live in Leandro-Test.
- `LEA_TRACE_FORMAT` accepts `tsv`, `jsonl` or `both` (default).
- Without `LEA_TRACE_FILE`, only TSV is written to stderr.
- `LEA_TRACE_DUMP` limits bytes per payload dump (default 65536); 0 disables dumps.
- Output is appended. Use a fresh file or truncate it before each run.
- Both renderers consume the same named fields. JSONL replaces a `.tsv` suffix
  with `.jsonl`, or appends `.jsonl` otherwise.
- Record definitions: `src/log.rs`. Leandro-Test provides `traceread.py --check`
  to compare TSV and JSONL records.
- At normal exit, stderr reports failed/short writes and FD registrations outside
  the 65536-slot table if either count is nonzero. Forked children inherit counters;
  `_exit` and fatal termination skip this diagnostic. Other interception gaps are
  not counted, so zero counters do not prove a complete trace.
