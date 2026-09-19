<!-- SPDX-License-Identifier: MIT -->
# Showcase

- Start a desktop and compute guest on one host GPU, then validate streaming.
- Reference hardware: GeForce RTX 2070, Turing (`sm_75`).
- The host retains the GPU; no passthrough, IOMMU assignment or second GPU is required.
- The commands use [Leandro-Test](../../Leandro-Test/README.md), checked out beside core. Each host block starts from the core checkout unless marked otherwise.
- Test coverage: [TESTING.md](TESTING.md). Display design: [DISPLAY.md](DISPLAY.md).

## 1. Prerequisites

| Host requirement | Purpose |
|---|---|
| Matching NVIDIA driver, `nvidia-smi`, host `libcuda.so.<version>` | GPU access and guest userspace |
| `git`, pinned Rust toolchain, `cc`, `make`, `pkg-config`, libclang | Build tools |
| `qemu-img`, `mkfs.vfat`, `mcopy` | Guest disks and cloud-init seed |
| `curl`, `ip`, `iptables`, `sudo` | Download and network setup |
| `/dev/kvm` | Run Cloud Hypervisor |
| `moonlight-qt` | Receive the desktop stream |

- Test `scripts/build.sh preflight` reports missing dependencies; core build tools remain in Leandro.
- Desktop guests need an image baked with `--with-desktop`; add `--with-steam`
  if needed. The bake uses the host's Sunshine version.
- The compute gate needs a native torch reference in `vendor/hostvenv`.
  `build.sh all` prepares it. To create it separately:

```sh
cd ../Leandro-Test
./scripts/build.sh hostvenv
```

- Enable persistence mode for comparisons. Historical measurements changed by
  up to 58% with it disabled.
- `make -C probe ptx` regenerates the committed PTX only when the card cannot
  run it; older architectures require `nvcc`.
- Cards without NVENC need `LEA_SUN_ENCODER=software`. Record the encoder used.

## 2. Build

```sh
# From Leandro, with the sibling Test checkout available:
./scripts/build.sh all
./scripts/check.sh
cd ../Leandro-Test
./scripts/build.sh preflight
./scripts/build.sh all
make -C probe ptx
```

- Start with the driver version in `DRIVER_VERSION`; preflight rejects a mismatch.
- Driver changes also require matching committed ABI bindings, Cargo features,
  vendor sources and guest userspace. See [nvrm-sys](../crates/nvrm-sys/README.md).
- Selecting a driver does not regenerate bindings or select Cargo features.
  Complete [the ABI workflow](abi-versions.md) before rebuilding the rig.
- The GPU-free check must pass before hardware testing. It does not establish
  that the hardware path works.

## 3. Prepare the rig

```sh
cd ../Leandro-Test
sudo nvidia-smi -pm 1
./scripts/showcase.sh state --check
./scripts/showcase.sh net up
```

- Save the `RIG` line with measurements: driver, persistence, governor,
  hypervisor, PCIe link and card.
- IP transport needs the bridge, taps and NAT rule. NixOS vsock guests do not.

## 4. Start two guests

```sh
cd ../Leandro-Test
./scripts/showcase.sh up --name desktop --index 5 --session gnome --with-steam --with-torch
./scripts/showcase.sh up --with-torch
./scripts/showcase.sh status
```

- `desktop` runs GNOME/Sunshine; `vm0` runs compute. Both use `virtio_nvrm`.
- Desktop guests additionally load NVIDIA display modules; neither loads the
  host RM driver `nvidia.ko`.
- `--with-torch` installs a missing guest venv, approximately 2.5 GiB.
  Baked fleet images can already contain it.
- `status` should report running instances and `MODULE loaded`.
- Interactive SSH prepares `LD_LIBRARY_PATH`, `NVPROBE_PTX` and the venv.
  Noninteractive test commands set their own environment.

```sh
cd ../Leandro-Test
./scripts/showcase.sh ssh --name desktop
```

- Inside that guest shell:

```sh
cd gpu
python rlprobe.py
# Alternative: ./nvprobe 3
```

## 5. Keep games on a separate disk

```sh
cd ../Leandro-Test
./scripts/showcase.sh games init
./scripts/showcase.sh up --name desktop --index 5 --session gnome --games-init
# Inside the guest: nvidia-run steam, log in, install the game.
./scripts/showcase.sh down --name desktop
./scripts/showcase.sh up --name desktop --index 5 --session gnome --games
./scripts/showcase.sh games status
```

- `games init` creates a sparse 200 GiB base disk. Ordinary guests use private
  overlays, so several guests can share installed games.
- The guest mounts it at `/games` and links Steam `steamapps` directories there.
- Existing nonempty libraries are retained; provisioning prints migration commands.
- `--fresh` replaces guest system state, not the shared games base.
- Test `scripts/guest/cs2-settings.sh` is staged into the guest; it selects low graphics settings
  and caps `fps_max` at the display rate.

## 6. Stream the desktop

```sh
cd ../Leandro-Test
./scripts/showcase.sh pair --name desktop
moonlight stream 192.168.100.15 Desktop --resolution 1920x1080 --fps 60 --bitrate 40000
```

- Pairing coordinates Moonlight's PIN with Sunshine's API. It detects an
  existing pairing and repairs missing Sunshine web credentials once.
- Read Sunshine's actual capture method and encoder from the startup output.
  The normal X11 setup reports:

