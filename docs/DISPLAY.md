<!-- SPDX-License-Identifier: MIT -->
# Display

- `virtio_nvrm.ko` exposes NVIDIA's displayless class and generates an EDID.
- Guest `nvidia-modeset`/`nvidia-drm` use its kernel RM interface.
- Guest hrtimers supply vblank timing; callback addresses remain in the guest.
- Queue 1 returns RM events for fence wakeups. See [Architecture](ARCHITECTURE.md).
- Setup: [Ubuntu installation](UBUNTU-DESKTOP.md) and [two desktop windows](QUICKSTART.md).

## Capture

- X11 recipe: GNOME/Xorg, Sunshine `capture=x11`, `encoder=nvenc`.
- [Wayland recipe](WAYLAND.md): GNOME/Wayland, Sunshine `capture=kms`, `encoder=nvenc`.
- Confirm Sunshine's selected capture method and encoder in its log.
- NvFBC produced black streams on the measured GeForce setup.
- Wayland portal capture may require an interactive permission grant.
- 2026-09-19: native GNOME/Wayland rendering and KMS/NVENC delivery passed a short client-frame check. See [tested configuration](WAYLAND.md#tested).
- Historical Wayland/KMS tests observed changing frames with `glmark2-wayland`.
  A separate 19m25s run on 2026-08-21 avoided earlier Xwayland/EGLImage failures
  but did not confirm moving game frames. See issues 17 and 35 in
  [Known issues](OPEN-QUESTIONS.md).

## Module parameters

| Parameter | Meaning / default |
|---|---|
| `display` | Enable kernel RM display operations |
| `vdisplay` | Enable the virtual display |
| `vdisplay_width`, `vdisplay_height` | Requested size; 1920×1080 |
| `vdisplay_vblank_hz` | Requested refresh; 60 Hz |
| `vdisplay_max_width`, `vdisplay_max_height`, `vdisplay_max_pixels` | NVKMS ceilings; 2560, 1600, 4096000 |
| `display_reserve_mib` | Display balloon; `-1` automatic, `0` disabled, positive MiB fixed |
| `stat_vblank_fired`, `stat_events_*` | Vblank and event counters |

- EDID generation clamps field widths; vblank uses the same effective refresh.
- Automatic display reserve uses scanout/cursor sizing from `nvrm_vram.c`.
  It is separate from the host VRAM limit/profile.
- Arithmetic/EDID tests at larger sizes do not prove presentation or capture works.

## Validation

- `tools/check.sh` checks software and EDID conformance.
- `tests/tools/` contains the EDID/frame tools and deterministic reference frame.
- A virtual-display test must verify modeset and pixel readback without a desktop.
- A desktop test must verify presentation, event latency, capture and received frames.
- Check event/vblank counters and kernel logs. A client's FPS counter proves no
  visible or received content.
- [Testing](TESTING.md) records automated gate coverage and the 2026-09-19 result.
- Older two-game measurements: [sottr-4q](measurements/sottr-4q/README.md).

## Limits

- X11 capture adds CPU overhead; performance depends on workload and capture path.
- Concurrent swapchains can fail under GPU pressure. A historical fourth creation
  failed beside a loading CS2 process.
- `LEA_FRL_HZ` is opt-in. Issues 38–41 record its unregister fix and rate measurements.
- The physical display engine rejected head setup with `NV_ERR_INSUFFICIENT_PERMISSIONS`;
  `CAP_SYS_ADMIN` did not make it work. The displayless path is intentional (issue 25).
- Connector detection after abnormal compositor exit remains unresolved (issue 16).
  Stop GPU clients before reloading display modules.
- Live device unbind/hot-unplug is unsupported. Display success does not establish
  [hostile-guest isolation](SECURITY.md).
