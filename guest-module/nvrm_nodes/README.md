<!-- SPDX-License-Identifier: GPL-2.0-only -->
# nvrm_nodes

- Supplies `/proc/driver/nvidia` from files copied from the host.
- Load with `create_nodes=0` beside [virtio_nvrm](../virtio_nvrm), which
  owns `/dev/nvidia*` and forwards calls.
- Optional placeholder nodes use NVIDIA majors 195/235 and return `ENODEV`.
- Exposes VA2GPA for the provisioning tool's memory-pinning self-test.
  Production forwarding uses virtio_nvrm's own pinning path.
- Coexistence rationale: [OPEN-QUESTIONS.md](../../docs/OPEN-QUESTIONS.md), item 2.

## Files

| File | Responsibility |
|---|---|
| `nvrm_nodes_main.c` | Module, proc files, pin records and ioctls |
| `nvrm-nodes-tool.c` | Provisioning and self-test |
| `nvrm_nodes_uapi.h` | Shared ioctl definitions |

## Build and use

```sh
make -C guest-module/nvrm_nodes
nvrm-nodes-tool version
nvrm-nodes-tool provision <name> <file>    # e.g. params params.txt
nvrm-nodes-tool gpa <MiB> [hold seconds]
```

- Run from the repository root with guest kernel headers installed; `KDIR`
  selects another header tree.
- `provision` requires `CAP_SYS_ADMIN`; `gpa` does not.
- Provision actual host data. Guest and host driver ABIs must match.

## VA2GPA contract

- Input address and length must be page-aligned; length must be nonzero.
- `FOLL_WRITE | FOLL_LONGTERM` resolves copy-on-write and pins physical pages.
- Pins remain until the file's last reference closes, including duplicated FDs.
- Insufficient output capacity returns `ENOSPC` and the required run count;
  no pages remain pinned from that call.
- `max_pin_mib` defaults to 1024 MiB **per call**. This legacy API has no
  cumulative quota across calls or FDs; it is not the production pinning API.