```text
sunshine: capture=x11 encoder=nvenc
Info: Screencasting with X11
Info: Found H.264 encoder: h264_nvenc [nvenc]
```

### Wayland

```sh
cd ../Leandro-Test
./scripts/build.sh bake --with-desktop --desktop-session wayland --with-steam --set-default
LEA_SUN_CAPTURE=kms ./scripts/showcase.sh up --name desktop --index 5 --session gnome --wayland --fresh
./scripts/showcase.sh pair --name desktop
```

- Select `--wayland` explicitly; verify the guest session type after startup.
- The KMS configuration has measured moving content. See
  [OPEN-QUESTIONS.md](OPEN-QUESTIONS.md), items 17 and 35, for separate
  presentation and crash-regression evidence.
- Default `LEA_SUN_CAPTURE=auto` selects `portal` for Wayland. Unpatched
  Sunshine can stop at an interactive permission dialog.
- Set capture when starting the desktop; setting it only during pairing
  does not reconfigure an already-running Sunshine session.

## 7. Set resource limits

| Setting | Default | Scope |
|---|---|---|
| `LEA_VRAM_LIMIT_MIB` / `--vram-limit N` | Off | Guest-visible VRAM allowance per VM |
| `LEA_VRAM_PROFILE_MIB` / `--vram-profile N` | Off | Profile minus `LEA_VRAM_RESERVE_MIB` (default 256 MiB) |
| Guest `max_pin_mib` / `--max-pin-mib N` | 1024 MiB | Cumulative production pin/pool quota |
| Backend `LEA_MAX_PIN_MIB=N` | 256 MiB | One pinned arena |
| Backend `LEA_MAX_PIN_TOTAL_MIB=N` | 1024 MiB, provisional | VM-wide page-rounded registrations, including retained records |

- VRAM limit and profile are mutually exclusive. A profile reserves space
  for driver overhead; it does not measure or strictly bound total card cost.
- Profiles may overcommit the card. The launcher warns when it can detect this.
- Measurements showed VRAM quota recovery after frees. They also found
  unaccounted overhead: [two-game run](measurements/sottr-4q/README.md).
- Set pin limits together with guest RAM. Defaults are 16 GiB for desktop
  guests (`LEA_DESKTOP_MEM`) and 4 GiB for compute (`LEA_MEM`).
- Host registration charges can remain after source frees; they are not a count
  of unique pinned pages. Repeated workloads need budget-growth validation.
- Raising the guest quota alone does not raise either host limit.
  A 512 MiB registration failed at the default backend cap and passed with
  `LEA_MAX_PIN_MIB=1024` (2026-08-20; [llm.md](llm.md)).
- Excessive pinning can exhaust guest RAM even with all limits raised.

```sh
cd ../Leandro-Test
LEA_MAX_PIN_MIB=2048 LEA_MAX_PIN_TOTAL_MIB=8192 ./scripts/showcase.sh up --name desktop --index 5 \
    --session gnome --games --max-pin-mib 4096
```

## 8. Troubleshooting

| Symptom | Check or action |
|---|---|
| Black stream | Inspect Sunshine capture. NvFBC failed on the measured GeForce setup; restart with explicit X11 or the tested Wayland/KMS configuration. |
| Steam Big Picture does nothing | Check Steam is installed. Stream `Desktop`, or bake with `--with-steam`. |
| Pairing timeout | Run `showcase.sh pair --name desktop`; it repairs the missing-login `/welcome` redirect once. |
| Swapchain creation fails after extended use | Check backend logs for `EMFILE`. Launcher FD limit defaults to 65536; override with `LEA_NOFILE`. |
| Backend driver mismatch | Check the selected ABI, features and binary build. Rebuild after completing the driver workflow. |
| `Driver/library version mismatch` | Loaded kernel driver and NVIDIA userspace differ. Reboot after a driver update. |
| Persistence mode off | Run `sudo nvidia-smi -pm 1`. |
| Probe JIT failure on an older GPU | Run `make -C probe ptx` with a compatible CUDA toolkit. |
| Compute gate reports missing host venv | Prepare the native torch reference in section 1. |
| `--with-torch` refused on vsock | Use a base image that already contains torch; the guest has no network interface. |
| Disconnected virtual monitor after compositor crash | Stop clients and reload the display modules; OPEN-QUESTIONS 16 tracks this issue. |

- After changing driver versions, refresh the guest display modules as well:
  `./scripts/test.sh vdisplay --fresh`.
- Do not unbind the virtio device with open GPU files or mappings.

## 9. Validate and stop

```sh
cd ../Leandro-Test
./scripts/showcase.sh demo --fast --pause
./scripts/showcase.sh down --all
./lea acceptance gpu vdisplay display
./scripts/showcase.sh down --all
./scripts/showcase.sh clean --dry-run
```

- Stop the demonstration before running acceptance; the gates own their VM instances and need an idle rig.
- `lea acceptance` runs all three full gates; `lea gate` is the separate MeisterStack smoke check.
- On 2026-09-19, all three gates passed from Leandro-Test after the old core harness was removed.
- Inspect `clean --dry-run` after a crash before running `clean` without it.
  Cleanup refuses while an instance is running.
- Recorded demonstrations cover unmodified CUDA, desktop/compute sharing,
  accounting recovery and module-unload protection while files are open.
- They do not establish hostile-guest isolation, live hot-unplug safety or
  performance parity. Report workload, limits and observed output with results.
- Known limitations and original measurements: [OPEN-QUESTIONS.md](OPEN-QUESTIONS.md).
