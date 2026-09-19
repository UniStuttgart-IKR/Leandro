<!-- SPDX-License-Identifier: MIT -->
# vhost-user-input

- External virtio-input backend for cloud-hypervisor's generic vhost-user device.
- Offers an evdev source for a host device and a FIFO source for scripted input.
- Source parsing and reads: `src/source.rs`; virtio queues and config: `src/lib.rs`.
- `src/lib.rs` implements config space, event/status queues and sources.
- `src/main.rs` parses arguments and starts the daemon.

## Run

```sh
vhost-user-input --socket <path> [--name <label>] --evdev <device>
vhost-user-input --socket <path> [--name <label>] --fifo <path>
```

- `--evdev` forwards events from the selected host device.
- `--fifo` reads `type code value` lines, allowing automated input tests without
  sending events to the host desktop.
- The crate-level documentation describes queue handling and source behavior.
