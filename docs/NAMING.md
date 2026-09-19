<!-- SPDX-License-Identifier: MIT -->
# Naming

- **Leandro** names the project and repository. Use it in documentation,
  packaging and the `LEA_*` environment prefix.
- Name components after their role and intercepted interface.
- Use functional names for new tools. Historical milestone names belong in
  dated notes; existing diagnostic names may still reflect earlier experiments.

## Current components

| Role | Name |
|---|---|
| Device and protocol | `virtio-nvrm` |
| Guest frontend | `virtio_nvrm.ko` |
| Guest node/proc helper | `nvrm_nodes.ko` |
| Host backend | `vhost-user-nvrm` |
| Generated driver bindings | `nvrm-sys` |
| Ioctl descriptions and translation | `nvrm-abi` |
| RM client and object ownership | `nvrm-client` |
| Wire schema | `nvrm-wire` |
| Userspace tracer | `nvrm-trace` |

- `nvrm` refers to NVIDIA Resource Manager. UVM calls use the same transport.
- The retired LD_PRELOAD forwarding shim is not a current component.
  `nvrm-trace` uses LD_PRELOAD only for observation.
- Leandro is not affiliated with NVIDIA; the namespace identifies an interface.

## Proposed vendor namespaces

- Additional vendors are future work. The current implementation targets RM/UVM.
- Keep vendor-specific ABI descriptions in separate namespaces.
- Extract shared libraries only after a second implementation demonstrates the
  common requirements; proposed names such as `leandro-arena` are not current crates.

| Vendor | Candidate interface | Proposed namespace | Proposed device |
|---|---|---|---|
| AMD | KFD and amdgpu | `kfd` | `virtio-kfd` |
| Intel | xe DRM | `xe` | `virtio-xe` |

## Avoid

- `nvshim`, `nvproxy`, `nvhost` and `nvvm`: existing uses make these ambiguous.
- `NVRM_*` for environment variables: use `LEA_*` to distinguish project
  settings from driver settings.
