<!-- SPDX-License-Identifier: MIT -->
# Future work

- Current capabilities: [README](../README.md).
- Recorded failures and measurements: [OPEN-QUESTIONS](OPEN-QUESTIONS.md).
- Items marked **proposed** need design review or measurements before implementation.

## Before the version freeze

- **Proposed: make memory ownership explicit.** Use an owned resource for the
  arena, RM OS descriptor and UVM mapping so every partial failure has a defined
  cleanup path. Verify teardown order on the GPU rig before changing it.
- **Proposed: document resource namespaces.** RM objects need a client plus a
  handle; process sessions and FD tokens have different scopes. Guest labels
  support routing and reporting, not a host-enforced per-process trust boundary.
- **Proposed: extract narrow session responsibilities.** Separate request
  validation, RM execution and lifetime bookkeeping behind existing tests.
  Keep protocol behavior stable and review one extraction at a time.
- **Proposed: define host-memory accounting.** `LEA_MAX_PIN_MIB` bounds an arena;
  it is not an aggregate host-side quota. A per-VM pin budget needs shared
  accounting and an explicit policy for overlapping registrations.
- **Proposed: clarify repository boundaries.** Keep protocol/runtime checks with
  Leandro; move rig orchestration and workload evidence through the agreed
  Leandro-Test split. Update every consumer before deleting shared helpers.
- **Proposed: add a short maintenance path.** Document one build, one software
  check and one hardware smoke run before expanding deployment instructions.

## Delivered foundations

- **NixOS:** host and guest modules, packages and a guest-image derivation exist.
  A NixOS guest passed the compute gate on 2026-08-19. See
  [DEVELOPMENT](../DEVELOPMENT.md).
- **Apptainer/SLURM:** `build.sh package` and `bench.sh slurm` package, submit and
  collect runs. A single-node SLURM run passed the compute gate on 2026-08-19.
- **Versioned ABI:** generated layouts and exact-version runtime selection cover
  configured 580, 595, 610 and 615 entries. See [driver versions](abi-versions.md)
  for the distinction between layout checks and hardware validation.
- **VRAM policy:** explicit guest allocations share a per-VM ledger. Reserved and
  card-derived profiles reduce the guest-visible budget to allow for RM overhead.

## Near term

- **Deployment guide:** connect the existing package, host configuration and
  guest setup into a procedure usable without reading all development notes.
- **Display capture:** investigate DMA-BUF/KMS capture into NVENC. The recorded
  X11 software capture used substantial host CPU; issues 17 and 35 describe
  earlier blockers. Measure the current path before choosing a change.
- **One guest module:** deliver required `/proc/driver/nvidia` data through the
  device, then evaluate folding `nvrm_nodes` into `virtio_nvrm`. This requires a
  protocol and refresh policy.
- **Guest kernel updates:** persistent boot setup exists, but rebuilding guest
  modules for a new kernel still needs an explicit packaging strategy.
- **Higher display modes:** EDID generation has software coverage beyond 1080p.
  Higher resolutions and refresh rates still need full event/capture validation;
  generated EDID alone does not prove that path works.
- **Host observability:** expose the backend ledger with clear units and scope.
  It reports intercepted allocations, not total GPU residency.

## Validation and compatibility

- Run `abi-verify.sh` on additional configured driver/GPU pairs and publish its
  inputs and raw results. A matching layout is necessary but not sufficient.
- Turing has the main differential evidence; Blackwell has recorded compute and
  display gates. Ampere/Ada and broader workloads remain validation work.
- Track exercised and unverified ioctl/class coverage from native traces and
  guest results, rather than inferring completeness from a passing workload.
- **Multi-GPU guests:** unimplemented; tables, sessions and policy need explicit
  GPU identity before one backend can serve several cards.
- **QEMU:** unimplemented and unmeasured. Check generic vhost-user support and
  the shared-memory window requirements before porting the VMM integration.
- **Snapshot, suspend and migration:** unsupported. Their resource lifecycle
  requires coordinated VMM and backend work.

## Memory and isolation limits

- The VRAM ledger does not see RM's internal context, USERD or firmware
  allocations. Reserved profiles allow for measured overhead without bounding it.
- Managed memory uses pinned guest RAM and is outside the VRAM ledger.
- A single backend serves one VM. Backends do not share admission control, so
  profiles can overcommit the physical card.
- Cross-VM admission and scheduling belong to an orchestrator such as
  [MeisterStack](https://github.com/UniStuttgart-IKR/MeisterStack). A host-wide
  policy requires a separate owner of card capacity and VM lifetimes.
- Forwarding a scheduler control does not give this backend ownership of GPU
  runlists or provide a scheduling guarantee.

## Longer-term research

- **Other drivers:** test the table model against a second interface before
  extracting a generic device. See [VIRTIO-UAPI](VIRTIO-UAPI.md).
- **Remote GPU:** a network transport cannot directly preserve the local mapping
  window. Investigate memory registration, polling and submission costs first;
  RDMA feasibility for the relevant GPU mappings remains unmeasured.
- **Upstream VMM support:** review the shared-memory and device-feature patches
  independently of the NVIDIA backend.

## Working method

- Measure native and guest behavior under the same conditions.
- Publish workload definitions, versions, raw output and limits with benchmark claims.
- Keep proposed behavior separate from measured results.
- Record rejected approaches and their evidence in [llm.md](llm.md).
