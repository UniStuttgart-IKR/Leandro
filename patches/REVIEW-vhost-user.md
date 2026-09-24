<!-- SPDX-License-Identifier: MIT -->
# Review of the previous patch series against the vhost-user specification

This review covers the three patches as they stood at commit `e2f1aec`
(`patches/0001-0003` before backlog item N2), checked against Cloud Hypervisor
v53.0 and the specifications listed below. Each finding gives the deviation,
the form the specification requires, and what Leandro core relied on. The
rewritten patches and [README.md](README.md) implement the corrected form.

## Sources

| Source | Version used | Where it came from |
|---|---|---|
| vhost-user specification | QEMU `docs/interop/vhost-user.rst` at commit `1276e6c85dcc` (2026-09-11), sha256 `684b11b15330ee23f2922aab9abd116efa1f48eb15b1b02b0233418e1a224257` | `https://gitlab.com/qemu-project/qemu/-/raw/1276e6c85dcc/docs/interop/vhost-user.rst`. The spec is not vendored; `vendor/qemu-vfio-user` holds only vfio-user files. |
| SHMEM history in that file | `588acb45c29f` (2026-06-03) added SHMEM_MAP/UNMAP with the feature at bit 22 (define then named `..._F_SHMEM_MAP`), `060ed6a4bb4d` added GET_SHMEM_CONFIG, `6d31fef92ade` renamed the define to `..._F_SHMEM` | same repository |
| Reference frontend | QEMU `hw/virtio/vhost-user.c` and `hw/virtio/vhost-user-base.c` at `1276e6c85dcc` | same repository; used to confirm how the spec is read, not as a spec |
| VIRTIO specification | VIRTIO 1.4 cs01, oasis-tcs/virtio-spec tag `v1.4-cs01` (`917e900e0246`): `content.tex` 2.2 "Feature Bits", `shared-mem.tex`, `transport-pci.tex` "Shared memory capability" | `https://github.com/oasis-tcs/virtio-spec` |
| Rust implementation | `vhost` 0.16.0 (pinned by Cloud Hypervisor v53.0 and by Leandro before N2) and 0.17.0, `vhost-user-backend` 0.22.0 and 0.23.0 | `~/.cargo/registry` sources |
| Cloud Hypervisor | v53.0 (`9ed824d6d`), `virtio-devices/src/vhost_user/`, `virtio-devices/src/transport/pci_device.rs`, `vmm/src/device_manager.rs` | `vendor/cloud-hypervisor` (copied, not modified) |

