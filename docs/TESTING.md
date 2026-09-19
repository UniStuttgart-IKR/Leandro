<!-- SPDX-License-Identifier: MIT -->
# Testing and Measuring

- Run software checks after each small refactor; run the relevant hardware gate before calling behavior unchanged.
- Core owns software checks and canonical `tests/tools/` sources. [Leandro-Test](../../Leandro-Test/README.md) owns hardware gates, provisioning, probes and measurements.
- Keep both revisions with every result. On 2026-09-19, `gpu`, `vdisplay` and `display` passed from Leandro-Test with the old core harness removed.
- Older measurements, procedures and corrections are preserved in the [2026-09-18 snapshot](history/TESTING-2026-09-18.md). They are evidence from those runs, not current acceptance results.

## 1. Two tiers: with GPU and without

- Start with `cargo fmt --all -- --check` and `scripts/ci/check-c-format.sh` (clang-format 22.1.8).
- Run `scripts/check.sh` for the complete software check. It needs the pinned Rust toolchain, a C compiler, libclang, `edid-decode` and the vendor headers fetched by `scripts/build.sh vendor`.
- That check covers debug and release Rust tests, doctests, rustdoc, Clippy, generated ABI/header consistency, the C descriptor interpreter, EDID, vendor class sizes, kernel API layout, shell syntax and repository conventions.
- Clippy runs with warnings denied. Two repository allowances remain for NVIDIA bindings: `unnecessary_cast` and `field_reassign_with_default`.
- Use `scripts/ci/check-c.sh all --sanitize` for C tests with ASan/UBSan; `CC` selects the compiler. `scripts/ci/check-c.sh host-tools` checks the host tools and frame fixtures separately.
- [GitHub Actions](../.github/workflows/check.yml) also checks shell/workflow syntax, builds guest modules against Linux 6.8 and 6.12, and evaluates the Nix configuration. Software CI does not prove a GPU workload works.
- Run the full hardware suite from Test:

```sh
cd ../Leandro-Test
./scripts/showcase.sh state --check
./lea acceptance gpu vdisplay display
```

- Individual Test commands remain `scripts/test.sh gpu`, `scripts/test.sh vdisplay` and `scripts/test.sh display`. `lea gate` is the narrower MeisterStack smoke check; its coverage is different.
- Hardware gates require built, current binaries and a ready GPU/VM rig. They do not build the workspace; a missing prerequisite produces a skip.
- A gate writes one JSON verdict to stdout and human output to stderr. Exit codes are **0 pass, 1 fail, 2 skip**. A skip is not a pass. The gate aggregator preserves that distinction.
- Test `probe/run/suites.sh` runs broader CUDA suites with known failures; it is separate from acceptance gates. Its async-stream expectation still needs the aggregate registration budget incorporated.

### What the gate covers

- **gpu:** descriptor tables, native/guest `nvidia-smi`, memory mapping, CUDA, PyTorch equality, process cleanup, the VM process list and NVENC output. The process-list stage matters because the general `nvidia-smi` comparison masks host PIDs.
- **vdisplay:** descriptor tables, DRM device, EDID, modeset and deterministic framebuffer readback. This is the first display gate after a refactor; it needs no desktop or stream.
- **display:** desktop capture, swapchain/presentation, ray tracing, event latency, streaming and kernel state. An FPS counter alone does not prove frames reached the screen.
- Run debug and release tests: unchecked arithmetic can panic in debug and wrap in release.
- A passing corpus or gate covers its exercised paths. It does not establish hostile-guest isolation; see [security limits](SECURITY.md).

## 2. The fuzz corpus

- Target: `crates/vhost-user-nvrm/fuzz`, `handle_msg`. Its input is an untrusted guest message; fake syscalls stop before the NVIDIA driver.
- `cargo test` replays the committed corpus through `the_fuzz_corpus_still_goes_through`, without nightly or a GPU.
- For a longer fuzz run, install cargo-fuzz and use:

```sh
cd crates/vhost-user-nvrm/fuzz
cargo +nightly fuzz run handle_msg -- -max_total_time=3600 -max_len=131072
```

- `LEA_CAPTURE_DIR` captures real requests for corpus growth. Capture on the rig, minimize the corpus, and keep any new failure as a regression.
- The archived run completed 136,520,904 executions in 3601 seconds without a crash. This is a dated result, not a bound on undiscovered defects.

