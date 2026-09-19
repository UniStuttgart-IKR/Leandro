#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# Run the 17 software checks from a writable source checkout; no GPU or VM needed.
# Usage: tools/check.sh
# Each check runs even if an earlier check fails. Exit 0 if all pass, 1 otherwise.
set -uo pipefail

usage() { echo "Usage: tools/check.sh"; exit "${1:-0}"; }
error() { printf 'ERROR: %s\n' "$*" >&2; }

case ${1:-} in -h|--help) usage ;; esac
LEA_ROOT=${LEA_ROOT:-$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd -P)}
cd "$LEA_ROOT" || { error "cannot cd to $LEA_ROOT"; exit 1; }
[[ -f Cargo.toml ]] || { error "check.sh requires a writable Leandro source checkout"; exit 2; }

LEA_GRN=""; LEA_RED=""; LEA_R=""
if [[ ${LEA_COLOR:-} == 1 || ( -z ${LEA_COLOR:-} && -z ${NO_COLOR:-} && -t 1 ) ]]; then
    LEA_GRN=$'\e[32m'; LEA_RED=$'\e[31m'; LEA_R=$'\e[0m'
fi
lea_head() { printf '== %s ==\n' "$*"; }

# Read the generator's version list without loading VM configuration.
lea_supported_drivers() {
    grep -oE '^\[versions\."[^"]+"\]' "$LEA_ROOT/crates/nvrm-sys/abi.toml" \
        | sed -E 's/.*"(.*)".*/\1/'
}

# SDK constants vary in integer type; parameter fields follow header order.
CLIPPY_ALLOW=(-A clippy::unnecessary_cast -A clippy::field_reassign_with_default)

