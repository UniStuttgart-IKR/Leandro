#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# Production builds. Acceptance images, probes and provisioning live in Leandro-Test.
set -uo pipefail

error() { printf 'ERROR: %s\n' "$*" >&2; }
usage() {
    cat <<'HELP'
Usage: tools/build.sh [COMMAND] [OPTIONS]

Production commands (no VM configuration required):
  all                     vendor + ch + cargo (default)
  vendor                  fetch NVIDIA source at LEA_DRIVER or DRIVER_VERSION
  vendor-abi [VERSION...]  fetch the header sets declared in nvrm-sys/abi.toml
  ch                      build pinned cloud-hypervisor with patches/
  cargo                   build this workspace in release mode
  check-driver            compare the running driver, target and vendor tag

Options: --driver VERSION|auto  --jobs N  --dry-run  --help
  --driver overrides this invocation; it does not rewrite DRIVER_VERSION.
  Only --driver auto and check-driver inspect the running NVIDIA driver.

Images, probes and hardware acceptance commands live in Leandro-Test.
HELP
}

LEA_ROOT=${LEA_ROOT:-$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd -P)}
cd "$LEA_ROOT" || { error "cannot cd to $LEA_ROOT"; exit 1; }
LEA_ROOT=$PWD

# A pinned software build does not depend on a loaded NVIDIA module.
lea_driver_version() {
    grep -oE '[0-9]+\.[0-9]+\.[0-9]+' /proc/driver/nvidia/version 2>/dev/null | head -1
}
lea_want_driver_file() { tr -d '[:space:]' < "$LEA_ROOT/DRIVER_VERSION"; }
lea_want_driver() {
    local version
    if [[ -n ${LEA_DRIVER:-} ]]; then version=${LEA_DRIVER//[[:space:]]/}
    else version=$(lea_want_driver_file) || return 1; fi
    [[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { error "invalid driver version: $version"; return 1; }
    printf '%s' "$version"
}
lea_supported_drivers() {
    grep -oE '^\[versions\."[^"]+"\]' "$LEA_ROOT/crates/nvrm-sys/abi.toml" \
        | sed -E 's/.*"(.*)".*/\1/'
}
lea_driver_supported() {
    local version=$1 supported
    while read -r supported; do [[ $supported == "$version" ]] && return 0; done < <(lea_supported_drivers)
    return 1
}

# Fetch the full NVIDIA tree; the guest NVKMS build needs sources as well as headers.
do_vendor() {
    local ver dst have
    ver=$(lea_want_driver) || return 1
    dst=$LEA_ROOT/vendor/open-gpu-kernel-modules
    if [[ -d $dst/.git ]]; then
        have=$(git -C "$dst" describe --tags --exact-match 2>/dev/null || echo none)
        if [[ $have == "$ver" ]]; then echo "vendor: $ver already present"; return 0; fi
        echo "vendor: $have != $ver, re-fetching"; rm -rf "$dst"
    fi
    mkdir -p "$LEA_ROOT/vendor"
    git clone --filter=blob:none --depth 1 --branch "$ver" \
        https://github.com/NVIDIA/open-gpu-kernel-modules.git "$dst" || return 1
    echo "vendor: $ver ok"
}

# Commit the per-version transitive header closure used by cargo xtask abi.
do_vendor_abi() {
    local -a want=("$@")
    local abi=$LEA_ROOT/crates/nvrm-sys/abi.toml
    [[ -f $abi ]] || { error "no $abi"; return 1; }
    command -v clang >/dev/null || { error "vendor-abi needs clang (the header closure is computed with clang -MM)"; return 1; }
    if [[ ${#want[@]} -eq 0 ]]; then
        mapfile -t want < <(grep -oE '^\[versions\."[^"]+"\]' "$abi" | sed -E 's/.*"(.*)".*/\1/')
    fi
    [[ ${#want[@]} -gt 0 ]] || { error "no versions in $abi"; return 1; }
    local v rc=0
    for v in "${want[@]}"; do do_vendor_abi_one "$v" || rc=1; done
    return $rc
}

do_vendor_abi_one() {
    local ver=$1
    local dirs=(kernel-open/common/inc src/common/sdk/nvidia/inc
                src/nvidia/arch/nvalloc/unix/include kernel-open/nvidia-uvm
                kernel-open/nvidia-modeset)
    local work=$LEA_ROOT/target/abi-headers/$ver
    local dst=$LEA_ROOT/vendor/nvidia-rm-headers/$ver
    if [[ ! -d $work/.git ]]; then
        rm -rf "$work"; mkdir -p "$(dirname "$work")"
        # blob:none + a sparse checkout of the five include roots: the whole
        # tree is ~170 MB and five versions of it is not what this needs.
        git clone --quiet --filter=blob:none --no-checkout --depth 1 --branch "$ver" \
            https://github.com/NVIDIA/open-gpu-kernel-modules.git "$work" || return 1
        git -C "$work" sparse-checkout set --no-cone "${dirs[@]}" COPYING >/dev/null || return 1
        git -C "$work" checkout --quiet || return 1
    fi
    local commit; commit=$(git -C "$work" rev-parse HEAD) || return 1

    local -a inc=(); local d
    for d in "${dirs[@]}"; do inc+=(-I"$work/$d"); done
    # -MG so a header this version does not have is REPORTED (as a bare
    # relative path) instead of aborting the closure.
    local out; out=$(clang -MM -MG "$LEA_ROOT/crates/nvrm-sys/wrapper.h" "${inc[@]}" \
                       -DNV_LINUX -D__linux__ -std=gnu11 2>/dev/null | tr ' ' '\n')
    local -a hdrs absent
    mapfile -t hdrs < <(grep "^$work/" <<<"$out" | sed "s|^$work/||" | sort -u)
    mapfile -t absent < <(grep -vE "^($work/|\\\\|\$|.*wrapper\.[oh]:?\$)" <<<"$out" | sort -u)
    [[ ${#hdrs[@]} -gt 0 ]] || { error "$ver: clang -MM produced no headers"; return 1; }

    rm -rf "$dst"; mkdir -p "$dst"
    local h
    for h in "${hdrs[@]}"; do
        mkdir -p "$dst/$(dirname "$h")"
        cp "$work/$h" "$dst/$h" || return 1
    done
    cp "$work/COPYING" "$dst/COPYING" || return 1
    {
        echo "open-gpu-kernel-modules @ $ver"
        echo
        echo "upstream:  https://github.com/NVIDIA/open-gpu-kernel-modules"
        echo "tag:       $ver"
        echo "commit:    $commit"
        echo "licence:   as stated in COPYING beside this file (MIT/GPLv2 dual)"
        echo "headers:   ${#hdrs[@]}, copied verbatim"
        echo
        echo "The transitive closure of crates/nvrm-sys/wrapper.h under this"
        echo "version's five include roots, computed with clang -MM. Produced by"
        echo "tools/build.sh vendor-abi $ver, which reproduces it exactly."
        echo "Nothing in this directory is edited."
        if [[ ${#absent[@]} -gt 0 ]]; then
            echo
            echo "Headers wrapper.h names that this version does NOT have:"
            printf '  %s\n' "${absent[@]}"
        fi
    } > "$dst/PROVENANCE"
    echo "vendor-abi: $ver ok -- ${#hdrs[@]} headers, ${#absent[@]} absent"
}

# Apply the ordered CH patch series, preserving unrelated local changes.
do_ch() {
    local want dir bin have i
    want=$(tr -d '[:space:]' < "$LEA_ROOT/CH_VERSION")
    dir=$LEA_ROOT/vendor/cloud-hypervisor
    bin=$dir/target/release/cloud-hypervisor
    # The whole series in numeric order, not a single path: the order has to
    # be fixed as soon as a second patch touches the same area.
    local -a patches abs=()
    mapfile -t patches < <(ls "$LEA_ROOT"/patches/[0-9][0-9][0-9][0-9]-*.patch 2>/dev/null | sort)
    [[ ${#patches[@]} -gt 0 ]] || { error "no patches in patches/"; return 1; }
    if [[ ! -d $dir/.git ]]; then
        git clone --depth 1 --branch "$want" \
            https://github.com/cloud-hypervisor/cloud-hypervisor.git "$dir" || return 1
    fi
    have=$(git -C "$dir" describe --tags --always 2>/dev/null || echo none)
    [[ $have == "$want" ]] || { error "$dir is at '$have', expected '$want'. Fix: rm -rf $dir && $0 ch"; return 1; }
    for i in "${patches[@]}"; do abs+=("$i"); done

    # Compare patched file blobs with a temporary index. Reverse-checking
    # patches separately fails when later patches change earlier context.
    # Accept exact prefixes so a previously built checkout can gain new patches
    # without discarding unrelated edits.
    series_applied() {
        local n=$1 idx rc=0 f a b i
        local -a pre=("${abs[@]:0:$n}")
        [[ ${#pre[@]} -gt 0 ]] || return 1
        idx=$(mktemp)
        GIT_INDEX_FILE="$idx" git -C "$dir" read-tree HEAD 2>/dev/null || { rm -f "$idx"; return 1; }
        for i in "${pre[@]}"; do
            GIT_INDEX_FILE="$idx" git -C "$dir" apply --cached "$i" 2>/dev/null \
                || { rm -f "$idx"; return 1; }
        done
        [[ "$(git -C "$dir" diff --name-only | sort -u)" \
           == "$(git -C "$dir" apply --numstat "${pre[@]}" | cut -f3 | sort -u)" ]] || rc=1
        while read -r f; do
            [[ -n $f ]] || continue
            a=$(git -C "$dir" hash-object "$f" 2>/dev/null || true)
            b=$(GIT_INDEX_FILE="$idx" git -C "$dir" rev-parse ":$f" 2>/dev/null || true)
            [[ -n $a && $a == "$b" ]] || rc=1
        done < <(git -C "$dir" apply --numstat "${pre[@]}" | cut -f3 | sort -u)
        rm -f "$idx"
        return $rc
    }
    apply_series() {
        local i
        for i in "${!abs[@]}"; do
            git -C "$dir" apply "${abs[$i]}" \
                || { error "$(basename "${patches[$i]}") does not apply to $want."; return 1; }
            echo "applied: $(basename "${patches[$i]}")"
        done
    }
    if git -C "$dir" diff --quiet && git -C "$dir" diff --cached --quiet; then
        apply_series || return 1
    elif series_applied "${#abs[@]}"; then
        echo "series already fully applied (${#patches[@]} patches)."
    else
        # An OLDER prefix of this same series is not handwork. Prove it, then
        # heal it -- longest prefix first, so the message names what was there.
        local k found=0
        for (( k=${#abs[@]}-1; k>=1; k-- )); do
            if series_applied "$k"; then
                echo "tree carries the first $k of ${#abs[@]} patches -- reapplying the series."
                git -C "$dir" checkout -- . || return 1
                apply_series || return 1
                found=1
                break
            fi
        done
        if [[ $found -eq 0 ]]; then
            error "$dir is modified, and the changes are not this patch series
(neither the whole of it nor any earlier part of it), so they are somebody's
own work and this will not throw them away. Either save them, or discard:
  git -C $dir checkout -- . && $0 ch"
            return 1
        fi
    fi
    cargo build --release --manifest-path "$dir/Cargo.toml" --bin cloud-hypervisor || return 1
    # Counter-check on the built tree: the patch brings exactly ONE
    # capability, and without it there is no host-visible window (no CUDA).
    grep -q 'get_shmem_config' "$dir/virtio-devices/src/vhost_user/generic_vhost_user.rs" \
        || { error "patch marker (SHMEM) missing from the source."; return 1; }
    "$bin" --version
    echo "OK  $bin"
}

# Refresh executable mtimes after Cargo validates its dependency graph.
# Acceptance freshness checks compare all sources with the oldest binary.
do_cargo()  {
    cargo build --release || return 1
    local b
    for b in "$LEA_ROOT"/target/release/*; do
        [[ -f $b && -x $b ]] && touch "$b"
    done
    return 0
}

# Hardware compatibility check; never part of the default software build.
do_check_driver() {
    local want have vend
    want=$(lea_want_driver) || return 1
    have=$(lea_driver_version); have=${have:-none}
    vend=$(git -C "$LEA_ROOT/vendor/open-gpu-kernel-modules" describe --tags --exact-match 2>/dev/null || echo none)
    printf 'want=%s  running=%s  vendor=%s\n' "$want" "$have" "$vend"
    if [[ $have != "$want" ]]; then
        # A supported second driver needs a run override, not a new ABI layout.
        if lea_driver_supported "$have"; then
            error "host driver is $have and this run targets $want.
Both are measured -- crates/nvrm-sys carries a layout for each. To MEASURE
$have, point the run at it and re-fetch the headers the catalogue resolves
against:
     export LEA_DRIVER=$have
     ./tools/build.sh vendor
     # Follow docs/abi-versions.md to select matching bindings and features.
DRIVER_VERSION stays $(lea_want_driver_file): it is what the tree is BUILT for."
        else
            error "host driver differs, and $have has no measured layout.
Supported: $(lea_supported_drivers | tr '\n' ' ')
Adding it is one entry in crates/nvrm-sys/abi.toml plus
     ./tools/build.sh vendor-abi $have && cargo xtask abi"
        fi
        return 1
    fi
    [[ $vend == "$want" ]] || { error "vendor/ differs (tools/build.sh vendor)"; return 1; }
    echo OK
}

CMD=all
if [[ $# -gt 0 ]]; then
    case $1 in
        all|vendor|vendor-abi|ch|cargo|check-driver) CMD=$1; shift ;;
        -h|--help) usage; exit 0 ;;
        --*) ;;
        *) error "unknown command: $1"; usage >&2; exit 2 ;;
    esac
fi

DRY=0
VERSIONS=()
while [[ $# -gt 0 ]]; do
    case $1 in
        --driver)
            [[ $# -ge 2 && -n $2 && $2 != --* ]] || { error "--driver requires VERSION or auto"; exit 2; }
            LEA_DRIVER=$2
            if [[ $LEA_DRIVER == auto ]]; then
                LEA_DRIVER=$(lea_driver_version) || { error "no running NVIDIA driver"; exit 1; }
                [[ -n $LEA_DRIVER ]] || { error "no running NVIDIA driver"; exit 1; }
            fi
            LEA_DRIVER=$(lea_want_driver) || exit 2
            export LEA_DRIVER
            shift 2 ;;
        --jobs)
            [[ $# -ge 2 && $2 =~ ^[1-9][0-9]*$ ]] || { error "--jobs requires a positive integer"; exit 2; }
            export CARGO_BUILD_JOBS=$2
            shift 2 ;;
        --dry-run) DRY=1; shift ;;
        --yes|-y) shift ;;
        -h|--help) usage; exit 0 ;;
        --minimal|--full|--skip-checks)
            error "$1 is an acceptance option; run ../Leandro-Test/scripts/build.sh directly"
            exit 2 ;;
        --*) error "unknown option: $1"; exit 2 ;;
        *)
            [[ $CMD == vendor-abi && $1 =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
                || { error "unexpected argument: $1"; exit 2; }
            VERSIONS+=("$1"); shift ;;
    esac
done

run() {
    if [[ $DRY -eq 1 ]]; then printf 'dry-run:'; printf ' %q' "$@"; printf '\n'
    else "$@"; fi
}
case $CMD in
    all) run do_vendor && run do_ch && run do_cargo ;;
    vendor) run do_vendor ;;
    vendor-abi) run do_vendor_abi "${VERSIONS[@]}" ;;
    ch) run do_ch ;;
    cargo) run do_cargo ;;
    check-driver) run do_check_driver ;;
esac