## 3. Measuring — and the traps

### 3.1 The rig state is part of the result

- In Leandro-Test, record `scripts/showcase.sh state`; require `scripts/showcase.sh state --check` before benchmarking.
- Keep driver versions, GPU, persistence state, CPU governor, guest OS, transport, workload, warm/cold state and competing load with every result.
- The benchmark harness writes `rig.txt` and rejects mixed-state summaries. Do not combine harness results with a freehand native reference.
- Unset timing diagnostics such as `LEA_DEBUG`; log collection changes the hot path. Use diagnostics to count calls, then measure timing separately.

### 3.2 Persistence mode, concretely

- Keep persistence mode and `nvidia-persistenced` identical between native and guest runs.
- Historical native `cuInit` wall times were 209 ms with persistence off, 174 ms with `nvidia-smi -pm 1`, and 132 ms with the persistence daemon as well. The [original table and limits](history/TESTING-2026-09-18.md#32-persistence-mode-concretely) remain archived.
- The backend holds RM clients open. A cold native run compared with a warm guest run measures different GPU state.

### 3.3 How to measure correctly

- Rotate and interleave variants; report sample counts, median and p10/p90.
- Keep correctness checks beside timing columns. A faster incorrect result fails.
- Measure both hot-loop and spaced calls when claiming transport cost; queue wakeup can dominate scattered requests.
- Change one variable per comparison. Save the exact command, software revision, rig state and raw output.
- Run GPU gates sequentially on an otherwise idle rig. Check `scripts/showcase.sh status` first.

### 3.4 The probes

- Test `probe/c/ioctlping.c` and `ctrlping.c`: ioctl latency and payload size.
- `crates/nvrm-client/src/bin/mmapping.rs`: mapping-window round trips.
- Test `probe/c/managedprobe.c` and `probe/python/vramcap.py`: managed memory and cap/refusal behavior.
- `crates/nvrm-client/src/bin/smipids.rs`: direct process-list control comparison.
- Test `scripts/bench.sh transport`, `fleet`, `render`, `stream`, `diag`, `vk`: benchmark entry points. Use whole hours or `--minutes`; fractional hours are rejected.
- Keep probes and gate inputs in step with all provisioned fleet VMs.

## 4. Traps in the rig itself

- Stop instances through Test `scripts/showcase.sh down`. One backend serves one VM connection; killing it can hang or terminate that VM.
- Use recorded PIDs or the rig's pidfiles. Pattern-based `pgrep -f`/`pkill -f` can match the controlling shell; long process names can be truncated in `comm`.
- Do not edit a shell script while it runs; Bash reads it incrementally.
- `LEA_MANAGED_COMPAT` is a host setting. Setting it inside the guest has no effect.
- Rebuild after restoring source during mutation checks. Restoring an old mtime can leave Cargo running the mutated binary.
- Keep measurements separate from tracing; filtered `strace` still adds overhead.

## 5. What a new test must satisfy

- Reproduce a concrete behavior or failure boundary. Assert the observable result, including retained resources on failure.
- Show the regression fails against the old behavior, then passes with the fix, when practical without unsafe hardware experiments.
- Keep hardware prerequisites explicit. Tests must not quietly turn missing tools, skipped probes or stale binaries into a pass.
- Document limits: fake syscalls verify request handling, while driver semantics and GPU timing need hardware evidence.

## 2026-09-19 checkpoint

- Relocated gates: GPU 126 s, virtual display 48 s, desktop 144 s; all passed.
- Runtime tables: version 1, checksum `0x43ee86ab`, 9240 bytes; 100 ioctl, 128 class, 50 control and 17 nested rows.
- Virtual-display readback matched the committed 1600×900 frame hash. Desktop presented 120 Vulkan frames and delivered NVENC frames through Moonlight.
- One of ten fence samples used the poll fallback; streaming reported 5.43% network drops. The run establishes functional delivery, not a performance or lossless-streaming claim.
- No kernel oops; known flip/cache-query warnings remain. No new fuzz campaign was run.
- Historical scripts under `docs/measurements/` belong to their recorded revisions; use those revisions to reproduce old runs.
