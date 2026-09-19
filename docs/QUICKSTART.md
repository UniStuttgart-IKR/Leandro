<!-- SPDX-License-Identifier: MIT -->
# Two Ubuntu desktops on one GPU

- Direct commands for the host backends, patched Cloud Hypervisor and Moonlight. No Leandro-Test checkout or launcher is required.
- Linux x86-64, KVM access, NVIDIA GPU with NVENC, and matching host/guest NVIDIA userspace. Use trusted guests; [isolation is incomplete](SECURITY.md).
- Complete [host installation, patches, network and disk preparation](VM-PREPARATION.md) first. Continue in that host Bash shell; it defines `LEANDRO_SRC`, `VM_WORK` and `CH`.
- This manual recipe is command-checked, but has not been tested from a fresh installation. Previous hardware results used the acceptance harness.

- Desktop: [GNOME/X11](UBUNTU-DESKTOP.md), or apply the [Wayland setup](WAYLAND.md) before starting GDM. Host VM commands are identical.
- Desktop input uses Moonlight and Sunshine; no host input backend is needed.
  For evdev forwarding, see [Input](INPUT.md).

## 1. Start the backends

```sh
ulimit -n 65536
for i in 0 1; do
  "$LEANDRO_SRC/target/release/vhost-user-nvrm" --nvrm "$VM_WORK/vm$i/nvrm.sock" \
    > "$VM_WORK/vm$i/backend.log" 2>&1 &
  echo $! > "$VM_WORK/vm$i/backend.pid"

done
```

- Wait until both backend sockets exist; inspect the logs if a backend exits. Every VM needs its own backend and sockets.
- The device is attached with `--generic-vhost-user`, not PCI passthrough (`--device`). `shared=on` is required.

## 2. Boot two Ubuntu VMs

```sh
"$CH" --cpus boot=4 --memory size=8G,shared=on \
  --kernel "$VM_WORK/ubuntu/vmlinuz" --initramfs "$VM_WORK/ubuntu/initrd" \
  --cmdline 'root=/dev/vda1 rw console=ttyS0 net.ifnames=0' \
  --disk path="$VM_WORK/vm0/rootfs.qcow2",image_type=qcow2,backing_files=on \
         path="$VM_WORK/vm0/seed.img",readonly=on \
  --net tap=lea-tap0,mac=02:00:00:00:00:10 \
  --generic-vhost-user "device_type=60,socket=$VM_WORK/vm0/nvrm.sock,queue_sizes=[256,256]" \
  --serial file="$VM_WORK/vm0/serial.log" --console off \
  > "$VM_WORK/vm0/hypervisor.log" 2>&1 &
echo $! > "$VM_WORK/vm0/hypervisor.pid"

"$CH" --cpus boot=4 --memory size=8G,shared=on \
  --kernel "$VM_WORK/ubuntu/vmlinuz" --initramfs "$VM_WORK/ubuntu/initrd" \
  --cmdline 'root=/dev/vda1 rw console=ttyS0 net.ifnames=0' \
  --disk path="$VM_WORK/vm1/rootfs.qcow2",image_type=qcow2,backing_files=on \
         path="$VM_WORK/vm1/seed.img",readonly=on \
  --net tap=lea-tap1,mac=02:00:00:00:00:11 \
  --generic-vhost-user "device_type=60,socket=$VM_WORK/vm1/nvrm.sock,queue_sizes=[256,256]" \
  --serial file="$VM_WORK/vm1/serial.log" --console off \
  > "$VM_WORK/vm1/hypervisor.log" 2>&1 &
echo $! > "$VM_WORK/vm1/hypervisor.pid"
```

- Wait for SSH at `192.168.100.10` and `.11`. Login: `leandro`, key: `$VM_WORK/id_ed25519`.
- Complete the [Ubuntu guest installation](UBUNTU-DESKTOP.md) in both VMs. It installs the guest modules, NVIDIA userspace, GNOME and Sunshine, including Xorg's required PCI identity view.
- Keep each guest's final desktop-start terminal open while streaming.

## 3. Open two desktop windows

On the host's graphical desktop, pair each VM once:

```sh
moonlight pair 192.168.100.10
moonlight pair 192.168.100.11
```

- For each pairing, enter Moonlight's PIN in Sunshine's web UI at `https://192.168.100.10:47990` or `.11:47990`. Use the credentials configured in the guest. Sunshine uses a self-signed certificate.
- Open each UI while its pairing command is waiting, then start the streams:

```sh
moonlight stream 192.168.100.10 Desktop --display-mode windowed \
  --resolution 1920x1080 --fps 60 --bitrate 20000 &
moonlight stream 192.168.100.11 Desktop --display-mode windowed \
  --resolution 1920x1080 --fps 60 --bitrate 20000 &
```

