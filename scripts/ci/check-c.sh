#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
set -euo pipefail

usage() {
    echo "Usage: $0 [all|tables|edid|host-tools] [--sanitize]"
    echo "CC selects the compiler; CFLAGS adds compiler flags."
}

mode=all
sanitize=0
for arg in "$@"; do
    case "$arg" in
        all|tables|edid|host-tools) mode=$arg ;;
        --sanitize) sanitize=1 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
done

LEA_ROOT=${LEA_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)}
cd "$LEA_ROOT"
compiler=${CC:-cc}
flags=(-O2 -g -Wall -Wextra -Werror)
extra_flags=()
read -r -a extra_flags <<< "${CFLAGS:-}"
flags+=("${extra_flags[@]}")
variant=plain
if ((sanitize)); then
    flags+=(-O1 -fno-omit-frame-pointer '-fsanitize=address,undefined' -fno-sanitize-recover=all)
    variant=sanitize
fi
out="$LEA_ROOT/target/ci-c/$variant"
mkdir -p "$out"

compile() {
    local name=$1 source=$2
    "$compiler" "${flags[@]}" "$source" -o "$out/$name"
}

tables() {
    local name
    for name in tabcheck tabreject vramcheck; do
        compile "$name" "guest-module/virtio_nvrm/test/$name.c"
    done
    cargo run --locked --quiet --bin nvrm-genhdr -- --dump-tables "$out/stream.bin"
    cargo run --locked --quiet --bin nvrm-genhdr -- --expect-dump "$out/expected.txt"
    "$out/tabcheck" "$out/stream.bin" > "$out/actual.txt"
    diff -u "$out/expected.txt" "$out/actual.txt"
    "$out/tabreject" "$out/stream.bin"
    "$out/vramcheck"
}

edid() {
    command -v edid-decode >/dev/null || {
        echo "Install edid-decode to run the EDID checks." >&2
        return 1
    }
    compile edidcheck guest-module/virtio_nvrm/test/edidcheck.c
    compile edidclamp guest-module/virtio_nvrm/test/edidclamp.c
    compile edid-verify probe/c/edid-verify.c
    "$out/edidclamp"
    local size width height
    for size in 800x600 1280x720 1920x1080 2560x1440 2560x1600 3840x2160; do
        width=${size%x*}
        height=${size#*x}
        "$out/edidcheck" "$width" "$height" "$out/$size.bin"
        "$out/edid-verify" "$out/$size.bin" "$width" "$height"
        if ! edid-decode --check "$out/$size.bin" > "$out/$size.txt" 2>&1; then
            cat "$out/$size.txt" >&2
            return 1
        fi
    done
}

host_tools() {
    compile edid-verify probe/c/edid-verify.c
    compile vdisp-frame probe/c/vdisp-frame.c
    awk 'NF && $1 !~ /^#/' probe/data/vdisp-frame.ref > "$out/frame-expected.txt"
    if [[ ! -s $out/frame-expected.txt ]]; then
        echo "No frame hashes found in probe/data/vdisp-frame.ref." >&2
        return 1
    fi
    local size _hash
    while read -r size _hash; do
        "$out/vdisp-frame" --reference "$size"
    done < "$out/frame-expected.txt" > "$out/frame-actual.txt"
    diff -u "$out/frame-expected.txt" "$out/frame-actual.txt"
}

case "$mode" in
    tables) tables ;;
    edid) edid ;;
    host-tools) host_tools ;;
    all) tables; edid; host_tools ;;
esac
echo "C checks passed ($mode, $compiler, $variant)."