# Compare Rust class parameter sizes with sizeof() from the vendor headers.
# resource_list.h supplies the class-to-parameter mapping.
class_sizes() {
    local vendor=$LEA_ROOT/vendor/open-gpu-kernel-modules
    local sdk=$vendor/src/common/sdk/nvidia/inc rl out=$LEA_ROOT/target/class-sizes
    rl=$vendor/src/nvidia/src/kernel/rmapi/resource_list.h
    [[ -f $rl ]] || { echo "no $rl -- run tools/build.sh vendor"; return 1; }
    mkdir -p "$out" || return 1
    python3 - "$sdk" "$rl" "$out/sizes.c" <<'PY' || return 1
import re, sys, os, glob

sdk, rl_path, out_c = sys.argv[1], sys.argv[2], sys.argv[3]

# --- 1. RS_ENTRY: external class -> alloc param struct --------------------
lines = open(rl_path, errors="ignore").read().split("\n")
entries, i = [], 0
while i < len(lines):
    if lines[i].strip().startswith("RS_ENTRY("):
        block, depth = [], 0
        while i < len(lines):
            block.append(lines[i])
            depth += lines[i].count("(") - lines[i].count(")")
            if depth == 0 and len(block) > 1:
                break
            i += 1
        b = "\n".join(block)
        ext = re.search(r"/\* External Class\s*\*/\s*([A-Za-z0-9_]+)", b)
        par = re.search(r"/\* Alloc Param Info\s*\*/\s*(RS_REQUIRED|RS_OPTIONAL|RS_NONE)"
                        r"\s*(?:\(\s*([A-Za-z0-9_]+)\s*\))?", b)
        if ext and par and par.group(1) != "RS_NONE" and par.group(2):
            entries.append((ext.group(1), par.group(2)))
    i += 1

# --- 2. class name -> number ---------------------------------------------
classdef = {}
for path in glob.glob(f"{sdk}/**/*.h", recursive=True):
    for n, line in enumerate(open(path, errors="ignore"), 1):
        m = re.match(r"\s*#define\s+([A-Z0-9_]+)\s+\(?(0[xX][0-9a-fA-F]+)[uU]?\)?\s*(/\*.*)?$", line)
        if m and m.group(1) not in classdef and int(m.group(2), 16) <= 0xFFFF:
            classdef[m.group(1)] = int(m.group(2), 16)

# --- 3. locate each struct so the C program can include its header --------
hdr_of = {}
for path in glob.glob(f"{sdk}/**/*.h", recursive=True):
    src = open(path, errors="ignore").read()
    rel = os.path.relpath(path, sdk)
    for _, st in entries:
        if st in hdr_of:
            continue
        # typedef struct {...} NAME;  |  typedef struct NAME {...} NAME;
        # |  #define NAME OTHER  (alias)  |  typedef <scalar> NAME;
        if (re.search(r"\}\s*" + re.escape(st) + r"\s*;", src)
                or re.search(r"#define\s+" + re.escape(st) + r"\s+\w+", src)
                or re.search(r"typedef\s+\w+\s+" + re.escape(st) + r"\s*;", src)):
            hdr_of[st] = rel

pairs, missing = [], set()
for ext, st in entries:
    if ext not in classdef:
        continue
    if st not in hdr_of:
        missing.add(st)
        continue
    pairs.append((classdef[ext], ext, st))

c = ["#include <stdio.h>", "#include <nvtypes.h>", "#include <nvmisc.h>",
     "#include <nvstatus.h>", "#include <nvlimits.h>", "#include <nvos.h>"]
for h in sorted({hdr_of[st] for _, _, st in pairs}):
    c.append(f'#include "{h}"')
c.append("int main(void){")
for num, ext, st in sorted(set(pairs)):
    c.append(f'  printf("%u\\t%zu\\t%s\\n", {num}u, sizeof({st}), "{ext}");')
c.append("  return 0;}")
open(out_c, "w").write("\n".join(c) + "\n")
if missing:
    print("note: struct not located, not checked: " + ", ".join(sorted(missing)),
          file=sys.stderr)
print(f"note: {len(set(pairs))} classes resolved from resource_list.h", file=sys.stderr)
PY
    cc -o "$out/sizes" -I"$sdk" -I"$sdk/class" -I"$sdk/alloc" -I"$sdk/ctrl" \
       -I"$vendor/src/common/inc" "$out/sizes.c" || { echo "FAIL: the sizeof probe did not compile"; return 1; }
    "$out/sizes" | sort -n > "$out/vendor.tsv" || return 1
    cargo run --locked --quiet --bin nvrm-genhdr -- --expect-dump "$out/expect.txt" || return 1
    awk '$1=="class"{print $2"\t"$3}' "$out/expect.txt" | sort -n > "$out/table.tsv"
    # Compare only classes the table actually lists; report every mismatch.
    local bad=0 num size want name checked
    while IFS=$'\t' read -r num size; do
        want=$(awk -F'\t' -v n="$num" '$1==n{print $2; exit}' "$out/vendor.tsv")
        name=$(awk -F'\t' -v n="$num" '$1==n{print $3; exit}' "$out/vendor.tsv")
        [[ -z $want ]] && continue          # class not in resource_list.h (or RS_NONE)
        if [[ $want != "$size" ]]; then
            printf 'MISMATCH class %#06x %-34s table=%s vendor sizeof=%s\n' "$num" "$name" "$size" "$want"
            bad=1
        fi
    done < "$out/table.tsv"
    checked=$(while IFS=$'\t' read -r num _; do
                  awk -F'\t' -v n="$num" '$1==n{print n}' "$out/vendor.tsv"
              done < "$out/table.tsv" | wc -l)
    [[ $bad -eq 0 ]] && echo "class sizes: $checked of $(wc -l < "$out/table.tsv") table entries checked against vendor sizeof, all agree"
    return $bad
}

