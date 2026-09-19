<!-- SPDX-License-Identifier: MIT -->
# Manual Ubuntu guest installation

- Continue after booting the two VMs in [QUICKSTART.md](QUICKSTART.md#2-boot-two-ubuntu-vms).
- Repeat the guest steps for `.10` and `.11`. No private repository is used.
- Initial installation needs network access and takes longer than subsequent starts. Keep the running guest kernel: Cloud Hypervisor boots the matching kernel/initrd supplied by the host.

## Copy the guest package and host identity

In the original **host** shell, after SSH is ready:

```sh
for ip in 192.168.100.10 192.168.100.11; do
  scp -i "$VM_WORK/id_ed25519" "$VM_WORK/guest-deb/"*.deb \
    "$VM_WORK/NVIDIA.run" "$VM_WORK/DRIVER_VERSION" "$VM_WORK/params" "leandro@$ip:"
  scp -r -i "$VM_WORK/id_ed25519" "$VM_WORK/host-gpu" "leandro@$ip:"
done
ssh -i "$VM_WORK/id_ed25519" leandro@192.168.100.10
```

## Install inside each guest

```sh
sudo cloud-init status --wait
sudo apt-get update
sudo apt-get install -y build-essential dkms "linux-headers-$(uname -r)" \
  ubuntu-desktop-minimal mesa-utils vulkan-tools curl
sudo systemctl stop gdm3
DRIVER=$(cat ~/DRIVER_VERSION)
sudo apt-get install -y --no-install-recommends ./leandro-guest-dkms_*.deb
sudo dkms install -m leandro-guest -v "0.1+$DRIVER" -k "$(uname -r)"
modinfo -n virtio_nvrm
sudo install -Dm644 ~/params /etc/leandro/params
sudo cp -a ~/host-gpu /etc/leandro/
sudo sh ~/NVIDIA.run --no-kernel-modules
```

- Review NVIDIA's installer prompts. Install userspace only; do not install `nvidia.ko`, `nvidia_uvm.ko`, or a distro driver metapackage. Leandro provides their guest interfaces.
- The DKMS build supplies `nvrm_nodes`, `virtio_nvrm`, and NVIDIA's `nvidia-modeset`/`nvidia-drm` linked against Leandro. A successful package installation alone is insufficient: the explicit DKMS command must succeed.
- If headers for `uname -r` are unavailable, stop and select a matching image/kernel/header set. Installing headers for another kernel does not fix the build.

Enable the virtual display before loading NVKMS:

```sh
sudo tee /etc/modprobe.d/leandro-display.conf >/dev/null <<'CONF'
options virtio_nvrm display=1 vdisplay=1 bdf_mediation=1 vdisplay_width=1920 vdisplay_height=1080 vdisplay_vblank_hz=60
options nvidia-drm modeset=1 vblank=1
CONF
sudo systemctl start leandro-nvrm
# Also cover an early udev load with the old parameter defaults.
for setting in display:1 vdisplay:1 bdf_mediation:1 vdisplay_width:1920 vdisplay_height:1080 vdisplay_vblank_hz:60; do
  echo "${setting#*:}" | sudo tee "/sys/module/virtio_nvrm/parameters/${setting%%:*}" >/dev/null
done
sudo systemctl start leandro-display
nvidia-smi
```

## GNOME and Sunshine

Inside each guest:

```sh
curl -fL https://github.com/LizardByte/Sunshine/releases/download/v2026.516.143833/sunshine-ubuntu-24.04-amd64.deb -o sunshine.deb
sudo apt-get install -y ./sunshine.deb
sudo usermod -aG input,render,video leandro
sudo modprobe uinput
sudo chgrp input /dev/uinput
sudo chmod 660 /dev/uinput
sudo tee /etc/udev/rules.d/60-sunshine-uinput.rules >/dev/null <<'RULE'
KERNEL=="uinput", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"
RULE
echo uinput | sudo tee /etc/modules-load.d/uinput.conf >/dev/null
mkdir -p ~/.config/sunshine ~/.config/autostart
printf 'capture = x11\nencoder = nvenc\n' > ~/.config/sunshine/sunshine.conf
read -rsp 'Choose Sunshine web password: ' SUN_PASS; echo
sunshine --creds reviewer "$SUN_PASS"
unset SUN_PASS
cat > ~/.config/autostart/sunshine.desktop <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Sunshine
Exec=sunshine
X-GNOME-Autostart-enabled=true
DESKTOP
sudo tee /etc/gdm3/custom.conf >/dev/null <<'GDM'
[daemon]
AutomaticLoginEnable=true
AutomaticLogin=leandro
WaylandEnable=false
GDM
sudo mkdir -p /var/lib/AccountsService/users
sudo tee /var/lib/AccountsService/users/leandro >/dev/null <<'ACCOUNT'
[User]
Session=ubuntu-xorg
XSession=ubuntu-xorg
SystemAccount=false
ACCOUNT
sudo systemctl restart accounts-daemon
sudo tee /etc/X11/xorg.conf >/dev/null <<'XORG'
Section "Device"
    Identifier "leandro"
    Driver "nvidia"
EndSection
Section "Screen"
    Identifier "screen"
    Device "leandro"
    DefaultDepth 24
    Option "AllowEmptyInitialConfiguration" "true"
    SubSection "Display"
        Depth 24
        Virtual 1920 1080
    EndSubSection
EndSection
XORG
```

- For GNOME on Wayland, apply [these changes](WAYLAND.md) before starting the desktop below.

## Start the desktop with its PCI identity view

- NVIDIA's Xorg driver probes PCI identity. The underlying device is virtio, so Xorg needs an NVIDIA identity view in a private mount namespace.
- The commands below change only that namespace's sysfs view. They do not change host PCI identity or pass through the physical GPU.
- Run inside **each guest**, as root in a new mount namespace. Keep the SSH terminal open. Repeat this section after guest reboot; it is deliberately not a boot service.

```sh
sudo systemctl stop gdm3
sudo unshare --mount --propagation slave bash
```

Now inside that root shell:

```sh
set -e
node=$(readlink -f /sys/bus/virtio/drivers/virtio_nvrm/virtio* | head -1)
real=$(dirname "$node")
bdf=$(basename "$real")
case "$bdf" in 0000:*) ;; *) echo 'No bound virtio_nvrm PCI device'; exit 1 ;; esac
view=$(mktemp -d /run/leandro-pci.XXXXXX)
cp -a /sys/bus/pci/devices/. "$view/"
unlink "$view/$bdf"
mkdir "$view/$bdf"
cp /etc/leandro/host-gpu/* "$view/$bdf/"
printf '0x030200\n' > "$view/$bdf/class"
printf '0\n' > "$view/$bdf/boot_vga"
printf '0\n' > "$view/$bdf/irq"
printf '1\n' > "$view/$bdf/enable"
for i in 0 1 2 3 4 5 6; do
  printf '0x0000000000000000 0x0000000000000000 0x0000000000000000\n'
done > "$view/$bdf/resource"
mount --bind "$view" /sys/bus/pci/devices
for attr in vendor device subsystem_vendor subsystem_device revision class config; do
  mount --bind "$view/$bdf/$attr" "$real/$attr"
done
exec /usr/sbin/gdm3
```

- GNOME autologin starts Sunshine. Its web username is `reviewer`; use the password chosen above.
- Return to [pairing and opening two windows](QUICKSTART.md#3-open-two-desktop-windows).
- Inside the desktop, `glxinfo -B` should name NVIDIA. Use `glxgears` to check moving content. A working SSH connection or `nvidia-smi` alone does not verify graphics.
- If the stream is black, inspect `journalctl -b`, Xorg's log, and Sunshine's log under `~/.config/sunshine/`. Check NVIDIA userspace version, `nvidia_drm`, the namespace mounts, and Sunshine's `x11`/`nvenc` selection.
