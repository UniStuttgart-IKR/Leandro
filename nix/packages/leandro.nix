# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# The Rust workspace: vhost-user-nvrm (the host end of virtio-nvrm),
# vhost-user-input, nvrm-genhdr, mmapping, smipids, vsockconnect, and
# libnvrm_trace.so.
#
# `crates` narrows the build to named workspace members (`packages
# .vhost-user-nvrm` is this derivation with crates = ["vhost-user-nvrm"]);
# empty builds the whole workspace.
#
# crates/nvrm-sys generates its bindings with bindgen over
# vendor/open-gpu-kernel-modules at DRIVER_VERSION; build.rs canonicalises
# that path, so a symlink into the store is what it gets here.
{ lib, rustPlatform, nvidiaHeaders, src, crates ? [ ] }:
rustPlatform.buildRustPackage {
  pname = if crates == [ ] then "leandro" else lib.concatStringsSep "-" crates;
  version = "0.0.1";
  src = lib.fileset.toSource {
    root = src;
    fileset = lib.fileset.unions [
      (src + "/Cargo.toml") (src + "/Cargo.lock") (src + "/DRIVER_VERSION")
      # The fuzz target is a separate cargo project (nightly, sanitizers)
      # and stays out of the store build.
      (lib.fileset.difference (src + "/crates") (src + "/crates/vhost-user-nvrm/fuzz"))
    ];
  };
  # No git dependencies in Cargo.lock, so no hash to maintain: every
  # Cargo.lock change is picked up by itself.
  cargoLock.lockFile = src + "/Cargo.lock";
  nativeBuildInputs = [ rustPlatform.bindgenHook ];
  postPatch = ''
    mkdir -p vendor
    ln -s ${nvidiaHeaders} vendor/open-gpu-kernel-modules
  '';
  cargoBuildFlags = lib.concatMap (c: [ "-p" c ]) crates;
  cargoTestFlags = lib.concatMap (c: [ "-p" c ]) crates;
  # The tests are the GPU-free ones test.sh check runs; the sandbox has no
  # /dev/nvidia*, exactly like a CI runner.
  meta = {
    description = "Leandro host side: vhost-user-nvrm and its tools";
    license = lib.licenses.mit;
    mainProgram = "vhost-user-nvrm";
    platforms = [ "x86_64-linux" ];
  };
}
