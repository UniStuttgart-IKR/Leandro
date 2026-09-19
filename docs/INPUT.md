<!-- SPDX-License-Identifier: MIT -->
# Input

Leandro uses upstream [rust-vmm vhost-device-input](https://github.com/rust-vmm/vhost-device/tree/main/vhost-device-input)
for optional host evdev forwarding. The former local `vhost-user-input` crate
has been removed. GPU forwarding does not depend on an input backend.

## Desktop streaming

Moonlight sends keyboard/mouse input to Sunshine inside the guest. Sunshine
uses guest `/dev/uinput`; no host input backend is needed. The
[Ubuntu guide](UBUNTU-DESKTOP.md) configures this path.

## Host evdev forwarding

Build the pinned upstream revision, using either Cargo or Nix:

```sh
cargo install --git https://github.com/rust-vmm/vhost-device.git \
  --rev 93f867e1b00061d425686e4faa5f2ca40125f18c --locked vhost-device-input
# Alternatively:
nix build .#vhost-device-input
```

Run one source per backend for a single input device:

```sh
vhost-device-input --socket-path /path/input.sock --event-list /dev/input/eventN
```

Add to Cloud Hypervisor's arguments:

```text
--memory size=8G,shared=on
--generic-vhost-user 'device_type=input,socket=/path/input.sock0,queue_sizes=[256,256]'
```

- Upstream appends `0` to the socket prefix for the first event device.
  Multiple comma-separated sources produce separate sockets/devices.
- Linux guests use their standard `virtio_input` driver. Queue 0 carries events;
  queue 1 carries device status such as keyboard LEDs.
- Give the backend read access to the selected event device. Use dedicated socket
  paths and restrict access. Stop the VM before terminating its backend.
- Forward only devices intended for the guest. Opening an evdev device alone
  does not make it exclusive to the VM.
- The pinned Nix package builds unmodified upstream source. The host Debian
  package includes its static binary and license notices.

- Upstream `main` checked on 2026-09-19: `93f867e1b00061d425686e4faa5f2ca40125f18c`.
- Published release 0.1.0 panics on one-byte configuration writes from Cloud
  Hypervisor. The pinned upstream revision includes the fix; no local patch is used.

## Test input

- Upstream accepts evdev devices, not the old `type code value` text FIFO.
- Automated tests can create a dedicated uinput device and pass its evdev node.
  Mark it `LIBINPUT_IGNORE_DEVICE=1` in a host udev rule before creating it, so
  synthetic test events do not reach the host desktop.
- Verify the event in the guest; a listening backend socket alone proves no delivery.
- Older MeisterStack input drivers use the removed CLI/FIFO format and need migration.
  Keep their existing pinned core version until the driver is updated.

## Recorded validation

- On 2026-09-19, the pinned revision delivered F24 press, sync, release and sync
  through patched Cloud Hypervisor v53.0 to an Ubuntu guest.
- MeisterStack `f83cd70` also passed driver-managed guest delivery and backend/socket cleanup using Leandro-Test. Existing Test pins remain unchanged.
- All 10 upstream unit tests pass in Nix; the test mock needs pollable stdin,
  supplied by the package recipe. No source patch or test exclusion is applied.
- The static host package builds with upstream input and its license notices.
- This checks the tested keyboard path, not every evdev device or disconnect case.
