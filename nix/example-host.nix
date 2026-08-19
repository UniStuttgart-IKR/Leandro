# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# A stub NixOS host that imports the module -- evaluated by `nix flake
# check` (nixosConfigurations.example), never built or booted. It exists so
# the module's assertions, options and the driver-version warning are
# exercised on every check without a NixOS machine.
{ ... }: {
  services.leandro = {
    enable = true;
    user = "leandro";
    dev.enable = true;
    backend.enable = true;
    nat.externalInterface = "eth0";
  };
  users.users.leandro = { isNormalUser = true; extraGroups = [ "kvm" ]; };
  services.xserver.videoDrivers = [ "nvidia" ];
  hardware.nvidia.open = true;
  nixpkgs.config.allowUnfree = true;
  boot.loader.grub.device = "nodev";
  fileSystems."/" = { device = "/dev/null"; fsType = "ext4"; };
  system.stateVersion = "26.05";
}
