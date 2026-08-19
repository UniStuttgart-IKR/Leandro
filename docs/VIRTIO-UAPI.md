<!-- SPDX-License-Identifier: MIT -->
# A generic device: table-described UAPI forwarding

Design note, 2026-08-07. No code and no measurements except where one is
named. It answers three questions that keep coming up: what the device
would have to be called, what other than NVIDIA could run on it, and why
virtio-gpu is not the channel.

## The claim

**`virtio-nvrm` is not an NVIDIA device, despite its name.** It is a
device that carries a **table-described UAPI** (userspace ABI: the
ioctl surface a driver offers) across the VM boundary —
`open`/`ioctl`/`mmap` of a foreign driver, where the description of the
calls arrives from the host at runtime instead of being compiled into the
guest.

The evidence is checkable in the module: the guest driver looks every call
up in a descriptor table the host sends at startup — payload size,
embedded pointers, fd fields — and carries **no NVIDIA knowledge**. The
NVIDIA specifics sit entirely in the table *contents* and in the host
backend, i.e. in **data** and in **host userspace**, not in the guest
kernel and not in the protocol.

Counted in `crates/nvrm-wire/src/lib.rs`: of its ten message kinds, eight
are already generic (`Hello`, `Open`, `Close`, `Ioctl`, `MapPrepare`,
`GetTables`, `MapRelease`, `ProcGone`). **Two** carry NVIDIA semantics:
`UvmPoolBack`, "back a UVM span with guest pages" (UVM: NVIDIA's
unified-memory driver), and `EventFired`, the host→guest firing on the
second queue, whose envelope — an unsolicited completion addressed to a
token — is generic while its fields are RM's (`hClass`, `hEvent`,
`notifyIndex`). Plus one enum that would need renaming: `DevTag` is a
list of NVIDIA node names today and would generically be a **node
index** the backend names.

`UvmPoolBack` generalises cleanly to "back a device-managed VA span with
guest pages" — not an NVIDIA concept but the shape of any device with a
unified address space. `EventFired` generalises the same way: keep the
envelope, make the class field a backend-defined selector.

**What that means for standardisation:** you would not specify NVIDIA
semantics, you would specify the **envelope** — framing, table format,
window rules, lifetimes. The unstable part becomes *payload* rather than
specification. That is the same trick by which `virtio-fs` carries a FUSE
payload without specifying FUSE.

## The name

The virtio specification names devices after **what they are**, not after
the market they are sold into. `virtio-hpcdevice` is out for that reason —
"HPC" describes a customer, and it excludes the very case measured here,
which is rendering and streaming.

| Candidate | For | Against |
|---|---|---|
| **`virtio-uapi`** | names the mechanism, no market claim, spec-shaped: "a device carries another device's UAPI" | "UAPI" in kernel usage means the *entire* userspace interface; reads too broad |
| `virtio-accel` | short, mirrors `drivers/accel` | describes accelerators, not RDMA — and `drivers/accel` is DRM-based, which is exactly the family that does **not** fit |
| `virtio-devproxy` | honest; gVisor's `nvproxy` established the word | "proxy" sounds like a workaround, not a device |
| `virtio-nativectx` | connects to virtio-gpu's "native context" | inherits a term from the GPU context it is leaving |

## What a device needs to be carryable

Derived from what is measured here. These decide what could run on such a
device at all.

| | Criterion | What it hangs on |
|---|---|---|
| **K1** | The **caller** chooses the handles | RM's `hObjectNew` (the handle an allocation will get, chosen by the caller) is caller-specified, so handles travel verbatim. DRM/GEM handles are assigned by the kernel per `struct drm_file`, which would need a translation layer. |
| **K2** | The hot path is **not** the ioctl path | Zero ioctls per kernel launch, steady-state rate zero, 12.15 µs median per call. |
| **K3** | There is a **mappable resource** for the window whose semantics need no flush point | The copy fallback is rejected with reasons in [`llm.md`](llm.md): without a window it is a different architecture. |
| **K4** | The surface is **tabulatable** | Of about sixty controls NVIDIA's EGL touches, exactly seven carry an `NvP64` (RM's embedded-pointer type — the thing a table must describe). |
| **K5** | Request/response **plus a one-way notification channel** | Compute never needed a wakeup; the display path did, and got queue 1 (`EventFired`): the host posts firings and never waits. A carryable device needs that channel, not more. |
| **K6** | The value sits in **proprietary userspace** | If userspace is open you rewrite it and do not need the device. |

