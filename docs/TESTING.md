<!-- SPDX-License-Identifier: MIT -->
# Testing

Software checks run in core without a GPU. Hardware validation needs the exact
host driver, guest userspace, kernel and workload under test.

## Software checks

```sh
./tools/build.sh vendor
cargo fmt --all -- --check
./tools/ci/check-c-format.sh
./tools/check.sh
./tools/ci/check-c.sh all --sanitize
```

- Dependencies: [Development](../DEVELOPMENT.md#requirements).
- `check.sh`: debug/release tests, doctests, rustdoc, Clippy, generated ABI/header
  checks, C tables, EDID, vendor layouts, shell syntax and repository conventions.
- Clippy denies warnings; NVIDIA binding allowances are `unnecessary_cast` and
  `field_reassign_with_default`.
- `CC` selects the sanitizer compiler; `check-c.sh host-tools` selects EDID/frame tools.
- [CI](../.github/workflows/check.yml) also checks workflows, builds Linux 6.8/6.12
  guest modules and checks Nix. Compilation does not validate GPU operation.

## Hardware validation

- The [quickstart](QUICKSTART.md) provides public manual setup and display checks.
  Its fresh-install sequence is not yet validated end to end.
- The optional private Leandro-Test harness owns automated gates. Core does not
  require it. With access and a prepared rig, rebuild core release binaries, then:

```sh
cd ../Leandro-Test
./scripts/showcase.sh state --check
./lea acceptance gpu vdisplay display
```

| Gate | Coverage |
|---|---|
| `gpu` | Tables, native/guest NVML, mapping, CUDA, PyTorch results, cleanup, process attribution and NVENC |
| `vdisplay` | DRM device, EDID, modeset and deterministic frame readback; no desktop required |
| `display` | Desktop capture, Vulkan presentation/ray tracing, event latency, streaming and kernel state |

- `lea gate` is a separate, narrower smoke check.
- Gates require current binaries and prerequisites; they do not build the workspace.
- Result: one JSON verdict on stdout, diagnostics on stderr. Exit codes:
  **0 pass, 1 fail, 2 skip**. Missing prerequisites must never count as a pass.
- Run sequentially on an idle rig. Stop VMs before backends; use recorded PIDs.
- These gates exercise functionality. [Isolation and lifetime tests](SECURITY.md#validation-needed-before-stronger-claims)
  remain separate requirements.

## Fuzzing

- `crates/vhost-user-nvrm/fuzz` feeds guest messages to `handle_msg` with fake
  driver calls. It does not exercise NVIDIA or kernel scheduling.
- `cargo test` replays the committed corpus in `the_fuzz_corpus_still_goes_through`.
- With cargo-fuzz and nightly installed:

```sh
cd crates/vhost-user-nvrm/fuzz
cargo +nightly fuzz run handle_msg -- -max_total_time=3600 -max_len=131072
```

- `LEA_CAPTURE_DIR` records real requests for corpus growth. Minimize failures and
  add regressions. Captures may contain workload data; review before publishing.
- An archived campaign completed 136,520,904 executions in 3601 seconds without
  a crash. No new campaign was run for the 2026-09-19 refactor.

## Measurement rules

- Record commits, driver/userspace, kernels, GPU, guest image, workload, transport,
  persistence state, CPU governor, competing load and warm/cold state.
- Keep native/guest conditions equal. An open backend keeps RM clients alive;
  comparing it with a cold native run changes GPU state.
- Rotate/interleave variants. Report sample count, median and p10/p90 with raw data.
- Check correctness alongside timing. Measure hot-loop and spaced calls separately.
- Disable tracing and timing diagnostics such as `LEA_DEBUG` during benchmarks.
- Observe changed pixels or received frames; FPS counters alone prove neither.
- Distinguish accounting limits, physical residency and availability.
- New tests should reproduce an observable failure and verify cleanup/retention.
  Fake-driver tests need hardware follow-up for driver semantics and DMA lifetime.

## 2026-09-19 checkpoint

- Refactor baseline: `e4134ded1c7fc717bfd816ea6b8893ba3b039d90`;
  hardware harness: `e046e0493724bf8127c4cdc3305aa5b7e5d722f6`.
- RTX 2070, driver 610.57.04. Recorded gates passed: compute 126 s,
  virtual display 48 s, desktop 144 s.
- Tables: format 1, checksum `0x43ee86ab`, 9240 bytes;
  100 ioctl, 128 class, 50 control and 17 nested rows.
- Virtual-display pixels matched the committed 1600×900 frame hash.
  Desktop presented 120 Vulkan frames and delivered NVENC frames through Moonlight.
- One of ten fence samples used the poll fallback; streaming reported 5.43% network
  drops. No kernel oops; known flip/cache-query warnings remained.
- These results do not validate later changes, fresh-image installation or isolation.
- Older procedures and corrections: [testing archive](history/TESTING-2026-09-18.md).
  Measurement scripts belong to their recorded revisions.
