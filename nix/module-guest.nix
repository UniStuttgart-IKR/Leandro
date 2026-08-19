# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# NixOS GUEST module: what scripts/lib/provision.sh does to an Ubuntu guest
# by hand, declared -- the two kernel modules, the coexistence order they
# have to be loaded in, the params file the first CUDA start needs, and the
# groups a user needs to reach the nodes.
#
#   services.leandro-guest = {
#     enable = true;
#     params.file = ./params.txt;      # the HOST's /proc/driver/nvidia/params
#     users = [ "leandro" ];
#     nvidiaUserspaceDir = "/opt/nvrm/lib";   # staged there by other means
#   };
#
# WARNING -- WHAT THIS MODULE DOES NOT AND CANNOT DO: it does not fetch
# NVIDIA's userspace. The guest is handed the HOST's own libcuda, which is
# not redistributable (LICENSES.md) and has to match the running host
# nvidia.ko exactly, down to the struct offsets. So the userspace is a PATH
# or a package the operator supplies -- exactly what lea_payload_stage
# stages into /opt/nvrm/lib on the Ubuntu guests.
#
# Verified 2026-08-19: a NixOS guest built from this module passed the
# compute gate against a Leandro backend, and four of them shared one GPU
# (flake.nix header). One structural difference from the Ubuntu path
# stands: the library search path below is LD_LIBRARY_PATH via
# environment.sessionVariables, not /etc/ld.so.conf.d -- NixOS has no FHS
# ld.so.conf to write into, while the Ubuntu images deliberately use
# ld.so.conf.d BECAUSE LD_LIBRARY_PATH is lost across sudo, su and
# systemd units (measured on a booted guest, scripts/lib/provision.sh's
# nvrm.conf block). Reaching CUDA from a systemd unit in a NixOS guest
# therefore needs the variable set in that unit's own environment.
{ guestModulesFor }:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.leandro-guest;

  # The params file, from either shape of the option.
  paramsFile =
    if cfg.params.file != null then cfg.params.file
    else pkgs.writeText "nvrm-params.txt" cfg.params.text;

  # nvrm_nodes MUST be told create_nodes=0 -- virtio_nvrm owns the nodes.
  # These are OPTIONS; the ORDER is the unit below, because modprobe.d
  # cannot express "this one first, then provision, then that one".
  nodesParams = [ "create_nodes=0" ]
    ++ lib.optional (cfg.maxPinMiB != null) "max_pin_mib=${toString cfg.maxPinMiB}";
  nvrmParams =
    lib.optional (cfg.maxPinMiB != null) "max_pin_mib=${toString cfg.maxPinMiB}"
    ++ lib.optionals cfg.display.enable [
      "display=1" "vdisplay=1"
      "vdisplay_width=${toString cfg.display.width}"
      "vdisplay_height=${toString cfg.display.height}"
      "vdisplay_vblank_hz=${toString cfg.display.refresh}"
    ]
    ++ lib.optional cfg.bdfMediation "bdf_mediation=1"
    ++ cfg.extraModuleParams;

  boot-sh = pkgs.writeShellScript "leandro-nvrm-boot" ''
    set -e
    # WHICH params file. The params are a property of the HOST this guest is
    # attached to, not of the image: the same image run against a second host
    # needs that host's copy, and the copy baked in at build time is then
    # wrong. So a file dropped at runtime WINS over the built-in one. That is
    # what the Ubuntu path does too -- scripts/lib/provision.sh ships the
    # host's /proc/driver/nvidia/params as ~/gpu/params.txt on every `up` and
    # provisions from there.
    params=${paramsFile}
    ${lib.optionalString (cfg.params.runtimeFile != null) ''
      [ -s ${cfg.params.runtimeFile} ] && params=${cfg.params.runtimeFile}
    ''}
    # Idempotent, and in THIS order. nvrm_nodes first and with
    # create_nodes=0 (otherwise it takes the nodes virtio_nvrm is meant to
    # own), the params BEFORE the first CUDA start, virtio_nvrm last. The
    # same three steps as /usr/local/sbin/nvrm-boot.sh on the Ubuntu guests
    # (scripts/guest/nvrm-setup.sh --persist writes that one).
    ${pkgs.kmod}/bin/lsmod | ${pkgs.gnugrep}/bin/grep -q '^nvrm_nodes ' \
      || ${pkgs.kmod}/bin/modprobe nvrm_nodes
    ${cfg.package}/bin/nvrm-nodes-tool provision params "$params"
    ${pkgs.kmod}/bin/lsmod | ${pkgs.gnugrep}/bin/grep -q '^virtio_nvrm ' \
      || ${pkgs.kmod}/bin/modprobe virtio_nvrm
  '';
