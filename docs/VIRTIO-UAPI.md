<!-- SPDX-License-Identifier: MIT -->
# Table-described UAPI forwarding

- **Status:** design proposal, originally recorded on 2026-08-07.
- Current implementation: `virtio-nvrm`, targeting NVIDIA RM/UVM.
- Proposed generalization: carry another driver's userspace interface using the
  same framing, descriptor tables, mapping window and notification channel.
- No second driver has demonstrated that these abstractions are sufficient.

## What is reusable today

- Requests cover open, close, ioctl, mapping, process teardown and table transfer.
- Descriptor tables describe payload sizes, embedded pointers and FD fields.
  The host builds them; the guest validates and interprets them.
- The mapping window exposes host resources to the guest without copying each
  submission through the control channel.
- Queue 1 carries asynchronous host-to-guest notifications.

## What remains NVIDIA-specific

- `UvmPoolBack` registers guest pages for UVM; `EventFired` carries RM event fields.
- `DevTag` names NVIDIA device nodes.
- The guest module also contains UVM, DRM and display handling and uses generated
  NVIDIA constants. Table-driven forwarding does not make the entire guest generic.
- Generalization would need backend-defined node indices, event selectors and
  memory-registration semantics, with explicit lifetime and validation rules.

## Conditions to investigate for a second driver

| Question | Why it matters |
|---|---|
| Who allocates handles? | RM lets the caller choose object handles. Kernel-assigned handles need translation. |
| Where is the submission path? | Mapped rings and doorbells avoid a round trip per submission. |
| Can resources be mapped through the window? | A copy fallback changes coherence and submission semantics. |
| Can ioctl arguments be described? | Nested pointers, FD ownership and variable lengths need a complete description. |
| Which asynchronous events exist? | The notification channel must preserve their delivery and teardown rules. |
| Why preserve the existing userspace ABI? | A modifiable userspace driver may offer simpler integration options. |

- ML accelerators and RDMA verbs are possible research subjects, not supported
  devices. Handle allocation, DMA registration and event semantics need a
  driver-specific review before implementation.
- See [`llm.md`](llm.md) for the shared-window rationale and rejected copy path.

## Naming and standardization

- `virtio-uapi`, `virtio-accel` and `virtio-devproxy` are candidate names only.
- A proposed specification would need to define framing, tables, window access,
  resource lifetimes and errors. Whether backend-defined payload semantics are
  sufficient remains open.
- The current device uses experimental type 60. It has no assigned project
  device ID; an upstream device needs the relevant virtio allocation process.
- Migration and dependence on a proprietary driver ABI are additional issues
  to resolve, not assumed upstream acceptance criteria.

## Why this prototype uses its own virtio device

- The earlier virtio-gpu carrier was removed on 2026-08-04.
- Moving interception into a guest kernel module required a kernel-facing
  transport; the existing virtio-gpu context/blob interface was used from
  userspace and did not provide the needed exported kernel API.
- Recorded median control-call latency was 12.15 µs with virtio-nvrm versus
  17.64 µs with the previous carrier, at 90 ioctls per `cuInit`. These are
  historical single-rig measurements, not a general transport benchmark.
- Mesa-based native contexts operate from guest userspace; that integration
  route does not directly apply to unmodified proprietary `libcuda`.

## Independent VMM work

- [`0001-generic-vhost-user-shmem.patch`](../patches/0001-generic-vhost-user-shmem.patch)
  adds shared-memory support to cloud-hypervisor's generic vhost-user device.
- [`0002-generic-vhost-user-device-features.patch`](../patches/0002-generic-vhost-user-device-features.patch)
  carries device-specific feature bits through the generic device.
- These patches are candidates for separate upstream review. QEMU support
  remains unimplemented and unmeasured in this project.
