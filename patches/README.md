<!-- SPDX-License-Identifier: MIT -->
# Cloud Hypervisor patches

Apply all three patches, in order, to the tag in [CH_VERSION](../CH_VERSION),
currently `v53.0`. `tools/build.sh ch` fetches, patches and builds it. The
[manual setup](../docs/VM-PREPARATION.md#build-and-patch-cloud-hypervisor)
shows the equivalent commands. The patches are `git format-patch` output, so
`git apply` and `git am` both accept them.

The patches extend Cloud Hypervisor's generic vhost-user device
(`--generic-vhost-user`) and the vhost-user backend channel. None of them
refers to NVIDIA or to Leandro; virtio-nvrm is one user.

| Patch | Change | Files |
|---|---|---|
| [0001](0001-generic-vhost-user-shmem.patch) | VIRTIO Shared Memory Regions: `SHMEM` negotiation, `GET_SHMEM_CONFIG`, `SHMEM_MAP`/`SHMEM_UNMAP`; vhost-user frontend on `vhost` 0.17 | `Cargo.toml`, `Cargo.lock`, `virtio-devices/Cargo.toml`, `virtio-devices/src/vhost_user/*.rs`, `virtio-devices/src/lib.rs`, `virtio-devices/src/transport/pci_device.rs`, `vmm/src/device_manager.rs` |
| [0002](0002-generic-vhost-user-device-features.patch) | Offer the device-specific feature bits | `virtio-devices/src/vhost_user/generic_vhost_user.rs` |
| [0003](0003-generic-vhost-user-refused-request.patch) | Keep serving the backend channel after a refused request | `virtio-devices/src/vhost_user/mod.rs` |

[REVIEW-vhost-user.md](REVIEW-vhost-user.md) lists what the previous version of
this series got wrong and why each point changed.

## Specifications

| Specification | Version | Sections implemented |
|---|---|---|
| vhost-user | QEMU `docs/interop/vhost-user.rst` at `1276e6c85dcc` (2026-09-11), sha256 `684b11b15330ee23f2922aab9abd116efa1f48eb15b1b02b0233418e1a224257` | "Message Specification" (header), "MMAP request", "VIRTIO Shared Memory Region configuration", "Back-end communication", "Protocol features", `VHOST_USER_GET_SHMEM_CONFIG`, `VHOST_USER_BACKEND_SHMEM_MAP`, `VHOST_USER_BACKEND_SHMEM_UNMAP`, "VHOST_USER_PROTOCOL_F_REPLY_ACK" |
| VIRTIO | 1.4 cs01 (oasis-tcs/virtio-spec `v1.4-cs01`, `917e900e0246`) | 2.2 "Feature Bits", "Shared Memory Regions" and "Addressing within regions", PCI "Shared memory capability" |

The vhost-user specification is not vendored in this repository. It was
fetched from `https://gitlab.com/qemu-project/qemu/-/raw/1276e6c85dcc/docs/interop/vhost-user.rst`.
`VHOST_USER_PROTOCOL_F_SHMEM` has been bit 22 since `588acb45c29f` added it.

## Crate versions

The `vhost` crate 0.16.0 that Cloud Hypervisor v53.0 pins defines
`VhostUserProtocolFeatures::SHMEM` as bit 21. The specification assigns bit 21
to `VHOST_USER_PROTOCOL_F_GPA_ADDRESSES` and bit 22 to SHMEM. `vhost` 0.17.0
corrects this (rust-vmm/vhost#367). Therefore:

- Patch 0001 adds a second `vhost` dependency, `vhost-frontend = { package =
  "vhost", version = "0.17.0" }`, and moves the vhost-user frontend
  (`virtio-devices/src/vhost_user/`: the block, fs, net and generic devices)
  to it. Only the generic device's offered features change. vDPA and vhost-kern code stay on
  `vhost` 0.16: they pass `vm-memory` 0.17 types, and `vhost` 0.17 uses
  `vm-memory` 0.18. The standalone `vhost_user_block` and `vhost_user_net`
  backends keep `vhost` 0.16 and `vhost-user-backend` 0.22. `Cargo.lock` gains
  `vhost` 0.17.0 and `vm-memory` 0.18.0; the other lock changes are the
  resulting version qualifiers.
- Leandro's `vhost-user-nvrm` uses `vhost` 0.17, `vhost-user-backend` 0.23,
  `virtio-queue` 0.18 and `vm-memory` 0.18.

Both ends must be updated together: a 0.16 peer and a 0.17 peer never
negotiate SHMEM (see [Compatibility](#compatibility)).

## Message formats

All numbers are in the host's native byte order (little-endian on x86-64).

### Header (12 bytes, every message)

| Offset | Size | Field | Value |
|---:|---:|---|---|
| 0 | 4 | request | message id |
| 4 | 4 | flags | bits 0-1 version (`0x1`), bit 2 `REPLY` (`0x4`), bit 3 `NEED_REPLY` (`0x8`) |
| 8 | 4 | size | payload bytes |

### Messages this series uses

| Channel | Message | Id | Request payload | Reply |
|---|---|---:|---|---|
| main (frontend to backend) | `VHOST_USER_GET_PROTOCOL_FEATURES` | 15 | none | u64 feature mask |
| main | `VHOST_USER_SET_PROTOCOL_FEATURES` | 16 | u64 feature mask | u64 status if `NEED_REPLY` |
| main | `VHOST_USER_SET_BACKEND_REQ_FD` | 21 | none, one fd in `SCM_RIGHTS` | u64 status if `NEED_REPLY` |
| main | `VHOST_USER_GET_SHMEM_CONFIG` | 44 | none | `VhostUserShMemConfig` (2056 bytes) |
| backend (backend to frontend) | `VHOST_USER_BACKEND_SHMEM_MAP` | 9 | `VhostUserMMap` (40 bytes), one fd in `SCM_RIGHTS` | u64: 0 success, non-zero failure; only with `REPLY_ACK` negotiated and `NEED_REPLY` set |
| backend | `VHOST_USER_BACKEND_SHMEM_UNMAP` | 10 | `VhostUserMMap` (40 bytes) | as SHMEM_MAP |

### `VhostUserShMemConfig` (GET_SHMEM_CONFIG reply, 2056 bytes)

| Offset | Size | Field | Meaning |
|---:|---:|---|---|
| 0 | 4 | nregions | number of non-zero entries in `memory_sizes`; at most 256 |
| 4 | 4 | padding | |
| 8 + 8 * shmid | 8 | memory_sizes[shmid] | size of region `shmid` in bytes; 0 = shmid unused; otherwise a multiple of the host page size |

The array index is the shmid. `nregions` is a count, not a prefix length: one
region with shmid 1 is `nregions = 1`, `memory_sizes = [0, S, 0, ...]`.
The configuration is valid for the lifetime of the connection.

### `VhostUserMMap` (SHMEM_MAP and SHMEM_UNMAP payload, 40 bytes)

| Offset | Size | Field | Meaning |
|---:|---:|---|---|
| 0 | 1 | shmid | region to map into |
| 1 | 7 | padding | |
| 8 | 8 | fd_offset | offset into the passed fd (SHMEM_MAP only); page aligned |
| 16 | 8 | shm_offset | offset from the start of region `shmid`; page aligned |
| 24 | 8 | len | bytes; non-zero, page aligned |
| 32 | 8 | flags | bit 0: 1 = read-write, 0 = read-only; other bits must be 0 |

## Patch 0001: VIRTIO Shared Memory Regions

### Protocol features

| Bit | Mask | Name | Offered by the generic device | Why |
|---:|---|---|---|---|
| 3 | `0x8` | `REPLY_ACK` | yes (already in v53.0) | replies to backend requests |
| 5 | `0x20` | `BACKEND_REQ` | yes (already in v53.0) | the backend channel |
| 10 | `0x400` | `BACKEND_SEND_FD` | **added** | "Back-end communication": fds on the backend channel require it; SHMEM_MAP carries one |
| 21 | `0x20_0000` | `GPA_ADDRESSES` | no | not implemented. The previous series offered this bit under the name SHMEM |
| 22 | `0x40_0000` | `SHMEM` | **added** | GET_SHMEM_CONFIG, SHMEM_MAP, SHMEM_UNMAP |

The existing offers (`CONFIG`, `MQ`, `CONFIGURE_MEM_SLOTS`, `INFLIGHT_SHMFD`,
`LOG_SHMFD`, `DEVICE_STATE`) are unchanged.

### Handshake

The order is that of `GenericVhostUser::new` and activation in Cloud
Hypervisor. Numbers are message ids.

1. `SET_OWNER` (3).
2. `GET_FEATURES` (1). The backend must advertise
   `VHOST_USER_F_PROTOCOL_FEATURES` (bit 30) for the next steps.
3. `GET_PROTOCOL_FEATURES` (15). acked = offered & advertised.
4. `SET_PROTOCOL_FEATURES` (16) with the acked set. From here the frontend
   sets `NEED_REPLY` on its requests if `REPLY_ACK` was acked.
5. `GET_QUEUE_NUM` (17), only if `MQ` was acked.
6. `GET_SHMEM_CONFIG` (44), only if `SHMEM` was acked. The frontend validates
   the reply: `nregions <= 256`, `nregions` equal to the number of non-zero
   sizes, every size a multiple of the page size. A violation fails device
   creation with `Invalid shared memory config from the backend: ...`.
7. `device_manager` backs the regions (below), before the PCI BARs are laid out.
8. Guest driver activation: `SET_FEATURES` (2), memory table, vrings, then
   `SET_BACKEND_REQ_FD` (21) with a fresh backend channel if `BACKEND_REQ`
   was acked. Every activation creates a new channel.
9. The backend sends `SHMEM_MAP` (9) and `SHMEM_UNMAP` (10) on that channel at
   any time while the device is active.

### Regions and the PCI BAR

- One host window backs all regions. Regions sit back to back in shmid
  order; unused shmids take no space. The window is rounded up to a power of
  two (a BAR size) and allocated from the segment's 64-bit MMIO space with
  natural alignment.
- The window is an anonymous `PROT_NONE`, `MAP_NORESERVE` mapping registered
  as a KVM userspace mapping. Unmapped window space is inaccessible.
- The device's region list is indexed by shmid; unused shmids are zero-length
  entries. `pci_device` emits one `VIRTIO_PCI_CAP_SHARED_MEMORY_CFG`
  capability per non-empty entry, with `cap.id = shmid`, `bar = 2`, and the
  region's offset and length within the BAR. A guest driver finds region `n`
  with `virtio_get_shm_region(vdev, &region, n)`.

### SHMEM_MAP

The frontend refuses the request (non-zero reply, mapping unchanged) when:

| Condition | errno in the reply |
|---|---|
| `BACKEND_SEND_FD` not negotiated | `EINVAL` |
| the device has no regions | `EINVAL` |
| `shmid` names no region, or an unused shmid | `EINVAL` |
| `len == 0`, or `shm_offset`, `len` or `fd_offset` not page aligned | `EINVAL` |
| `shm_offset + len` beyond the region | `EINVAL` |
| the range overlaps an existing mapping of that region | `EBUSY` |
| `mmap` fails | its errno |

Checks the `vhost` crate makes before the handler runs, and that end the
channel instead of replying: exactly one fd attached, payload size 40, a known
flag bit, `len != 0`, no overflow of `fd_offset + len` or `shm_offset + len`.

Otherwise the frontend maps the fd with `MAP_SHARED | MAP_FIXED` at
window + region offset + `shm_offset`, `PROT_READ | PROT_WRITE` if flags bit 0
is set, else `PROT_READ`, records the mapping, and replies 0. A failed `mmap`
restores the `PROT_NONE` placeholder over the range.

### SHMEM_UNMAP

The range must equal one earlier SHMEM_MAP of the same shmid (same
`shm_offset` and `len`); otherwise the reply is `EINVAL` and nothing changes.
The same region, alignment and bounds checks apply. On success the frontend
lays a `PROT_NONE` placeholder over the range (rather than leaving a hole in
the window), forgets the mapping and replies 0.

### Reset, snapshot, reconnect

- Device reset: after the backend worker has been joined, every mapping is
  replaced by the placeholder and forgotten ("mappings are automatically
  unmapped by the front-end across device reset operation").
- Snapshot and live migration: refused for a device with regions. The
  mappings are backend state that no migration stream carries.
- Backend reconnect: the window layout is kept and existing mappings are not
  dropped. A new backend process that reuses offsets gets `EBUSY`. Leandro's
  backend exits when the VMM hangs up, so this does not arise in its use.

### Replies

The `vhost` crate's `FrontendReqHandler` sends the reply: 0 on success, the
negated errno as u64 on failure, only when `REPLY_ACK` was negotiated and the
request has `NEED_REPLY` set. Without a reply the backend cannot know the
outcome; backends that map memory should negotiate `REPLY_ACK`.

## Patch 0002: device feature bits

`GET_FEATURES` returns a 64-bit mask. The generic device offers
`DEFAULT_VIRTIO_FEATURES` (transport and vhost-user bits: 26, 28, 29, 30, 32,
35, 36, 38) plus:

| Bits | Mask | Reason |
|---|---|---|
| 0 to 23 | `0x0000_0000_00ff_ffff` | device specific (VIRTIO 1.4, 2.2) |
| 50 to 63 | `0xfffc_0000_0000_0000` | device specific (50 to 127), limited to the 64-bit word |

Bits 41 and 42 are not offered: VIRTIO 1.4 lists them as device specific only
because legacy virtio-net features use them, and its reserved-bit list also
defines bit 41 as `VIRTIO_F_ADMIN_VQ`, which this transport lacks.

Negotiation keeps `offered & backend features`, so only bits the backend
advertises reach the guest. `SET_FEATURES` at activation sends the bits the
guest acked, plus `VHOST_USER_F_PROTOCOL_FEATURES`. A backend that advertises
no device-specific bits negotiates exactly what it did before.

## Patch 0003: refused backend requests

`FrontendReqHandler::handle_request` returns:

| Result | Meaning | Worker |
|---|---|---|
| `Ok` | request handled, reply 0 sent if asked for | continues |
| `Err(ReqHandlerError)` | the handler refused the request; the non-zero reply was sent if asked for, before returning | **continues** (was: ended) |
| any other `Err` | socket broken, stream out of step, malformed or unknown request | ends, device marked disconnected (unchanged) |

The specification lets SHMEM_MAP fail ("requests shall fail" on overlap, "can
fail when there are no resources available"), so a refusal is an outcome of
the request, not a channel failure. The change applies to every vhost-user
device's backend channel; only devices whose handler can refuse (the generic
device's SHMEM handlers, and config-change signalling) are affected.

## How Leandro uses the series

| Item | virtio-nvrm |
|---|---|
| Device | `--generic-vhost-user "device_type=60,socket=...,queue_sizes=[256,256]"` |
| Virtio features | `VERSION_1` (32), `RING_INDIRECT_DESC` (28), `VHOST_USER_F_PROTOCOL_FEATURES` (30); no device-specific bits, so 0002 changes nothing for it |
| Protocol features | `REPLY_ACK`, `BACKEND_REQ`, `BACKEND_SEND_FD`, `SHMEM` |
| GET_SHMEM_CONFIG | `nregions = 1`, `memory_sizes[1] = 8 GiB` (`HOST_VISIBLE_SIZE`); shmid 0 unused |
| Guest | `virtio_get_shm_region(vdev, &shm, 1)`; the module allocates page slots in the region and sends their region-relative offset in `MAP_PREPARE` |
| SHMEM_MAP | shmid 1, `fd_offset = 0`, `shm_offset` = guest-chosen offset, `len` = page-rounded RM mapping size, flags `1` (read-write); the fd is the RM mapping file. The backend checks alignment, bounds and overlap itself before sending |
| SHMEM_UNMAP | shmid 1, the same `shm_offset` and `len`, flags 0. The backend frees the slot only after reply 0 |
| Refusal | the backend reports `EIO` to the guest request and keeps the device; `LEA_TEST_SHMEM_MAP_OOB=1` injects one out-of-region SHMEM_MAP to exercise this path |
| Device reset | the backend forgets its window table when the next activation sends `SET_BACKEND_REQ_FD` |

Code: `crates/vhost-user-nvrm/src/nvrm.rs` (`shmem_config`,
`window_request`, `protocol_features`, `set_backend_req_fd`,
`on_map_prepare`, `release_window`) and
`guest-module/virtio_nvrm/virtio_nvrm.c` (`NVRM_SHM_ID_HOST_VISIBLE`).

## Compatibility

There is no transition switch. An old `vhost-user-nvrm` (vhost 0.16)
advertises SHMEM as bit 21, which the patched frontend does not offer, so the
device comes up without a window. The guest module logs
`virtio_nvrm: no host-visible window -- mmap will fail`. A new backend with an
old Cloud Hypervisor build fails the same way. Rebuild both.

A `vendor/cloud-hypervisor` tree that carries the previous series is modified
in a way `tools/build.sh ch` does not recognise as a prefix of the new series;
it stops and asks for a reset. Reset and rebuild:

```sh
git -C vendor/cloud-hypervisor checkout -- .
tools/build.sh ch
```

## Verification

### Build and unit tests

```sh
git clone --depth 1 --branch "$(cat CH_VERSION)" \
  https://github.com/cloud-hypervisor/cloud-hypervisor.git /tmp/ch
for patch in patches/[0-9][0-9][0-9][0-9]-*.patch; do git -C /tmp/ch apply "$patch" || exit 1; done
cargo build --locked --release --manifest-path /tmp/ch/Cargo.toml --bin cloud-hypervisor
cargo test --locked --manifest-path /tmp/ch/Cargo.toml -p virtio-devices
tools/check.sh
```

Recorded 2026-09-23 on the rewritten series: the three patches apply to
v53.0 with `git apply` and `git am`; the release build succeeds;
`cargo test -p virtio-devices` passes 112 tests, 10 of them new (v53.0 has 102):

| Test (virtio-devices) | Checks |
|---|---|
| `generic_vhost_user::tests::shmem_config_is_indexed_by_shmid` | index = shmid, `nregions` as a count, trailing unused ids dropped |
| `generic_vhost_user::tests::shmem_config_is_validated` | prefix-style `nregions`, count mismatch, unaligned size, `nregions > 256` rejected |
| `generic_vhost_user::tests::map_offsets_are_relative_to_the_region` | a mapping lands at region start + `shm_offset`, not window + `shm_offset` |
| `generic_vhost_user::tests::map_refuses_what_the_specification_does_not_allow` | unused and unknown shmid, out of region, unaligned offset, length and fd offset, overlap (`EBUSY`), adjacent mapping allowed |
| `generic_vhost_user::tests::unmap_needs_exactly_one_mapping` | partial and unknown ranges refused, double unmap refused, remap after unmap |
| `generic_vhost_user::tests::reset_drops_every_mapping` | reset clears all mappings |
| `generic_vhost_user::tests::backend_requests_get_a_zero_or_non_zero_reply` | over a real backend channel with `REPLY_ACK`: map 0, overlapping map non-zero, partial unmap non-zero, unmap 0 |
| `generic_vhost_user::tests::map_needs_backend_send_fd` | SHMEM_MAP refused without `BACKEND_SEND_FD` |
| `generic_vhost_user::tests::device_features_are_the_device_specific_bits` | exactly bits 0-23 and 50-63, disjoint from the default set |
| `vhost_user::tests::only_a_refused_backend_request_keeps_the_worker` | `ReqHandlerError` continues; `InvalidMessage` and `SocketBroken` end the worker |

`nix build .#cloud-hypervisor` builds the patched tree (the package applies
the series as `cargoPatches`, since 0001 changes `Cargo.lock`). `cargo clippy
-p virtio-devices --all-targets -D warnings` is clean; clippy on the `vmm`
crate reports one unfulfilled lint expectation at `vmm/src/lib.rs:775`, which
unpatched v53.0 reports with the same toolchain (Rust 1.89).

`vhost-user-nvrm` adds `nvrm::vhost_user_tests`: the protocol feature bits
against the specification's numbers, the byte layout of the
GET_SHMEM_CONFIG reply and of the SHMEM_MAP/UNMAP payloads, and the handshake
of the patched frontend (steps 1 to 8 above, without SET_FEATURES) against the
real `VhostUserDaemon`, followed by a SHMEM_MAP with an fd and a SHMEM_UNMAP
over the handed-over backend channel with `REPLY_ACK`.

### Manual VM check

Not run for this revision; a v1 VM was not available. After building both
ends, follow [QUICKSTART.md](../docs/QUICKSTART.md) sections 1 and 2 with the
new binaries, then:

1. Hypervisor: start one VM with `-v` added to the `"$CH"` command line. The
   hypervisor log shows `generic vhost-user _generic_vhost_user0: shared
   memory window 0x200000000 at 0x...` (8 GiB) and no `Invalid shared memory
   config` error. Without `-v` only warnings and errors are logged.
2. Guest: `sudo dmesg | grep virtio_nvrm` shows
   `virtio_nvrm: window 8192 MiB @0x...`, not `no host-visible window`.
   `sudo lspci -vv -d 1af4:` lists a vendor-specific capability for the shared
   memory region on BAR 2 of the virtio-nvrm device.
3. Mappings: `LD_LIBRARY_PATH=/opt/nvrm/lib nvidia-smi` on a compute guest, or
   the desktop start of an Ubuntu guest, succeeds. Its first CUDA or display
   mapping is a SHMEM_MAP of the GPU doorbell.
4. Refusal recovery: restart one backend with `LEA_TEST_SHMEM_MAP_OOB=1` and
   the VM. The backend log shows the injected request and
   `vhost-user-nvrm: SHMEM_MAP: ...`; the hypervisor log shows
   `shared memory request 0x200000000+... exceeds shmid 1` and `vhost-user
   backend request refused`; the guest's first mapping fails with `EIO`, and
   `nvidia-smi` run again succeeds.
5. Reset: in a guest with no NVIDIA client running, unload the guest module
   stack (`virtio_nvrm` last), which resets the device, load it again, and
   repeat step 3. With `LEA_DEBUG=1` the backend logs `new backend channel:
   N window mapping(s) dropped by the device reset` when N mappings were live.
6. A second VM on the same host behaves the same way.

## Upstreaming

- The supported build target is the pinned tag. Before submission, rebase each
  patch onto the intended upstream revision and retest; upstream may already
  have moved to `vhost` 0.17, which removes the `vhost-frontend` alias.
- Submit the three changes separately. Describe the generic device behaviour
  and include a reproducer; GPU use is one application.
