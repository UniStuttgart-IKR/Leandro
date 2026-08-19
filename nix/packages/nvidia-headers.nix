# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# NVIDIA's open-gpu-kernel-modules at exactly DRIVER_VERSION, headers only:
# the four include directories crates/nvrm-sys/build.rs reads. A sparse
# partial clone, like `build.sh vendor` -- but this is a fixed-output
# derivation, so the store path is reproducible per version.
#
# NOT a replacement for vendor/open-gpu-kernel-modules in a checkout: the
# guest-side NVKMS build ships the full source tree from there.
{ lib, fetchFromGitHub, driverVersion }:
fetchFromGitHub {
  name = "open-gpu-kernel-modules-headers-${driverVersion}";
  owner = "NVIDIA";
  repo = "open-gpu-kernel-modules";
  rev = driverVersion;
  sparseCheckout = [
    "kernel-open/common/inc"
    "src/common/sdk/nvidia/inc"
    "src/nvidia/arch/nvalloc/unix/include"
    "kernel-open/nvidia-uvm"
  ];
  # Fill from `nix build .#nvidia-headers` (the mismatch message names the
  # right one) whenever DRIVER_VERSION moves.
  hash = "sha256-fQtY7jNlgWIbJRf/LgYyCpwhpEB+NdWlanUKbuW9m3w=";
}
