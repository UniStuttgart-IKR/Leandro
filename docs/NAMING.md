<!-- SPDX-License-Identifier: MIT -->
# Naming scheme — Project Leandro

**Leandro** is the project and repository name. It names the
undertaking, not a component. It disappears as soon as parts go
upstream — upstream, only the role names apply. Leandro is meant to be
vendor-neutral: the name binds to the undertaking (cooperative GPU
paravirtualization without SR-IOV), not to NVIDIA.

## One rule per level

1. **Project/umbrella:** `leandro`. Appears in the repository name,
   documentation titles, the paper and the env prefix (`LEA_*`).
   Nowhere else.

2. **Transport ends** are named after their role in the virtio world,
   the device after what it transports (virtio convention:
   virtio-net, virtio-blk, … → virtio-nvrm):
   - device/protocol:     `virtio-nvrm`
   - guest frontend:      `virtio_nvrm.ko`
   - host backend:        `vhost-user-nvrm`
   - guest helper module: `nvrm_nodes.ko` (device nodes + /proc; transitional)

3. **Libraries (Rust crates)** carry the interface namespace
   `nvrm-<role>` — they describe the NVIDIA RM interface and would be
   usable even without virtualization:
   - `nvrm-sys`    generated ABI (bindgen)
   - `nvrm-abi`    curated ABI: escapes, DRF, tables, xlate
   - `nvrm-client` RM object tree from the client's point of view
   - `nvrm-wire`   wire protocol (congruent with `nvrm_wire.h`)
   - `nvrm-trace`  measurement tool (userspace tracer + line format)

4. **Binaries and scripts** are named after their function, never after
   milestones. Milestone names (S0–S4, M1/M2) survive only in dated
   historical notes; no current gate label, script or argument carries
   one.

## Terminology

- **nvrm** = NVIDIA Resource Manager, the core of the NVIDIA kernel
  driver and namesake of the intercepted ioctl interface
  (`NV_ESC_RM_*`). Pars pro toto: the UVM ioctls ride along under the
  same device name.
- No component is named "shim" — none has existed since the LD_PRELOAD
  shim was removed on 2026-08-04.

## Multiple vendors

Leandro can — as working time and token budget permit — take further
backends under the same umbrella (AMD, Intel, each without SR-IOV).
The architecture is identical: the guest frontend intercepts the
vendor's kernel interface, the host backend translates in front of the
real driver. Only the interface namespace changes.

Rule: **the namespace is named after the intercepted kernel interface**
— just like `nvrm` for NVIDIA:

| Vendor | Interface                | Namespace  | Device        |
|--------|--------------------------|------------|---------------|
| NVIDIA | RM/UVM ioctls            | `nvrm`     | `virtio-nvrm` |
| AMD    | KFD (/dev/kfd) + amdgpu  | `kfd`      | `virtio-kfd`  |
| Intel  | xe DRM ioctls            | `xe`       | `virtio-xe`   |

Everything else follows mechanically: `virtio_kfd.ko`,
`vhost-user-kfd`, `kfd-abi`, `kfd-wire`, `kfd-trace`, …
The namespaces stay strictly separate; no crate mixes vendors.

Vendor-neutral building blocks (arena/window, session scaffolding,
trace line format) that get factored out once a second backend exists
are the only ones to carry the project prefix: `leandro-<role>`
(e.g. `leandro-arena`, `leandro-traceformat`).

## Do not use

- `nvshim*` (former name, collides with a Windows service),
  `nvproxy` (gVisor), `nvhost` (Tegra driver), `nvvm` (NVIDIA IR).
- `NVRM_*` as env prefix (confusable with NVIDIA's `NVreg_*`).

*Leandro is not affiliated with NVIDIA; "nvrm" denotes the virtualized
interface, not an NVIDIA product.*