# Check the NVKMS mirror against vendor sizes, alignments and field offsets.
kapi_abi() {
    local vendor=$LEA_ROOT/vendor/open-gpu-kernel-modules out=$LEA_ROOT/target/kapi-abi
    [[ -d $vendor/kernel-open/common/inc ]] || { echo "no $vendor -- run tools/build.sh vendor"; return 1; }
    mkdir -p "$out/linux" || return 1
    # The mirrored header speaks kernel types. Give it just enough of them to
    # compile in userspace -- nothing else about it is changed.
    cat > "$out/kernel_types.h" <<'EOH'
#ifndef _KAPI_CHECK_KERNEL_TYPES_H_
#define _KAPI_CHECK_KERNEL_TYPES_H_
#include <stdint.h>
#include <stddef.h>
typedef uint8_t  __u8;
typedef uint32_t __u32;
typedef uint64_t __u64;
#endif
EOH
    echo '#include "../kernel_types.h"' > "$out/linux/types.h"
    echo '#include <stddef.h>'          > "$out/linux/stddef.h"
    cat > "$out/abi.c" <<'EOC'
/* libc first: the shims below put a fake <linux/types.h> on the include
 * path, and glibc's headers must not be parsed underneath it. */
#include <stdio.h>
#include <stddef.h>
#include <stdint.h>

/* Theirs. */
#include "nvtypes.h"
#include "nv-gpu-info.h"
#include "nv-modeset-interface.h"
#include "nv-kernel-rmapi-ops.h"

/* Ours. */
#include "nvrm_kapi.h"

#define SAME_SIZE(a, b) \
    _Static_assert(sizeof(a) == sizeof(b), #a " vs " #b ": size")
#define SAME_ALIGN(a, b) \
    _Static_assert(_Alignof(a) == _Alignof(b), #a " vs " #b ": alignment")
#define SAME_OFF(a, af, b, bf) \
    _Static_assert(offsetof(a, af) == offsetof(b, bf), #a "." #af " vs " #b "." #bf)

/* ---- nv_gpu_info_t ---- */
SAME_SIZE(nv_gpu_info_t, struct nvrm_gpu_info);
SAME_ALIGN(nv_gpu_info_t, struct nvrm_gpu_info);
SAME_OFF(nv_gpu_info_t, gpu_id,           struct nvrm_gpu_info, gpu_id);
SAME_OFF(nv_gpu_info_t, pci_info,         struct nvrm_gpu_info, pci_info);
SAME_OFF(nv_gpu_info_t, pci_info.domain,  struct nvrm_gpu_info, pci_info.domain);
SAME_OFF(nv_gpu_info_t, pci_info.bus,     struct nvrm_gpu_info, pci_info.bus);
SAME_OFF(nv_gpu_info_t, pci_info.slot,    struct nvrm_gpu_info, pci_info.slot);
SAME_OFF(nv_gpu_info_t, pci_info.function, struct nvrm_gpu_info, pci_info.function);
SAME_OFF(nv_gpu_info_t, needs_numa_setup, struct nvrm_gpu_info, needs_numa_setup);
SAME_OFF(nv_gpu_info_t, is_soc_disp,      struct nvrm_gpu_info, is_soc_disp);
SAME_OFF(nv_gpu_info_t, os_device_ptr,    struct nvrm_gpu_info, os_device_ptr);
_Static_assert(NV_MAX_GPUS == NVRM_NV_MAX_GPUS, "NV_MAX_GPUS");

/* ---- nvidia_modeset_callbacks_t ---- */
SAME_SIZE(nvidia_modeset_callbacks_t, struct nvrm_modeset_callbacks);
SAME_OFF(nvidia_modeset_callbacks_t, suspend, struct nvrm_modeset_callbacks, suspend);
SAME_OFF(nvidia_modeset_callbacks_t, resume,  struct nvrm_modeset_callbacks, resume);
SAME_OFF(nvidia_modeset_callbacks_t, remove,  struct nvrm_modeset_callbacks, remove);
SAME_OFF(nvidia_modeset_callbacks_t, probe,   struct nvrm_modeset_callbacks, probe);

/* ---- nvidia_modeset_rm_ops_t: the table nvidia_get_rm_ops() fills in ---- */
SAME_SIZE(nvidia_modeset_rm_ops_t, struct nvrm_modeset_rm_ops);
SAME_ALIGN(nvidia_modeset_rm_ops_t, struct nvrm_modeset_rm_ops);
SAME_OFF(nvidia_modeset_rm_ops_t, version_string, struct nvrm_modeset_rm_ops, version_string);
SAME_OFF(nvidia_modeset_rm_ops_t, system_info,    struct nvrm_modeset_rm_ops, system_info);
SAME_OFF(nvidia_modeset_rm_ops_t, system_info.allow_write_combining,
         struct nvrm_modeset_rm_ops, system_info.allow_write_combining);
SAME_OFF(nvidia_modeset_rm_ops_t, alloc_stack,    struct nvrm_modeset_rm_ops, alloc_stack);
SAME_OFF(nvidia_modeset_rm_ops_t, free_stack,     struct nvrm_modeset_rm_ops, free_stack);
SAME_OFF(nvidia_modeset_rm_ops_t, enumerate_gpus, struct nvrm_modeset_rm_ops, enumerate_gpus);
SAME_OFF(nvidia_modeset_rm_ops_t, open_gpu,       struct nvrm_modeset_rm_ops, open_gpu);
SAME_OFF(nvidia_modeset_rm_ops_t, close_gpu,      struct nvrm_modeset_rm_ops, close_gpu);
SAME_OFF(nvidia_modeset_rm_ops_t, op,             struct nvrm_modeset_rm_ops, op);
SAME_OFF(nvidia_modeset_rm_ops_t, set_callbacks,  struct nvrm_modeset_rm_ops, set_callbacks);

/* ---- nvidia_kernel_rmapi_ops_t: where the params union starts ---- */
_Static_assert(offsetof(nvidia_kernel_rmapi_ops_t, params) == NVRM_KAPI_PARAMS_OFF,
               "NVRM_KAPI_PARAMS_OFF");

/* ---- the return type of nvidia_get_rm_ops ----
 * The declaration itself is hidden here (see nvrm_kapi.h); what makes the
 * two the same function is the layout assertions above plus this: NV_STATUS
 * is an unsigned 32-bit type, which is what the module returns.
 */
_Static_assert(sizeof(NV_STATUS) == sizeof(__u32), "NV_STATUS width");
_Static_assert((NV_STATUS)-1 > 0, "NV_STATUS is unsigned");

/* ---- status codes ---- */
_Static_assert(NV_OK == NVRM_NV_OK, "NV_OK");
_Static_assert(NV_ERR_GENERIC == NVRM_NV_ERR_GENERIC, "NV_ERR_GENERIC");

int main(void)
{
    printf("kapi-abi: nvidia_modeset_rm_ops_t %zu bytes, params union at %zu, %d GPUs max\n",
           sizeof(nvidia_modeset_rm_ops_t),
           offsetof(nvidia_kernel_rmapi_ops_t, params),
           NV_MAX_GPUS);
    return 0;
}
EOC
    "${CC:-cc}" -std=gnu11 -Wall -Wextra -Wno-unused-parameter -DNVRM_KAPI_ABI_CHECK \
        -o "$out/abi" "$out/abi.c" \
        -I "$out" -I "$LEA_ROOT/guest-module/virtio_nvrm" \
        -I "$vendor/kernel-open/common/inc" \
        -I "$vendor/src/nvidia/arch/nvalloc/unix/include" \
        -I "$vendor/src/common/sdk/nvidia/inc" \
        -I "$vendor/src/common/inc" 2>&1 || {
        echo "kapi-abi: FAIL -- nvrm_kapi.h disagrees with the vendor headers"; return 1; }
    "$out/abi" || return 1
    echo "kapi-abi: PASS"
}

# Check the C parser against Rust output and test VRAM arithmetic.
c_interpreter() {
    "$LEA_ROOT/tools/ci/check-c.sh" tables
}

# Validate generated EDIDs and the size/refresh clamps.
edid_conformity() {
    "$LEA_ROOT/tools/ci/check-c.sh" edid
}

shell_syntax() {
    local rc=0 f
    local -a files=()
    # All .sh files plus the extensionless shebang scripts under tools/.
    while IFS= read -r f; do
        files+=("$f")
        if ! bash -n "$f"; then echo "syntax error: $f"; rc=1; fi
    done < <(git ls-files 'tools/**' 'tools/*' \
        | while IFS= read -r p; do
              [[ "$p" == *.sh ]] && { echo "$p"; continue; }
              [[ -f "$p" ]] && head -c 64 "$p" | head -1 | grep -qE '^#!.*(bash|sh)$' && echo "$p"
          done)

    # Match CI severity levels when ShellCheck is available.
    if command -v shellcheck >/dev/null 2>&1; then
        shellcheck -x -S error "${files[@]}" || rc=1
        shellcheck -x -S warning tools/*.sh tools/ci/*.sh || rc=1
    else
        echo "note: shellcheck not installed -- only 'bash -n' ran here."
        echo "      CI requires ShellCheck warnings to pass for the core scripts."
    fi
    return "$rc"
}

# Check SPDX identifiers: GPL-2.0-only in guest-module/, MIT elsewhere.
licence_headers() {
    local rc=0 f want got
    # Files that cannot carry a comment, or must not: the licence texts
    # themselves, single-token data files read by scripts, cargo- and
    # nix-generated lockfiles, a generated cscope index and a kbuild artifact.
    local -A exempt=(
        [LICENSE]=1 [guest-module/LICENSE]=1
        [LICENSES/MIT.txt]=1 [LICENSES/GPL-2.0-only.txt]=1
        [CH_VERSION]=1 [DRIVER_VERSION]=1
        [Cargo.lock]=1 [crates/vhost-user-nvrm/fuzz/Cargo.lock]=1 [flake.lock]=1
        [cscope.files]=1
        [guest-module/virtio_nvrm/.virtio_nvrm.o.d]=1
        # Binary Debian control files have no comment syntax.
        [packaging/guest-deb/control]=1 [packaging/host-deb/control]=1
    )
    while IFS= read -r f; do
        [[ -f $f ]] || continue
        [[ -n ${exempt[$f]:-} ]] && continue
        case "$f" in *.png|*.jpg|*.ppm|*.bin|*.img|*.ico) continue ;; esac
        # JSON has no comment syntax.
        case "$f" in *.json) continue ;; esac
        # Require a line-leading identifier, not a mention in prose.
        got=$(head -20 "$f" \
              | grep -oE '^[[:space:]]*(#|//|/\*|\*|<!--|;)?[[:space:]]*SPDX-License-Identifier: [A-Za-z0-9.+-]*' \
              | head -1 | grep -oE '[A-Za-z0-9.+-]*$')
        if [[ -z $got ]]; then echo "no SPDX-License-Identifier: $f"; rc=1; continue; fi
        case "$f" in guest-module/*) want=GPL-2.0-only ;; *) want=MIT ;; esac
        if [[ $got != "$want" ]]; then echo "wrong licence: $f has $got, expected $want"; rc=1; fi
    # The fuzz corpus is binary inputs -- no SPDX header is possible, the
    # way the .png/.bin cases above are skipped for their content.
    done < <(git ls-files | grep -vE '^(vendor/|patches/|crates/vhost-user-nvrm/fuzz/corpus/)')
    return "$rc"
}

# Reject obsolete marker glyphs; build patterns at runtime to avoid self-matches.
no_markers() {
    local B=$'\x60' W=$'\u26a0' G hits
    G="$B!$B|$B\\?$B|$W"
    hits=$(git ls-files | grep -vE '^(vendor/|patches/|crates/vhost-user-nvrm/fuzz/corpus/)' \
           | while IFS= read -r f; do [[ -f $f ]] || continue; grep -InE "$G" "$f" /dev/null; done)
    [[ -z $hits ]] && return 0
    echo "marker glyphs found -- say it in words instead:"; echo "$hits"
    return 1
}

# Keep Nix sparse-checkout paths equal to the ABI generator include paths.
# Strip comments so documentation cannot satisfy the check.
nix_sparse_dirs() {
    local nixf=nix/packages/nvidia-headers.nix rsf=crates/xtask/src/abi/mod.rs
    [[ -f $nixf && -f $rsf ]] || { echo "missing $nixf or $rsf"; return 1; }
    local want have
    want=$(sed -n '/const INCLUDE_DIRS/,/^];/p' "$rsf" \
           | sed 's://.*::' | grep -oE '"[^"]+"' | tr -d '"' | sort -u)
    have=$(sed -n '/sparseCheckout = \[/,/\];/p' "$nixf" \
           | sed 's:#.*::' | grep -oE '"[^"]+"' | tr -d '"' | sort -u)
    [[ -n $want && -n $have ]] || { echo "could not read one of the two lists"; return 1; }
    local missing extra
    missing=$(comm -23 <(echo "$want") <(echo "$have"))
    extra=$(comm -13 <(echo "$want") <(echo "$have"))
    [[ -z $missing && -z $extra ]] && return 0
    echo "$rsf and $nixf disagree about the NVIDIA include directories."
    [[ -n $missing ]] && { echo "  bindgen reads these and the derivation does NOT fetch them:";
                           echo "$missing" | sed 's/^/    /'
                           echo "    -> nix build fails with a 'file not found' on a header."; }
    [[ -n $extra ]] && { echo "  the derivation fetches these and bindgen does not read them:";
                         echo "$extra" | sed 's/^/    /'
                         echo "    -> harmless, but the lists should say the same thing."; }
    echo
    echo "Adding a path to sparseCheckout CHANGES THE HASH, and a fixed-output"
    echo "derivation's store path comes from the hash -- so nix hands back the"
    echo "old tree instead of rebuilding. Force it with a deliberately wrong"
    echo "hash and read the 'got:' line."
    return 1
}

# Check regeneration, each driver feature separately, then all features together.
abi_generated() {
    cargo run --locked --quiet --package xtask -- abi --check || return 1
    local v f
    while read -r v; do
        f="v${v%%.*}"
        cargo build --locked --quiet -p nvrm-sys --no-default-features --features "$f" || {
            echo "nvrm-sys does not build with only $f enabled"; return 1; }
    done < <(lea_supported_drivers)
    cargo build --locked --quiet -p nvrm-sys --all-features || {
        echo "nvrm-sys does not build with every version enabled at once"; return 1; }
    cargo build --locked --quiet --workspace --all-features || {
        echo "the workspace does not build with every version enabled at once"; return 1; }
    echo "abi: no diff; $(lea_supported_drivers | wc -l) versions, each alone and all together"
}

dangling_refs() {
    local J='JOURNA''L' P='prompt''s/' hits
    hits=$(git ls-files | grep -vE "^(vendor/|patches/|crates/vhost-user-nvrm/fuzz/corpus/|${P}|docs/${J}\.md$)" \
           | while IFS= read -r f; do [[ -f $f ]] || continue; grep -InE "\\b${J}\\b|${P}" "$f" /dev/null; done)
    [[ -z $hits ]] && return 0
    echo "references to material that is not in the public tree:"
    echo "$hits"
    echo
    echo "Resolve each one by CLASS, never by blanket deletion:"
    echo "  load-bearing -- the comment claims something the reference"
    echo "                  justified. Write the EVIDENCE into the comment:"
    echo "                  the number, the measurement, the date."
    echo "  decorative   -- a date or pointer carrying no claim. Delete it."
    echo "  navigating   -- \"the whole story is in X\". Point at a document"
    echo "                  that exists (docs/OPEN-QUESTIONS.md), or delete it."
    return 1
}

do_check() {
    [[ $# -eq 0 ]] || { error "check takes no options"; usage 2; }
    local fail=0
    local -a results=()
    step() {
        local name="$1" log t0 t1; shift
        log=$(mktemp); t0=$(date +%s)
        if "$@" >"$log" 2>&1; then
            t1=$(date +%s); results+=("${LEA_GRN}PASS${LEA_R}  $name  ($((t1 - t0))s)")
        else
            t1=$(date +%s); results+=("${LEA_RED}FAIL${LEA_R}  $name  ($((t1 - t0))s)"); fail=1
            echo "==== FAIL: $name -- last 40 lines ===="; tail -40 "$log"; echo "==== end $name ===="
        fi
        rm -f "$log"
    }
    step "cargo test"           cargo test --locked --workspace --all-targets
    # Release arithmetic wraps where debug arithmetic panics. Test both.
    step "cargo test (release)" cargo test --locked --workspace --all-targets --release
    step "cargo test --doc"     cargo test --locked --workspace --exclude nvrm-sys --doc
    # Check documentation links and markup; bindgen output is excluded.
    step "cargo doc"            env RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps --exclude nvrm-sys
    step "clippy"               cargo clippy --locked --workspace --all-targets -- -D warnings "${CLIPPY_ALLOW[@]}"
    step "clippy (all features)" cargo clippy --locked --workspace --all-targets --all-features -- -D warnings "${CLIPPY_ALLOW[@]}"
    step "nvrm-genhdr"          cargo run --locked --bin nvrm-genhdr -- --check
    step "c-interpreter"        c_interpreter
    step "edid"                 edid_conformity
    step "class-sizes"          class_sizes
    step "kapi-abi"             kapi_abi
    step "bash -n"              shell_syntax
    step "licence"              licence_headers
    step "abi"                  abi_generated
    step "nix-sparse"           nix_sparse_dirs
    step "dangling-refs"        dangling_refs
    step "no-markers"           no_markers
    echo
    lea_head "check.sh"
    printf '%s\n' "${results[@]}"
    exit "$fail"
}

do_check "$@"
