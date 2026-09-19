<!-- SPDX-License-Identifier: MIT -->
# Table-described UAPI forwarding

**Proposal, first recorded 2026-08-07.** The implementation targets NVIDIA RM/UVM.
No second driver has validated the proposed generalization.

## Current pieces

- Requests: open, close, ioctl, mapping, process teardown and descriptor transfer.
- Host-built tables describe payload sizes, embedded pointers and FD fields;
  the guest validates and interprets them.
- A shared-memory window exposes host resources without copying every submission.
- Queue 1 carries asynchronous notifications.

## Driver-specific parts

- Device tags identify NVIDIA nodes; UVM pool backing and RM events have NVIDIA semantics.
- Guest UVM, DRM and display code uses generated NVIDIA constants.
- Another driver needs defined node IDs, event semantics, memory registration,
  ownership and cleanup. Tables alone do not provide that contract.

| Question | Required evidence |
|---|---|
| Who allocates handles? | Whether kernel-assigned handles need translation |
| Where does submission happen? | Whether mapped rings/doorbells avoid control round trips |
| Can resources use the shared window? | Mapping, coherence and lifetime rules |
| Can arguments be described? | Nested pointers, variable lengths and FD ownership |
| Which events are asynchronous? | Delivery, cancellation and teardown behavior |
| Must userspace remain unchanged? | Whether a driver-specific userspace integration is simpler |

## Transport choice

- The earlier virtio-gpu carrier was removed on 2026-08-04.
- Kernel interception needed a kernel-facing transport; the earlier userspace
  context/blob interface did not expose the required kernel API.
- Historical median control-call latency: 12.15 µs with virtio-nvrm versus
  17.64 µs with the previous carrier, at 90 ioctls per `cuInit`. This is one rig's
  measurement, not a general comparison of transports.
- Ordinary mapped loads/stores have no ABI flush point for a copy fallback.
  The [archived rationale](history/llm-2026-09-18.md#why-there-is-no-copy-based-fallback-for-the-window)
  records the coherence problem and unverified cost estimate.

## Open design work

- `virtio-uapi`, `virtio-accel` and `virtio-devproxy` are candidate names only.
- A specification needs framing, tables, mapping access, lifetimes and error rules.
- Device type 60 is experimental, not an assigned project ID.
- Migration, ABI dependence and a second driver remain unresolved.
- ML accelerators and RDMA are research candidates, not supported devices.
- The generic [Cloud Hypervisor patches](../patches/README.md) can be reviewed
  independently. QEMU integration is unimplemented and unmeasured.
