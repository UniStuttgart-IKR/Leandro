# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# The guest driver as a Debian package for Ubuntu guests:
#
#   nix build .#guest-deb   ->  result/leandro-guest-dkms_<version>_amd64.deb
#   (in the guest)              apt install ./leandro-guest-dkms_*.deb
#
# What goes in is in packaging/guest-deb/, as plain files: the dkms.conf and
# the Makefile dkms runs, the two units, modprobe.d, tmpfiles.d and the
# control files. This expression only puts them where Debian wants them,
# next to the sources dkms builds from -- Leandro's two guest modules and the
# parts of NVIDIA's open-gpu-kernel-modules that nvidia-modeset and
# nvidia-drm are built from -- and runs dpkg-deb. No Debian toolchain is
# needed on the host.
#
# NVIDIA's userspace is not in it and cannot be (LICENSES.md).
{ lib, stdenv, pkgsStatic, dpkg, fetchFromGitHub, src, driverVersion, hash }:

let
  version = "0.1+${driverVersion}";
  nvidiaSrc = fetchFromGitHub {
    owner = "NVIDIA";
    repo = "open-gpu-kernel-modules";
    tag = driverVersion;
    inherit hash;
  };
  # Static, so the package is indifferent to the guest's libc.
  tool = pkgsStatic.stdenv.mkDerivation {
    pname = "nvrm-nodes-tool";
    inherit version;
    src = "${src}/guest-module/nvrm_nodes";
    buildPhase = "$CC -O1 -Wall -Wextra -static -o nvrm-nodes-tool nvrm-nodes-tool.c";
    installPhase = "install -D -m755 nvrm-nodes-tool $out/bin/nvrm-nodes-tool";
  };
in
stdenv.mkDerivation {
  pname = "leandro-guest-deb";
  inherit version;
  dontUnpack = true;
  nativeBuildInputs = [ dpkg ];

  buildPhase = ''
    runHook preBuild
    p=${src}/packaging/guest-deb
    root=$PWD/root
    s=$root/usr/src/leandro-guest-${version}
    subst() { sed -e 's/@VERSION@/${version}/g' -e 's/@DRIVER@/${driverVersion}/g' "$1"; }

    # The dkms source tree.
    mkdir -p $s/nvkms/src
    subst $p/dkms.conf > $s/dkms.conf
    cp $p/Makefile $s/Makefile
    cp -r ${src}/guest-module/nvrm_nodes ${src}/guest-module/virtio_nvrm $s/
    cp -r ${nvidiaSrc}/kernel-open ${nvidiaSrc}/utils.mk ${nvidiaSrc}/version.mk \
          ${nvidiaSrc}/nv-compiler.sh ${nvidiaSrc}/COPYING $s/nvkms/
    cp -r ${nvidiaSrc}/src/nvidia-modeset ${nvidiaSrc}/src/common $s/nvkms/src/
    chmod -R u+w $root
    rm -rf $s/virtio_nvrm/test

    # The rest of the package.
    install -D -m755 ${tool}/bin/nvrm-nodes-tool $root/usr/bin/nvrm-nodes-tool
    install -D -m644 $p/leandro-nvrm.service $root/usr/lib/systemd/system/leandro-nvrm.service
    install -D -m644 $p/leandro-display.service $root/usr/lib/systemd/system/leandro-display.service
    install -D -m644 $p/leandro-guest.modprobe.conf $root/usr/lib/modprobe.d/leandro-guest.conf
    install -D -m644 $p/leandro-display.modprobe.conf $root/etc/modprobe.d/leandro-display.conf
    install -D -m644 $p/leandro.tmpfiles.conf $root/usr/lib/tmpfiles.d/leandro.conf

    mkdir -p $root/DEBIAN
    subst $p/control > $root/DEBIAN/control
    echo "Installed-Size: $(du -sk --exclude=DEBIAN $root | cut -f1)" >> $root/DEBIAN/control
    echo /etc/modprobe.d/leandro-display.conf > $root/DEBIAN/conffiles
    subst $p/postinst > $root/DEBIAN/postinst
    subst $p/prerm > $root/DEBIAN/prerm
    chmod 755 $root/DEBIAN/postinst $root/DEBIAN/prerm
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    mkdir -p $out
    dpkg-deb --root-owner-group -Zxz --build root $out/leandro-guest-dkms_${version}_amd64.deb
    runHook postInstall
  '';

  meta = {
    description = "Leandro guest driver as a DKMS .deb (nvrm_nodes, virtio_nvrm, NVIDIA's nvidia-modeset/nvidia-drm)";
    license = with lib.licenses; [ gpl2Only mit ];
    platforms = [ "x86_64-linux" ];
  };
}
