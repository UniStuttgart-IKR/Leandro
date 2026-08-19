# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# NixOS host module: what scripts/showcase.sh net up does by hand on Arch,
# declared -- a bridge, user-owned taps, NAT out of the box, the driver's
# persistence mode, and the host binaries on PATH. Optionally the wrapped
# scripts (dev.enable) and a template unit for backends started outside the
# scripts (backend.enable).
#
#   services.leandro = {
#     enable = true;
#     user = "silas";              # owns the taps; cloud-hypervisor runs as this user
#     dev.enable = true;           # leandro-showcase, leandro-test, ... on PATH
#     nat.externalInterface = "enp39s0";   # or leave null: masquerade on any egress
#   };
#
# WARNING: the NVIDIA driver is the system's job (hardware.nvidia), not this
# module's -- the userspace is not redistributable and the guest is handed
# the host's own libcuda. This asserts the driver is configured and WARNS
# when its version is not DRIVER_VERSION: a mismatch is misread struct
# offsets in the guest, not a comfort problem.
{ leandroPackages, driverVersion }:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.leandro;
  tapNames = lib.genList (i: "${cfg.taps.prefix}${toString i}") cfg.taps.count;
  # 192.168.100.1/24 -> 192.168.100.0/24
  octets = lib.splitString "." cfg.bridge.address;
  subnet = "${lib.concatStringsSep "." (lib.take 3 octets)}.0/${toString cfg.bridge.prefixLength}";
