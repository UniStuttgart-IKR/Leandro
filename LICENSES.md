<!-- SPDX-License-Identifier: MIT -->
# Licensing

| Source | License |
| --- | --- |
| `guest-module/`, including the generated kernel wire header | GPL-2.0-only; [text](guest-module/LICENSE) |
| Rust workspace, core tools and documentation | MIT; [text](LICENSE) |
| Cloud Hypervisor patches | Upstream Apache-2.0/BSD-3-Clause terms |

- Per-file SPDX headers are authoritative. Copyright holders are Silas Müller and Universität Stuttgart, IKR where stated.
- The host backend and guest kernel modules are separate programs connected by the wire protocol.
- Hardware probes and VM scripts moved to Leandro-Test retain their MIT headers.
- Full local license texts: [LICENSES](LICENSES).

## Third-party sources

- `vendor/open-gpu-kernel-modules`: NVIDIA source used for ABI definitions, checks and guest NVKMS builds. Preserve its `COPYING` and per-file notices.
- `vendor/cloud-hypervisor`: upstream hypervisor with the patches in `patches/`.
- Generated driver layouts and provenance are recorded in `crates/nvrm-sys` and its generation inputs.
- Layout/object-model work also references gVisor `pkg/abi/nvgpu` (Apache-2.0).
- The optional `vendor/virtio-spec` checkout is reference material. It is not included in this repository's distribution.
- NVIDIA userspace libraries and tools are not bundled; deployment uses the version matching the host driver.

## License history

- On 2026-08-17, the author relicensed the Rust workspace, host scripts and probes from AGPL-3.0-or-later to MIT.
- Guest kernel sources remained GPL-2.0-only; third-party sources and patches retained their respective terms.
