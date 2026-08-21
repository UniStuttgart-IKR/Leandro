<!-- SPDX-License-Identifier: MIT -->
# The three cloud-hypervisor patches

`scripts/build.sh ch` clones cloud-hypervisor at [`CH_VERSION`](../CH_VERSION)
and applies all of these before building it. They are the only changes
this project needs in a VMM, and neither of them mentions NVIDIA, CUDA or
Leandro: they close two gaps in cloud-hypervisor's **generic** vhost-user
device, and any backend behind `--generic-vhost-user` gets them.

That is the argument for sending them upstream, and it is why they are
kept as two separate patches rather than one local fork.

| | What it closes |
|---|---|
| `0001-generic-vhost-user-shmem.patch` | the shared memory window is never negotiated, so a backend cannot expose host memory to the guest at all |
| `0002-generic-vhost-user-device-features.patch` | only transport feature bits reach the guest, so every device type is reduced to its featureless form |
| `0003-generic-vhost-user-refused-request.patch` | a request the backend's handler refuses is treated like a dead socket, so one refused `SHMEM_MAP` kills the whole device |

## 0001 -- the shared memory window

cloud-hypervisor v53 has the plumbing (the `cache` field, `get_shm_regions`,
userspace mappings) but never advertises `SHMEM`, never asks the backend
for region sizes, and `device_manager` always passes `None`. The window
therefore never exists.

The patch advertises the protocol feature, calls `GET_SHMEM_CONFIG` when
the backend acks it, allocates one host window covering all regions,
registers it as a KVM userspace mapping and implements `shmem_map` /
`shmem_unmap` -- both bounds-checked against the window, because a backend
is not trusted to stay inside it. `unmap` restores an anonymous
`PROT_NONE` region rather than leaving a hole.

Why a generic device needs it, in one sentence that is not about this
project: a vhost-user backend has no other way to put host memory in front
of a guest, and the shared memory region is the mechanism virtio defines
for exactly that.

Why *this* project needs it: the first mapping CUDA asks for is
`TURING_USERMODE_A`, the doorbell -- 64 KiB of MMIO on the card. It cannot
come from guest RAM, and routing its writes through the command stream
would turn a zero-roundtrip submission path into one roundtrip per
submission. [`../docs/VIRTIO-UAPI.md`](../docs/VIRTIO-UAPI.md) has the
protocol side.

## 0002 -- the device-specific feature bits

The generic device offers the guest `DEFAULT_VIRTIO_FEATURES`, which is
the transport set (bits 28..=38). Bits 0..=23 are device specific (virtio
1.4 section 6) and never reach the guest.

The measurement in the patch is deliberately **not** from this project: a
crosvm standalone virtio-gpu backend behind `--generic-vhost-user
device_type=gpu` came up as a 2D framebuffer, with `-virgl -edid
-resource_blob -host_visible -context_init` in the guest's own log,
because `VIRTIO_GPU_F_VIRGL` (bit 0) and `VIRTIO_GPU_F_CONTEXT_INIT`
(bit 4) were filtered out before the guest ever saw them.

The device cannot enumerate those bits itself -- it is generic precisely
because it does not interpret `device_type` -- but it does not have to.
Feature negotiation is `avail & backend`, so offering the whole
device-specific range delegates the choice to the only party that knows
what the bits mean. A bit the backend does not advertise is never acked.

## Status against upstream

Measured 2026-08-19:

- Both patches are written against **v53.0** and apply cleanly there;
  that is what `build.sh ch` uses.
- Neither applies to `main` as it stands (checked against `e99a7e4`).
  The drift is small: the three files they touch moved by 21 insertions
  and 17 deletions between v53.0 and that commit, and `git apply -3`
  resolves 0001 on its own.
- A rebase onto `main` is therefore the first step of any upstream
  submission, and it is a small one -- but it must be done and re-measured
  rather than assumed, because a patch that "should still apply" is how a
  VMM ends up with a window nobody bounds-checks.

## Sending them upstream

They are independent and should go as two changes: 0002 is small and
self-contained, 0001 is about 250 lines and touches the device manager.
Sending 0002 first is the cheaper conversation.

The claim that carries both is the same: this is the *generic* vhost-user
device, and a backend behind it currently cannot use shared memory or its
own feature bits. Anything about GPUs is an example, not the reason.