The `vhost` 0.16 crate is not a correct rendering of the specification: it
defines `VhostUserProtocolFeatures::SHMEM` as `0x20_0000` (bit 21). The
specification has assigned bit 21 to `VHOST_USER_PROTOCOL_F_GPA_ADDRESSES` and
bit 22 to SHMEM since SHMEM was added. `vhost` 0.17.0 fixes this
(rust-vmm/vhost#367, changelog "Fix `SHMEM` protocol feature bit position
(bit 21 -> bit 22) to align with the vhost-user spec, and add missing
`GPA_ADDRESSES` at bit 21"). Both ends of the old series used 0.16, so they
agreed with each other and disagreed with every conforming peer. This is the
"half-half wrong" part of the series.

## Patch 0001: shared memory window

| # | Area | Old series | Specification | Correct form | Leandro dependence |
|---|---|---|---|---|---|
| 1 | Feature bit | Offered and acked `VhostUserProtocolFeatures::SHMEM` of vhost 0.16, which is bit 21. | "Protocol features": `VHOST_USER_PROTOCOL_F_GPA_ADDRESSES 21`, `VHOST_USER_PROTOCOL_F_SHMEM 22`. | Negotiate bit 22 (vhost 0.17). The old frontend offered GPA_ADDRESSES under the name SHMEM: a conforming backend that supports GPA_ADDRESSES would have it acked and then read ring and memory-table addresses as guest-physical while Cloud Hypervisor sends user addresses. A conforming SHMEM backend never saw SHMEM acked. | `vhost-user-nvrm` advertised bit 21 through vhost 0.16 and only worked with the patched Cloud Hypervisor that made the same mistake. |
| 2 | GET_SHMEM_CONFIG parsing | Took `memory_sizes[..nregions]`, treating `nregions` as a prefix length. | "VIRTIO Shared Memory Region configuration": `num regions` is "the number of valid regions (non-zero size)", "The array index corresponds to the shared memory ID (shmid)". QEMU's `vhost-user-base.c` scans the whole array for non-zero entries. | Scan all 256 entries; the index is the shmid; `nregions` equals the count of non-zero sizes. A backend declaring only shmid 1 (`nregions = 1`, `sizes[1] = S`) got no region at all from the old code. | The backend sent `nregions = 2`, `sizes = [0, 8 GiB]`, which is invalid (one non-zero size) and only worked because of the prefix reading. |
| 3 | GET_SHMEM_CONFIG validation | Silently clamped `nregions` to the array; no size checks. | "The Shared Memory Region size must be a multiple of the page size supported by mmap(2)"; `num regions` bounded by the 256-entry array (QEMU rejects more). | Reject `nregions > 256`, a count that disagrees with the non-zero sizes, and sizes that are not page multiples. | None beyond finding 2. |
| 4 | SHMEM_MAP/UNMAP addressing | Ignored `shmid`; treated `shm_offset` as an offset into the whole window. Any shmid, including unused ones, was accepted. | "MMAP request": `shmid` identifies the region, `shm_offset` is "a 64-bit offset from the start of the pointed shared memory region". VIRTIO 1.4 "Addressing within regions": "offsets from the beginning of the region". | Look up the region by `shmid` (refuse unknown or unused ids) and resolve the offset against that region's start. | The backend's region 1 happened to start at window offset 0 because region 0 was empty, so window and region offsets coincided. |
| 5 | Mapping over a mapping | `MAP_FIXED` silently replaced whatever was mapped. | SHMEM_MAP: "Mapping over an already existing map is not allowed and requests shall fail." | Track mappings per shmid and refuse an overlapping request. | The backend checks overlap itself before sending, so it never relied on the replacement. |
| 6 | Unmap granularity | Accepted any in-bounds range, including partial or never-mapped ranges. | SHMEM_UNMAP: "the given range shall correspond to the entirety of a valid mapped region." | Accept only a range equal to one earlier SHMEM_MAP; refuse the rest. | The backend unmaps exactly what it mapped. |
| 7 | Alignment | No page-alignment checks on `shm_offset`, `len` or `fd_offset`. `mmap` rounds `len` up, so an unaligned length could replace the next mapping's first page. | Region sizes are page multiples; mappings are made with mmap(2), whose offset and address must be page aligned. QEMU maps through memory regions that impose the same. | Refuse unaligned `shm_offset`, `len` and `fd_offset`, and `len == 0`. | The backend already sends page-aligned requests. |
| 8 | Device reset | Mappings survived a device reset. | SHMEM_MAP: "mappings are automatically unmapped by the front-end across device reset operation." | Drop every mapping when the device is reset. | The backend kept its window table across resets. It must forget it when the next activation starts (see "Core changes"). |
| 9 | File descriptor passing | Did not offer `BACKEND_SEND_FD`, yet accepted the fd of SHMEM_MAP. | "Back-end communication": "If `VHOST_USER_PROTOCOL_F_BACKEND_SEND_FD` protocol feature is negotiated, back-end can send file descriptors ... using this fd communication channel." SHMEM_MAP's payload is "fd and struct VhostUserMMap". rust-vmm's `vhost-device-media` SHMEM backend advertises BACKEND_SEND_FD. | Offer BACKEND_SEND_FD; refuse SHMEM_MAP when it was not negotiated. | The backend did not advertise BACKEND_SEND_FD. |
| 10 | Snapshot/restore | The restore path skipped GET_SHMEM_CONFIG ("a restored device already has its window"), but device_manager passed no window on restore, so a restored device silently lost its BAR. | GET_SHMEM_CONFIG: the configuration is valid "for the entire lifetime of the connection"; a restore is a new connection, and mappings are backend state. QEMU installs a migration blocker for devices with regions. | Refuse to snapshot a device that has regions. | Leandro does not snapshot VMs. |
| 11 | Reply semantics | The handler returned `Ok(0)` or an errno; the vhost crate sent `0` or `-errno`. | SHMEM_MAP/UNMAP: with REPLY_ACK negotiated and NEED_REPLY set, "the front-end must respond with zero when operation is successfully completed, or non-zero otherwise." | Unchanged; conforming. | The backend requires the acknowledgement to release a slot. |
| 12 | PCI capabilities | Skipped zero-length regions. | VIRTIO 1.4 PCI "Shared memory capability": `cap.id` identifies a region and "MUST be unique"; the spec allows size 0 for an unused shmid. | Kept: the region list is indexed by shmid, unused ids are zero-length entries without a capability, so `cap.id == shmid`. | The guest looks up shmid 1. |
| 13 | Patch format | No `From`/`Date` headers or index lines; not `git format-patch` output. | Not a specification point. | `git format-patch` against v53.0. | None. |

## Patch 0002: device features

| # | Area | Old series | Specification | Correct form | Leandro dependence |
|---|---|---|---|---|---|
| 1 | Device-specific range | Offered bits 0 to 23 only. | VIRTIO 1.0 to 1.3: "0 to 23, and 50 to 127: Feature bits for the specific device type". VIRTIO 1.4 cs01 2.2: "0 to 23, 41, 42 and 50 to 127". | Offer 0 to 23 and 50 to 63 (the part of the range that fits the 64-bit vhost-user feature word). Leave out 41 and 42: 1.4 lists them as device specific only because legacy virtio-net features occupy them, and its reserved-bit list also defines bit 41 as `VIRTIO_F_ADMIN_VQ`, a transport feature Cloud Hypervisor does not implement. | None: the backend advertises bits 28, 30 and 32 only, so its negotiated set is unchanged. |
| 2 | Project reference | The code comment named `crates/vhost-user-nvrm/src/nvrm.rs` and virtio-nvrm. | Not a specification point. | Generic comment and commit message. | None. |

## Patch 0003: refused requests

| # | Area | Old series | Specification | Correct form | Leandro dependence |
|---|---|---|---|---|---|
| 1 | Refusal outcome | Kept the worker after `ReqHandlerError`; other errors fatal. | SHMEM_MAP requests "shall fail" on overlap and "can fail when there are no resources"; the reply is non-zero. The spec does not require the frontend to close the channel after a failed backend request, and QEMU keeps serving. | Unchanged in substance. The decision is now a small function with a unit test. | The backend's refusal-recovery test (`LEA_TEST_SHMEM_MAP_OOB`) depends on it. |
| 2 | Reply precondition | The comment said the backend "has already been told" of every refusal. | A reply is sent only with REPLY_ACK negotiated and NEED_REPLY set. | Comment and commit message state the condition. Without NEED_REPLY no reply is sent and the channel is still in step. | The backend negotiates REPLY_ACK, and vhost 0.17 sets NEED_REPLY on every backend request once it is acked. |
| 3 | Unknown or malformed requests | Fatal. | For an unknown backend request QEMU replies failure and continues. | Kept fatal. `FrontendReqHandler` returns `InvalidMessage` both for an unknown code (after replying) and for malformed headers, sizes or fds (before replying), so the frontend cannot tell whether the stream is in step. | None. |
| 4 | Patch format | Missing headers and index lines. | Not a specification point. | `git format-patch`. | None. |

## Core changes that follow

- `vhost` 0.17, `vhost-user-backend` 0.23, `virtio-queue` 0.18 and `vm-memory`
  0.18 in the workspace. SHMEM is bit 22 on both ends.
- `get_shmem_config()` returns `nregions = 1`, `memory_sizes[1] = 8 GiB`.
- `protocol_features()` adds `BACKEND_SEND_FD`.
- `set_backend_req_fd()` clears the window table. Cloud Hypervisor hands over a
  new backend channel on every activation, and it dropped every mapping across
  the reset before it.
- The guest module is unchanged apart from a comment: it already looked up
  shmid 1 and used region-relative offsets.

## Compatibility

The switch is clean and has no transition mode. An old backend (vhost 0.16)
advertises bit 21, which the new frontend does not offer, so SHMEM is not
negotiated and the device has no window; the guest module then logs
"no host-visible window -- mmap will fail". A new backend against the old
frontend fails the same way. The Cloud Hypervisor build and `vhost-user-nvrm`
must be updated together.
