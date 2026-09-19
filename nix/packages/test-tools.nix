# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
{ lib, stdenv, src }:
stdenv.mkDerivation {
  pname = "leandro-test-tools";
  version = "0.0.1";
  src = lib.fileset.toSource { root = src; fileset = src + "/tests/tools"; };
  buildPhase = ''
    runHook preBuild
    $CC -O2 -Wall -Wextra -Werror tests/tools/edid-verify.c -o edid-verify
    $CC -O2 -Wall -Wextra -Werror tests/tools/vdisp-frame.c -o vdisp-frame
    runHook postBuild
  '';
  doCheck = true;
  checkPhase = ''
    runHook preCheck
    while read -r size expected; do
      test -z "$size" && continue
      case "$size" in \#*) continue ;; esac
      test "$(./vdisp-frame --reference "$size" | awk '{print $2}')" = "$expected"
    done < tests/tools/data/vdisp-frame.ref
    runHook postCheck
  '';
  installPhase = ''
    runHook preInstall
    install -Dm755 edid-verify "$out/bin/edid-verify"
    install -Dm755 vdisp-frame "$out/bin/vdisp-frame"
    install -Dm644 tests/tools/data/vdisp-frame.ref "$out/share/leandro/vdisp-frame.ref"
    runHook postInstall
  '';
  meta = {
    description = "EDID validation and deterministic DRM frame probe";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
  };
}
