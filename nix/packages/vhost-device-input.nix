# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
{ lib, rustPlatform, fetchFromGitHub }:
rustPlatform.buildRustPackage {
  pname = "vhost-device-input";
  version = "0.1.0-unstable-2026-09-19";
  src = fetchFromGitHub {
    owner = "rust-vmm";
    repo = "vhost-device";
    rev = "93f867e1b00061d425686e4faa5f2ca40125f18c";
    sha256 = "0ldmn172vvinb6dqhi37z32pbqxqmh2rvwsbisscr9ad3n90bkhn";
  };
  cargoHash = "sha256-uUmGwlMEefPYuk2J4ObcVQLi4JYAzZ8b1/GuNV7ZSl4=";
  cargoBuildFlags = [ "-p" "vhost-device-input" ];
  cargoTestFlags = [ "-p" "vhost-device-input" ];
  # The upstream test mock registers stdin with epoll; /dev/null is not pollable.
  preCheck = ''
    exec < <(printf '\n')
  '';
  postInstall = ''
    install -Dm644 LICENSE-APACHE $out/share/licenses/vhost-device-input/LICENSE-APACHE
    install -Dm644 LICENSE-BSD-3-Clause $out/share/licenses/vhost-device-input/LICENSE-BSD-3-Clause
  '';
  meta = {
    description = "rust-vmm virtio-input backend for evdev devices";
    homepage = "https://github.com/rust-vmm/vhost-device/tree/main/vhost-device-input";
    license = with lib.licenses; [ asl20 bsd3 ];
    mainProgram = "vhost-device-input";
    platforms = lib.platforms.linux;
  };
}
