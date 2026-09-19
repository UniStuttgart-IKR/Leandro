<!-- SPDX-License-Identifier: MIT -->
# Cloud Hypervisor patches

Apply all three patches to the tag in [CH_VERSION](../CH_VERSION), currently
`v53.0`. `tools/build.sh ch` fetches, patches and builds it. The
[manual setup](../docs/VM-PREPARATION.md#build-and-patch-cloud-hypervisor)
shows the equivalent commands.

These changes extend the generic vhost-user device; none depends on NVIDIA.

| Patch | Change |
|---|---|
| [0001](0001-generic-vhost-user-shmem.patch) | Negotiate and map a shared-memory window |
| [0002](0002-generic-vhost-user-device-features.patch) | Forward device-specific feature bits |
| [0003](0003-generic-vhost-user-refused-request.patch) | Keep serving after an acknowledged handler refusal |

## Shared-memory window

- v53.0 has mapping support but does not negotiate `SHMEM`, obtain region sizes
  or pass a window to the generic device.
- Patch 0001 queries `GET_SHMEM_CONFIG`, allocates the window and registers it with KVM.
- `SHMEM_MAP` and `SHMEM_UNMAP` check window bounds. Unmap restores `PROT_NONE`
  instead of leaving an address-space hole.
- Leandro uses the window for host mappings, including GPU doorbells. Guest writes
  reach those mappings without a control request per write.

## Device features

- v53.0 offers transport features but omits device-specific bits 0–23.
- Patch 0002 offers that range; negotiation retains only bits the backend advertises.
- The recorded check used a crosvm virtio-gpu backend: filtering these bits removed
  its virgl, EDID, blob, host-visible and context features.

## Refused requests

- v53.0 terminates the backend-request worker on every handler error.
- Patch 0003 keeps it running after `ReqHandlerError`: the protocol handler has
  already acknowledged that refusal. Socket/protocol errors remain fatal.
- This lets a backend recover from a rejected mapping request without losing the device.

## Validation and upstreaming

- The supported build target is the pinned tag. Patch application is checked by builds;
  runtime mapping and refusal behavior need integration tests.
- The 2026-08-19 upstream check covered patches 0001/0002 against `e99a7e4`.
  It is historical; it says nothing about current upstream or patch 0003.
- Before submission, rebase and test each patch against the intended upstream revision.
- Submit independent changes separately. Describe the generic device behavior and
  include a reproducer; GPU use is one application.
