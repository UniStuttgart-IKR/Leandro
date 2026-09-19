<!-- SPDX-License-Identifier: MIT -->
# Manual host and VM preparation

- Supports the [direct-command quickstart](QUICKSTART.md). Run host blocks in one Bash shell.
- Start in a fresh checkout/work directory. Requires a Linux x86-64 NVIDIA host, readable/writable `/dev/kvm` and NVIDIA device nodes, sudo for networking, and Internet access.
- Keep at least 16 GiB RAM free for two desktops, plus host headroom. Each guest disk below has a sparse 40 GiB maximum.
- Install the NVIDIA host driver and userspace matching `DRIVER_VERSION` (`610.57.04` at this revision). Reboot after replacing a loaded driver.

## Build and patch Cloud Hypervisor

Ubuntu 24.04 host packages:

```sh
sudo apt-get update
sudo apt-get install -y git curl build-essential pkg-config clang libclang-dev \
  libssl-dev libzstd-dev python3 qemu-utils cloud-image-utils openssh-client \
  iproute2 iptables
```

Install [Rustup](https://rustup.rs/), [Nix with flakes](https://nixos.org/download/) for the guest packages, and [Moonlight Qt](https://moonlight-stream.org/) for the host windows. On NixOS, use the core `nix develop` environment for build tools and install `cloud-utils` and Moonlight separately.

```sh
git clone --branch refactor-thesis https://github.com/UniStuttgart-IKR/Leandro.git
cd Leandro
export LEANDRO_SRC="$PWD"
export VM_WORK="$LEANDRO_SRC/target/manual-vms"
umask 077
mkdir -p "$VM_WORK" vendor
DRIVER=$(cat DRIVER_VERSION)
CH_TAG=$(cat CH_VERSION)
nvidia-smi --query-gpu=driver_version --format=csv,noheader
# Stop here unless the reported driver equals $DRIVER.

git clone --depth 1 --branch "$DRIVER" https://github.com/NVIDIA/open-gpu-kernel-modules.git vendor/open-gpu-kernel-modules
git clone --depth 1 --branch "$CH_TAG" https://github.com/cloud-hypervisor/cloud-hypervisor.git vendor/cloud-hypervisor
for patch in "$LEANDRO_SRC"/patches/[0-9][0-9][0-9][0-9]-*.patch; do
  git -C vendor/cloud-hypervisor apply "$patch" || exit 1
done
# Every patch must apply successfully before building.
cargo build --locked --release --manifest-path vendor/cloud-hypervisor/Cargo.toml --bin cloud-hypervisor
cargo build --locked --release
export CH="$LEANDRO_SRC/vendor/cloud-hypervisor/target/release/cloud-hypervisor"
"$CH" --version
nix build .#guest-deb -o "$VM_WORK/guest-deb"
```

- Rustup selects `rust-toolchain.toml` automatically. The patch series is required for shared mappings and device features; stock Cloud Hypervisor is insufficient.
- `guest-deb` builds a DKMS source package for the Ubuntu guests, not a host driver. Nix applies no changes to the running host kernel.

## Driver files for the guests

```sh
cp "$LEANDRO_SRC/DRIVER_VERSION" "$VM_WORK/DRIVER_VERSION"
cp /proc/driver/nvidia/params "$VM_WORK/params"
curl -fL "https://us.download.nvidia.com/XFree86/Linux-x86_64/$DRIVER/NVIDIA-Linux-x86_64-$DRIVER.run" -o "$VM_WORK/NVIDIA.run"
mkdir "$VM_WORK/host-gpu"
GPU_BDF=$(nvidia-smi --query-gpu=pci.bus_id --format=csv,noheader | head -1 | sed -E 's/^[[:xdigit:]]{8}:/0000:/')
GPU_SYS="/sys/bus/pci/devices/${GPU_BDF,,}"
for attr in vendor device subsystem_vendor subsystem_device revision; do
  cat "$GPU_SYS/$attr" > "$VM_WORK/host-gpu/$attr"
done
sudo dd if="$GPU_SYS/config" of="$VM_WORK/host-gpu/config" bs=64 count=1 status=none
```

- This recipe uses GPU 0. Preserve its real identity; do not hardcode another model's PCI ID.
- NVIDIA userspace is installed only inside the Ubuntu guests. The installer offers a [userspace-only option](https://download.nvidia.com/XFree86/Linux-x86_64/595.58.03/README/kernel_open.html); guest kernel modules come from Leandro.

## Host network

Use an unused `192.168.100.0/24` subnet. These commands create a new bridge; do not reuse an existing interface with these names.

```sh
UPLINK=$(ip -4 route show default | awk 'NR==1 {print $5}')
cat /proc/sys/net/ipv4/ip_forward > "$VM_WORK/ip-forward.before"
sudo ip link add lea-br0 type bridge
sudo ip address add 192.168.100.1/24 dev lea-br0
sudo ip link set lea-br0 up
for i in 0 1 2 3; do
  sudo ip tuntap add dev "lea-tap$i" mode tap user "$(id -un)"
  sudo ip link set "lea-tap$i" master lea-br0
  sudo ip link set "lea-tap$i" up
done
sudo sysctl -w net.ipv4.ip_forward=1
sudo iptables -I FORWARD -i lea-br0 -o "$UPLINK" -s 192.168.100.0/24 -j ACCEPT
sudo iptables -I FORWARD -i "$UPLINK" -o lea-br0 -d 192.168.100.0/24 -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
sudo iptables -t nat -A POSTROUTING -s 192.168.100.0/24 -o "$UPLINK" -j MASQUERADE
```

Host firewall managers may require equivalent rules in their own configuration. Do not disable the firewall or flush unrelated rules.

## Ubuntu disks and SSH

The image/kernel/initrd below are the matching Ubuntu snapshot used by the rig. If upstream removes it, select and validate another complete snapshot rather than mixing kernels and disks.

```sh
mkdir "$VM_WORK/ubuntu"
UBUNTU_URL=https://cloud-images.ubuntu.com/releases/noble/release-20260801
curl -fL "$UBUNTU_URL/ubuntu-24.04-server-cloudimg-amd64.img" -o "$VM_WORK/ubuntu/base.qcow2"
curl -fL "$UBUNTU_URL/unpacked/ubuntu-24.04-server-cloudimg-amd64-vmlinuz-generic" -o "$VM_WORK/ubuntu/vmlinuz"
curl -fL "$UBUNTU_URL/unpacked/ubuntu-24.04-server-cloudimg-amd64-initrd-generic" -o "$VM_WORK/ubuntu/initrd"
(cd "$VM_WORK/ubuntu" && sha256sum -c <<'SUMS'
0533b0655c32e68b31d792ecd6ccfca95abdbc536c4446874fe0513bd4140ffe  base.qcow2
09d55bc6cb91a2926e363516a3b249fdf8b3fc3d32c3811b84e6577e63644ef2  vmlinuz
eed8cba8ffe0c6c8f4086f01c1a5609547321418a8e2155e9989ce9a14d4a4d4  initrd
SUMS
)
ssh-keygen -t ed25519 -N '' -f "$VM_WORK/id_ed25519"
for i in 0 1; do
  mkdir "$VM_WORK/vm$i"
  qemu-img create -f qcow2 -F qcow2 -b "$VM_WORK/ubuntu/base.qcow2" "$VM_WORK/vm$i/rootfs.qcow2" 40G
  cat > "$VM_WORK/vm$i/user-data" <<CLOUD
#cloud-config
users:
  - name: leandro
    groups: [adm, sudo]
    shell: /bin/bash
    sudo: ALL=(ALL) NOPASSWD:ALL
    lock_passwd: true
    ssh_authorized_keys:
      - $(cat "$VM_WORK/id_ed25519.pub")
ssh_pwauth: false
CLOUD
  printf 'instance-id: desktop%s\nlocal-hostname: desktop%s\n' "$i" "$i" > "$VM_WORK/vm$i/meta-data"
  cat > "$VM_WORK/vm$i/network-config" <<NET
version: 2
ethernets:
  eth0:
    dhcp4: false
    addresses: [192.168.100.$((10+i))/24]
    routes:
      - to: default
        via: 192.168.100.1
    nameservers:
      addresses: [1.1.1.1, 8.8.8.8]
NET
  cloud-localds --network-config="$VM_WORK/vm$i/network-config" \
    "$VM_WORK/vm$i/seed.img" "$VM_WORK/vm$i/user-data" "$VM_WORK/vm$i/meta-data"
done
```

Return to [start the backends and VMs](QUICKSTART.md#1-start-the-backends). Keep backing disks and the Nix image output link for as long as their overlays exist.

## NixOS userspace after boot

For the optional compute guests, extract NVIDIA's userspace on the host without installing it:

```sh
(cd "$VM_WORK" && sh NVIDIA.run --extract-only)
mkdir -p "$VM_WORK/nvidia/lib" "$VM_WORK/nvidia/bin"
cp -a "$VM_WORK/NVIDIA-Linux-x86_64-$DRIVER/"*.so* "$VM_WORK/nvidia/lib/"
install -m755 "$VM_WORK/NVIDIA-Linux-x86_64-$DRIVER/nvidia-smi" "$VM_WORK/nvidia/bin/"
for lib in "$VM_WORK/nvidia/lib/"*.so*; do
  soname=$(objdump -p "$lib" | awk '$1=="SONAME" {print $2; exit}')
  if [ -n "$soname" ] && [ "$soname" != "$(basename "$lib")" ]; then
    ln -sfn "$(basename "$lib")" "$VM_WORK/nvidia/lib/$soname"
  fi
done
for ip in 192.168.100.12 192.168.100.13; do
  scp -i "$VM_WORK/id_ed25519" "$VM_WORK/params" "$LEA_NIXOS_USER@$ip:params"
  tar -C "$VM_WORK/nvidia" -cf - lib bin | \
    ssh -i "$VM_WORK/id_ed25519" "$LEA_NIXOS_USER@$ip" 'sudo tar -C /opt/nvrm -xf -'
  ssh -i "$VM_WORK/id_ed25519" "$LEA_NIXOS_USER@$ip" \
    'sudo install -m644 ~/params /var/lib/leandro/params.txt && sudo systemctl restart leandro-nvrm && LD_LIBRARY_PATH=/opt/nvrm/lib nvidia-smi'
done
```

- NixOS uses its configured loader environment, not Ubuntu's `ldconfig`.
- These files enable driver access. Install your CUDA application and its runtime separately; they are not bundled in the compute image.

## Network cleanup

After **all four VMs have stopped**, in the original host shell:

```sh
sudo iptables -t nat -D POSTROUTING -s 192.168.100.0/24 -o "$UPLINK" -j MASQUERADE
sudo iptables -D FORWARD -i lea-br0 -o "$UPLINK" -s 192.168.100.0/24 -j ACCEPT
sudo iptables -D FORWARD -i "$UPLINK" -o lea-br0 -d 192.168.100.0/24 -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
for i in 0 1 2 3; do sudo ip link delete "lea-tap$i"; done
sudo ip link delete lea-br0
sudo sysctl -w "net.ipv4.ip_forward=$(cat "$VM_WORK/ip-forward.before")"
```

Restore forwarding only if no new workload started relying on it during this session.
