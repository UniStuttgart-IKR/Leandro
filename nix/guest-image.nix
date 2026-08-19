# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# THE NIXOS GUEST IMAGE: one nixosConfiguration, built two ways.
#
#   nix build .#guest-image        kernel + initrd + qcow2, for DIRECT KERNEL BOOT
#   nix build .#guest-image-uefi   the same system as a UEFI-bootable qcow2
#   scripts/build.sh bake --nixos  builds it and copies it into LEA_VM_DIR
#
# WHY THIS EXISTS. The Ubuntu path (build.sh bake) is a convenience: it boots
# a cloud image, runs apt in it and syspreps the result, and it is
# reproducible only up to whatever the Ubuntu archive served that day. That
# is fine on this machine and useless on someone else's cluster. This one is
# a derivation: the same inputs give the same image, and the only thing it
# reaches out for is the NVIDIA userspace, which cannot be shipped at all
# (LICENSES.md).
#
# WHY NOT microvm.nix -- the question is worth an answer rather than a
# silence, because microvm.nix does declaratively run NixOS microVMs on
# cloud-hypervisor and is the obvious candidate. It was read
# (github:astro/microvm.nix, rev 71beea00, 2026-08-09) and not used, for one
# structural reason and three practical ones. STRUCTURAL: microvm.nix's
# cloud-hypervisor runner (lib/runners/cloud-hypervisor.nix) renders the
# whole argv into a store-path shell script at BUILD time
# (`lib.escapeShellArgs [...]`). Every argument Leandro varies per instance
# is runtime state -- the tap index, the MAC, the overlay path, the VRAM cap,
# and above all the vhost-user socket of a backend that must already be
# LISTENING before cloud-hypervisor starts. Adopting it would put a second
# argv builder next to lea_vm_start, which is the one thing scripts/lib is
# organised against. PRACTICAL: (1) `microvm.cloud-hypervisor.extraArgs` can
# carry `--generic-vhost-user`, but only as a build-time constant, so eight
# fleet slots would be eight nixosConfigurations; (2) its host half is a set
# of systemd units for a NixOS host, and this host is Arch -- Leandro's
# lifecycle is pidfiles and vm/<name>/ directories, which already refuse a
# `down` that would hit somebody else's run; (3) it boots from an erofs
# store disk plus virtiofs shares, while the rig wants a plain qcow2 whose
# per-instance overlay is disposable. What is genuinely useful there is the
# image-building half, and nixpkgs already has that as
# nixos/lib/make-disk-image.nix, which is what is used below. Measured cost
# of finding this out: one afternoon, and it was worth it.
#
# THE GUEST IDENTITY IS ON THE KERNEL COMMAND LINE, and there is no seed
# disk. The Ubuntu guests get theirs from a FAT "CIDATA" image that
# cloud-init reads on first boot (_lea_seed_write in scripts/lib/rig.sh); a
# stock NixOS reads nothing of the sort, and adding cloud-init to it would be
# adding a second configuration system to a system that exists to not need
# one. With direct kernel boot the host already writes the command line, so
# the six things that differ per instance ride along on it:
#
#   lea_user=<name> lea_host=<name> lea_ip=<A.B.C.D> lea_prefix=<24>
#   lea_gw=<A.B.C.D> lea_sshkey=<base64 of one authorized_keys line>
#
# base64 because the key line contains spaces and the kernel splits on them;
# no dots in the names because the kernel reads `foo.bar=` as a module
# parameter and complains about it on every boot. Measured 2026-08-18: the
# ed25519 public key the rig generates is 92 bytes, 124 base64 characters,
# and the whole command line comes to well under x86's COMMAND_LINE_SIZE of
# 2048. leandro-identity.service below reads them out of /proc/cmdline
# before the network comes up.
#
# WHICH KERNEL: the 6.12 LTS, the same pin and for the same reason as
# nix/packages/guest-modules.nix -- 6.15 removed hrtimer_init and 6.18
# removed nth_page, and neither is done. boot.kernelPackages is what
# services.leandro-guest builds its module package against, so a guest that
# moves off the pin finds out at BUILD time.
#
# THE NVIDIA USERSPACE, and why it is a parameter rather than a default.
# `nvidiaUserspace` takes a package supplying lib/libcuda.so.<driverVersion>;
# with it null (the default) the image reserves /opt/nvrm/lib and the rig
# stages the HOST's own libraries there at `up` time, which is what
# lea_payload_stage has always done for the Ubuntu guests. The obvious
# alternative is nixpkgs' own builder:
#
#   nvidiaUserspace = pkgs.linuxKernel.packages.linux_6_12.nvidiaPackages.mkDriver {
#     version = "610.43.03";
#     sha256_64bit = "sha256-ReLUwTSiPDXlDyU6SqY+fl6NF+PRhdSgfIpY6WEu05I=";
#     sha256_32bit = lib.fakeSha256;   # replace, or disable32Bit
#     useSettings = false; usePersistenced = false;
#   };
#
# and it works -- measured 2026-08-19, against DRIVER_VERSION 610.43.03:
#
#   * NVIDIA's CDN has that version (HTTP 200 on the .run), and the .run's
#     libcuda.so.610.43.03 is BYTE-IDENTICAL to this host's distribution
#     package: sha256 ba35b4ba.. both. Nothing is redistributed by anyone --
#     the builder fetches from NVIDIA (LICENSES.md, "What is deliberately not
#     shipped").
#   * What mkDriver PRODUCES is not byte-identical, and that is not a
#     disagreement: sha256 100ee979.., 24,344 bytes larger, because patchelf
#     appends a store RUNPATH. Its .text, .rodata and .data.rel.ro are byte
#     for byte the host's. lea_libcuda_check reports exactly this, and
#     accepts it -- see there for why the weaker-looking answer is the true
#     one.
#
# It is NOT the default for one reason: nixpkgs gates nvidia-x11 behind
# `config.allowUnfree` AND `config.nvidia.acceptLicense`. Accepting NVIDIA's
# licence is the operator's act, not this flake's, and setting those here
# would be doing it on their behalf silently -- `nix build .#guest-image`
# would also then stop working for anyone who has not. So the mechanism is
# here, spelled out and measured, and switching it on is one edit and one
# licence decision.
#
# The Ubuntu-side staging stays the fallback regardless: a vGPU or datacentre
# driver that is not on the public CDN cannot be fetched by any builder, and
# the payload path is the only way it reaches a guest at all.
{ lib
, nixpkgs
, system
, guestModule          # self.nixosModules.guest
, driverVersion        # contents of DRIVER_VERSION
, guestUser ? "leandro"   # config.sh's LEA_GUEST_USER default; build.sh checks it back
, nvidiaUserspace ? null  # a package providing lib/libcuda.so.<driverVersion>, or null
}:

