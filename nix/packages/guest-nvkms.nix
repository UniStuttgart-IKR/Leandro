# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# NVIDIA's own nvidia-modeset.ko and nvidia-drm.ko for a GUEST, unmodified,
# built against virtio_nvrm instead of nvidia.ko.
#
#   nix build .#guest-nvkms
#   boot.extraModulePackages = [ ... ];   nixosModules.guest does it with display.enable
#
# nvidia-modeset links against exactly ONE symbol of nvidia.ko,
# nvidia_get_rm_ops, and virtio_nvrm exports it (nvrm_kapi.h); nvidia-drm
# links against nvidia-modeset. So the two modules are NVIDIA's source at
# exactly DRIVER_VERSION -- virtio_nvrm refuses any other NV_VERSION_STRING
# -- with KBUILD_EXTRA_SYMBOLS pointing at guest-modules' Module.symvers.
# That is the same build scripts/lib/provision.sh (lea_guest_build_nvkms)
# runs inside an Ubuntu guest, as a derivation.
#
# Two steps, not NVIDIA's top-level `make modules`: that one also builds
# nv-kernel.o and nvidia.ko, which a guest must not have. The OS-agnostic
# half of NVKMS (src/nvidia-modeset, C++) is built first and handed to Kbuild
# as the .o_binary the kernel-open tree expects. The recipe around it is
# nixpkgs' nvidia-x11/kernel-modules.nix.
#
# The source is NVIDIA's open-gpu-kernel-modules, MIT; the modules it links
# into are dual MIT/GPLv2 (its COPYING).
{ lib, stdenv, fetchFromGitHub, kernel
, guestModules, driverVersion, hash }:

let
  kdir = "${kernel.dev}/lib/modules/${kernel.modDirVersion}";
  # What linuxPackages hands its module packages as kernelModuleMakeFlags;
  # spelled out because this is called outside that scope.
  flags = kernel.commonMakeFlags ++ [
    "KBUILD_OUTPUT=${kdir}/build"
    "IGNORE_PREEMPT_RT_PRESENCE=1"
    "SYSSRC=${kdir}/source"
    "SYSOUT=${kdir}/build"
    "DATE="
    "TARGET_ARCH=${stdenv.hostPlatform.parsed.cpu.name}"
  ];
in
stdenv.mkDerivation {
  pname = "leandro-guest-nvkms";
  version = "${driverVersion}-${kernel.version}";

  src = fetchFromGitHub {
    owner = "NVIDIA";
    repo = "open-gpu-kernel-modules";
    tag = driverVersion;
    inherit hash;
  };

  nativeBuildInputs = kernel.moduleBuildDependencies;

  dontStrip = true;
  dontPatchELF = true;

  buildPhase = ''
    runHook preBuild
    make -C src/nvidia-modeset -j$NIX_BUILD_CORES ${lib.escapeShellArgs flags}
    ln -sf ../../src/nvidia-modeset/_out/Linux_${stdenv.hostPlatform.parsed.cpu.name}/nv-modeset-kernel.o \
      kernel-open/nvidia-modeset/nv-modeset-kernel.o_binary
    make -C kernel-open modules -j$NIX_BUILD_CORES ${lib.escapeShellArgs flags} \
      NV_KERNEL_MODULES="nvidia-modeset nvidia-drm" \
      KBUILD_EXTRA_SYMBOLS=${guestModules}/share/leandro/virtio_nvrm.symvers
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    d=$out/lib/modules/${kernel.modDirVersion}/extra
    install -D -m444 kernel-open/nvidia-modeset.ko $d/nvidia-modeset.ko
    install -D -m444 kernel-open/nvidia-drm.ko $d/nvidia-drm.ko
    runHook postInstall
  '';

  meta = {
    description = "NVIDIA nvidia-modeset and nvidia-drm, linked against Leandro's virtio_nvrm";
    homepage = "https://github.com/NVIDIA/open-gpu-kernel-modules";
    license = with lib.licenses; [ mit gpl2Only ];
    platforms = [ "x86_64-linux" ];
  };
}
