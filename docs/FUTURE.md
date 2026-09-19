<!-- SPDX-License-Identifier: MIT -->
# Future work

Current behavior is documented in [Architecture](ARCHITECTURE.md).
Numbered investigations and evidence are in [Known issues](OPEN-QUESTIONS.md).

## Correctness and isolation

- Track VM ownership of source RM objects across DUP, sharing and UVM imports.
- Track backing aliases through export/import and release guest pages only after
  verified native release. Source FREE/CLOSE is insufficient.
- Complete NVKMS/SHMEM teardown barriers before supporting live unbind.
- Audit remaining pointer, FD and client-handle fields; extend translation or refuse them.
- Enforce mapping acknowledgement negotiation before accepting shared-memory requests.
- Measure retained pin charges, FD counts and waiter latency under repeated workloads.
  A larger quota does not reclaim retained backing.
- Requirements and test cases: [Security](SECURITY.md#remaining-risks).

## Deployment and display

- Validate the [manual installation](QUICKSTART.md) from fresh images.
- Test guest kernel upgrades through the existing DKMS package and pinned Nix image.
- Measure KMS/DMA-BUF capture against X11 before choosing a capture change.
- Validate higher resolutions/refresh rates through presentation, capture and streaming.
- Consider merging `nvrm_nodes` only after defining how host proc data reaches the guest.
- Expose accounted, retained and quarantined resources with explicit units and scope.

## Compatibility

- Run additional exact driver/GPU pairs. Layout agreement alone proves no workload.
- Turing has the main hardware evidence. Blackwell has compute and virtual-display
  results; its full desktop gate remains unrun.
- Extend per-ioctl coverage from traces and checked workload results.
- Multi-GPU guests need explicit GPU identity in tables, sessions and policy.
- QEMU integration, snapshots, suspend and migration remain unimplemented.
- Host-wide admission/scheduling needs an external owner of card capacity and VM
  lifetimes. Independent backend limits can overcommit the card.

## Research

- Test a second driver before extracting a generic device: [UAPI proposal](VIRTIO-UAPI.md).
- A remote transport needs a replacement for local shared mappings; registration,
  coherence and submission costs are unmeasured.
- Prepare the generic [Cloud Hypervisor patches](../patches/README.md) for upstream review.
- Record versions, workload inputs, raw results and failed approaches with each claim.
