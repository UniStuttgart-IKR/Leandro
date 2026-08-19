# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# A stub NixOS GUEST that imports the guest module -- evaluated by `nix
# flake check` (nixosConfigurations.example-guest), never built or booted.
# It exists so the module's options, its assertions and the modprobe.d
# ordering are exercised on every check without a guest.
#
# The params below are the SHAPE of /proc/driver/nvidia/params, not a
# recording of one rig's file: what the guest needs is its own host's copy
# (scripts/lib/provision.sh ships it as ~/gpu/params.txt).
{ ... }: {
  services.leandro-guest = {
    enable = true;
    params.text = ''
      ResmanDebugLevel: 4294967295
      RmLogonRC: 1
      ModifyDeviceFiles: 1
      DeviceFileUID: 0
      DeviceFileGID: 0
      DeviceFileMode: 438
    '';
    users = [ "leandro" ];
    maxPinMiB = 2048;
    bdfMediation = true;
    nvidiaUserspaceDir = "/opt/nvrm/lib";
  };
  users.users.leandro = { isNormalUser = true; };
  boot.loader.grub.device = "nodev";
  fileSystems."/" = { device = "/dev/null"; fsType = "ext4"; };
  system.stateVersion = "26.05";
}
