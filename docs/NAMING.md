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