let
  inherit (nixpkgs) lib;

  # ---- the identity unit --------------------------------------------------
  # ONE unit covers the address, the hostname and the SSH key, because all
  # three come off the same command line and a second reader is a second
  # chance to disagree with the first.
  #
  # WantedBy=sysinit.target, NOT network-pre.target. Measured 2026-08-18, on
  # the first NixOS boot: hung on network-pre.target the unit never ran at
  # all, the guest came up as `nixos` with no address, and sshd started
  # happily on nothing. systemd-networkd.service is `After=network-pre.target`
  # but nothing in a NixOS guest *Wants* that target, so it is never in the
  # transaction and a unit hung off it is never pulled in. sysinit.target is
  # always reached; Before= it plus DefaultDependencies=no is the same
  # arrangement systemd's own early units use.
  identity = { pkgs, ... }: {
    systemd.services.leandro-identity = {
      description = "Leandro guest identity from the kernel command line (address, hostname, SSH key)";
      wantedBy = [ "sysinit.target" ];
      before = [ "sysinit.target" "network-pre.target" "systemd-networkd.service" "sshd.service" ];
      after = [ "systemd-remount-fs.service" "local-fs.target" ];
      unitConfig.DefaultDependencies = "no";
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      path = with pkgs; [ coreutils gnugrep gnused inetutils ];
      script = ''
        # WHERE THE IDENTITY COMES FROM, and there are two places because
        # there are two ways this image is booted.
        #
        #   direct kernel boot   the host writes the whole kernel command
        #                        line (cloud-hypervisor --cmdline), so the
        #                        six words ride on /proc/cmdline. This is the
        #                        rig's way and the measured one.
        #   firmware boot        there is NO host-written command line: the
        #                        firmware loads systemd-boot, which loads the
        #                        kernel with the command line baked into the
        #                        image's own boot entry. Measured 2026-08-19:
        #                        a UEFI boot with --cmdline set reaches
        #                        multi-user perfectly and ignores every word
        #                        of it -- the guest comes up as `nixos` with
        #                        no address and no key. So the identity has
        #                        to travel out of band, and SMBIOS type 11
        #                        OEM strings are the channel cloud-hypervisor
        #                        offers (--platform oem_strings=[...]).
        #
        # The command line WINS when both are present: it is the more
        # specific answer, written by the thing that started this VM.
        cmdline_val() {
            for w in $(cat /proc/cmdline); do
                case "$w" in "$1"=*) printf '%s' "''${w#*=}"; return 0 ;; esac
            done
            return 1
        }
        # SMBIOS type 11 is a run of NUL-terminated strings after a 5-byte
        # header; tr turns them into lines and the header into leading junk
        # that no `lea_*=` pattern can match.
        smbios_val() {
            local f
            for f in /sys/firmware/dmi/entries/11-*/raw; do
                [ -r "$f" ] || continue
                tr '\0' '\n' < "$f" | while IFS= read -r line; do
                    case "$line" in "$1"=*) printf '%s' "''${line#*=}"; exit 0 ;; esac
                done | grep . && return 0
            done
            return 1
        }
        val() { cmdline_val "$1" || smbios_val "$1"; }
        user=$(val lea_user  || echo ${guestUser})
        host=$(val lea_host  || echo nixos)
        ip=$(val   lea_ip    || true)
        prefix=$(val lea_prefix || echo 24)
        gw=$(val   lea_gw    || true)
        key=$(val  lea_sshkey || true)

        # Say which channel answered, on the console. A guest that came up
        # with the wrong identity is otherwise indistinguishable from one
        # that came up with none, and both look like a network fault.
        if cmdline_val lea_ip >/dev/null 2>&1; then
            echo "leandro-identity: from the kernel command line (direct kernel boot)"
        elif smbios_val lea_ip >/dev/null 2>&1; then
            echo "leandro-identity: from SMBIOS OEM strings (firmware boot)"
        else
            echo "leandro-identity: NEITHER a kernel command line nor SMBIOS OEM strings carried an identity."
            echo "leandro-identity: this guest keeps the image defaults -- no address, no authorized key."
        fi

        # Hostname: the sethostname(2) one only. NixOS keeps /etc/hostname in
        # the store when networking.hostName is set, so this configuration
        # leaves it empty and sets the running name here.
        hostname "$host"
        printf '%s\n' "$host" > /etc/hostname

        # The address. A drop-in under /run rather than /etc: it describes
        # THIS boot of THIS instance, and an instance that is given a
        # different index next time must not find the old one.
        if [ -n "$ip" ] && [ -n "$gw" ]; then
            mkdir -p /run/systemd/network
            # The terminator has to stand in column 0 and so does the body:
            # an indented <<EOF delimiter is not a delimiter, and the shell
            # then swallows the rest of the script into the file.
            cat > /run/systemd/network/10-leandro-eth0.network <<EOF