## What else could run on it

All speculation — only NVIDIA is measured on this rig.

**Good fit:** ML accelerators with their own character device (AWS Neuron,
Google TPU, Habana/Gaudi, Qualcomm Cloud AI). Proprietary runtime (K6),
submission over mapped rings and doorbells (K2, K3), version lockstep
already a given. **But** what has landed in `drivers/accel/` is DRM-based
and inherits K1 as a problem — the kernel community pushed accelerators
into precisely the subsystem that fits worst here. Checkable per driver in
a minute: `grep drm_gem_handle_create`.

**Structurally the closest relative: RDMA verbs** (`/dev/infiniband/uverbs*`).
The control path is a described command structure, the data path is mapped
QP/CQ rings and a doorbell; K2, K3 and K4 fit almost word for word. K1
breaks, because uverbs object ids are assigned by the kernel — but it
would be a **uniform** translation over one object space rather than a
field-by-field one, which is a different order of difficulty. K5 fits
once the notification channel is stated: completion events are exactly
what queue 1 carries here.

## Why not virtio-gpu

The virtio-gpu carrier was in service until it was removed on
2026-08-04. **The reason is an asymmetry, not a judgement.** virtio-gpu is
reachable only from userspace, and userspace was exactly what the guest
kernel module abolished:

- **Not one `EXPORT_SYMBOL`** in the whole virtio-gpu driver — there is no
  kernel API for contexts and blobs.
- **`filp_open` on the DRM node fails without `set_fs()`**, which has not
  existed since 5.10; the UAPI entry points dereference `__user` pointers
  that a module cannot supply.

The switch made things **simpler, not harder**: the entire blob/EXECBUFFER
dance existed only because an *existing* guest driver had to be reused. It
also measured faster — 12.15 µs against 17.64 µs median, at an identical
number of calls across the boundary (90 ioctls per `cuInit` either way).

This also explains why AMD and Intel may do it differently: virglrenderer's
native contexts have their **guest side in Mesa, i.e. in userspace**, so
they may use virtio-gpu. That is not available here, because `libcuda` is
closed.

## Toward a device ID of its own

`device_type = 60` is squatting today, and the number is a measured
constraint: virtio-PCI *modern* maps PCI device `0x1040 + type` and
accepts only `0x1040..0x107f`, i.e. types 0 to 63. The obvious choice
`0x4E56` ("NV") would never have bound.

The formally correct route is a device ID through the OASIS virtio
process, then a guest driver in `drivers/virtio` or `drivers/misc`.
Prospects are poor. Three objections, hardest last:

1. **No specified payload semantics.** Answer: you specify the envelope,
   not the letter. Precedent: `virtio-fs`.
2. **No migration.** Answer: the class is "bound to a host resource", like
   VFIO.
3. **A conduit for a proprietary out-of-tree ABI.** There is no technical
   answer to this one, and it is where it would fail.

**Independently upstreamable, and currently unused by anyone else:** the
SHMEM patch for cloud-hypervisor's *generic* vhost-user device
(`patches/0001-generic-vhost-user-shmem.patch`, about 250 lines, no NVIDIA
in it). It was verified generic during development by pointing a throwaway
test device at it, which got the then 256 MiB region without knowing
anything about this project. QEMU's `vhost-user-device` has the same gap.
The second patch in the series (`0002-generic-vhost-user-device-features.patch`)
is generic for the same reason: it lets a backend offer device-specific
feature bits through the generic device.

## Open points

1. Generalise `UvmPoolBack`, `EventFired`'s class field and `DevTag` —
   the three places where NVIDIA still appears in the protocol.
2. Whether the table description really carries a second device, or
   silently contains NVIDIA assumptions. Only a second user answers that,
   and there is none.
3. Whether a specification that leaves payload semantics open can be a
   virtio device at all — or whether the honest home is `drivers/misc`
   without a spec.
