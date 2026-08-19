<!-- SPDX-License-Identifier: GPL-2.0-only -->
# `virtio_nvrm` — the guest kernel module

The driver that actually serves `/dev/nvidia*` inside the guest. The device
is called virtio-nvrm; its far end is
[`vhost-user-nvrm`](../../crates/vhost-user-nvrm).

What travels here is the **NVIDIA RM escape surface** (RM = the Resource
Manager, the ioctl interface of `nvidia.ko`) — not CUDA, which sits one
layer above. An unmodified application runs in the guest without
`LD_PRELOAD` because `/dev/nvidia*` are real character devices here, and
the module holds **no NVIDIA constant of its own**: the host sends a
descriptor table at startup and this code interprets it. The full
statement of scope — what "dumb on purpose" means, why the table keys on
`(device type, nr)`, what the display half is and is not — opens
`virtio_nvrm.c` itself; this README is the map.

| File | What it is |
|---|---|
| `virtio_nvrm.c` | the module: nodes, virtqueues, forwarding, the virtual display, event pump |
| `nvrm_tables.c` | the table interpreter — parse, check, look up |
| `nvrm_edid.c` | the EDID the module invents for the virtual display |
| `nvrm_wire.h` | **generated** from `crates/nvrm-wire`; never edit by hand |
| `nvrm_kapi.h` | the interface `nvidia-modeset.ko` expects from `nvidia.ko` — mirrored, not included |
| `test/tabcheck.c` | runs `nvrm_tables.c` in user space against the real stream |
| `test/edidcheck.c` | runs `nvrm_edid.c` in user space so `edid-decode` can read the bytes |
| `test/edidclamp.c` | drives `nvrm_edid_effective`'s clamp over a size/rate matrix (no `edid-decode`) |
| `test/tabreject.c` | runs `nvrm_tables.c`'s refusals over damaged copies of the stream |

`nvrm_tables.c` and `nvrm_edid.c` are each **one translation unit for two
worlds**, pulled in by `#include`: the kernel module and the user-space
test binary compile the *same* code. That is what makes `scripts/test.sh check`
able to diff reading against writing field by field with no guest kernel
and no VM in the loop.

## Building

Build inside the **guest** — that is where the kernel headers are:

    make -C ~/guest-module/virtio_nvrm

The Ubuntu guests build it against whatever kernel the pinned cloud
image (`GUEST_IMAGE`, noble) boots — a 6.8 series at the pinned serial,
and the kernel the measurements were taken on. `nix build .#guest-modules`
builds the same sources against nixpkgs' 6.12 LTS, the newest kernel they
are known to compile against; 6.15 and later are **not** done, and
[`nix/packages/guest-modules.nix`](../../nix/packages/guest-modules.nix)
says exactly what stands in the way.

`nvrm_wire.h` is generated but checked in, which is why the guest needs no
Rust toolchain. Anyone changing the wire structs regenerates it on the host
and checks it in with the change:

    cargo run --release --bin nvrm-genhdr -- guest-module/virtio_nvrm/nvrm_wire.h

Every offset in that header carries a `_Static_assert`, so the C side
cannot even be built against a wrong layout.

## Module parameters worth knowing

`create_nodes`, `gpu_count`, `display`, `vdisplay*` (the virtual display's
geometry and refresh), `bdf_mediation`, and a set of read-only `stat_*`
counters that are the module's own instrumentation — `stat_vblank_fired`,
`stat_events_delivered`, `stat_events_dropped` and its per-reason
breakdown, `stat_semsurf_waiters`, `stat_semsurf_fired`. Read them instead
of guessing; several findings in
[`../../docs/OPEN-QUESTIONS.md`](../../docs/OPEN-QUESTIONS.md) exist only because a
counter disagreed with a theory.

`max_pin_mib` caps **cumulative** pinned guest memory (default 1024 MiB).
The `stat_*` parameters expose the accounting that the GPU gate checks
balances back to zero.

## Two rules the measurements paid for

**Check the params pointer before looking up the class.** The first
version looked `hClass` up in the table *before* it looked at the params
pointer. `NV01_ROOT_CLIENT` (hClass 0) has no alloc params at all and
therefore stands in no table — `cuInit` got `EOPNOTSUPP` on the very first
alloc. `xlate::embedded_ptr` checks the pointer first; this module does
too. **The order is semantics here, not style.** Reversing it rebuilds the
bug.

**Guest pages for the UVM pool are charged, not simply allocated.**
`managedprobe` stage 3 asks for 9216 MiB of managed memory in a 4 GiB VM.
The UVM pool path allocates real guest pages, and with plain `GFP_USER` it
did so until the machine was empty: `oom_kill_process` ← `nvrm_node_mmap`.
The process shot was `managedprobe` itself, but it could have been anyone.
Now `__GFP_RETRY_MAYFAIL|__GFP_NOWARN` **plus** a quota (`nvrm_charge`,
capped by the module parameter `max_pin_mib`, which the error message
names). Asking for too much gets an honest `ENOMEM`; the neighbour is left
alone.

## Coexistence with `nvrm_nodes.ko`

`/proc/driver/nvidia` belongs to [`nvrm_nodes`](../nvrm_nodes) and is not
touched here. Both modules run side by side: `nvrm_nodes.ko` with
`create_nodes=0`, this module owning the nodes and the forwarding.
Rationale: [`../../docs/OPEN-QUESTIONS.md`](../../docs/OPEN-QUESTIONS.md) nr 2.

## Why `nvrm_tables.c` is its own translation unit

Because being `#include`d by two worlds is what tests it: the interpreter
would otherwise be **the largest untested surface in the system**, and the
test binaries in the table above run it without a guest kernel and without
a VM.

That is also why the file contains only `memcpy`, the wire structs and
error codes. Whoever allocates memory or prints diagnostics is the
**includer**, not this file — otherwise it could not be compiled into user
space.
