<!-- SPDX-License-Identifier: MIT -->
# `vhost-user-nvrm` — the host daemon

The far end of the guest kernel module. A **pure forwarder and translator**:
it takes RM ioctls out of a virtqueue, makes them valid in host terms, and
runs them on the real driver. It holds no RM client of its own — the guest
allocates its own `ROOT_CLIENT` through the forwarded `RM_ALLOC`, and every
call runs on exactly the open file description the guest used
(`src/mirror.rs` says why nothing else works). Exactly one transport:
virtio-nvrm (`--nvrm <socket>`), guest driver `virtio_nvrm.ko`.

Each module carries its own reasoning in its header — this README is the
map, not a second copy:

| File | What it is |
|---|---|
| `src/main.rs` | the daemon: argument handling, signal reporting, startup |
| `src/nvrm.rs` | the virtio-nvrm device side — the counterpart to `virtio_nvrm.ko`; the host-visible window and its sizing history |
| `src/session.rs` | one guest session: `prepare()`/`execute()` over every forwarded call, checked against a possibly lying guest |
| `src/syscalls.rs` | the seam between **deciding** and **doing**: the only three syscalls, behind a trait — what makes the guest-lies tests possible |
| `src/mirror.rs` | `token → real device fd`; the OFD argument |
| `src/host_pool.rs` | attach guest pages to a GPU VA without an mmap on the host's uvm fd; the pin limit (`LEA_MAX_PIN_MIB`) |
| `src/vram.rs` | the per-VM VRAM ledger and cap, and the controls rewritten on the way back |
| `src/waiters.rs` | semaphore-surface waiter poller: one thread, `poll(2)`, deliberately not epoll — the registration race is why |
| `src/guest_words.rs` | guest-supplied numbers as their own types — arithmetic exists only checked |
| `fuzz/` | `cargo-fuzz` target over `handle_msg`, corpus committed; the replay runs in every `cargo test` |

## What the guest sees of its card

`vram.rs` rewrites five controls on the return path (the table with
command numbers and conditions is in its header): the process list and
per-process memory always become this VM's own view — measured, the
host's table had already crossed the boundary before this existed — the
two `FB_GET_INFO` doors report the capped card under a cap, and the GPU
name always reads `Leandro <model>` (the `-2G` profile suffix joins it
under a cap; `LEA_GPU_NAME_RAW=1` keeps the driver's string).

## Running

    vhost-user-nvrm --nvrm /path/to/socket

It must not be killed under a running guest. A guest then waits forever
on a host that is gone, and the first visible symptom appears somewhere
else entirely. `scripts/lib/common.sh` has the pidfile discipline that
keeps gates from doing this to each other.
