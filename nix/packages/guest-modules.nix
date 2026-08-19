# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# The two GUEST kernel modules as an out-of-tree module package, built
# against one nixpkgs kernel.
#
#   nix build .#guest-modules                      the flake's default kernel
#   boot.extraModulePackages = [ pkgs.leandro-guest-modules ];   in a guest
#
# nvrm_nodes.ko  device nodes with the right majors, /proc/driver/nvidia,
#                and the VA->GPA resolution that lets CUDA run as an
#                ordinary user (guest-module/nvrm_nodes/README.md).
# virtio_nvrm.ko the guest driver that serves those nodes over virtio-nvrm.
#
# WHICH KERNEL, and this is not a preference. The guests everything here is
# measured on run Ubuntu 24.04's 6.8. Walking the modules forward, one
# nixpkgs kernel at a time (measured 2026-08-18):
#
#   <= 6.11   builds as written
#      6.12   removed no_llseek                -> version guard, in the tree
#      6.13   MODULE_IMPORT_NS wants a string  -> version guard, in the tree
#      6.15   removed hrtimer_init             -> NOT done
#      6.18   removed nth_page                 -> NOT done
#
# The two guards are spellings and cost nothing. The last two are not:
# hrtimer_setup takes the callback the initialiser used to leave to the
# caller, and nth_page vanished because folios became contiguous -- both are
# behaviour, on a module whose behaviour is only measured on 6.8. They are
# left for whoever actually needs a newer guest kernel, and until then this
# package pins the 6.12 LTS: the newest kernel it builds against that is an
# LTS, and the closest one to what the guests really run. Override `kernel`
# for anything else -- nixosModules.guest passes
# config.boot.kernelPackages.kernel, so a guest configuration that picks a
# newer kernel finds out at build time rather than at insmod time.
#
# NO RUST IS NEEDED HERE, and that is by construction: virtio_nvrm's wire
# header (nvrm_wire.h) is GENERATED on the host by `cargo run --bin
# nvrm-genhdr` and CHECKED IN, so the guest build is plain Kbuild. The
# check band verifies the checked-in copy is current (`test.sh check`,
# nvrm-genhdr step).
#
# The sources are GPL-2.0-only (they link against the guest kernel); this
# expression, like the rest of nix/, is MIT.
{ lib, stdenv, kernel, src }:

stdenv.mkDerivation {
  pname = "leandro-guest-modules";
  version = "0-${kernel.version}";
  inherit src;

  nativeBuildInputs = kernel.moduleBuildDependencies;
  # Kernel modules are built with the kernel's own flags; the wrappers'
  # hardening collides with them (this is the standard nixpkgs incantation
  # for out-of-tree modules, not a local workaround).
  hardeningDisable = [ "pic" "format" ];

  KDIR = "${kernel.dev}/lib/modules/${kernel.modDirVersion}/build";

  # A .ko is a relocatable object, not an executable: patchelf reports
  # "wrong ELF type" on every one of them and `strip` has no business
  # touching sections the module loader reads.
  dontStrip = true;
  dontPatchELF = true;

  buildPhase = ''
    runHook preBuild
    # Both Makefiles take KDIR; nvrm_nodes also builds its userspace tool,
    # which is what provisions /proc/driver/nvidia/params.
    make -C guest-module/nvrm_nodes  KDIR=$KDIR
    make -C guest-module/virtio_nvrm KDIR=$KDIR
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    d=$out/lib/modules/${kernel.modDirVersion}/extra
    install -D -m444 guest-module/nvrm_nodes/nvrm_nodes.ko   $d/nvrm_nodes.ko
    install -D -m444 guest-module/virtio_nvrm/virtio_nvrm.ko $d/virtio_nvrm.ko
    install -D -m555 guest-module/nvrm_nodes/nvrm-nodes-tool $out/bin/nvrm-nodes-tool
    # A fingerprint of the SOURCES this .ko was built from, beside the .ko.
    # An out-of-tree module in an IMAGE is the one that can silently be older
    # than the checkout the host is running from: on the Ubuntu guests the
    # module is recompiled on every `up`, here it is whatever the last
    # `build.sh bake --nixos` produced. lea_guest_build_nvrm reads this and
    # says so rather than measuring a stale module.
    cat guest-module/virtio_nvrm/*.c guest-module/virtio_nvrm/*.h \
      | sha256sum | cut -c1-12 > $d/.leandro-src
    runHook postInstall
  '';

  meta = with lib; {
    description = "Leandro guest kernel modules (nvrm_nodes, virtio_nvrm)";
    license = licenses.gpl2Only;
    platforms = platforms.linux;
    # A module package is only ever meaningful for the kernel it was built
    # against; nothing here is a runtime dependency of the host side.
    broken = !stdenv.hostPlatform.isLinux;
  };
}