Inside each X11 desktop, run `glxinfo -B` and `glxgears`; for native Wayland use `glmark2-wayland`. Expect an NVIDIA renderer and moving content in both windows. `llvmpipe` is software rendering.

## 4. NixOS compute guests

- Requires Nix with flakes enabled on the host; the host need not run NixOS.
- Uses slots 2 and 3 from the network setup. No desktop or input backend.

```sh
cd "$LEANDRO_SRC"
nix build .#guest-image -o "$VM_WORK/nixos"
source "$VM_WORK/nixos/image.env"
LEA_SSHKEY=$(base64 -w0 "$VM_WORK/id_ed25519.pub")
for i in 2 3; do
  mkdir "$VM_WORK/vm$i"
  qemu-img create -f qcow2 -F qcow2 -b "$VM_WORK/nixos/rootfs.qcow2" "$VM_WORK/vm$i/rootfs.qcow2"
  "$LEANDRO_SRC/target/release/vhost-user-nvrm" --nvrm "$VM_WORK/vm$i/nvrm.sock" \
    > "$VM_WORK/vm$i/backend.log" 2>&1 &
  echo $! > "$VM_WORK/vm$i/backend.pid"
done
```

Wait for both backend sockets, then launch:

```sh
"$CH" --cpus boot=2 --memory size=4G,shared=on \
  --kernel "$VM_WORK/nixos/kernel" --initramfs "$VM_WORK/nixos/initrd" \
  --cmdline "init=$LEA_NIXOS_INIT $LEA_NIXOS_CMDLINE_BASE lea_user=$LEA_NIXOS_USER lea_host=compute0 lea_ip=192.168.100.12 lea_prefix=24 lea_gw=192.168.100.1 lea_sshkey=$LEA_SSHKEY" \
  --disk path="$VM_WORK/vm2/rootfs.qcow2",image_type=qcow2,backing_files=on \
  --net tap=lea-tap2,mac=02:00:00:00:00:12 \
  --generic-vhost-user "device_type=60,socket=$VM_WORK/vm2/nvrm.sock,queue_sizes=[256,256]" \
  --serial file="$VM_WORK/vm2/serial.log" --console off \
  > "$VM_WORK/vm2/hypervisor.log" 2>&1 &
echo $! > "$VM_WORK/vm2/hypervisor.pid"

"$CH" --cpus boot=2 --memory size=4G,shared=on \
  --kernel "$VM_WORK/nixos/kernel" --initramfs "$VM_WORK/nixos/initrd" \
  --cmdline "init=$LEA_NIXOS_INIT $LEA_NIXOS_CMDLINE_BASE lea_user=$LEA_NIXOS_USER lea_host=compute1 lea_ip=192.168.100.13 lea_prefix=24 lea_gw=192.168.100.1 lea_sshkey=$LEA_SSHKEY" \
  --disk path="$VM_WORK/vm3/rootfs.qcow2",image_type=qcow2,backing_files=on \
  --net tap=lea-tap3,mac=02:00:00:00:00:13 \
  --generic-vhost-user "device_type=60,socket=$VM_WORK/vm3/nvrm.sock,queue_sizes=[256,256]" \
  --serial file="$VM_WORK/vm3/serial.log" --console off \
  > "$VM_WORK/vm3/hypervisor.log" 2>&1 &
echo $! > "$VM_WORK/vm3/hypervisor.pid"
```

- The image includes Leandro's modules, but needs the host's parameters and NVIDIA userspace before compute works. Follow [NixOS userspace setup](VM-PREPARATION.md#nixos-userspace-after-boot).
- `nvidia-smi` verifies device access; it does not prove a CUDA kernel executed. Run your compute workload afterward.

## 5. Stop

Close Moonlight. For each running Ubuntu VM (`.10`, `.11`):

```sh
ssh -i "$VM_WORK/id_ed25519" leandro@192.168.100.10 sudo poweroff
ssh -i "$VM_WORK/id_ed25519" leandro@192.168.100.11 sudo poweroff
```

- For NixOS, use the same commands with `$LEA_NIXOS_USER` and `.12` / `.13`.
- Wait for each hypervisor to exit before stopping any remaining backends. In the original host Bash shell, `wait "$(cat "$VM_WORK/vm0/hypervisor.pid")"` waits for VM 0; repeat for the other running slots.
- Then terminate any remaining backend processes whose saved PIDs still identify those executables. Keep the overlays and their backing images together.
- Backend and serial logs are under `$VM_WORK/vmN/`. These commands do not remove the host bridge; [network cleanup](VM-PREPARATION.md#network-cleanup) is separate.