[Match]
Name=eth0

[Network]
Address=$ip/$prefix
Gateway=$gw
DNS=8.8.8.8
DNS=1.1.1.1
EOF
        else
            echo "leandro-identity: no lea_ip/lea_gw on the command line -- leaving eth0 to networkd" >&2
        fi

        # The key. /etc/ssh/authorized_keys.d/<user> is one of NixOS's own
        # authorizedKeysFiles, so this needs no writable home and works
        # before the user has ever logged in.
        if [ -n "$key" ]; then
            mkdir -p /etc/ssh/authorized_keys.d
            printf '%s' "$key" | base64 -d > /etc/ssh/authorized_keys.d/"$user"
            chmod 444 /etc/ssh/authorized_keys.d/"$user"
        else
            echo "leandro-identity: no lea_sshkey on the command line -- this guest accepts no key" >&2
        fi
      '';
    };
  };

  # What a manylinux wheel expects to find on "the system". Measured
  # 2026-08-18 by running the gate's own torch stage in a NixOS guest and
  # reading off what failed, rather than by listing what might be needed:
  #   libstdc++.so.6, libgcc_s.so.1, libgomp.so.1   torch's _C.so   (gcc lib)
  #   libz.so.1                                     numpy           (zlib)
  # and nothing else -- with those four, `import numpy, torch` succeeds and
  # torch.cuda sees the card.
  #
  # Deliberately NOT glibc: putting a second libc on a search path ahead of
  # the loader's own is how a guest starts failing in ways nobody can read.
  wheelRuntimeFor = pkgs: pkgs.buildEnv {
    name = "leandro-wheel-runtime";
    paths = with pkgs; [ (lib.getLib stdenv.cc.cc) (lib.getLib zlib) ];
    pathsToLink = [ "/lib" ];
  };

  # ---- what every Leandro guest is ----------------------------------------
  common = { config, pkgs, lib, ... }:
  let wheelRuntime = wheelRuntimeFor pkgs; in {
    system.stateVersion = "26.05";
    nixpkgs.hostPlatform = system;

    boot.kernelPackages = pkgs.linuxKernel.packages.linux_6_12;
    # Enough to find the root disk and the console. virtio_console is not
    # used (the rig runs --console off) but costs nothing and turns a
    # mis-set --serial into a visible boot rather than a silent one.
    boot.initrd.availableKernelModules = [
      "virtio_pci" "virtio_blk" "virtio_net" "virtio_console" "ext4"
    ];
    # The instance disk is a 40 GiB overlay (LEA_DISK_SIZE) on an image sized
    # to its contents, so the partition has to grow on first boot or the
    # guest has a few hundred MiB of slack and the torch venv does not fit.
    boot.growPartition = true;
    fileSystems."/" = {
      device = "/dev/vda1";
      fsType = "ext4";
      autoResize = true;
    };

    # THE GUEST RUNS NO NVIDIA KERNEL DRIVER -- nixosModules.guest blacklists
    # it, and showcase.sh demo has a section that proves it.
    services.leandro-guest = {
      enable = true;
      users = [ guestUser ];
      # The SHAPE of /proc/driver/nvidia/params, not a recording of one rig's
      # file: the authoritative copy is the host's own, dropped at
      # /var/lib/leandro/params.txt by every `up` (params.runtimeFile wins).
      params.text = ''
        ResmanDebugLevel: 4294967295
        RmLogonRC: 1
        ModifyDeviceFiles: 1
        DeviceFileUID: 0
        DeviceFileGID: 0
        DeviceFileMode: 438
      '';
      nvidiaUserspaceDir =
        if nvidiaUserspace != null then "${lib.getLib nvidiaUserspace}/lib" else "/opt/nvrm/lib";
    };

    # WHY nix-ld AND NOT A NIX PACKAGE FOR THE PROBES. The payload that
    # reaches a guest is host-built ELF: probe/bin/nvprobe and friends,
    # mmapping and smipids out of LEA_BIN_DIR -- and nvidia-smi, which is
    # NVIDIA's own prebuilt binary. That last one settles it: nvidia-smi
    # cannot be rebuilt as a nix package (it is not redistributable and there
    # is no source), so an FHS loader has to be there whatever is decided
    # about the probes. Given that, packaging the probes as well would add a
    # second mechanism without removing the first -- and it would fork
    # lea_guest_setup, the one function that provisions BOTH guests, into two
    # paths. So: /lib64/ld-linux-x86-64.so.2 exists here (environment.ldso),
    # and lea_guest_setup stays byte for byte the same function for Ubuntu
    # and for NixOS.
    programs.nix-ld.enable = true;
    programs.nix-ld.libraries = with pkgs; [
      stdenv.cc.cc.lib   # libstdc++, libgcc_s -- the Rust and C++ halves
      zlib zstd xz bzip2
      libGL              # what an EGL/GL probe dlopens
      ncurses
      openssl
      libxml2
      numactl            # torch's wheels want it
      glibc
    ];

    # The guest user. NOPASSWD sudo because every provisioning step and half
    # the gate stages are `sudo insmod` / `sudo dmesg`, over a key-only SSH
    # login on a bridge that never leaves the host.
    users.users.${guestUser} = {
      isNormalUser = true;
      home = "/home/${guestUser}";
      createHome = true;
      extraGroups = [ "wheel" "render" "video" ];
      # The rig's key arrives on the kernel command line; this is the
      # fallback that makes a console login possible when it did not.
      password = guestUser;
    };
    security.sudo.wheelNeedsPassword = false;

    services.openssh = {
      enable = true;
      # NOT socket-activated: the Ubuntu images had to be switched off
      # ssh.socket for exactly this reason -- it reports "Listening" and then
      # refuses connections after a restart.
      startWhenNeeded = false;
      settings = {
        PermitRootLogin = "no";
        PasswordAuthentication = false;
      };
    };

    networking = {
      # Empty on purpose: leandro-identity sets the running name from the
      # command line, and a name here would put /etc/hostname in the store.
      hostName = "";
      useNetworkd = true;
      useDHCP = false;
      firewall.enable = false;   # a host-only bridge; the host does the NAT
    };
    systemd.network.enable = true;

    # WHAT THE GATE ACTUALLY RUNS IN THE GUEST. Every one of these is called
    # by name by scripts/test.sh's gpu gate or by scripts/lib/provision.sh,
    # and a missing one fails a stage rather than the boot -- which is the
    # expensive way to find out.
    environment.systemPackages = with pkgs; [
      kmod                 # lsmod, insmod, rmmod, modprobe
      pciutils             # lspci
      python3              # the robustness stage, and the venv's interpreter
      gnutar gzip          # lea_guest_tar unpacks into the guest
      gcc gnumake          # lea_guest_cc compiles probe sources IN the guest
      util-linux procps    # dmesg is systemd's, but taskset/ps are not
      curl                 # the torch venv's downloads
      file binutils        # objdump, for the audit
      (ffmpeg-full.override { withNvenc = true; withNvdec = true; })
      # nvidia-smi ON PATH, without shipping nvidia-smi. The gpu gate's `own`
      # stage calls it by bare name, and the Ubuntu images answer that with
      # /usr/local/bin/nvidia-smi -> /opt/nvrm/bin/nvidia-smi. NixOS has no
      # writable directory on PATH, so the indirection is declared instead:
      # this is a two-line wrapper around whatever the operator staged, and
      # it carries none of NVIDIA's bytes (LICENSES.md).
      (writeShellScriptBin "nvidia-smi" ''
        exec /opt/nvrm/bin/nvidia-smi "$@"
      '')
    ];

    # The rig talks to the guest as a normal user and needs a real HOME and a
    # writable /var/lib/leandro for the host's params file.
    #
    # /opt/nvrm/wheel-runtime IS NOT DECORATION. The gate's torch stage runs
    # `venv/bin/python`, and that venv is built from the SAME pip wheels as
    # the host's reference venv (vendor/hostvenv, torch 2.13.0+cu130) --
    # nixpkgs' own torch would make the stage compare two libraries instead
    # of two transport paths. A manylinux wheel's .so files need a C++ and
    # OpenMP runtime that they do not carry, and on NixOS nothing supplies
    # one: measured 2026-08-18 in a booted guest, `ldconfig` there cannot
    # even write a cache -- glibc's cache path points INSIDE the read-only
    # store (/nix/store/...-glibc-2.42/etc/ld.so.cache), so the ld.so.conf
    # mechanism the Ubuntu images use does not merely differ here, it does
    # not exist. Hence a directory with a stable name, which the rig links
    # into the guest's library path beside the NVIDIA payload.
    systemd.tmpfiles.rules = [
      "d /var/lib/leandro 0755 root root -"
      "d /opt/nvrm/lib 0755 root root -"
      "d /opt/nvrm/bin 0755 root root -"
      "L+ /opt/nvrm/wheel-runtime - - - - ${wheelRuntime}/lib"
    ];

    # Nothing in a guest ever builds a derivation; the daemon is 100 MB of
    # state and a boot-time unit for nobody.
    nix.enable = false;
    documentation.enable = false;
    documentation.nixos.enable = false;
  };

  directBoot = { ... }: {
    # No bootloader at all: the host supplies kernel, initrd and the command
    # line (cloud-hypervisor --kernel/--initramfs/--cmdline), exactly as it
    # does for the Ubuntu guests.
    boot.loader.grub.enable = false;
    boot.loader.systemd-boot.enable = false;
  };

  uefiBoot = { lib, ... }: {
    # For MeisterStack and anything else that boots an image the normal way.
    boot.loader.systemd-boot.enable = true;
    boot.loader.efi.canTouchEfiVariables = false;
    boot.loader.timeout = 0;

    # THE ROOT IS NOT ON THE SAME PARTITION AS IN THE DIRECT-BOOT IMAGE, and
    # that is the whole difference between the two layouts. `partitionTableType
    # = "efi"` puts a FAT ESP on vda1 and the root on vda2; the direct-boot
    # image is MBR with one ext4 partition on vda1, which is why `common` says
    # /dev/vda1 and why this has to override it. Measured 2026-08-19 with the
    # first firmware boot, which got as far as the initrd and then said
    # `EXT4-fs (vda1): VFS: Can't find ext4 filesystem` -- it was reading the
    # ESP.
    #
    # BY LABEL rather than by node, and only here: an orchestrator that
    # attaches this image is under no obligation to make it vda, and there is
    # no host-written command line to correct it with. nixos/ESP are what
    # nixos/lib/make-disk-image.nix writes (`mkfs.ext4 -L nixos`,
    # `mkfs.vfat -n ESP`).
    fileSystems."/" = lib.mkForce {
      device = "/dev/disk/by-label/nixos";
      fsType = "ext4";
      autoResize = true;
    };
    fileSystems."/boot" = {
      device = "/dev/disk/by-label/ESP";
      fsType = "vfat";
      options = [ "umask=0077" ];
    };

    # There is no host to write a command line here, so the image carries its
    # own -- including the console the rig reads the boot on.
    boot.kernelParams = [ "console=ttyS0" "net.ifnames=0" "loglevel=4" ];
  };

  mkSystem = extra: lib.nixosSystem {
    inherit system;
    modules = [ guestModule common identity extra ];
  };

  nixos = mkSystem directBoot;
  nixosUefi = mkSystem uefiBoot;

  mkDiskImage = { config, pkgs, name, partitionTableType, installBootLoader }:
    import "${nixpkgs}/nixos/lib/make-disk-image.nix" {
      inherit config pkgs lib partitionTableType installBootLoader name;
      format = "qcow2";
      diskSize = "auto";
      # Room for the torch venv (~2.5 GiB, probe/python's rlprobe and
      # convburn are what the gate's torch stage runs) plus the payload's
      # ~220 MiB of NVIDIA userspace. Without it the first `up` fills the
      # image before the venv finishes.
      additionalSpace = "4096M";
      copyChannel = false;
    };

  # ---- the two outputs ----------------------------------------------------
  # A directory rather than a bare qcow2: direct kernel boot needs three
  # files and one fact (which `init=` this image's system is), and a rig that
  # has to guess any of them is a rig that boots the wrong generation.
  image =
    let
      cfg = nixos.config;
      pkgs = nixos.pkgs;
      disk = mkDiskImage {
        config = cfg;
        inherit pkgs;
        name = "leandro-guest-nixos";
        partitionTableType = "legacy";
        installBootLoader = false;
      };
    in pkgs.runCommand "leandro-guest-image-${driverVersion}" { } ''
      mkdir -p $out
      ln -s ${disk}/nixos.qcow2 $out/rootfs.qcow2
      ln -s ${cfg.system.build.kernel}/bzImage $out/kernel
      ln -s ${cfg.system.build.initialRamdisk}/initrd $out/initrd
      cat > $out/image.env <<EOF
      # Sourced by scripts/lib/config.sh (LEA_NIXOS_DIR/image.env). Written by
      # nix/guest-image.nix; every value is a FACT about the image beside it.
      LEA_NIXOS_INIT=${cfg.system.build.toplevel}/init
      LEA_NIXOS_CMDLINE_BASE='root=/dev/vda1 rw console=ttyS0 net.ifnames=0 loglevel=4'
      LEA_NIXOS_KERNEL_VERSION=${cfg.boot.kernelPackages.kernel.modDirVersion}
      LEA_NIXOS_USER=${guestUser}
      LEA_NIXOS_DRIVER=${driverVersion}
      LEA_NIXOS_TOPLEVEL=${cfg.system.build.toplevel}
      LEA_NIXOS_USERSPACE=${cfg.services.leandro-guest.nvidiaUserspaceDir}
      EOF
    '';

  imageUefi =
    let
      cfg = nixosUefi.config;
      pkgs = nixosUefi.pkgs;
      disk = mkDiskImage {
        config = cfg;
        inherit pkgs;
        name = "leandro-guest-nixos-uefi";
        partitionTableType = "efi";
        installBootLoader = true;
      };
    in pkgs.runCommand "leandro-guest-image-uefi-${driverVersion}" { } ''
      mkdir -p $out
      ln -s ${disk}/nixos.qcow2 $out/rootfs.qcow2
      cat > $out/image.env <<EOF
      # A UEFI-bootable image: no kernel or initrd beside it, because the
      # firmware reads them out of the ESP. It carries the SAME
      # nixosConfiguration as the direct-boot one.
      LEA_NIXOS_KERNEL_VERSION=${cfg.boot.kernelPackages.kernel.modDirVersion}
      LEA_NIXOS_USER=${guestUser}
      LEA_NIXOS_DRIVER=${driverVersion}
      LEA_NIXOS_TOPLEVEL=${cfg.system.build.toplevel}
      EOF
    '';
in {
  inherit nixos nixosUefi image imageUefi;
}
