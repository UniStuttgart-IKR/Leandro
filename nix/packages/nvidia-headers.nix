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
  # THIS LIST MUST MATCH `INCLUDE_DIRS` IN crates/nvrm-sys/build.rs, which is
  # what bindgen is actually handed. They drifted once and the flake was the
  # only thing that noticed: the NVKMS work (OPEN-QUESTIONS 48) added
  # kernel-open/nvidia-modeset to build.rs and to wrapper.h, and not here, so
  # `nix build` died on `'nvkms-ioctl.h' file not found` while every local
  # build was fine -- a checkout has the whole tree and only this derivation
  # is sparse. `scripts/test.sh check` compares the two lists now.
  sparseCheckout = [
    "kernel-open/common/inc"
    "src/common/sdk/nvidia/inc"
    "src/nvidia/arch/nvalloc/unix/include"
    "kernel-open/nvidia-uvm"
    "kernel-open/nvidia-modeset"
  ];
  # Fill from `nix build .#nvidia-headers` (the mismatch message names the
  # right one) whenever DRIVER_VERSION moves.
  #
  # The COMMIT this resolves to is recorded beside the hash on purpose. `rev`
  # is a TAG, and a tag is mutable: without the commit, a future mismatch
  # cannot tell "upstream retagged" from "the hash was wrong", and those want
  # opposite responses. 610.57.04 is an ANNOTATED tag -- `refs/tags/610.57.04`
  # is the tag object 8d9087246664ad54dab9af48657a62d5c1d82d6a and its peel
  # `^{}` is the commit:
  #
  #   e4a5faa2567f28c8eabe0ebb6422b6d0abcf37eb  (2026-08-03)
  #
  # `git ls-remote <url> 'refs/tags/<v>*'` shows both lines; asking for the
  # bare tag name shows only the tag object, which reads like a moved tag and
  # is not one.
  #
  # Corrected twice on 2026-08-21, for two different reasons. The value it
  # started with, sha256-fQtY7jNlgWIbJRf/LgYyCpwhpEB+NdWlanUKbuW9m3w=, had
  # never built -- it is the file's first and only commit and the flake job
  # failed on it. NOT a moved tag (the peel above is intact) and NOT a
  # nixpkgs bump: flake.lock is committed and pins nixpkgs at 0dd31db7e6db,
  # so the fetcher never changed underneath it. Then it moved again because
  # the fifth sparse directory was added, which is a content change and must
  # change the hash.
  #
  # Verified rather than pasted, because a fixed-output hash is the only
  # integrity check on third-party source here. Two machines on two networks
  # produced the same hash for the same input, and the 862 files it covers
  # are byte-for-byte identical to vendor/open-gpu-kernel-modules -- which
  # `scripts/build.sh vendor` clones by a completely different route and
  # every gate builds against -- across all five directories above.
  #
  # A note for whoever changes this next: adding a sparseCheckout path does
  # NOT invalidate the store path on its own. A fixed-output derivation's
  # output path comes from the HASH, so nix will happily hand back the old,
  # smaller tree and the build fails somewhere confusing. Force the fetch by
  # putting a deliberately wrong hash in and reading the `got:` line.
  hash = "sha256-KOdv0WpLEQ04iyv7K8y0ZuAsq1J6pXszntoSZelKHpo=";
}
