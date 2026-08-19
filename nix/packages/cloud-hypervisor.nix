# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# cloud-hypervisor at CH_VERSION with this repository's patch series
# (patches/0001-*, 0002-*): the generic vhost-user device learns SHARED
# MEMORY REGIONS, which is what every RM mapping into the guest runs over --
# without it the device comes up and ioctls work, but there is no
# host-visible window and hence no CUDA. Its own derivation rather than an
# override of nixpkgs' cloud-hypervisor: that one is whatever version the
# host's nixpkgs ships, and the SHMEM patch is written against exactly v53.0.
{ lib, rustPlatform, fetchFromGitHub, pkg-config, openssl, zstd, chVersion, patchDir }:
rustPlatform.buildRustPackage {
  pname = "cloud-hypervisor";
  version = lib.removePrefix "v" chVersion;
  src = fetchFromGitHub {
    owner = "cloud-hypervisor";
    repo = "cloud-hypervisor";
    rev = chVersion;
    hash = "sha256-fPTGf8bAITDA8QwllWbbGXA7tJ6p/SxRDfcBQVRvCTI=";
  };
  # The patches touch no Cargo.lock, so the vendor hash is upstream's.
  cargoHash = "sha256-+RbW/9ap/69MyODUk/bHBlH6ZuqYYIyKaarYSMQ2G7w=";
  # The whole series in numeric order -- the same rule build.sh ch follows.
  patches = lib.sort lib.lessThan (lib.filter (p: lib.hasSuffix ".patch" (toString p))
    (lib.filesystem.listFilesRecursive patchDir));
  # Counter-check on the patched tree, as build.sh ch does: the series brings
  # exactly ONE capability, and without it there is no window.
  postPatch = ''
    grep -q get_shmem_config virtio-devices/src/vhost_user/generic_vhost_user.rs \
      || { echo "patch marker (SHMEM) missing from the source"; exit 1; }
  '';
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ openssl zstd ];
  env.OPENSSL_NO_VENDOR = true;
  env.ZSTD_SYS_USE_PKG_CONFIG = true;
  cargoBuildFlags = [ "--bin" "cloud-hypervisor" ];
  # The test suite wants /dev/kvm, /dev/net/tun and io_uring; none of it is
  # available in the sandbox and none of it is ours.
  doCheck = false;
  meta = {
    description = "cloud-hypervisor ${chVersion} with Leandro's generic-vhost-user SHMEM patches";
    license = lib.licenses.asl20;
    mainProgram = "cloud-hypervisor";
    platforms = [ "x86_64-linux" ];
  };
}
