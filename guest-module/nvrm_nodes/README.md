<!-- SPDX-License-Identifier: GPL-2.0-only -->
# `nvrm_nodes` — the guest-side crutch remover

A small guest kernel module that replaces three crutches a pure user-space
setup needed:

1. `mknod /dev/nvidiactl|nvidia0|nvidia-uvm` → **real** character devices
   with the correct majors (195/235). devtmpfs creates the nodes itself.
2. A bind mount over `/proc/devices` → unnecessary, because a real chrdev
   registration shows up there anyway.
3. A tmpfs over `/proc/driver` plus a copied `params` →
   `/proc/driver/nvidia/params` comes from the module, filled from the
   real host file (lockstep rule: never guess).

Today it runs **beside** [`virtio_nvrm`](../virtio_nvrm), not instead of
it: loaded with `create_nodes=0`, it keeps `/proc/driver/nvidia/params`
while `virtio_nvrm.ko` owns the device nodes and the forwarding. The whole
`SET_PROC` machinery including `nvrm-nodes-tool` stays here — neither
duplicated nor moved. Rationale:
[`../../docs/OPEN-QUESTIONS.md`](../../docs/OPEN-QUESTIONS.md) nr 2.

| File | What it is |
|---|---|
| `nvrm_nodes_main.c` | the module |
| `nvrm-nodes-tool.c` | provisioning and self-test |
| `nvrm_nodes_uapi.h` | the ioctl interface, shared between module and tool |

## The tool

    nvrm-nodes-tool version
    nvrm-nodes-tool provision <name> <file>    # e.g. params params.txt
    nvrm-nodes-tool gpa <MiB> [hold seconds]   # check VA2GPA against pagemap

`provision` needs root (`CAP_SYS_ADMIN`). `gpa` explicitly does **not** —
that is the entire point of the module.

## Building

    make -C ~/guest-module/nvrm_nodes

## `NVRM_NODES_IOC_VA2GPA`, and what it is for today

In-kernel VA→GPA (guest-virtual to guest-physical) resolution was the
reason this module existed while a user-space shim carried the calls: no
root in the caller, pages genuinely held against migration. Today
`virtio_nvrm.ko` pins pages itself on the OS-descriptor path, so the
production data path never issues this ioctl; it stays as a wire-level
interface with a self-test (`nvrm-nodes-tool gpa`). The module's header
(`nvrm_nodes_main.c`) carries the full history.
