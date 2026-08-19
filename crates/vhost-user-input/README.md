<!-- SPDX-License-Identifier: MIT -->
# `vhost-user-input` — virtio-input as an external backend

cloud-hypervisor has no virtio-input of its own, but
`--generic-vhost-user` does know `b"input" => VIRTIO_ID_INPUT` — so the
device can live behind a socket like every other one here. Why nothing
off the shelf could supply that end (upstream crosvm's standalone
devices stop short of input; crosvm is not part of this tree), what the
two queues carry, and the two-source design are all in the crate docs,
`src/lib.rs` — this README is the map, not a second copy.

| File | What it is |
|---|---|
| `src/lib.rs` | the device: config space, `eventq`/`statusq`, the evdev and fifo sources |
| `src/main.rs` | argument handling and the vhost-user daemon loop |

## Running

    vhost-user-input --socket <path> [--name <label>] --evdev <device>
    vhost-user-input --socket <path> [--name <label>] --fifo <path>

`--evdev` forwards a real host input device verbatim; `--fifo` reads
`type code value` lines. `--fifo` is scriptable, so a test can press a
key without a human and without synthesising input into somebody's live
desktop session — which is what lets a gate prove anything here.
