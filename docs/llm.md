<!-- SPDX-License-Identifier: MIT -->
# Maintenance notes

The filename is retained for existing links. Start with
[Architecture](ARCHITECTURE.md), [Testing](TESTING.md) and [Security](SECURITY.md).
Dated investigations are preserved in the [archive](history/llm-2026-09-18.md).

## Changes

- Keep NVIDIA layout knowledge in host ABI descriptions and generated files.
  The guest interprets tables but still has driver-specific UVM/display code.
- Regenerate bindings with `cargo xtask abi`; verify with `cargo xtask abi --check`.
- Regenerate the wire header with
  `cargo run --locked --bin nvrm-genhdr -- guest-module/virtio_nvrm/nvrm_wire.h`.
- Do not edit generated output or vendored sources. Change their maintained inputs.
- Comments should explain ownership, invariants or reasons. Put investigations in
  dated evidence; distinguish observations from hypotheses.
- Keep issue numbers and links stable. Use English for maintained code and docs.
- Run focused checks while editing, then the [software suite](TESTING.md#software-checks)
  and affected hardware workloads.

## Failure cases

- Guest process IDs are untrusted. The host security boundary is the VM.
- Check NULL allocation parameters before class lookup; root clients have no params table.
- Reserve quota before allocation. Failed setup must release acquired resources or
  retain an explicit owner and charge if cleanup is uncertain.
- Check syscall and embedded RM status before removing bookkeeping.
- Retain FD ownership through worker snapshots and cancellation acknowledgement.
  Poller retirement does not prove GPU backing release.
- Session-local pool addresses can repeat across guest processes.
- An empty environment variable is still present; match the reader's semantics.
- Rebuild after restoring source during mutation tests; restored timestamps can
  otherwise leave a stale binary.

## Measurements

- Follow [Testing](TESTING.md#measurement-rules). Disable debug/tracing overhead.
- Use saved PIDs; process-name patterns can match the controlling shell.
- Do not edit running shell scripts or run competing gates on one GPU.
- Verify library lookups as well as ioctls. Missing `dlopen` dependencies can
  prevent initialization despite successful RM calls.
- Verify changing pixels. Separate fresh and aged compositor state.
- VRAM accounting excludes driver-owned overhead; quotas are not physical reservations.

## Historical evidence

- [Measurements](history/llm-2026-09-18.md#4-numbers-that-are-measured):
  transport, rendering, sharing and accounting observations.
- [Rejected hypotheses](history/llm-2026-09-18.md#5-hypotheses-that-were-tested-and-are-wrong):
  counter-tests before reopening a diagnosis.
- [Event ring sizing](history/llm-2026-09-18.md#event-ring-sizing-measured-twice):
  burst measurements used to choose capacity.
- [Ray-tracing initialization](history/llm-2026-09-18.md#raytracing-initialisation-four-causes-only-the-last-fatal):
  GPU-ID translation, UVM flags and rtcore staging.
- [Shared-window rationale](history/llm-2026-09-18.md#why-there-is-no-copy-based-fallback-for-the-window):
  ordinary CPU loads/stores have no ABI flush point for a copy fallback.
