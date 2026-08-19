<!-- SPDX-License-Identifier: MIT -->
# Licensing

Leandro carries two licenses, split along the boundary that the code itself
is built around: the parts that run inside a guest kernel, and everything
else.

| Part | License | Why |
|---|---|---|
| `guest-module/` — `virtio_nvrm.ko`, `nvrm_nodes.ko` | **GPL-2.0-only** (`guest-module/LICENSE`) | They are Linux kernel modules. Both declare `MODULE_LICENSE("GPL")` and carry an `SPDX-License-Identifier: GPL-2.0-only` header; a kernel module that uses GPL-only symbols has no other option. |
| everything else — the Rust workspace, scripts, probes, docs | **MIT** (`LICENSE`, `LICENSES/MIT.txt`) | Decided 2026-08-17 for publication: the point of releasing this is that kernel and virtualization people can read, copy and reuse it without a licence conversation first. It carries a shared copyright, Silas Müller and University of Stuttgart, IKR. This REPLACED AGPL-3.0-or-later, under which the workspace was written; the relicensing is the author's own. |

`SPDX-License-Identifier` headers state this per file, and each source\nfile carries `SPDX-FileCopyrightText` lines for the two copyright holders. The two licenses are
not combined into one binary: `virtio_nvrm.ko` runs in the guest kernel and
`vhost-user-nvrm` is a separate user-space process on the host. What crosses
between them is the virtio protocol, not linked code. The one place both
sides must agree is the wire layout, which lives in `crates/nvrm-wire` and is
generated into `guest-module/virtio_nvrm/nvrm_wire.h`; that generated header
is part of the kernel module and carries the module's license.

## Third-party material

- `vendor/open-gpu-kernel-modules` — NVIDIA's open kernel modules, **MIT**
  (see `vendor/open-gpu-kernel-modules/COPYING`). Vendored as a *reference*:
  the build reads its headers to derive ABI constants and struct sizes.
  No NVIDIA code is copied into Leandro's sources; what is taken are
  numbers, each cited at its header and line.
- `vendor/cloud-hypervisor` — Apache-2.0/BSD-3-Clause, with the local patch in
  `patches/` under the same terms.
- `vendor/virtio-spec` — the OASIS VIRTIO specification source
  (https://github.com/oasis-tcs/virtio). Vendored as a *reference* only, for
  the normative text on cross-device object export (§2.11) and the GPU
  device's blob and UUID commands. **Not modified and not shipped**: the
  OASIS IPR policy permits copying and redistribution of the document
  unchanged with its copyright notice, and explicitly not in modified form,
  so it is gitignored like every other vendored tree and re-cloned by
  whoever needs it.
- Struct layouts cross-checked against gVisor's `pkg/abi/nvgpu` (Apache-2.0).

## What is deliberately not shipped

NVIDIA's user-space libraries (`libcuda.so`, `libnvidia-ml.so`,
`nvidia-smi`). Their license does not permit redistribution here. The guest
must obtain the version matching the host driver itself — which is also why
the packaging enforces a version check rather than bundling anything.

## Relicensing, 2026-08-17

The Rust workspace, the host scripts and the probes carried
`AGPL-3.0-or-later` until 2026-08-17 and now carry `MIT` — 73 files at
the time (the tree has grown since). The
reason is publication: AGPL asks every reader who might run this as a
service to think about reciprocity first, and that is a poor greeting for
the audience this code is being published FOR.

What was deliberately NOT relicensed:

- `guest-module/**` stays **GPL-2.0-only**. It links against the kernel ABI;
  nothing else is possible, and nothing else was wanted.
- `patches/` stays with the licence of what it patches — cloud-hypervisor
  is Apache-2.0/BSD-3-Clause. A patch is a derivative of its target, not of
  this tree.
- `vendor/` is not ours and is not in git.

The full texts live in `LICENSES/` (`MIT.txt`, `GPL-2.0-only.txt`) so that
every identifier used in an SPDX header has its text in the tree.
