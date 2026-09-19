<!-- SPDX-License-Identifier: MIT -->
# Display path

- `virtio_nvrm.ko` exposes NVIDIA's displayless class and generates an EDID.
- Guest `nvidia-modeset.ko` and `nvidia-drm.ko` use its kernel RM interface.
- Local hrtimer callbacks supply vblank timing. Guest kernel callback addresses
  never travel to the host.
- A second virtqueue carries RM events back to the guest for fence wakeups.
- See [ARCHITECTURE.md](ARCHITECTURE.md) for the transport and
  [SHOWCASE.md](SHOWCASE.md) for setup.

## Start an X11 desktop

```sh
cd ../Leandro-Test
./scripts/showcase.sh up --name desktop --index 5 --session gnome --with-steam
./scripts/showcase.sh pair --name desktop
moonlight stream 192.168.100.15 Desktop --resolution 1920x1080 --fps 60 --bitrate 40000
```

- Requires a desktop image with GNOME, Xorg and Sunshine.
- Provisioning loads `virtio_nvrm` before the NVIDIA display modules.
- `--session openbox` starts a minimal desktop; `--display` starts only the rig X server.
- `--keep-vm` reuses the guest. Use `--fresh` when changing its base image or driver.
- Sunshine defaults to `LEA_SUN_CAPTURE=auto` and `LEA_SUN_ENCODER=nvenc`.
  Inspect its reported capture method and encoder.
- NvFBC produced black streams on the measured GeForce setup. The scripts
  configure capture explicitly; use `x11` for the Xorg desktop.

## Wayland

- Pass `--wayland` with `--session gnome` to select a Wayland desktop.
- Automatic capture selects `portal` when it detects the Wayland socket.
  Unpatched Sunshine may need an interactive portal permission grant.
- KMS capture has also been exercised. For that configuration, set
  `LEA_SUN_CAPTURE=kms` when starting the desktop. OPEN-QUESTIONS 17 records
  changing, nonzero frames through mmap, GL and CUDA readers with
  `glmark2-wayland`; its idle control remained static.
- The historical Xwayland failures in OPEN-QUESTIONS 22-C, 23, 32, 33, 35 and
  44 were closed after a 2026-08-21 GNOME Wayland/KMS run: 19m25s without the
  recorded crashes or EGLImage failures. That run did **not** confirm moving
  game frames. See [OPEN-QUESTIONS.md](OPEN-QUESTIONS.md), item 35 and its
  follow-up measurement, for the evidence and limits.

## Parameters

| Parameter | Meaning |
|---|---|
| `display` | Enable kernel RM display operations |
| `vdisplay` | Enable the virtual display |
| `vdisplay_width`, `vdisplay_height` | Requested geometry; defaults 1920x1080 |
| `vdisplay_vblank_hz` | Requested refresh; default 60 Hz |
| `vdisplay_max_width`, `vdisplay_max_height`, `vdisplay_max_pixels` | NVKMS allocation ceilings; defaults 2560, 1600, 4096000 |
| `display_reserve_mib` | Display balloon: automatic `-1`, disabled `0`, or fixed MiB |
| `stat_vblank_fired`, `stat_events_*` | Timing and event-channel counters |

- `nvrm_edid.c` clamps requested timings to EDID field widths. EDID and vblank
  pacing use the same effective refresh.
- `nvrm_vram.c` sizes an automatic display reserve from scanout buffers and
  cursor space. Its measured sizing inputs are recorded beside the formula.
- The display balloon is distinct from the host's per-VM VRAM limit/profile.
- Arithmetic checks at large sizes or rates do not establish end-to-end support.

## Validation

```sh
# From Leandro:
./scripts/check.sh
cd ../Leandro-Test
./lea acceptance vdisplay display
```

- Core `scripts/check.sh`: GPU-free C/Rust tests, generated ABI checks and EDID conformance.
- Test `lea acceptance`: full relocated hardware gates; `lea gate` is a narrower smoke check.
- `vdisplay`: virtual monitor setup and frame readback without X.
- `display`: desktop, presentation, event latency, capture and streaming.
- Canonical EDID/frame tools and the reference frame stay in core `tests/tools/`; Test builds or packages them from there.
- Run hardware gates sequentially on an available rig. See [TESTING.md](TESTING.md).
- On 2026-09-19, the relocated Leandro-Test runner passed virtual-display and desktop gates against the refactored core.

| Reader | Evidence |
|---|---|
| Test `scripts/guest/fencetime.c` | Fence wait latency; gate target below 1 ms |
| Test `scripts/guest/vkprobe.c` | Presentation, raytracing and queue/extension checks |
| Test `scripts/guest/fbprobe.c` | Framebuffer content and changes |
| `stat_events_*`, `stat_vblank_fired` | Event delivery and timing |
| `crates/nvrm-trace` | RM requests and replies |

- A client's FPS counter measures swaps; it does not prove displayed or received frames.
- RTX 2070 measurements include CS2 at 55–60 FPS, fence latency falling from
  10.10 to 0.12 ms, and a desktop sharing the card with PyTorch. These are
  workload-specific results, not performance guarantees; see [llm.md](llm.md).
- Two-game measurements: [sottr-4q](measurements/sottr-4q/README.md).

## Limits

- X11 capture adds CPU overhead. Historical 60 FPS measurements recorded Xorg
  at 24% and Sunshine at 53%; see [FUTURE.md](FUTURE.md) for capture work.
- Concurrent swapchain capacity depends on the workload; a fourth creation
  failed beside a loading CS2 in the original display measurements.
- The frame limiter remains opt-in (`LEA_FRL_HZ`). Its late-unregister race
  and later 30/60/120 Hz measurements are documented in OPEN-QUESTIONS 38–41.
- The displayless path is intentional. The physical display engine rejected
  head configuration with `NV_ERR_INSUFFICIENT_PERMISSIONS`; granting
  `CAP_SYS_ADMIN` did not produce a working alternative (OPEN-QUESTIONS 25).
- Connector detection after an abnormal compositor exit remains an open
  issue (OPEN-QUESTIONS 16). Reloading the display modules recovered the
  measured cases; stop GPU clients before doing so.
- Stop GPU clients before driver/device teardown. Live hot-unplug is unsupported.
- Successful display tests do not establish isolation against hostile guests.
