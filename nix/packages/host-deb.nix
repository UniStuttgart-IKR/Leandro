# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# The host side as a Debian package for Ubuntu/Debian hosts:
#
#   nix build .#host-deb    ->  result/leandro-host_<version>_amd64.deb
#
# Statically linked (pkgsStatic, musl): a binary built by Nix against Nix's
# glibc does not run on Ubuntu, a static one runs anywhere. The package files
# are in packaging/host-deb/.
{ lib, stdenv, dpkg, src, driverVersion, chVersion, leandroStatic, cloudHypervisorStatic }:

let
  version = "0.1+${driverVersion}";
in
stdenv.mkDerivation {
  pname = "leandro-host-deb";
  inherit version;
  dontUnpack = true;
  nativeBuildInputs = [ dpkg ];

  buildPhase = ''
    runHook preBuild
    p=${src}/packaging/host-deb
    root=$PWD/root
    subst() { sed -e 's/@VERSION@/${version}/g' -e 's/@DRIVER@/${driverVersion}/g' -e 's/@CH@/${chVersion}/g' "$1"; }

    for b in vhost-user-nvrm vhost-user-input vgpuprofile; do
      install -D -m755 ${leandroStatic}/bin/$b $root/usr/bin/$b
    done
    install -D -m755 ${cloudHypervisorStatic}/bin/cloud-hypervisor $root/usr/lib/leandro/cloud-hypervisor
    install -D -m644 "$p/leandro-backend@.service" "$root/usr/lib/systemd/system/leandro-backend@.service"

    mkdir -p $root/DEBIAN
    subst $p/control > $root/DEBIAN/control
    echo "Installed-Size: $(du -sk --exclude=DEBIAN $root | cut -f1)" >> $root/DEBIAN/control
    subst $p/postinst > $root/DEBIAN/postinst
    chmod 755 $root/DEBIAN/postinst
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    mkdir -p $out
    dpkg-deb --root-owner-group -Zxz --build root $out/leandro-host_${version}_amd64.deb
    runHook postInstall
  '';

  # The .deb must not carry store paths: nothing on an Ubuntu host has them.
  disallowedReferences = [ leandroStatic cloudHypervisorStatic ];

  meta = {
    description = "Leandro host side as a .deb (static vhost-user-nvrm, vhost-user-input, vgpuprofile, cloud-hypervisor)";
    license = with lib.licenses; [ mit asl20 ];
    platforms = [ "x86_64-linux" ];
  };
}
