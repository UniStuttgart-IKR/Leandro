<!-- SPDX-License-Identifier: MIT -->
# Notes for working on this code

- Short maintainer notes; the filename stays for existing links.
- Start with [architecture](ARCHITECTURE.md), [testing](TESTING.md), [security limits](SECURITY.md) and the [question index](OPEN-QUESTIONS.md).
- The [2026-09-18 snapshot](history/llm-2026-09-18.md) preserves earlier measurements, corrections and rejected hypotheses. Treat historical implementation claims as dated.

## 1. Rules this code follows

- Keep NVIDIA layout knowledge in the host ABI description and generated files. The guest interprets the descriptor table.
- Regenerate bindings with `cargo xtask abi`; verify with `cargo xtask abi --check`. Do not edit generated Rust. The generator uses the pinned rustfmt.
- Regenerate `guest-module/virtio_nvrm/nvrm_wire.h` with `cargo run --locked --bin nvrm-genhdr -- guest-module/virtio_nvrm/nvrm_wire.h`; verify with `--check`.
- Write short comments about invariants, ownership and reasons. Retain safety conditions and measurements that constrain the code; move investigation narratives to dated evidence.
- Keep question numbers permanent. Never renumber or reuse them.
- Do not edit `vendor/`. Fetch the versions named by `DRIVER_VERSION` and the ABI configuration.
- Use English in code, comments, documentation and commit messages.
- Separate source evidence, runtime measurements and hypotheses. Record version, date and setup when a result depends on them.
- A guest process ID is guest-controlled. The host trust boundary is the VM/backend; FD routing and RM sharing require separate ownership checks.

## 2. Gates

- Run focused tests while changing code, then `scripts/check.sh` and the formatting checks before review.
- Use `scripts/ci/check-c.sh all --sanitize` for C interpreter, EDID and host-tool memory checks.
- Run the relevant full GPU gate from Leandro-Test (`lea acceptance gpu vdisplay display`) after software checks pass. The [testing guide](TESTING.md) defines coverage and the pass/fail/skip contract.
- Preserve debug and release coverage; arithmetic failure behavior differs.
- Core owns production and fast tests; Test owns provisioning, probes, workloads and measurements. Keep both revisions in results. `lea gate` is a narrower smoke check, not the full acceptance suite.

## 3. Traps

- An empty environment value is still present. Match each `LEA_*` reader's semantics to what the rig exports; presence alone is unsuitable for an on/off switch passed as an empty value.
- Use pidfiles and recorded PIDs. Avoid process-name patterns that match the controlling shell or assume an untruncated name.
- Do not edit running shell scripts. Do not run competing gates on the same GPU.
- Check the backend process before attributing a hung guest to the GPU.
- Keep debug logging out of timing runs; driver logging can also dominate or overwrite useful diagnostics.
- Verify presentation with pixels or observation, not a client's FPS counter.
- Keep fresh and aged Xwayland/compositor state separate in measurements.
- The RM tracer must not interpret DRM payloads using NVIDIA layouts.
- A null alloc-parameter pointer must be handled before looking up its class; root-client allocation has no parameter struct.
- Charge guest pins and pool allocations before committing them; failure must release reservations.
- Do not release host bookkeeping on ioctl return alone. RM's embedded status must also report success.
- A removed poll registration may still exist in a worker snapshot. Cancellation and FD reuse need an explicit lifetime contract; see question 77.

## 4. Numbers that are measured

- Historical values are in the [snapshot](history/llm-2026-09-18.md#4-numbers-that-are-measured). Do not present them as current benchmarks or universal limits.

### Limits

- Pin limits and VRAM policy are different controls. Check the host and guest settings separately; increasing only one pin limit may change nothing.
- See the [historical limit table](history/llm-2026-09-18.md#limits) and current [memory policy](../crates/vhost-user-nvrm/README.md).

### The VRAM ledger

- The ledger tracks mediated allocations. Driver-owned context memory remains outside it; a cap is not a physical reservation.
- The historical estimate was about 175 MiB of additional usage per backend in those runs. [Evidence and qualifications](history/llm-2026-09-18.md#the-vram-ledger).

### The event back-channel

- The archived comparison reduced fence waits from 10.10 ms to 0.12 ms; later runs still found occasional fallback waits. [Measurements](history/llm-2026-09-18.md#the-event-back-channel), [open question 14](OPEN-QUESTIONS.md#14-fence-waits-sometimes-fall-back-to-the-polling-timer).

### Sharing one card

- Four-VM compute and mixed compute/display runs demonstrated workload coexistence on one tested GPU. They did not establish hostile-tenant isolation. [Results](history/llm-2026-09-18.md#sharing-one-card).

### Rendering and presentation

- Off-screen render speed and visible presentation are separate measurements. [Historical render results and corrections](history/llm-2026-09-18.md#rendering-and-presentation).

### The descriptor table

- Generate current counts/checksum with `nvrm-genhdr --dump-tables`; do not copy the dated counts into acceptance criteria. [Earlier table sizes](history/llm-2026-09-18.md#the-descriptor-table).

### Protocol version

- The version and compatibility rule live in [nvrm-wire](../crates/nvrm-wire/src/lib.rs), beside `PROTO_VERSION`. [Historical version note](history/llm-2026-09-18.md#protocol-version).

## 5. Hypotheses that were tested and are wrong

- Read the [archived counter-tests](history/llm-2026-09-18.md#5-hypotheses-that-were-tested-and-are-wrong) before reopening a diagnosis.
- Pool GPU addresses belong to a guest-process session: separate processes can choose the same virtual address.
- Missing guest libraries can break initialization even when every forwarded RM call succeeds; inspect file opens as well.
- Keep negative results scoped to the versions and conditions tested.

## 6. Display path: derivations worth keeping

### Event ring sizing, measured twice

- Startup and a game plus streaming produced larger bursts than idle desktop tests. The archived 8192-entry ring held an 80,415 events/s spike with no ring-full drops. [Full measurements](history/llm-2026-09-18.md#event-ring-sizing-measured-twice).

### Raytracing initialisation: four causes, only the last fatal

- The fix required GPU-ID translation, preserving UVM initialization flags and staging `libnvidia-rtcore.so`. [Failure sequence](history/llm-2026-09-18.md#raytracing-initialisation-four-causes-only-the-last-fatal).

### The host-visible window

- Size the window from actual mapping demand; the earlier 1 GiB window filled during CS2. [Evidence](history/llm-2026-09-18.md#the-host-visible-window).

### Why there is no copy-based fallback for the window

- CPU mappings use ordinary loads/stores with no ABI flush point. Copying would need write tracking and a different coherence design. [Derivation and explicitly unverified cost estimate](history/llm-2026-09-18.md#why-there-is-no-copy-based-fallback-for-the-window).
