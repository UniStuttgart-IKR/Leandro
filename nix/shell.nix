# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# `nix-shell nix/shell.nix` -- the flake's devShells.default for a nix
# without flakes enabled, via flake-compat (pinned in ../flake.lock).
(import
  (let lock = builtins.fromJSON (builtins.readFile ../flake.lock); in
   fetchTarball {
     url = "https://github.com/edolstra/flake-compat/archive/${lock.nodes.flake-compat.locked.rev}.tar.gz";
     sha256 = lock.nodes.flake-compat.locked.narHash;
   })
  { src = ../.; }).shellNix
