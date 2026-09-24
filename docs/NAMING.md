<!-- SPDX-License-Identifier: MIT -->
# Naming

- Project: **Leandro**. Environment variables: `LEA_*`.
- `nvrm` denotes NVIDIA Resource Manager; UVM uses the same transport.
- Component names describe their role and interface. Leandro is not affiliated with NVIDIA.

| Role | Name |
|---|---|
| Protocol/device | `virtio-nvrm` |
| Guest frontend | `virtio_nvrm.ko` |
| Guest proc/pinning helper | `nvrm_nodes.ko` |
| Host backend | `vhost-user-nvrm` |
| Driver bindings | `nvrm-sys` |
| Ioctl descriptions/translation | `nvrm-abi` |
| Diagnostic RM ownership | `nvrm-client` |
| Wire schema | `nvrm-wire` |
| Userspace tracer | `nvrm-trace` |
| Input backend | upstream `vhost-device-input` |

- The old LD_PRELOAD forwarding shim is retired; `nvrm-trace` observes calls only.
- Avoid ambiguous names already used elsewhere: `nvshim`, `nvproxy`, `nvhost`, `nvvm`.
- New vendor backends should use separate ABI namespaces. `virtio-kfd` and
  `virtio-xe` are proposals, not implemented devices.
- Extract shared libraries only after a second backend demonstrates common requirements.

## Guest-visible GPU name

The name the guest's driver reports for a Leandro virtual GPU (`nvidia-smi`, NVML,
`GET_NAME_STRING`) is generated in one place, `nvrm_abi::naming`
([crates/nvrm-abi/src/naming.rs](../crates/nvrm-abi/src/naming.rs)). No other code
builds a complete name. This specification (P17, 2026-09-23) comes from the operator's
brief for the synthetic GPU naming model of the v2 (Caraxes) device, which shares the
module with v1.

- The fields are independent: platform `Leandro`; transport `VFIO` (the v2 vfio-user
  synthetic PCI GPU) or `VirtIO` (v1, `vhost-user-nvrm`); personality, the board the guest
  is told it has (`RTX 2070`, `A100`); profile, the guest framebuffer size (`4G`,
  `1333M`); backend (`OpenRM`, `nova-core`).
- The backend is an implementation detail and never appears in the name.
- Default format (since 2026-09-24): `Leandro <Transport> <Personality>-<Profile>`, for
  example `Leandro VirtIO RTX 2070-4G` or `Leandro VFIO RTX 2070-4G`.
- Legacy format, an explicit opt-in: `Leandro <Personality>-<Profile>`
  (`Leandro RTX 2070-4G`), the string both devices emitted before. v1 selects it with
  `LEA_GPU_NAME_FORMAT=legacy`, read once at start; an unknown value keeps the default.
  `parse_name` reads names of either format back into their fields. The gates in
  `../Leandro-Test` accept both from its commit `3edea73` on (`lea_mediated_name_re`);
  an older gate compares against the legacy name and fails on the default.
- The personality of a real card drops the vendor words: `NVIDIA GeForce RTX 2070`
  becomes `RTX 2070`.
- The profile is always a size: `<n>G` for whole GiB, `<n>M` otherwise. A vGPU-style
  type such as `4Q` names what the VM costs the card, not what the guest sees, so it is
  resolved through `vgpu::Catalogue::resolve` first (`RTX2070-4Q` with 3072 MiB of
  guest framebuffer is named `-3G`). `LEA_VRAM_LIMIT_MIB=1333` gives `-1333M`.
- v1's name field is 64 bytes. A name that does not fit loses its size suffix, and one
  that still does not fit becomes `Leandro GPU`.
- The model is `VirtualGpuSpec { transport, personality, profile, backend }`, so that an
  orchestrator can later supply the spec and both devices derive the guest-visible
  identity from it.
- Nothing in the catalogue arithmetic, the v1 policy selection (`LEA_VGPU_*`,
  `LEA_VRAM_*`), the protocol or the RM paths depends on the name format. Historical
  records (`docs/measurements/`, `docs/history/`, `matrix/`) keep the names they
  recorded.
