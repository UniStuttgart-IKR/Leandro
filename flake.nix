# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# Leandro on Nix: the host-side binaries as packages, a NixOS module for the
# host (bridge, taps, NAT, driver persistence, binaries on PATH, optionally
# the scripts), and the development shell.
#
#   nix build .#vhost-user-nvrm .#cloud-hypervisor      the two things a host needs
#   nix build .#guest-modules                            nvrm_nodes.ko + virtio_nvrm.ko
#   nix build .#guest-image                              the NixOS guest: kernel + initrd + qcow2
#   nix build .#guest-image-uefi                         the same system, UEFI-bootable
#   nix develop                                          the dev shell (cargo, bindgen, qemu-img, ...)
#   nix develop .#prebuilt                               the same, with LEA_BIN_DIR/LEA_CH pointing at
#                                                        store binaries -- showcase.sh up without a build
#   nix flake check                                      evaluates the module against nix/example-host.nix
#
# In a NixOS configuration:
#   inputs.leandro.url = "github:<owner>/leandro";
#   imports = [ inputs.leandro.nixosModules.default ];
#   services.leandro = { enable = true; user = "me"; dev.enable = true; };
#
# In a GUEST configuration (nixosModules.guest):
#   imports = [ inputs.leandro.nixosModules.guest ];
#   services.leandro-guest = { enable = true; params.file = ./params.txt; users = [ "me" ]; };
#
# A ready-made guest is nix/guest-image.nix, and `scripts/build.sh bake
# --nixos` builds it into LEA_VM_DIR. Booted and gated on 2026-08-19: the
# compute gate passes on it and four of them share one GPU. The display path
# is NOT implemented there -- see DEVELOPMENT.md section 10.
#
# WARNING: the NVIDIA driver is NOT provided here and cannot be -- the guest
# is handed the host's own libcuda, which is not redistributable and has to
# match the running nvidia.ko exactly (DRIVER_VERSION). On NixOS that is
# hardware.nvidia in the system configuration; the module asserts it is
# there and warns when the version differs.
#
# Flakes only see git-tracked files: `git add` new nix/ files before
# `nix build`. vendor/, target/ and vm/ are ignored and invisible here, which
# is what makes the packages hermetic.
{
  description = "Leandro: cooperative GPU paravirtualisation over virtio-nvrm -- host side";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    flake-compat = { url = "github:edolstra/flake-compat"; flake = false; };
  };

  outputs = { self, nixpkgs, ... }:
    let
      system = "x86_64-linux";
      lib = nixpkgs.lib;
      pkgs = import nixpkgs { inherit system; overlays = [ self.overlays.default ]; };
      driverVersion = lib.fileContents ./DRIVER_VERSION;   # "610.57.04"
      chVersion = lib.fileContents ./CH_VERSION;           # "v53.0"

      # The GUEST side, declared once and built two ways. Kept in the `let`
      # rather than under `packages` because the flake's output set is not
      # recursive and both the packages and the nixosConfigurations below
      # need it. See nix/guest-image.nix -- including why microvm.nix was
      # read and not used.
      guestModule = import ./nix/module-guest.nix {
        guestModulesFor = kernel: pkgs.callPackage ./nix/packages/guest-modules.nix {
          src = ./.; inherit kernel;
        };
      };
      guestImage = import ./nix/guest-image.nix {
        inherit lib nixpkgs system driverVersion guestModule;
        # config.sh's LEA_GUEST_USER default. It is a build-time constant
        # here and a runtime variable there, so the image RECORDS the name it
        # was built with (image.env) and `build.sh bake --nixos` compares the
        # two rather than assuming they agree.
        guestUser = "leandro";
      };
    in {
      overlays.default = final: prev: {
        leandro-nvidia-headers = final.callPackage ./nix/packages/nvidia-headers.nix { inherit driverVersion; };
        leandro = final.callPackage ./nix/packages/leandro.nix {
          nvidiaHeaders = final.leandro-nvidia-headers; src = ./.;
        };
        leandro-cloud-hypervisor = final.callPackage ./nix/packages/cloud-hypervisor.nix {
          inherit chVersion; patchDir = ./patches;
        };
        leandro-scripts = final.callPackage ./nix/packages/leandro-scripts.nix {
          src = ./.; leandro = final.leandro; cloud-hypervisor = final.leandro-cloud-hypervisor;
        };
        # The GUEST modules, built against ONE kernel. The default is
        # nixpkgs' default kernel; a guest configuration gets its own
        # (nixosModules.guest builds it for config.boot.kernelPackages).
        leandro-guest-modules = final.callPackage ./nix/packages/guest-modules.nix {
          src = ./.; kernel = final.linuxKernel.packages.linux_6_12.kernel;
        };
      };

      packages.${system} = {
        default = pkgs.leandro;
        leandro = pkgs.leandro;
        vhost-user-nvrm = pkgs.leandro.override { crates = [ "vhost-user-nvrm" ]; };
        vhost-user-input = pkgs.leandro.override { crates = [ "vhost-user-input" ]; };
        cloud-hypervisor = pkgs.leandro-cloud-hypervisor;
        nvidia-headers = pkgs.leandro-nvidia-headers;
        leandro-scripts = pkgs.leandro-scripts;
        guest-modules = pkgs.leandro-guest-modules;
        # kernel + initrd + qcow2 + image.env, for direct kernel boot.
        guest-image = guestImage.image;
        # The same system, UEFI-bootable, for an orchestrator that boots
        # images the normal way.
        guest-image-uefi = guestImage.imageUefi;
      };

      nixosModules.default = import ./nix/module.nix {
        leandroPackages = self.packages.${system};
        inherit driverVersion;
      };
      nixosModules.host = self.nixosModules.default;

      # The GUEST side: the two kernel modules as a module package, the
      # coexistence order, the params unit, the groups. It does NOT provide
      # NVIDIA's userspace and cannot (see nix/module-guest.nix).
      nixosModules.guest = guestModule;

      # An evaluation-only smoke test of the module: `nix flake check` walks
      # the assertions and the version warning without a NixOS machine.
      nixosConfigurations.example = lib.nixosSystem {
        inherit system;
        modules = [ self.nixosModules.default ./nix/example-host.nix ];
      };

      # The same for the guest module.
      nixosConfigurations.example-guest = lib.nixosSystem {
        inherit system;
        modules = [ self.nixosModules.guest ./nix/example-guest.nix ];
      };

      # The real guest, both ways. These are complete systems rather than
      # evaluation stubs: `nix build .#guest-image` builds the disk under
      # them, and `nix flake check` evaluates them.
      nixosConfigurations.guest = guestImage.nixos;
      nixosConfigurations.guest-uefi = guestImage.nixosUefi;

      devShells.${system} =
        let
          # What every build and every script reaches for. NixOS has no
          # FHS, so a toolchain from anywhere else generally cannot link.
          tools = with pkgs; [
            # Rust: rustup honours rust-toolchain.toml (the pinned 1.89.0),
            # so cargo in this shell is the pinned one, not nixpkgs'.
            rustup pkg-config openssl zstd
            # C: probes, the table interpreter, cloud-hypervisor's build.
            gcc gnumake binutils
            # bindgen for crates/nvrm-sys: LIBCLANG_PATH via the hook.
            rustPlatform.bindgenHook
            # Disk and cloud-init images.
            qemu-utils dosfstools mtools e2fsprogs
            # Network setup: bridge, taps, NAT.
            iproute2 iptables
            # Fetching and general shell work; procps for `pkill -x`.
            git curl openssh gnutar gawk gnugrep gnused procps util-linux coreutils python3
            # test.sh check: edid-decode for the EDID step, shellcheck for the shell job.
            edid-decode shellcheck
          ];
          hook = ''
            echo "leandro dev shell"
            if [ ! -r /proc/driver/nvidia/version ]; then
              echo "  WARNING: no NVIDIA driver loaded (/proc/driver/nvidia/version)."
              echo "  Enable hardware.nvidia in the SYSTEM configuration -- it cannot come from this shell."
            else
              echo "  host driver: $(grep -oE '[0-9]+\.[0-9]+\.[0-9]+' /proc/driver/nvidia/version | head -1)"
              echo "  this tree targets: ${driverVersion}"
            fi
            [ -d /run/opengl-driver/lib ] && echo "  driver userspace: /run/opengl-driver/lib" \
              || echo "  NOTE: /run/opengl-driver/lib absent -- if libcuda cannot be found, set LEA_NVIDIA_LIB_DIR."
            echo "  LEA_CH=$LEA_CH"
          '';
        in {
          default = pkgs.mkShell {
            name = "leandro-dev";
            nativeBuildInputs = tools;
            # The patched cloud-hypervisor is the expensive artefact nobody
            # wants to rebuild; the patches are pinned in the flake, so
            # `build.sh ch` becomes optional here.
            LEA_CH = "${pkgs.leandro-cloud-hypervisor}/bin/cloud-hypervisor";
            shellHook = hook;
          };
          # Everything from the store: `scripts/showcase.sh up` without
          # `build.sh cargo` or `build.sh ch`.
          prebuilt = pkgs.mkShell {
            name = "leandro-prebuilt";
            nativeBuildInputs = tools;
            LEA_CH = "${pkgs.leandro-cloud-hypervisor}/bin/cloud-hypervisor";
            LEA_BIN_DIR = "${pkgs.leandro}/bin";
            LEA_TRACE_LIB = "${pkgs.leandro}/lib/libnvrm_trace.so";
            shellHook = hook + ''echo "  LEA_BIN_DIR=$LEA_BIN_DIR"'';
          };
        };

      apps.${system} = {
        showcase = { type = "app"; program = "${pkgs.leandro-scripts}/bin/leandro-showcase"; meta.description = "the rig and the guided demo"; };
        test = { type = "app"; program = "${pkgs.leandro-scripts}/bin/leandro-test"; meta.description = "the check band and the gates"; };
        bench = { type = "app"; program = "${pkgs.leandro-scripts}/bin/leandro-bench"; meta.description = "the measurement track"; };
      };

      # Cheap checks only: the scripts package and the module evaluation.
      # The Rust workspace and cloud-hypervisor are minutes each and are
      # built on demand (`nix build .#leandro`), not on every check.
      checks.${system} = {
        leandro-scripts = pkgs.leandro-scripts;
        module-eval = pkgs.writeText "leandro-module-eval"
          (builtins.unsafeDiscardStringContext
            self.nixosConfigurations.example.config.system.build.toplevel.drvPath);
        guest-module-eval = pkgs.writeText "leandro-guest-module-eval"
          (builtins.unsafeDiscardStringContext
            self.nixosConfigurations.example-guest.config.system.build.toplevel.drvPath);
        # The guest IMAGE, evaluated and not built: it is a 4.7 GiB qcow2 and
        # a kernel, i.e. exactly the kind of artefact this check band leaves
        # to `nix build .#guest-image`. What is worth catching on every check
        # is an image that no longer EVALUATES -- a module option renamed, an
        # assertion tripped, a kernel that the guest modules refuse.
        guest-image-eval = pkgs.writeText "leandro-guest-image-eval"
          (builtins.unsafeDiscardStringContext
            (guestImage.image.drvPath + " " + guestImage.imageUefi.drvPath));
      };

      formatter.${system} = pkgs.nixfmt;
    };
}
