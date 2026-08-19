# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# The four entry points (scripts/{build,test,bench,showcase}.sh) with their
# library and guest files, wrapped so that LEA_* point at the store:
#   leandro-showcase, leandro-test, leandro-bench, leandro-build
# LEA_ROOT is $out/share/leandro (read-only); every artefact goes below
# LEA_VM_DIR, which defaults to ~/.local/state/leandro/vm here (instances,
# images, the upstream guest image, outputs -- set LEA_VM_DIR to move it).
# The binaries come from the `leandro` and `cloud-hypervisor` packages, so
# `leandro-build cargo|ch|vendor` are meaningless in the store -- the
# wrapper says so. `leandro-build image|bake` still work: they only write
# below the state directories.
{ lib, stdenvNoCC, makeWrapper, src, leandro, cloud-hypervisor
, bash, qemu-utils, dosfstools, mtools, e2fsprogs, iproute2, iptables, openssh, curl
, git, gnutar, gzip, gawk, gnugrep, gnused, procps, util-linux, coreutils, gcc, gnumake, python3
, glibc, ffmpeg-full }:
stdenvNoCC.mkDerivation {
  pname = "leandro-scripts";
  version = "0.0.1";
  src = lib.fileset.toSource {
    root = src;
    fileset = lib.fileset.unions [
      (src + "/scripts") (src + "/guest-module") (src + "/probe")
      (src + "/DRIVER_VERSION") (src + "/CH_VERSION") (src + "/GUEST_IMAGE") (src + "/patches")
    ];
  };
  nativeBuildInputs = [ makeWrapper ];
  dontBuild = true;
  installPhase = ''
    mkdir -p $out/share/leandro $out/bin
    cp -r . $out/share/leandro
    for s in $out/share/leandro/scripts/*.sh; do
      n=$(basename "$s" .sh)
      makeWrapper "$s" "$out/bin/leandro-$n" \
        --set LEA_ROOT    "$out/share/leandro" \
        --set LEA_BIN_DIR "${leandro}/bin" \
        --set LEA_TRACE_LIB "${leandro}/lib/libnvrm_trace.so" \
        --set LEA_CH      "${cloud-hypervisor}/bin/cloud-hypervisor" \
        --run 'export LEA_VM_DIR=''${LEA_VM_DIR:-''${XDG_STATE_HOME:-$HOME/.local/state}/leandro/vm}' \
        --prefix PATH : ${lib.makeBinPath [
          bash qemu-utils dosfstools mtools e2fsprogs iproute2 iptables openssh curl
          git gnutar gzip gawk gnugrep gnused procps util-linux coreutils gcc gnumake python3
          # getconf comes from glibc's bin output and bench.sh asks it for
          # CLK_TCK on every run; without it the backend-CPU columns are
          # empty and the failure is a bare "getconf: command not found"
          # from inside a container (measured 2026-08-19). gzip is what the
          # cluster harness's forensics tarball needs.
          glibc.bin
          # THE NATIVE REFERENCE'S OWN TOOL. The gpu gate's encode stage runs
          # ffmpeg twice -- once in the guest and once natively -- because
          # `-hwaccel cuda` on a yuv444p stream fails on the HOST too, and
          # without the counter-check the stage would report a
          # virtualisation gap that is a chroma format. In a checkout that
          # ffmpeg is the distribution's; in a package there is no
          # distribution, so it ships. nvenc/nvdec on, or the stage measures
          # nothing.
          (ffmpeg-full.override { withNvenc = true; withNvdec = true; })
        ]}
    done
    # sudo and nvidia-smi are deliberately NOT on that PATH: sudo must be the
    # setuid /run/wrappers/bin/sudo, and nvidia-smi comes from the system's
    # driver (lea_nvidia_bin finds /run/current-system/sw/bin).
  '';
  meta = {
    description = "Leandro's host scripts, wrapped for the Nix store";
    license = lib.licenses.mit;
    platforms = [ "x86_64-linux" ];
  };
}