in {
  options.services.leandro-guest = {
    enable = lib.mkEnableOption "the Leandro guest side (nvrm_nodes, virtio_nvrm, params, groups)";

    package = lib.mkOption {
      type = lib.types.package;
      default = guestModulesFor config.boot.kernelPackages.kernel;
      defaultText = lib.literalExpression "leandro-guest-modules built for config.boot.kernelPackages.kernel";
      description = ''
        The out-of-tree module package: nvrm_nodes.ko, virtio_nvrm.ko and
        nvrm-nodes-tool. It is built against ONE kernel; changing
        boot.kernelPackages rebuilds it.
      '';
    };

    params = {
      file = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = ''
          The HOST's /proc/driver/nvidia/params, copied into the guest
          configuration. nvrm_nodes serves it as the guest's own
          /proc/driver/nvidia/params, which libcuda reads on its first call.
        '';
      };
      text = lib.mkOption {
        type = lib.types.nullOr lib.types.lines;
        default = null;
        description = "The same content inline, for a configuration that would rather not carry a file.";
      };
      runtimeFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = "/var/lib/leandro/params.txt";
        description = ''
          A path IN THE GUEST that, when it exists and is non-empty, is used
          instead of params.file/params.text. The params belong to the host
          the guest is attached to, so an image that is to run against more
          than one host cannot carry the authoritative copy; the rig drops
          the current host's at every `up`. Set to null to pin the guest to
          the built-in copy.
        '';
      };
    };

    users = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = ''
        Users to put into the render and video groups. The transport runs
        over the DRM render node; without the group membership CUDA in the
        guest needs root, which is the crutch this project removed.
      '';
    };

    maxPinMiB = lib.mkOption {
      type = lib.types.nullOr lib.types.int;
      default = null;
      description = ''
        Cap on concurrently pinned guest memory, in MiB (module default
        1024). Raise it for workloads that pin gigabytes -- probe/suites'
        test_async_streams.py wants about 2048 and reports CUDA error 304
        below that.
      '';
    };

    bdfMediation = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Report the guest's own PCI address for the card in every RM answer.
        Needed by anything that looks the GPU up through libpciaccess -- the
        X driver does, which is why the desktop path sets it.
      '';
    };

    display = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = ''
          Serve the kernel-path RM operations a display needs and present
          NVKMS a virtual display.

          WARNING: this switches the MODULE on; it does not install NVKMS.
          nvidia-modeset and nvidia-drm still have to come from the system's
          NVIDIA packages and are loaded on top -- see docs/DISPLAY.md. On
          the Ubuntu guests lea_display_stage does that staging, and there
          is no NixOS equivalent here yet.
        '';
      };
      width = lib.mkOption { type = lib.types.int; default = 1920; description = "Virtual display width (vdisplay_width)."; };
      height = lib.mkOption { type = lib.types.int; default = 1080; description = "Virtual display height (vdisplay_height)."; };
      refresh = lib.mkOption { type = lib.types.int; default = 60; description = "Virtual vblank rate in Hz (vdisplay_vblank_hz)."; };
    };

    nvidiaUserspaceDir = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "/opt/nvrm/lib";
      description = ''
        Directory holding the NVIDIA userspace this guest is to use --
        libcuda and friends of exactly the HOST driver's version. A path on
        the guest, or a store path; either way it is the operator's to
        supply. Nothing here downloads it and nothing here may: it is not
        redistributable and it must match the host's nvidia.ko.

        When set, it is put on the dynamic linker's search path through
        LD_LIBRARY_PATH. Read the WARNING at the top of this file first:
        that is NOT what the Ubuntu images do, for a measured reason.
      '';
    };

    extraModuleParams = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "bdf_debug=1" ];
      description = "Further virtio_nvrm parameters, verbatim (see its MODULE_PARM_DESC list).";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = (cfg.params.file != null) != (cfg.params.text != null);
        message = ''
          services.leandro-guest: set exactly one of params.file and
          params.text. It is the HOST's /proc/driver/nvidia/params; libcuda
          reads it on its first call and a guest without it fails there,
          not later.
        '';
      }
    ];

    boot.extraModulePackages = [ cfg.package ];

    # The guest runs NO NVIDIA kernel driver -- that is the claim the whole
    # project rests on, and `showcase.sh demo` has a section that proves it.
    # nvidia_modeset and nvidia_drm are a different matter: they load on top
    # of virtio_nvrm, which exports the kernel-API symbols they link against
    # (guest-module/virtio_nvrm/nvrm_kapi.h).
    boot.blacklistedKernelModules = [ "nvidia" ];

    boot.extraModprobeConfig = ''
      options nvrm_nodes ${lib.concatStringsSep " " nodesParams}
    '' + lib.optionalString (nvrmParams != [ ]) ''
      options virtio_nvrm ${lib.concatStringsSep " " nvrmParams}
    '';

    systemd.services.leandro-nvrm = {
      description = "Leandro guest: load nvrm_nodes and virtio_nvrm in order, provision the params";
      wantedBy = [ "multi-user.target" ];
      after = [ "systemd-modules-load.service" ];
      unitConfig.ConditionVirtualization = "vm";
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = boot-sh;
      };
    };

    users.groups.render = { };
    users.users = lib.genAttrs cfg.users (_: { extraGroups = [ "render" "video" ]; });

    environment.systemPackages = [ cfg.package ];

    environment.sessionVariables = lib.mkIf (cfg.nvidiaUserspaceDir != null) {
      LD_LIBRARY_PATH = [ cfg.nvidiaUserspaceDir ];
    };

    warnings = lib.optional (cfg.nvidiaUserspaceDir == null) ''
      services.leandro-guest: no nvidiaUserspaceDir. The modules will load
      and the nodes will appear, but nothing in this guest can call CUDA
      until libcuda of the host driver's exact version is in place.
    '' ++ lib.optional cfg.display.enable ''
      services.leandro-guest.display: the module side is on, the NVKMS side
      is not provided here. Unverified on NixOS -- see docs/DISPLAY.md.
    '';
  };
}
