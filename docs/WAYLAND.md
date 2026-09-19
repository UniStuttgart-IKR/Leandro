<!-- SPDX-License-Identifier: MIT -->
# Ubuntu Wayland desktop

- Use the same host backends, Cloud Hypervisor commands and guest modules as [Quickstart](QUICKSTART.md).
- Complete [Ubuntu installation](UBUNTU-DESKTOP.md) through **GNOME and Sunshine**, then apply this section before starting GDM.
- GNOME uses Wayland; Sunshine captures DRM/KMS and encodes with NVENC. Moonlight carries mouse and keyboard input.
- Existing X11 session: close its stream and stop GDM before switching. If GDM runs in the guide's foreground namespace shell, stop it there.

## Select Wayland

Inside each guest:

```sh
sudo sed -i 's/^WaylandEnable=.*/WaylandEnable=true/' /etc/gdm3/custom.conf
sudo tee /var/lib/AccountsService/users/leandro >/dev/null <<'ACCOUNT'
[User]
Session=ubuntu-wayland
XSession=ubuntu-wayland
SystemAccount=false
ACCOUNT
sudo systemctl restart accounts-daemon
printf 'capture = kms\nencoder = nvenc\n' > ~/.config/sunshine/sunshine.conf
sudo setcap cap_sys_admin,cap_sys_nice+p "$(readlink -f "$(command -v sunshine)")"
sudo apt-get install -y glmark2-wayland
```

- `ubuntu-wayland` selects the session explicitly; `WaylandEnable=true` alone permits an X11 fallback.
- KMS capture gives Sunshine additional guest privileges. Use the distro package from the Ubuntu guide; AppImage/Flatpak do not support this capture path. See [Sunshine setup](https://docs.lizardbyte.dev/projects/sunshine/latest/md_docs_2getting__started.html).
- Start GDM with the [PCI identity view](UBUNTU-DESKTOP.md#start-the-desktop-with-its-pci-identity-view), then use the quickstart's unchanged Moonlight pairing and streaming commands.
- Sunshine's desktop autostart inherits the Wayland session environment. A plain SSH shell does not.

## Verify inside the streamed terminal

```sh
printf 'session=%s display=%s\n' "$XDG_SESSION_TYPE" "$WAYLAND_DISPLAY"
loginctl show-session "$XDG_SESSION_ID" -p Type
sudo cat /sys/module/nvidia_drm/parameters/modeset
glmark2-wayland --size 800x600 -b build:duration=10
```

- Expect `wayland`, a Wayland socket name, `Type=wayland`, and `Y` for modesetting.
- glmark2 must report an NVIDIA renderer and show moving content in Moonlight. `llvmpipe` means software rendering.
- `glxinfo` checks Xwayland compatibility; it does not establish native Wayland rendering.
- Check Sunshine's log for KMS capture and NVENC. A session that starts or an FPS counter alone does not prove frames reach the client.
- Known limits: [Display](DISPLAY.md), especially historical Xwayland/EGLImage failures and stalls under memory pressure.

## Tested

- 2026-09-19: Ubuntu 24.04, GNOME Wayland, RTX 2070, NVIDIA 610.57.04, Sunshine 2026.516.143833.
- Native glmark2 reported NVIDIA; KMS/NVENC delivered changing rendered content to Moonlight in three captured client frames.
- One Wayland guest ran beside the existing X11 guest. This was a short smoke test on a provisioned image, not a fresh-install test, two-Wayland-guest test or stability benchmark.
- Portal capture stopped at its permission dialog; use KMS for this recipe.

## Return to X11

- Stop GDM and close the stream.
- Set `WaylandEnable=false`, replace `ubuntu-wayland` with `ubuntu-xorg` in the AccountsService file, and set Sunshine `capture = x11`.
- Restart `accounts-daemon`, then start GDM using the same identity-view commands.
