<!-- SPDX-License-Identifier: GPL-2.0-only -->
# virtio_nvrm

- Guest driver for `/dev/nvidia*`; forwards RM open/ioctl/mmap calls to
  [vhost-user-nvrm](../../crates/vhost-user-nvrm).
- CUDA and graphics applications use the guest NVIDIA userspace unchanged.
- Guest and host NVIDIA driver ABIs must match.
- The host sends ioctl layouts through `GET_TABLES`. NVIDIA constants also
  enter through the generated `nvrm_wire.h`.
- `nvrm_nodes.ko`, loaded with `create_nodes=0`, supplies `/proc/driver/nvidia`.
- Architecture: [ARCHITECTURE.md](../../docs/ARCHITECTURE.md).

## Source map

| File | Responsibility |
|---|---|
| `virtio_nvrm.c` | Nodes, transport, forwarding, mappings, display and events |
| `nvrm_tables.c` | Table parsing and lookup |
| `nvrm_identity.h` | Session IDs, object keys and native reply validation |
| `nvrm_edid.c` | Virtual-display timings and EDID generation |
| `nvrm_vram.c` | Display reserve and balloon arithmetic |
| `nvrm_wire.h` | Generated wire and ABI definitions; do not edit |
| `nvrm_kapi.h` | Mirrored interface exported to `nvidia-modeset.ko` |
| `test/tabcheck.c` | Compare the C parser with Rust-generated tables |
| `test/tabreject.c` | Reject malformed tables, offsets and limits |
| `test/edidcheck.c`, `test/edidclamp.c` | EDID conformance and timing bounds |
| `test/vramcheck.c` | Display reserve and allocation policy |
| `test/identitycheck.c` | ID exhaustion, native replies and client separation |

- Production code and userspace tests share the tables, identity, EDID and VRAM helpers.
- Table keys include both device type and ioctl number; ctl and UVM reuse numbers.

## Build and check

From the repository root inside the guest, with matching kernel headers:

```sh
make -C guest-module/virtio_nvrm
```

From the repository root:

```sh
tools/ci/check-c.sh all
CC=clang tools/ci/check-c.sh all --sanitize
nix build .#guest-modules
```

- Ubuntu guests use the pinned image's 6.8 kernel series.
- The Nix package builds against 6.12 LTS. Newer kernel API changes are
  listed in [guest-modules.nix](../../nix/packages/guest-modules.nix).
- Kernel compilation and userspace tests do not validate GPU operation.
  Run the applicable [hardware gates](../../docs/TESTING.md) after behavior changes.
- The guest needs no Rust compiler. Regenerate the checked-in header on the host:

```sh
cargo run --locked --release --bin nvrm-genhdr -- guest-module/virtio_nvrm/nvrm_wire.h
```

## Parameters and invariants

- `create_nodes`, `gpu_count`: device-node registration.
- `display`, `vdisplay*`: kernel RM operations and virtual-display timing.
- `bdf_mediation`: present the GPU's guest PCI identity.
- `max_pin_mib`: cumulative user-pin, PRIME and UVM-pool quota, default 1024 MiB.
- `display_reserve_mib`: display balloon; `-1` automatic, `0` disabled,
  positive values select a fixed MiB reserve.
- `stat_*`: pin/pool accounting, events, vblanks and semaphore waiters.
- `stat_quarantined_pages`: charged pages retained after uncertain host completion.
  Guest restart clears quarantine; retained references prevent module unload.
- Pins and pools acquire their module reference before submission. Leftover
  fileless kernel backing may require device unbind before module unload.
- Check a null alloc-params pointer before looking up its class.
  `NV01_ROOT_CLIENT` has no params table entry.
- Charge UVM pool pages before allocation; quota failures return `ENOMEM`.
- A submitted ioctl must not restart after an ordinary signal; one-shot RM
  operations may already have completed on the host.
- Process-session IDs are monotonic within one module load and fail on
  exhaustion. Reloading the module requires a fresh backend session.
- Device state stays alive through process, request and mapping references.
  Removal stops submission, resets queues and wakes pending callers with `ENODEV`.
- Abandoned or malformed allocation replies retain backing under the quota.
  Recording a successful allocation precedes user-memory write-back.
- Normal source FREE/close still cannot prove the final lifetime of shared RM
  objects. DUP/export/import tracking and native release evidence remain needed.
- Normal pool-VMA close still needs coordination with native UVM backing lifetime.
- Stop clients and close mappings before removing the virtio device.
  Live unbind/hot-unplug remains unsupported: SHMEM BAR revocation and concurrent
  NVKMS teardown still need dedicated VM tests.