in {
  options.services.leandro = {
    enable = lib.mkEnableOption "the Leandro host rig (bridge, taps, NAT, driver persistence, binaries)";
    user = lib.mkOption {
      type = lib.types.str;
      description = ''
        The user who runs the guests: owner of the taps (cloud-hypervisor
        opens them unprivileged) and of the optional backend units. Must be
        declared in users.users; give it extraGroups = [ "kvm" ].
      '';
    };
    package = lib.mkOption {
      type = lib.types.package;
      default = leandroPackages.leandro;
      description = "vhost-user-nvrm, vhost-user-input and the tools.";
    };
    cloudHypervisorPackage = lib.mkOption {
      type = lib.types.package;
      default = leandroPackages.cloud-hypervisor;
      description = "cloud-hypervisor with the generic-vhost-user SHMEM patches.";
    };
    scriptsPackage = lib.mkOption {
      type = lib.types.package;
      default = leandroPackages.leandro-scripts.override {
        leandro = cfg.package; cloud-hypervisor = cfg.cloudHypervisorPackage;
      };
      defaultText = lib.literalExpression "leandro-scripts, wrapped around package and cloudHypervisorPackage";
      description = "The wrapped scripts (leandro-showcase and friends), installed by dev.enable.";
    };
    bridge = {
      name = lib.mkOption { type = lib.types.str; default = "br-poco"; description = "Bridge name (LEA_BRIDGE)."; };
      address = lib.mkOption { type = lib.types.str; default = "192.168.100.1"; description = "Bridge address, the guests' gateway (LEA_GATEWAY)."; };
      prefixLength = lib.mkOption { type = lib.types.int; default = 24; description = "Prefix length of the guest subnet (LEA_NETMASK)."; };
      trusted = lib.mkOption { type = lib.types.bool; default = false; description = "Add the bridge to networking.firewall.trustedInterfaces."; };
    };
    taps = {
      count = lib.mkOption { type = lib.types.int; default = 8; description = "tap0..tap<count-1> (LEA_MAX_VMS)."; };
      prefix = lib.mkOption { type = lib.types.str; default = "tap"; description = "Tap name prefix (LEA_TAP_PREFIX)."; };
    };
    nat = {
      enable = lib.mkOption { type = lib.types.bool; default = true; description = "Masquerade the guest subnet out of the host."; };
      externalInterface = lib.mkOption {
        type = lib.types.nullOr lib.types.str; default = null;
        description = "The uplink. null = masquerade on any egress interface (the declarative 'auto').";
      };
      forwardFirst = lib.mkOption {
        type = lib.types.bool; default = false;
        description = ''
          Insert ACCEPT rules for the guest subnet at position 1 of the
          FORWARD chain (iptables backend only). What the imperative
          setup does because Docker sets FORWARD to DROP; NixOS' own
          networking.nat rules are appended and normally suffice.
        '';
      };
    };
    persistence = lib.mkOption {
      type = lib.types.bool; default = true;
      description = "Run nvidia-persistenced (hardware.nvidia.nvidiaPersistenced). The gates refuse without it: off, the native reference is 58 % slower.";
    };
    backend = {
      enable = lib.mkEnableOption "the leandro-backend@<name>.service template (one vhost-user-nvrm per instance at /run/leandro/<name>/nvrm.sock)";
      environment = lib.mkOption {
        type = lib.types.attrsOf lib.types.str; default = { };
        example = { LEA_MANAGED_COMPAT = "1"; LEA_VRAM_LIMIT_MIB = "2048"; };
        description = "Environment for the backend units (LEA_DEBUG, LEA_MANAGED_COMPAT, LEA_MAX_PIN_MIB, LEA_VRAM_LIMIT_MIB).";
      };
    };
    dev.enable = lib.mkEnableOption "the wrapped scripts on PATH (leandro-showcase, leandro-test, leandro-bench, leandro-build)";
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      { assertion = config.hardware.nvidia.enabled or false;
        message = ''
          services.leandro needs the NVIDIA driver from the system
          configuration: services.xserver.videoDrivers = [ "nvidia" ] plus
          hardware.nvidia.open, at exactly version ${driverVersion}
          (DRIVER_VERSION). The guest is handed the host's libcuda.
        ''; }
      { assertion = config.users.users ? ${cfg.user};
        message = "services.leandro.user = \"${cfg.user}\" must be a user declared in users.users (with extraGroups = [ \"kvm\" ])."; }
    ];
    warnings = lib.optional
      ((config.hardware.nvidia.enabled or false) && config.hardware.nvidia.package.version != driverVersion)
      ''
        services.leandro: the host driver is ${config.hardware.nvidia.package.version},
        this Leandro targets ${driverVersion}. The ioctl layouts are version
        specific -- expect nvidia-smi in the guest to misread memory. Pin the
        driver (nvidiaPackages.mkDriver { version = "${driverVersion}"; ... })
        or rebuild Leandro with `build.sh --driver auto`.
      '';

    environment.systemPackages = [ cfg.package cfg.cloudHypervisorPackage ]
      ++ lib.optional cfg.dev.enable cfg.scriptsPackage;

    # The bridge and its taps. vnet_hdr is not needed at creation:
    # cloud-hypervisor sets IFF_VNET_HDR itself when it opens the tap, and
    # the kernel re-applies TUN_FEATURES flags on an existing device.
    networking.bridges.${cfg.bridge.name}.interfaces = tapNames;
    networking.interfaces = {
      ${cfg.bridge.name} = {
        useDHCP = false;
        ipv4.addresses = [ { inherit (cfg.bridge) address prefixLength; } ];
      };
    } // lib.genAttrs tapNames (_: {
      virtual = true;
      virtualType = "tap";
      virtualOwner = cfg.user;
      useDHCP = false;
    });

    networking.nat = lib.mkIf cfg.nat.enable {
      enable = true;
      internalIPs = [ subnet ];
      internalInterfaces = [ cfg.bridge.name ];
      externalInterface = cfg.nat.externalInterface;
    };
    boot.kernel.sysctl."net.ipv4.ip_forward" = lib.mkDefault true;
    networking.firewall.trustedInterfaces = lib.mkIf cfg.bridge.trusted [ cfg.bridge.name ];
    networking.firewall.extraCommands = lib.mkIf (cfg.nat.forwardFirst && !config.networking.nftables.enable) ''
      iptables -w -C FORWARD -d ${subnet} -j ACCEPT 2>/dev/null || iptables -w -I FORWARD 1 -d ${subnet} -j ACCEPT
      iptables -w -C FORWARD -s ${subnet} -j ACCEPT 2>/dev/null || iptables -w -I FORWARD 1 -s ${subnet} -j ACCEPT
    '';
    networking.firewall.extraStopCommands = lib.mkIf (cfg.nat.forwardFirst && !config.networking.nftables.enable) ''
      iptables -w -D FORWARD -d ${subnet} -j ACCEPT 2>/dev/null || true
      iptables -w -D FORWARD -s ${subnet} -j ACCEPT 2>/dev/null || true
    '';

    hardware.nvidia.nvidiaPersistenced = lib.mkIf cfg.persistence true;

    # One backend serves exactly ONE VM connection, hence a template. The
    # scripts start their own backends per instance under vm/<name>/; this
    # unit is for a cloud-hypervisor driven by hand:
    #   systemctl start leandro-backend@a
    #   cloud-hypervisor ... --generic-vhost-user device_type=60,socket=/run/leandro/a/nvrm.sock,queue_sizes=[256,256]
    systemd.services."leandro-backend@" = lib.mkIf cfg.backend.enable {
      description = "Leandro vhost-user-nvrm backend for guest %i";
      after = lib.optional cfg.persistence "nvidia-persistenced.service";
      environment = cfg.backend.environment;
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/vhost-user-nvrm --nvrm /run/leandro/%i/nvrm.sock";
        User = cfg.user;
        RuntimeDirectory = "leandro/%i";
        RuntimeDirectoryMode = "0750";
        Restart = "on-failure";
      };
    };
  };
}
