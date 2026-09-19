<!-- SPDX-License-Identifier: MIT -->
# Thesis freeze checklist

## Architecture decisions

- Keep the current transport and driver ABI stable during cleanup.
- Prioritize ownership fixes over moving functions between files.
- Use one typed key for RM objects: client root plus object handle. Track parent relationships where teardown is transitive.
- Implemented: transactional pool owners and a VM-wide pin budget; failed cleanup retains ownership and its charge.
- Implemented: acknowledged waiter cancellation, owned FD snapshots, and event generations.
- Implemented: monotonic guest process IDs, device references, and quarantine for uncertain submitted requests.
- Implemented: pure host-ABI request validation before event acquisition, private-client guards, and explicit refusal of known untranslated pointer/FD/capability operations.
- Keep the pin budget's 1024 MiB aggregate default provisional until repeated workloads establish retained-budget growth. It is registration accounting, not a native-reference ledger.
- Still needed: a reference ledger and release protocol covering duplicate/exported backing memory.
- Live unbind remains unsupported until NVKMS operations and shared-memory removal have a complete teardown barrier.
- Define and test a VM-wide policy for foreign RM source handles before accepting hostile guests.
- Keep the [current refusal policy](SECURITY.md#refused-operations) explicit in the freeze: 32 added controls and seven capability-FD classes can affect previously untested workloads.

## Suggested module boundaries

- **Guest transport:** virtqueues, request completion, cancellation, and event reception.
- **Guest device lifetime:** live/stopping/dead state and context/VMA references.
- **Guest mappings:** window slots, pins, and their release protocol.
- **Guest NVKMS/display:** kernel-facing adapters and virtual-display state.
- **Host request preparation:** `request_shape.rs` validates supported envelopes and translation metadata; `session.rs` resolves resources.
- **Host private clients:** `client_policy.rs` guards backend-owned clients in reviewed request layouts.
- **Host resource ownership:** FD/RM/UVM/arena owners with explicit rollback.
- **Host events:** registration, cancellation acknowledgement, and generation-checked completion.
- Extract one boundary at a time, preserving the wire format and validating it on the rig.
- Guest transport/mapping/NVKMS separation remains proposed; [ARCHITECTURE.md](ARCHITECTURE.md) describes current code.

## Leandro-Test split

- Implemented ownership: production Rust/C, wire/ABI definitions, fast tests and canonical `tests/tools/` remain in core.
- [Leandro-Test](../../Leandro-Test/README.md) owns VM provisioning, full hardware gates, probes, guest workloads, packages and measurement orchestration.
- Core entry points: `scripts/build.sh` for production builds and `scripts/check.sh` for software checks. Test entry point: `lea acceptance gpu vdisplay display`; `lea gate` remains a narrower smoke test.
- Core Nix exports production packages and `test-tools`; Test exports `acceptance-scripts`. Host `dev.enable` requires that package explicitly.
- Pin the core revision consumed by Test and record both revisions in every result.
- Preserve pass/fail/skip JSON, native-reference comparisons, stale-binary detection and teardown coverage during migration.
- On 2026-09-19, compute, virtual-display and desktop gates passed from Leandro-Test with the old core harness removed. These functional passes do not establish isolation.

## Version and evidence manifest

- Record the reviewed Leandro and Leandro-Test commit IDs.
- Keep `Cargo.lock`, `rust-toolchain.toml`, `flake.lock`, `CH_VERSION`, and the hypervisor patch series.
- Record the exact NVIDIA kernel driver and userspace versions/hashes, compiled ABI features, and generated table checksum.
- Record host/guest kernels, kernel configurations, GPU model, and guest image hash.
- Preserve Ubuntu bake/package manifests or pin a Nix image; a cloud-image checksum alone does not pin subsequent package installs.
- Record Python wheels, native-reference environment, compiler and sanitizer versions.
- Preserve gate JSON, logs, GPU state, and workload inputs beside the manifest.

## Exit criteria

- [ ] User review of each changed core component.
- [x] GPU-free suite, formatters, sanitizers, and both kernel builds pass.
- [x] Compute, virtual-display, and desktop gates pass on the refactor source snapshot; retain its commit and artifact hashes.
- [x] Relevant encode/display paths pass with the new control/class refusals; results are from this batch.
- [ ] PID-scoped sharing passes same-backend sharing and UVM tests on the GPU.
- [ ] Repeated allocation/free and waiter register/unregister runs record budget growth, FD counts and event/frame latency.
- [ ] Guest timeout/reset/failed-close cases retain backing and quota as intended; helper tests alone do not establish those runtime lifetimes.
- [ ] Remaining lifecycle and isolation limitations are accepted as thesis scope or fixed with targeted tests.
- [x] Relocated acceptance runner passes with the old core harness removed; record both resulting commits.
- [ ] Documentation commands are checked against the final scripts.
- [ ] Freeze manifest is stored with reproducible artifacts and a release tag.
- Keep architectural changes after the freeze on a separate development branch; thesis results refer to the frozen revision.
