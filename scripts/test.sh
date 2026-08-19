#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# The tests: the GPU-free check band, the three gates, and the band that
# runs the gates one after another.
#
#   scripts/test.sh check                       the GPU-free band, 14 steps, one exit code
#   scripts/test.sh gpu      [--keep-vm] [--instance NAME] [--index N] [--guest ubuntu|nixos]
#                            [--transport ip|vsock]
#   scripts/test.sh vdisplay [--keep-vm] [--fresh] [--instance NAME] [--index N] [--size WxH]
#   scripts/test.sh display  [--keep-vm] [--instance NAME] [--index N]
#   scripts/test.sh gates [gpu] [vdisplay] [display] | all     the gates, aggregated
#
# CHECK runs anywhere, including without /dev/nvidia*: cargo test (debug AND
# release -- u64 arithmetic panics in one and wraps in the other, so they are
# not the same program), doc tests, rustdoc's own lints (cargo doc -D
# warnings), clippy, the generated header, the C
# table interpreter against the Rust writer, EDID conformance, every class
# size against a sizeof() compiled from the vendor headers, the kernel-API
# mirror against the vendor originals, script syntax, licence headers, no
# dangling references, no marker glyphs. It explicitly does NOT replace the
# gates.
#
# A GATE is binary: anything other than PASS is a defect. Every gate ends in
# exactly ONE JSON object as the last line of stdout and carries nothing else
# there (human text goes to stderr); exit 0 pass, 1 fail, 2 skip. `skip`
# means a PRECONDITION was not met -- no GPU, binaries not built, rig not
# ready -- never "a stage could not measure". The contract lives in
# scripts/lib/common.sh (lea_gate_*). No gate builds the workspace: the
# binaries in LEA_BIN_DIR are a precondition, and one that is missing or
# OLDER than the sources it was built from is a skip naming the fix
# (LEA_ALLOW_STALE=1 measures a stale binary on purpose). No gate needs
# crosvm; there is no crosvm.
#
#   gpu       the compute chain over virtio-nvrm on the standard dev VM
#             (vm0): tables, smi, memory, kernel, torch, robustness, own,
#             encode, plus an unscored counter-check with the module
#             unloaded. Every stage is measured against the NATIVE run on
#             the host -- the same probes, the same torch (vendor/hostvenv).
#             --instance/--index put it on another instance, --guest on
#             another guest OS, --transport on the other transport; the
#             STAGES do not change, and neither does the verdict's meaning.
#             Which instance, which guest and which transport were measured
#             are in the JSON line's facts, because "the gpu gate passed" is
#             not comparable across guests or transports without them.
#   vdisplay  the fast virtual-display gate on its own instance (vdisplay,
#             index 6): setup, tables, device, edid, pixel, teardown. No X,
#             no Vulkan, no streaming -- minutes rather than the better part
#             of an hour, the one to run after a refactor. FAIL-FAST: each
#             stage is a precondition of the next. Its edid stage reads the
#             bytes with a parser that shares NO code with the builder.
#   display   the whole desktop path on the desktop instance (index 5):
#             rig, card, connector, edid, modeset, capture (three readers and
#             a colour sequence), swapchain, present, rt, events, stream
#             (Sunshine -> NVENC -> Moonlight on THIS host), kernel. Needs X
#             and Sunshine in the guest image, moonlight on the host, Vulkan.
#             Last in the band because it is the newest and the most fragile.
#
# GATES is an aggregator, not a gate: one JSON line per gate on stdout, a
# summary on stderr, vm/out-gates/summary.jsonl on disk. Every verdict is
# cross-checked against the gate's exit code -- a mismatch counts as a fail.
# It exits 0 when everything passed, 1 when anything failed, and 2 when
# nothing failed but something was skipped: a band that did not fully run
# is not green. Sequentially, never in parallel: every gate wants the GPU
# exclusively, and one backend serves exactly ONE VM connection.
#
# --keep-vm leaves the gate's instance running at the end (LEA_KEEP_VM=1 to
# the library). WARNING: the FIRST vdisplay/display run on a fresh instance
# builds nvidia-modeset.ko and nvidia-drm.ko inside the guest, which takes
# minutes; that build lives on the instance's disk and is kept between runs
# on purpose. `vdisplay --fresh` throws the disk away and pays for it again.
set -uo pipefail
LEA_ROOT=${LEA_ROOT:-$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd -P)}
# shellcheck source=scripts/lib/rig.sh
source "$LEA_ROOT/scripts/lib/rig.sh"
lea_cd_root

usage() { lea_usage_from_header; exit "${1:-0}"; }

CMD=${1:-}
case $CMD in
    check|gpu|vdisplay|display|gates) shift ;;
    all) CMD=gates; shift ;;
    -h|--help|"") usage 0 ;;
    *) error "unknown subcommand: $CMD"; usage 2 ;;
esac

# The gates' shorthands, one set for all three.
say()  { lea_gate_say "$@"; }
pass() { lea_gate_pass "$@"; }
fail() { lea_gate_fail "$@"; }

# ============================================================================
# check -- the GPU-free band
# ============================================================================
# Clippy policy: -D warnings, with exactly two permanently allowed classes,
# both of which come from mirroring NVIDIA's headers:
#   unnecessary_cast          bindgen emits constants as u32 in one header
#                             version and as an enum in the next -- the cast
#                             guards against that type drift.
#   field_reassign_with_default  param structs are default-initialised and
#                             then filled field by field in header order,
#                             the way the SDK sample code does it.
# Every other exception is either fixed or annotated LOCALLY with its
# reason. Any new warning class turns the step red.
CLIPPY_ALLOW=(-A clippy::unnecessary_cast -A clippy::field_reassign_with_default)

# Every alloc-param size in the class table against a sizeof() compiled
# from the vendor headers. alloc_param_size() answers "how many bytes is the
# alloc parameter buffer of this class". Where bindgen knows the struct, the
# size is derived and cannot drift. For the rest the number is transcribed
# from a header -- and a transcribed number is exactly what goes stale when
# the driver version moves. A wrong size is not a failed allocation, it is
# an out-of-bounds read in the driver's copy_from_user. Mechanical end to
# end: resource_list.h maps class -> alloc param struct (RS_ENTRY); the
# class number comes from the class headers, the struct size from a C
# program that includes those headers and prints sizeof(); nvrm-genhdr
# --expect-dump prints what the Rust table believes. Classes the Rust table
# does not list are not an error; listing one with the WRONG size is.
class_sizes() {
    local vendor=$LEA_ROOT/vendor/open-gpu-kernel-modules
    local sdk=$vendor/src/common/sdk/nvidia/inc rl out=$LEA_ROOT/target/class-sizes
    rl=$vendor/src/nvidia/src/kernel/rmapi/resource_list.h
    [[ -f $rl ]] || { echo "no $rl -- run scripts/build.sh vendor"; return 1; }
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
    cargo run --quiet --bin nvrm-genhdr -- --expect-dump "$out/expect.txt" || return 1
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

# guest-module/virtio_nvrm/nvrm_kapi.h MIRRORS the interface nvidia-modeset.ko
# expects from nvidia.ko. It has to be a mirror -- virtio_nvrm.ko builds in
# the guest, where there is no NVIDIA source tree -- and a mirror is exactly
# what goes stale when the driver version moves. A wrong offset here is not
# a failed call; it is nvidia-modeset.ko calling through a function pointer
# that is not the function we put there. One C program includes BOTH headers
# and asserts size, alignment and every field offset of ours against
# sizeof/offsetof of theirs. Any disagreement is a compile error.
kapi_abi() {
    local vendor=$LEA_ROOT/vendor/open-gpu-kernel-modules out=$LEA_ROOT/target/kapi-abi
    [[ -d $vendor/kernel-open/common/inc ]] || { echo "no $vendor -- run scripts/build.sh vendor"; return 1; }
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

# The kernel module's C interpreter (nvrm_tables.c) run in user space
# against the real stream: tabcheck READS, with the module's own code, what
# table::build() WROTE, and the diff compares reading against writing field
# by field. Catches struct layout drift, wrong section pointers and broken
# find_* lookups -- without a guest kernel and without a VM.
c_interpreter() {
    local d=$LEA_ROOT/target/tabcheck
    mkdir -p "$d" || return 1
    cc -O2 -Wall -Wextra -Werror -o "$d/tabcheck" guest-module/virtio_nvrm/test/tabcheck.c || return 1
    cc -O2 -Wall -Wextra -Werror -o "$d/tabreject" guest-module/virtio_nvrm/test/tabreject.c || return 1
    cargo run --quiet --bin nvrm-genhdr -- --dump-tables "$d/stream.bin" || return 1
    cargo run --quiet --bin nvrm-genhdr -- --expect-dump "$d/expected.txt" || return 1
    "$d/tabcheck" "$d/stream.bin" > "$d/actual.txt" || return 1
    diff -u "$d/expected.txt" "$d/actual.txt" || return 1
    # The other half of the same interpreter: a damaged stream is REFUSED
    # (wrong magic/version/length/counts/checksum, too many nested slots),
    # and a lookup for a key the stream lacks answers NULL. Same code, same
    # stream, one damaged copy per case.
    "$d/tabreject" "$d/stream.bin" > "$d/reject.txt" || { cat "$d/reject.txt"; return 1; }
}

# The EDID the virtual display hands out, through a parser that knows the
# spec. edidcheck runs the module's own builder (nvrm_edid.c, the same
# translation unit) and edid-decode reads the bytes. Three defects were
# found this way on a block nothing had ever parsed: 6 bpc where the comment
# said 8, a max dotclock ten times too high, and GTF claimed without the
# continuous-frequency bit. The sizes are swept because the block is DERIVED
# from the requested one, not tabulated.
edid_conformity() {
    local d=$LEA_ROOT/target/edidcheck wh w h
    mkdir -p "$d" || return 1
    command -v edid-decode >/dev/null || {
        echo "edid-decode not installed (Debian/Ubuntu: edid-decode, Arch: edid-decode)" >&2; return 1; }
    cc -O2 -Wall -Wextra -Werror -o "$d/edidcheck" guest-module/virtio_nvrm/test/edidcheck.c || return 1
    # The step BEFORE edid-decode: nvrm_edid_effective() clamps a requested
    # (w, h, rate) down to what the EDID's fixed-width fields can hold, and
    # the vblank hrtimer paces off the same clamped rate. A rate that slipped
    # past the clamp would wrap the DTD's 16-bit pixel clock and decode as a
    # wildly wrong refresh with no error anywhere. edidclamp runs that
    # arithmetic (same translation unit) over a matrix incl. 8K and 240 Hz.
    cc -O2 -Wall -Wextra -Werror -o "$d/edidclamp" guest-module/virtio_nvrm/test/edidclamp.c || return 1
    "$d/edidclamp" >&2 || return 1
    for wh in 800x600 1280x720 1920x1080 2560x1440 2560x1600 3840x2160; do
        w=${wh%x*}; h=${wh#*x}
        "$d/edidcheck" "$w" "$h" "$d/$wh.bin" || return 1
        if ! edid-decode --check "$d/$wh.bin" > "$d/$wh.txt" 2>&1; then
            echo "EDID for $wh is not conformant:" >&2
            sed -n '/^Warnings:/,$p' "$d/$wh.txt" >&2
            return 1
        fi
    done
}

shell_syntax() {
    local rc=0 f
    # All .sh files plus the extensionless shebang scripts under scripts/.
    while IFS= read -r f; do
        if ! bash -n "$f"; then echo "syntax error: $f"; rc=1; fi
    done < <(git ls-files 'scripts/**' 'scripts/*' \
        | while IFS= read -r p; do
              [[ "$p" == *.sh ]] && { echo "$p"; continue; }
              [[ -f "$p" ]] && head -c 64 "$p" | head -1 | grep -qE '^#!.*(bash|sh)$' && echo "$p"
          done)
    return "$rc"
}

# Every tracked file carries an SPDX-License-Identifier, and it is the RIGHT
# one: GPL-2.0-only under guest-module/ (that code links against the guest
# kernel), MIT everywhere else. Without this the next new file silently
# ships unlicensed, which for a repo whose whole point is "reusable without
# a licence conversation" is the one defect nobody notices until someone
# else tries to reuse it. Source files additionally carry two
# SPDX-FileCopyrightText lines; this check stays on the identifier alone,
# because the identifier is the line whose absence or wrong value changes
# what a reader may legally do.
licence_headers() {
    local rc=0 f want got
    # Files that cannot carry a comment, or must not: the licence texts
    # themselves, single-token data files read by scripts, cargo- and
    # nix-generated lockfiles, a generated cscope index and a kbuild artifact.
    local -A exempt=(
        [LICENSE]=1 [guest-module/LICENSE]=1
        [LICENSES/MIT.txt]=1 [LICENSES/GPL-2.0-only.txt]=1
        [CH_VERSION]=1 [DRIVER_VERSION]=1 [GUEST_IMAGE]=1
        [Cargo.lock]=1 [crates/vhost-user-nvrm/fuzz/Cargo.lock]=1 [flake.lock]=1
        [cscope.files]=1
        [guest-module/virtio_nvrm/.virtio_nvrm.o.d]=1
    )
    while IFS= read -r f; do
        [[ -f $f ]] || continue
        [[ -n ${exempt[$f]:-} ]] && continue
        case "$f" in *.png|*.jpg|*.ppm|*.bin|*.img|*.ico) continue ;; esac
        # The identifier must stand in a COMMENT AT THE START OF A LINE.
        # Matching it anywhere would count a file that merely TALKS about
        # SPDX headers as having one -- measured: LICENSES.md documents the
        # split in a table cell, and the loose pattern read that prose as the
        # file's own licence.
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

# No marker glyphs in comments or prose. This repo used to mark every
# statement with a glyph: one for "measured", one for "conjecture", one for
# "trap". It reads as cryptic to anyone who has not been told the key, and
# the information it carried belongs in words -- write "Measured
# 2026-08-18:" or "Unverified:" or "Warning:" instead. 846 of them were
# removed for publication; this keeps them from coming back one comment at
# a time. Patterns are built at runtime so this file does not match itself.
no_markers() {
    local B=$'\x60' W=$'\u26a0' G hits
    G="$B!$B|$B\\?$B|$W"
    hits=$(git ls-files | grep -vE '^(vendor/|patches/|crates/vhost-user-nvrm/fuzz/corpus/)' \
           | while IFS= read -r f; do [[ -f $f ]] || continue; grep -InE "$G" "$f" /dev/null; done)
    [[ -z $hits ]] && return 0
    echo "marker glyphs found -- say it in words instead:"; echo "$hits"
    return 1
}

# Nothing points at material that is not in this tree. This tree carries no
# chronological lab log and no agent handover directory, by design. Comments
# used to cite both as the reason for a magic number, a workaround or an
# ordering, and every one of those citations became a dangling pointer --
# which is how a workaround gets deleted by the next person. They were
# resolved by writing the EVIDENCE into the comment instead of a location.
# The patterns are split so this file does not match itself.
dangling_refs() {
    local J='JOURNA''L' P='prompt''s/' hits
    hits=$(git ls-files | grep -vE "^(vendor/|patches/|crates/vhost-user-nvrm/fuzz/corpus/|${P}|docs/${J}\.md$)" \
           | while IFS= read -r f; do [[ -f $f ]] || continue; grep -InE "${J}|${P}" "$f" /dev/null; done)
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
    # HOW TO WAIT FOR IT: this holds a pidfile for its lifetime. Wait on the
    # FILE, never on `pgrep -f` (lea_hold_pidfile has the long version):
    #   until ! lea_running vm/test-check.pid; do sleep 15; done
    lea_hold_pidfile "$LEA_VM_DIR/test-check.pid"
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
    step "cargo test"           cargo test --workspace --all-targets
    # WARNING: do not optimise this away. u64 arithmetic PANICS in the debug
    # profile and WRAPS in the release profile, so the two are not the same
    # program. The memory-corruption hole in Arena::build was only visible as
    # what it is in a release run. What ships is release.
    step "cargo test (release)" cargo test --workspace --all-targets --release
    step "cargo test --doc"     cargo test --workspace --exclude nvrm-sys --doc
    # rustdoc's own lints: broken intra-doc links and unclosed HTML in doc
    # comments. Not compile errors, so nothing else here would notice --
    # and a doc that links to an item that no longer exists is exactly the
    # kind of stale text this tree tries not to carry. nvrm-sys is excluded
    # for the same reason its doctests are (bindgen output).
    step "cargo doc"            env RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --exclude nvrm-sys
    step "clippy"               cargo clippy --workspace --all-targets -- -D warnings "${CLIPPY_ALLOW[@]}"
    step "nvrm-genhdr"          cargo run --bin nvrm-genhdr -- --check
    step "c-interpreter"        c_interpreter
    step "edid"                 edid_conformity
    step "class-sizes"          class_sizes
    step "kapi-abi"             kapi_abi
    step "bash -n"              shell_syntax
    step "licence"              licence_headers
    step "dangling-refs"        dangling_refs
    step "no-markers"           no_markers
    echo
    lea_head "test.sh check"
    printf '%s\n' "${results[@]}"
    exit "$fail"
}

# ============================================================================
# gpu -- the compute gate
# ============================================================================
gate_gpu() {
    local keep=0 NAME=vm0 IDX=0 GUEST="" TRANSPORT=""
    while [[ $# -gt 0 ]]; do
        case $1 in
            --keep-vm)   keep=1; shift ;;
            --instance)  NAME=$2; shift 2 ;;
            --index)     IDX=$2; shift 2 ;;
            --guest)     GUEST=$2; shift 2 ;;
            --transport) TRANSPORT=$2; shift 2 ;;
            # Before lea_gate_begin on purpose: --help writes to stdout, and
            # after it stdout is the machine channel.
            -h|--help) usage 0 ;;
            *) echo "unknown argument: $1" >&2; exit 2 ;;
        esac
    done
    # lea_gate_begin FIRST, and before any precondition is tested: it is what
    # installs the safety net that emits a result line even when this dies
    # somewhere below.
    lea_gate_begin gpu
    export LEA_KEEP_VM=$keep
    local IP OUT NVRM_LOG
    IP=$(lea_ip "$IDX"); OUT=$LEA_VM_DIR/out-gpu; NVRM_LOG=$LEA_VM_DIR/$NAME/nvrm.log
    # WHICH GUEST is measured is a FACT about the run, not a footnote: the
    # same stages against an Ubuntu and a NixOS guest are two different
    # measurements, and a JSON line that does not say which is a line nobody
    # can compare later.
    lea_gate_fact instance "$NAME"
    lea_gate_fact guest "${GUEST:-$(lea_guest_os "$NAME")}"
    lea_gate_fact transport "${TRANSPORT:-$(lea_transport_of "$NAME")}"
    mkdir -p "$OUT"
    g() { lea_ssh "$IP" "$@"; }

    # The native reference measures on the host. Without persistence mode it
    # is 58 % slower (docs/TESTING.md section 3.2) -- the guest path gets its
    # warmth for free from the host daemon, the native run does not. SKIP,
    # not fail: an unprepared rig is a precondition, not a defect in what
    # this gate measures. Reported BEFORE the cleanup handler is registered,
    # so a gate that never started does not tear down somebody's rig.
    lea_rig_check || lea_gate_skip "rig not ready -- 'showcase.sh state --check' says which knob"
    say "binaries"
    lea_require_built "$LEA_BIN_DIR/vhost-user-nvrm" "$LEA_BIN_DIR/nvrm-genhdr" \
        || lea_gate_skip "$LEA_BUILD_PROBLEM"
    # Another rig's instances: this gate tears vm0 down and brings it up
    # again, and it must not touch anybody else's -- measured 2026-08-07, when
    # a gate run SIGTERMed the desktop instance's backend and the guest hung
    # in nvrm_xfer_run forever. Nothing here kills by name any more, so this
    # is about the GPU being shared with a measurement, not about collateral.
    local foreign owner
    if foreign=$(lea_foreign_rigs "$NAME"); then
        lea_gate_skip "instances of another rig are running: $(tr '\n' ' ' <<<"$foreign") -- stop them first (showcase.sh down --name X)"
    fi
    if owner=$(lea_inst_owner "$NAME"); then
        lea_gate_skip "$NAME is in use by pid $owner (a bench or a showcase run?) -- not taking it over"
    fi
    lea_rig_down_on_exit "$NAME"
    # Waiters use this file, never `pgrep -f` (lea_hold_pidfile has the
    # long version):  until ! lea_running vm/test-gpu.pid; do sleep 10; done
    lea_hold_pidfile "$LEA_VM_DIR/test-gpu.pid"

    say "layout guard"
    "$LEA_BIN_DIR/nvrm-genhdr" --check guest-module/virtio_nvrm/nvrm_wire.h \
        || { fail layout "the generated C header is out of date (nvrm-genhdr --check)"; lea_gate_finish "nvrm_wire.h is stale"; }

    say "stop VM, start fresh backend, provision, load virtio_nvrm"
    # A FRESH backend on every run: one backend serves exactly ONE VM
    # connection. LEA_MANAGED_COMPAT: partial managed-memory support, opt-in;
    # managedprobe needs it, nothing else notices it is there.
    lea_rig_down "$NAME" >/dev/null 2>&1
    LEA_DEBUG=1 LEA_MANAGED_COMPAT=1 lea_rig_up "$NAME" --index "$IDX" ${GUEST:+--guest "$GUEST"} \
        ${TRANSPORT:+--transport "$TRANSPORT"} >"$OUT/rig.log" 2>&1 \
        || { tail -15 "$OUT/rig.log"; fail rig "the rig did not come up -- see $OUT/rig.log"; lea_gate_finish "rig up failed"; }
    local HOST_SUM
    HOST_SUM=$(grep -o 'checksum 0x[0-9a-f]*' "$NVRM_LOG" | head -1 | awk '{print $2}')
    lea_gate_fact host_checksum "$HOST_SUM"
    echo "host tables: $(grep 'tables v' "$NVRM_LOG" | head -1)"

    # ---- tables ----
    say "tables -- device, hello, descriptor tables"
    g 'sudo dmesg | grep virtio_nvrm' > "$OUT/dmesg.txt" 2>&1
    local GUEST_SUM
    GUEST_SUM=$(grep -o 'checksum 0x[0-9a-f]*' "$OUT/dmesg.txt" | tail -1 | awk '{print $2}')
    lea_gate_fact guest_checksum "$GUEST_SUM"
    grep -E "Hello accepted" "$OUT/dmesg.txt" | tail -1
    grep -E "tables v.* accepted" "$OUT/dmesg.txt" | tail -1
    if grep -q "Hello accepted" "$OUT/dmesg.txt" && [[ -n $GUEST_SUM && $GUEST_SUM == "$HOST_SUM" ]]; then
        pass tables "checksum guest $GUEST_SUM == host $HOST_SUM"
    else
        fail tables "guest $GUEST_SUM vs host $HOST_SUM"
    fi

    # ---- smi ----
    say "smi -- nvidia-smi WITHOUT LD_PRELOAD (reference: native)"
    g 'cd ~/gpu && export LD_LIBRARY_PATH=$PWD/nv/lib
        timeout 90 ./nv/bin/nvidia-smi > /tmp/smi-module.txt 2>&1; echo "module=$?"' > "$OUT/s41.log" 2>&1
    g 'cat /tmp/smi-module.txt' > "$OUT/smi-module.txt" 2>&1
    # The reference is the native host run. Masking: drop the header line
    # (time of day), drop the process list (nvidia-smi resolves the PIDs RM
    # reports in its OWN /proc -- host PIDs do not exist in the guest, a
    # property of the boundary, not a translation error), and mask digit
    # RUNS to one N (utilization flips between one and two digits) and
    # WHITESPACE runs (right-aligned columns pad differently for 50% and 7%).
    # What masking costs is checked back VERBATIM: driver version and card
    # name are string-compared. The guest is told a MEDIATED name -- `NVIDIA
    # GeForce RTX 2070` becomes `Leandro RTX 2070` (vram.rs guest_card_name,
    # modelled on NVIDIA's own `GRID A100-10C`), and the mask folds BOTH
    # spellings to one token rather than dropping the line.
    nvidia-smi > "$OUT/smi-host.txt" 2>&1
    local WANT GPUNAME MEDIATED
    WANT=$(lea_want_driver)
    GPUNAME=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1)
    MEDIATED="Leandro $(sed -e 's/^NVIDIA //' -e 's/^GeForce //' <<<"$GPUNAME")"
    mask() {
        sed -e '1d' -e '/| Processes:/,$d' \
            -e "s/$MEDIATED/CARD/g" -e "s/$GPUNAME/CARD/g" \
            -e 's/[0-9][0-9]*/N/g' -e 's/  */ /g' "$1"
    }
    diff <(mask "$OUT/smi-host.txt") <(mask "$OUT/smi-module.txt") > "$OUT/smi.diff"
    if [[ ! -s $OUT/smi.diff ]] && grep -q "module=0" "$OUT/s41.log" \
       && grep -q "$WANT" "$OUT/smi-module.txt" \
       && [[ -n $GPUNAME ]] && grep -qF "$MEDIATED" "$OUT/smi-module.txt"; then
        pass smi "nvidia-smi identical to the native host run (masked), driver $WANT, guest sees $MEDIATED"
    else
        head -20 "$OUT/smi.diff"
        fail smi "see $OUT/smi.diff and $OUT/s41.log"
    fi

    # ---- memory ----
    say "memory -- nvprobe 2 (RM mmap through the window)"
    g 'cd ~/gpu && export LD_LIBRARY_PATH=$PWD/nv/lib NVPROBE_PTX=$PWD/kernels.ptx
        timeout 120 ./nvprobe 2 2>&1 | tail -4' > "$OUT/s42.log" 2>&1
    cat "$OUT/s42.log"
    grep -q "stage 2 ok" "$OUT/s42.log" && pass memory "memory ok" || fail memory "see $OUT/s42.log"

    # ---- kernel ----
    say "kernel -- nvprobe 3, managedprobe, pinned; the books balance"
    # managedprobe RUNS TWICE, and that is the point: the first run was green
    # before as well -- only the second one exposed that two guest processes
    # were sharing one PoolState (host_pool.rs, back_pool). stdbuf, because a
    # process that gets killed otherwise takes its stdio buffer to the grave.
    g 'cd ~/gpu && export LD_LIBRARY_PATH=$PWD/nv/lib NVPROBE_PTX=$PWD/kernels.ptx
        timeout 120 ./nvprobe 3 2>&1 | tail -2
        if [ -x ./managedprobe ]; then
            for i in 1 2; do echo "managedprobe run $i:"; timeout 120 stdbuf -o0 ./managedprobe 2>&1 | grep -E "stage[123] |ERROR"; done
        else echo "managedprobe: not built"; fi
        NVRL_PIN=1 timeout 300 venv/bin/python rlprobe.py 2>&1 | tail -2
        echo "--- accounting ---"
        for f in /sys/module/virtio_nvrm/parameters/stat_*; do echo "$(basename $f)=$(cat $f)"; done' \
        > "$OUT/s43.log" 2>&1
    cat "$OUT/s43.log"
    local PINS LEFT POOL MP_OK
    PINS=$(grep -o 'stat_osdesc_pins=[0-9]*' "$OUT/s43.log" | cut -d= -f2)
    LEFT=$(grep -o 'stat_pinned_kib=[0-9]*' "$OUT/s43.log" | cut -d= -f2)
    POOL=$(grep -o 'stat_pool_pages=[0-9]*' "$OUT/s43.log" | cut -d= -f2)
    MP_OK=$(grep -c "stage2 ok" "$OUT/s43.log")
    if grep -q "stage 3 ok (kernel, result correct)" "$OUT/s43.log" \
       && grep -q "attached to GPU VA" "$NVRM_LOG" \
       && [[ ${MP_OK:-0} -eq 2 ]] \
       && [[ ${PINS:-0} -gt 0 && ${LEFT:-1} -eq 0 && ${POOL:-1} -eq 0 ]]; then
        pass kernel "result correct, managedprobe 2x to stage 2, $PINS OS descriptors pinned, 0 KiB / 0 pool pages left open"
    else
        fail kernel "managedprobe-stage2=$MP_OK/2 pins=$PINS open=${LEFT}KiB pool=${POOL} -- see $OUT/s43.log"
    fi

    # ---- torch ----
    say "torch -- PyTorch, bit-identical to the reference run (native)"
    g 'cd ~/gpu && export LD_LIBRARY_PATH=$PWD/nv/lib NVPROBE_PTX=$PWD/kernels.ptx
        echo "### MODULE (no LD_PRELOAD)"
        timeout 300 venv/bin/python rlprobe.py 2>&1 | tail -2
        timeout 900 venv/bin/python convburn.py 2>&1 | tail -1' > "$OUT/s44.log" 2>&1
    # The reference is the native host run: the same scripts and the same
    # torch version from vendor/hostvenv -- otherwise we would be comparing
    # two libraries instead of two transport paths. What is compared are the
    # CORRECTNESS VALUES, not the times.
    {
        echo "### REFERENCE (native, host)"
        local HOSTPY=$LEA_HOSTVENV/bin/python
        [[ -x $HOSTPY ]] || echo "ERROR: $LEA_HOSTVENV missing -- the native reference cannot be run
       (a checkout makes it by hand: uv venv --python 3.12 vendor/hostvenv;
        a package carries its own -- build.sh package)"
        NVPROBE_PTX=$LEA_ROOT/probe/kernels/kernels.ptx timeout 300 "$HOSTPY" probe/python/rlprobe.py 2>&1 | tail -2
        timeout 900 "$HOSTPY" probe/python/convburn.py 2>&1 | tail -1
    } >> "$OUT/s44.log" 2>&1
    cat "$OUT/s44.log"
    local M_MEAN R_MEAN M_ACC R_ACC
    M_MEAN=$(sed -n '/### MODULE/,/### REFERENCE/p' "$OUT/s44.log" | grep -o 'mean10=[0-9.]*' | tail -1)
    R_MEAN=$(sed -n '/### REFERENCE/,$p'          "$OUT/s44.log" | grep -o 'mean10=[0-9.]*' | tail -1)
    M_ACC=$(sed -n '/### MODULE/,/### REFERENCE/p' "$OUT/s44.log" | grep -o 'acc=[0-9.e+-]*' | tail -1)
    R_ACC=$(sed -n '/### REFERENCE/,$p'           "$OUT/s44.log" | grep -o 'acc=[0-9.e+-]*' | tail -1)
    if [[ -n $M_MEAN && $M_MEAN == "$R_MEAN" && -n $M_ACC && $M_ACC == "$R_ACC" ]]; then
        pass torch "rlprobe $M_MEAN and convburn $M_ACC bit-identical to the reference (native)"
    else
        fail torch "module($M_MEAN,$M_ACC) vs reference-native($R_MEAN,$R_ACC)"
    fi

    # ---- robustness ----
    say "robustness -- concurrency, SIGKILL, rmmod"
    g 'cd ~/gpu && export LD_LIBRARY_PATH=$PWD/nv/lib NVPROBE_PTX=$PWD/kernels.ptx
        echo "(a) two CUDA processes at once"
        ( timeout 120 ./nvprobe 3 >/tmp/p1.txt 2>&1 & timeout 120 ./nvprobe 3 >/tmp/p2.txt 2>&1 & wait )
        echo "  P1: $(tail -1 /tmp/p1.txt)"; echo "  P2: $(tail -1 /tmp/p2.txt)"
        echo "(b) kill -9 mid-run"
        timeout 120 ./nvprobe 3 >/dev/null 2>&1 & BG=$!
        sleep 2; kill -9 $BG 2>/dev/null; wait 2>/dev/null; sleep 2
        echo "  open after kill: $(cat /sys/module/virtio_nvrm/parameters/stat_pinned_kib) KiB, $(cat /sys/module/virtio_nvrm/parameters/stat_pool_pages) pool pages"
        echo "  next run: $(timeout 120 ./nvprobe 3 2>&1 | tail -1)"
        echo "(c) rmmod with an open FD"
        python3 -c "import time; f=open(\"/dev/nvidiactl\"); time.sleep(6)" & sleep 1
        sudo rmmod virtio_nvrm 2>&1 | head -1; wait 2>/dev/null' > "$OUT/robust.log" 2>&1
    cat "$OUT/robust.log"
    if [[ $(grep -c "stage 3 ok (kernel, result correct)" "$OUT/robust.log") -eq 3 ]] \
       && grep -q "open after kill: 0 KiB, 0 pool pages" "$OUT/robust.log" \
       && grep -q "is in use" "$OUT/robust.log"; then
        pass robustness "2 in parallel ok, SIGKILL without a leak, rmmod refused"
    else
        fail robustness "see $OUT/robust.log"
    fi

    # ---- own ----
    # The VM's own view of its card: the process list and, under a cap, the
    # size and the name. The `smi` stage cannot see any of this: its mask
    # drops the process list, and it compares against the HOST's nvidia-smi,
    # which knows nothing of a cap. Without this stage the whole feature
    # could be dead and the gate would stay green.
    say "own -- the VM sees its own processes, and its own card under a cap"
    g 'cd ~/gpu && export LD_LIBRARY_PATH=$PWD/nv/lib
        cat > /tmp/gate-hold.py <<EOP
import torch, time, os
t = torch.empty(64 * 1024 * 1024 // 4, dtype=torch.float32, device="cuda")
t.fill_(1.0); torch.cuda.synchronize()
print("holder pid", os.getpid(), flush=True)
time.sleep(20)
EOP
        venv/bin/python /tmp/gate-hold.py > /tmp/gate-hold.out 2>&1 &
        sleep 12
        echo "holder=$(grep -oP "holder pid \K[0-9]+" /tmp/gate-hold.out)"
        nvidia-smi | sed -n "/Processes:/,\$p"
        wait 2>/dev/null' > "$OUT/own.log" 2>&1
    cat "$OUT/own.log"
    local HOLDER
    HOLDER=$(grep -oP 'holder=\K[0-9]+' "$OUT/own.log")
    if [[ -n $HOLDER ]] && grep -qE "^\|[^|]*[[:space:]]${HOLDER}[[:space:]]" "$OUT/own.log"; then
        pass own "guest PID $HOLDER listed in the VM's own nvidia-smi"
    else
        fail own "guest PID ${HOLDER:-?} missing from the process list -- see $OUT/own.log"
    fi

    # ---- encode ----
    # NVENC and NVDEC in the guest. This stage exists because the whole video
    # block sat behind ONE missing nested-pointer annotation: without it
    # NV0080_CTRL_CMD_GPU_GET_CLASSLIST returned a truncated class list and
    # the guest reported "OpenEncodeSessionEx failed: unsupported device (2)".
    # 1080p on purpose: the failure this stage guards against second is
    # CAPACITY -- a 1080p session holds 250.6 MiB of the host-visible window,
    # and at the old 256 MiB window it died as "CreateBitstreamBuffer failed:
    # out of memory (10)" while everything below ~2.0 MPix passed. The native
    # run is the reference: `-hwaccel cuda` on a yuv444p stream fails on the
    # HOST too, so without the counter-check this stage would report a
    # virtualization gap that is a chroma format. Hence -pix_fmt yuv420p, and
    # hence both sides.
    say "encode -- NVENC and NVDEC in the guest, against the native run"
    ffmpeg -hide_banner -loglevel error -f lavfi -i testsrc=size=1920x1080:rate=30 \
        -frames:v 30 -c:v h264_nvenc -pix_fmt yuv420p -y /tmp/gate-native.mp4 > "$OUT/encode-native.log" 2>&1
    local NAT_BYTES G_BYTES
    NAT_BYTES=$(stat -c %s /tmp/gate-native.mp4 2>/dev/null || echo 0)
    lea_gate_fact native_bitstream_bytes "$NAT_BYTES"
    g 'cd ~/gpu && export LD_LIBRARY_PATH=$PWD/nv/lib
        ffmpeg -hide_banner -loglevel error -f lavfi -i testsrc=size=1920x1080:rate=30 \
            -frames:v 30 -c:v h264_nvenc -pix_fmt yuv420p -y /tmp/gate-enc.mp4 2>&1
        echo "bytes=$(stat -c %s /tmp/gate-enc.mp4 2>/dev/null || echo 0)"
        echo "--- decode ---"
        ffmpeg -hide_banner -loglevel error -stats -hwaccel cuda -i /tmp/gate-enc.mp4 \
            -f null - 2>&1 | tail -1' > "$OUT/encode.log" 2>&1
    cat "$OUT/encode.log"
    G_BYTES=$(grep -oP 'bytes=\K[0-9]+' "$OUT/encode.log"); G_BYTES=${G_BYTES:-0}
    lea_gate_fact guest_bitstream_bytes "$G_BYTES"
    # Three independent claims, each observed to fail on its own: a
    # bitstream at all (the annotation), a hardware decode of 30 frames (no
    # software fallback hiding the failure), a size within 4x of native.
    if [[ $NAT_BYTES -le 0 ]]; then
        fail encode "the NATIVE run produced no bitstream -- host problem, not the guest's"
    elif [[ $G_BYTES -le 0 ]]; then
        fail encode "guest produced no bitstream -- see $OUT/encode.log"
    elif ! grep -qE 'frame=[[:space:]]*30' "$OUT/encode.log"; then
        fail encode "guest decode did not reach 30 frames -- see $OUT/encode.log"
    elif grep -qiE 'lacking required capabilities|Failed setup for format cuda' "$OUT/encode.log"; then
        fail encode "guest decode fell back to software -- see $OUT/encode.log"
    elif [[ $((G_BYTES * 4)) -lt $NAT_BYTES || $((NAT_BYTES * 4)) -lt $G_BYTES ]]; then
        fail encode "guest bitstream $G_BYTES B vs native $NAT_BYTES B -- more than 4x apart"
    else
        pass encode "1080p h264_nvenc $G_BYTES B (native $NAT_BYTES B), hardware decode 30 frames"
    fi
    rm -f /tmp/gate-native.mp4

    # ---- counter-check: without the module there is no GPU ----
    say "counter-check -- virtio_nvrm unloaded"
    g 'sudo rmmod virtio_nvrm; cd ~/gpu && export LD_LIBRARY_PATH=$PWD/nv/lib
        timeout 60 ./nvprobe 0 2>&1 | tail -2; sudo insmod ~/guest-module/virtio_nvrm/virtio_nvrm.ko' \
        > "$OUT/counter-check.log" 2>&1
    cat "$OUT/counter-check.log"
    grep -qE "cuInit|stage 0" "$OUT/counter-check.log" && echo "  (counter-check logged)"

    # The detail describes what HAPPENED, not what the gate hoped for.
    if [[ ${#_LEA_GATE_FAILED[@]} -eq 0 ]]; then
        lea_gate_finish "tables..torch + robustness + own + encode (ref=native), details in $OUT/"
    else
        lea_gate_finish "failed: ${_LEA_GATE_FAILED[*]}; logs in $OUT/"
    fi
}

# ============================================================================
# vdisplay -- the fast virtual-display gate
# ============================================================================
gate_vdisplay() {
    local keep=0 fresh=0 NAME=vdisplay INDEX=6 size=""
    while [[ $# -gt 0 ]]; do
        case $1 in
            --keep-vm)  keep=1; shift ;;
            --fresh)    fresh=1; shift ;;
            --instance) NAME=$2; shift 2 ;;
            --index)    INDEX=$2; shift 2 ;;
            --size)     size=$2; shift 2 ;;
            -h|--help)  usage 0 ;;
            *) echo "unknown argument: $1" >&2; exit 2 ;;
        esac
    done
    # An EMPTY or standard name would be somebody else's rig: index 0 is the
    # dev VM every other script addresses, so this gate would tear down their
    # work and report it as its own teardown.
    [[ $NAME =~ ^[A-Za-z0-9][A-Za-z0-9_-]*$ ]] || { echo "--instance wants a name like 'vdisplay', got '$NAME'" >&2; exit 2; }
    [[ $INDEX =~ ^[0-9]+$ && $INDEX -ge 1 && $INDEX -lt $LEA_MAX_VMS ]] \
        || { echo "--index must be 1..$((LEA_MAX_VMS - 1)); 0 is the standard dev VM" >&2; exit 2; }

    # The constants this gate asserts against, named and collected: a
    # threshold that appears once, inline, is a threshold nobody can find
    # when it has to move. The table counts are DERIVED by scanning xlate.rs,
    # not tabulated, so they move when the translation surface moves -- and
    # during a refactor a changed count is a finding, not noise. Regenerate:
    #     $LEA_BIN_DIR/nvrm-genhdr --expect-dump /tmp/expect.txt; head -1 /tmp/expect.txt
    # whose `hdr` line carries magic, version, bytes, checksum, ioctls,
    # classes, controls, nested -- fields 3, 8 and 9 are the three constants.
    local LEA_VD_TABLE_VERSION=1 LEA_VD_EXPECT_CONTROLS=17 LEA_VD_EXPECT_NESTED=16
    # 1600x900 and NOT the module's own 1920x1080 default, deliberately: a
    # gate that asks for the default cannot tell "the parameters reached the
    # EDID" from "nothing was ever written".
    local W=1600 H=900 HZ=60
    [[ -n $size ]] && {
        [[ $size =~ ^([0-9]+)x([0-9]+)$ ]] || { echo "--size wants WxH, got '$size'" >&2; exit 2; }
        W=${BASH_REMATCH[1]}; H=${BASH_REMATCH[2]}
    }
    local FRAME_REF=$LEA_ROOT/probe/data/vdisp-frame.ref
    local IP OUT NVRM_LOG
    IP=$(lea_ip "$INDEX"); OUT=$LEA_VM_DIR/out-vdisplay; NVRM_LOG=$LEA_VM_DIR/$NAME/nvrm.log
    mkdir -p "$OUT"

    lea_gate_begin vdisplay
    # FAIL-FAST by design, unlike the two other gates: every stage below is a
    # precondition of the next -- there is no EDID to read without a DRM node
    # and no frame to write without a mode -- so a stage that fails ends the
    # run rather than producing five follow-on failures that all name the
    # same cause. `abort` is how it says so and still leaves a verdict.
    set -e
    abort() { fail "$1" "$2"; lea_gate_finish "$2"; }
    g() { lea_guest "$IP" "$@"; }
    lea_gate_fact instance "$NAME"
    lea_gate_fact ip "$IP"
    lea_gate_fact requested_size "${W}x${H}"

    # ---- preconditions: all SKIP, not FAIL -- the rig was not ready to be
    # measured, which is a different statement from "the virtual display is
    # broken". Checked before anything is started.
    say "preconditions"
    local TOOLS_ERR foreign
    TOOLS_ERR=$(lea_require_tools cc ssh scp qemu-img awk sed 2>&1) || lea_gate_skip "$TOOLS_ERR"
    lea_require_built "$LEA_BIN_DIR/vhost-user-nvrm" "$LEA_BIN_DIR/vhost-user-input" \
        || lea_gate_skip "$LEA_BUILD_PROBLEM"
    [[ -x $LEA_CH ]] || lea_gate_skip "$LEA_CH missing -- run: scripts/build.sh ch"
    [[ -e /dev/nvidiactl ]] || lea_gate_skip "no /dev/nvidiactl -- this host has no NVIDIA driver loaded"
    [[ -f $FRAME_REF ]] || lea_gate_skip "$FRAME_REF missing"
    # Our OWN instance from a previous run is not foreign -- it is what the
    # setup stage takes down first.
    if foreign=$(lea_foreign_rigs "$NAME"); then
        lea_gate_skip "instances of another rig are running: $(tr '\n' ' ' <<<"$foreign")"
    fi
    if foreign=$(lea_inst_owner "$NAME"); then
        lea_gate_skip "$NAME is in use by pid $foreign -- not taking it over"
    fi
    echo "  binaries present, $LEA_CH present, GPU present, reference hashes present"

    # ---- setup ----
    say "setup -- rig up at $IP (instance $NAME, index $INDEX)"
    local T_SETUP; T_SETUP=$(date +%s)
    export LEA_KEEP_VM=$keep
    lea_rig_down_on_exit "$NAME"
    lea_hold_pidfile "$LEA_VM_DIR/test-vdisplay.pid"
    lea_rig_down "$NAME" >/dev/null 2>&1 || true
    # --input, because the measured display rig carries the virtio-input
    # device; provisioned like every guest, because virtio_nvrm's coexistence
    # needs nvrm_nodes.ko and params.txt from it (the payload is re-sent only
    # when it changed, so this is seconds on a warm instance); --fresh drops
    # overlay AND seed together.
    local -a ropt=(); [[ $fresh -eq 1 ]] && ropt+=(--fresh)
    lea_rig_up "$NAME" --index "$INDEX" --input "${ropt[@]}" >"$OUT/rig.log" 2>&1 \
        || abort setup "the rig did not come up -- see $OUT/rig.log"
    # `stage` is deliberately NOT run: it ships the X/GBM pieces this gate has
    # no use for. `modules` builds nvidia-modeset.ko/nvidia-drm.ko itself when
    # they are missing, which is the multi-minute first run in the header.
    lea_display_modules "$NAME" --vdisplay-size "${W}x${H}" --vdisplay-hz "$HZ" >"$OUT/modules.log" 2>&1 \
        || abort setup "the display modules did not load -- see $OUT/modules.log"
    lea_gate_fact boot_s "$(( $(date +%s) - T_SETUP ))"
    # Read the parameters BACK rather than trusting the writes, and compare
    # against the constants: a rig that came up at the module's 1920x1080
    # default is a rig whose parameters never arrived.
    local VD GW GH
    VD=$(g 'cat /sys/module/virtio_nvrm/parameters/vdisplay' 2>/dev/null || echo "")
    GW=$(g 'cat /sys/module/virtio_nvrm/parameters/vdisplay_width' 2>/dev/null || echo "")
    GH=$(g 'cat /sys/module/virtio_nvrm/parameters/vdisplay_height' 2>/dev/null || echo "")
    lea_gate_fact vdisplay "${VD:-unknown}"
    lea_gate_fact module_size "${GW:-?}x${GH:-?}"
    [[ $VD == 1 ]] || abort setup "vdisplay=$VD -- the module offers no virtual display"
    [[ $GW == "$W" && $GH == "$H" ]] \
        || abort setup "the module is at ${GW}x${GH}, asked for ${W}x${H} -- the parameters did not arrive"
    pass setup "vdisplay=1 at ${GW}x${GH}, guest answers at $IP"

    # ---- tables ----
    # The descriptor tables are what the whole carrier stands on: a guest
    # that accepted a different table set than the host built forwards
    # ioctls by the wrong description, and every symptom points elsewhere.
    say "tables -- the guest accepted what the host built"
    g 'sudo dmesg | grep "virtio_nvrm: tables"' > "$OUT/tables.txt" 2>&1 || true
    sed 's/^/    /' "$OUT/tables.txt"
    if grep -q 'tables rejected:' "$OUT/tables.txt"; then
        abort tables "$(grep 'tables rejected:' "$OUT/tables.txt" | tail -1)"
    fi
    local TLINE T_VER T_BYTES T_SUM T_IOCTL T_CLASS T_CTRL T_NEST HOST_SUM
    TLINE=$(grep 'tables v.* accepted' "$OUT/tables.txt" | tail -1 || true)
    [[ -n $TLINE ]] || abort tables "the guest logged no 'tables ... accepted' line at all"
    # One regex for all seven numbers, against the module's own format string
    # (virtio_nvrm.c).
    read -r T_VER T_BYTES T_SUM T_IOCTL T_CLASS T_CTRL T_NEST <<<"$(
        sed -n 's/.*tables v\([0-9]*\) accepted -- \([0-9]*\) bytes, checksum \(0x[0-9a-f]*\) (\([0-9]*\) ioctls, \([0-9]*\) classes, \([0-9]*\) controls, \([0-9]*\) nested).*/\1 \2 \3 \4 \5 \6 \7/p' <<<"$TLINE")"
    HOST_SUM=$(grep -o 'checksum 0x[0-9a-f]*' "$NVRM_LOG" 2>/dev/null | head -1 | awk '{print $2}' || true)
    lea_gate_fact table_version "${T_VER:-unknown}"
    lea_gate_fact table_bytes "${T_BYTES:-unknown}"
    lea_gate_fact guest_checksum "${T_SUM:-unknown}"
    lea_gate_fact host_checksum "${HOST_SUM:-unknown}"
    lea_gate_fact table_ioctls "${T_IOCTL:-unknown}"
    lea_gate_fact table_classes "${T_CLASS:-unknown}"
    lea_gate_fact table_controls "${T_CTRL:-unknown}"
    lea_gate_fact table_nested "${T_NEST:-unknown}"
    [[ -n ${T_VER:-} ]] || abort tables "could not parse the tables line -- the module's format changed? [$TLINE]"
    [[ $T_VER == "$LEA_VD_TABLE_VERSION" ]] || abort tables "table version $T_VER, expected $LEA_VD_TABLE_VERSION"
    [[ $T_CTRL == "$LEA_VD_EXPECT_CONTROLS" && $T_NEST == "$LEA_VD_EXPECT_NESTED" ]] \
        || abort tables "$T_CTRL controls / $T_NEST nested, expected $LEA_VD_EXPECT_CONTROLS / $LEA_VD_EXPECT_NESTED -- the translation surface moved (regenerate the constants, see the header)"
    # The constants pin the SHAPE; this pins the two ends to each other.
    [[ -n $HOST_SUM ]] || abort tables "the backend log $NVRM_LOG carries no checksum -- did the backend start?"
    [[ $T_SUM == "$HOST_SUM" ]] || abort tables "guest checksum $T_SUM vs host $HOST_SUM -- the two ends built different tables"
    pass tables "v$T_VER, $T_BYTES bytes, checksum $T_SUM == host, $T_IOCTL ioctls / $T_CLASS classes / $T_CTRL controls / $T_NEST nested"

    # ---- device ----
    # The node NUMBER is discovered, never assumed: DRM_IOCTL_VERSION for the
    # driver name is the only answer that holds -- the PCI parent says
    # virtio-pci and debugfs is not mounted in the guest image.
    say "device -- the DRM node and its connector"
    lea_guest_cc "$NAME" drm-modeset.c >"$OUT/guest-build.log" 2>&1 \
        || abort device "drm-modeset.c does not build in the guest (no gcc? build.sh bake installs build-essential) -- see $OUT/guest-build.log"
    local NODE CARD NODES CONNS NCONN CONN STATUS MODES
    NODE=$(g 'sudo ~/drm-modeset --find' 2>/dev/null || true)
    CARD=${NODE##*/}
    NODES=$(g 'ls /dev/dri' 2>/dev/null | tr '\n' ' ' || true)
    lea_gate_fact drm_node "${NODE:-none}"
    [[ -n $NODE ]] || abort device "no node with driver nvidia-drm -- /dev/dri: ${NODES:-<nothing>}"
    # Exactly ONE connector, connected, offering the mode. More than one on a
    # display the module invents means somebody else's connector came along.
    CONNS=$(g "ls -d /sys/class/drm/$CARD-*/ 2>/dev/null" || true)
    NCONN=$(grep -c . <<<"${CONNS:-}" || true)
    CONN=$(head -1 <<<"${CONNS:-}")
    lea_gate_fact connectors "$NCONN"
    [[ $NCONN -eq 1 ]] || abort device "$CARD has $NCONN connectors, expected exactly 1: $(tr '\n' ' ' <<<"${CONNS:-none}")"
    STATUS=$(g "cat ${CONN}status" 2>/dev/null || true)
    MODES=$(g "cat ${CONN}modes" 2>/dev/null | tr '\n' ' ' || true)
    lea_gate_fact connector "$(basename "$CONN")"
    lea_gate_fact connector_status "${STATUS:-unknown}"
    lea_gate_fact connector_modes "${MODES:-none}"
    [[ $STATUS == connected ]] || abort device "$(basename "$CONN") is '$STATUS', not connected"
    [[ $MODES == *"${W}x${H}"* ]] || abort device "$(basename "$CONN") does not offer ${W}x${H} -- modes: ${MODES:-none}"
    pass device "$NODE, $(basename "$CONN") connected, offers ${W}x${H} (/dev/dri: $NODES)"

    # ---- edid ----
    # The bytes are read by a parser that shares NO source with the builder
    # that wrote them (probe/c/edid-verify.c). The display gate compares the
    # connector's EDID against edidcheck, which IS the module's own builder,
    # so a change in nvrm_edid.c moves both sides and that comparison stays
    # green -- measured, by mutating e[10] and watching it pass.
    say "edid -- read by a parser that shares no code with the writer"
    cc -O2 -Wall -Wextra -Werror -o "$OUT/edid-verify" probe/c/edid-verify.c \
        || abort edid "probe/c/edid-verify.c does not compile on this host"
    # lea_ssh, NOT lea_guest: lea_guest strips CRs and an EDID is BINARY --
    # every 0x0d byte would be deleted on the way.
    lea_ssh "$IP" "sudo cat ${CONN}edid" > "$OUT/edid.bin" 2>/dev/null || true
    [[ -s $OUT/edid.bin ]] || abort edid "the connector handed out an empty EDID"
    local EDLINE ED_RC
    EDLINE=$("$OUT/edid-verify" "$OUT/edid.bin" "$W" "$H") && ED_RC=0 || ED_RC=$?
    echo "    $EDLINE"
    lea_gate_fact edid_bytes "$(stat -c%s "$OUT/edid.bin")"
    lea_gate_fact edid_monitor "$(sed -n 's/.*monitor="\([^"]*\)".*/\1/p' <<<"$EDLINE")"
    lea_gate_fact edid_preferred "$(sed -n 's/.*preferred=\([0-9x@]*\).*/\1/p' <<<"$EDLINE")"
    lea_gate_fact edid_checksum_ok "$(sed -n 's/.*checksum=\([a-zA-Z]*\).*/\1/p' <<<"$EDLINE")"
    [[ $ED_RC -eq 0 ]] || abort edid "$EDLINE"
    # edid-decode is the reviewer that knows the SPEC. When it is installed
    # and says the block is non-conformant, that is a failure -- a gate that
    # treats an available reviewer's verdict as optional reports what it
    # prefers.
    if command -v edid-decode >/dev/null 2>&1; then
        if edid-decode --check "$OUT/edid.bin" >"$OUT/edid-decode.txt" 2>&1; then
            lea_gate_fact edid_decode ok
        else
            lea_gate_fact edid_decode nonconformant
            sed -n '/^Warnings:/,$p' "$OUT/edid-decode.txt" | sed 's/^/    /'
            abort edid "edid-decode --check calls the block non-conformant -- see $OUT/edid-decode.txt"
        fi
    else
        lea_gate_fact edid_decode "not installed"
    fi
    pass edid "$(stat -c%s "$OUT/edid.bin") bytes, checksum ok, preferred timing is ${W}x${H}"

    # ---- pixel ----
    say "pixel -- a frame written, and read back through the export path"
    local REF PIXLINE PIX_RC FRAME_HASH
    REF=$(awk -v k="${W}x${H}" '$1 == k { print $2; exit }' "$FRAME_REF")
    [[ -n $REF ]] || abort pixel "no reference hash for ${W}x${H} in $FRAME_REF -- generate it: cc -O2 -o /tmp/vf probe/c/vdisp-frame.c && /tmp/vf --reference ${W}x${H} >> $FRAME_REF"
    lea_gate_fact frame_ref "$REF"
    lea_scp probe/c/vdisp-frame.c "$IP:vdisp-frame.c" >/dev/null 2>&1 || abort pixel "could not copy vdisp-frame.c to the guest"
    g 'gcc -O2 -Wall -Wextra -o ~/vdisp-frame ~/vdisp-frame.c' >>"$OUT/guest-build.log" 2>&1 \
        || abort pixel "vdisp-frame.c does not build in the guest -- see $OUT/guest-build.log"
    # gnome-shell opens the node the moment it appears and holds DRM master;
    # SETCRTC is then EACCES. Stopped, and NOT started again.
    g 'sudo systemctl stop gdm3 2>/dev/null || true; sleep 2; sudo pkill -9 -x gnome-shell 2>/dev/null || true; sleep 1' >/dev/null 2>&1 || true
    PIXLINE=$(g "sudo ~/vdisp-frame $NODE" 2>&1) && PIX_RC=0 || PIX_RC=$?
    echo "$PIXLINE" > "$OUT/pixel.txt"
    sed 's/^/    /' "$OUT/pixel.txt"
    FRAME_HASH=$(sed -n 's/.*readback=\(0x[0-9a-f]*\).*/\1/p' <<<"$PIXLINE")
    lea_gate_fact frame_hash "${FRAME_HASH:-unknown}"
    lea_gate_fact frame_written "$(sed -n 's/.*written=\(0x[0-9a-f]*\).*/\1/p' <<<"$PIXLINE")"
    lea_gate_fact readback_via "$(sed -n 's/.*readback_via=\([a-z]*\).*/\1/p' <<<"$PIXLINE")"
    lea_gate_fact fb_id "$(sed -n 's/.* fb=\([0-9]*\) .*/\1/p' <<<"$PIXLINE")"
    case $PIX_RC in
        0) ;;
        1) abort pixel "what came back is not what went in -- see $OUT/pixel.txt" ;;
        2) abort pixel "the frame chain refused before it could compare (no master? no mode?) -- see $OUT/pixel.txt" ;;
        *) abort pixel "vdisp-frame exited $PIX_RC, which it never does on purpose -- see $OUT/pixel.txt" ;;
    esac
    # The probe compared its own write against its own read; the reference
    # catches the pattern itself changing under both halves at once.
    [[ $FRAME_HASH == "$REF" ]] \
        || abort pixel "round trip is self-consistent at $FRAME_HASH but the committed reference for ${W}x${H} is $REF -- see $FRAME_REF"
    pass pixel "${W}x${H} written and read back as $FRAME_HASH, matching the committed reference"

    # ---- teardown ----
    # A gate that only reads its own output misses the oops that happened
    # beside it, and a gate that never checks what it left behind is how
    # seven orphaned backends accumulate (measured 2026-08-07).
    say "teardown -- nothing left behind, nothing broken beside us"
    g 'sudo dmesg' > "$OUT/dmesg.txt" 2>/dev/null || true
    local OOPS WARNS
    OOPS=$(grep -cE "BUG:|Oops:|kernel NULL pointer" "$OUT/dmesg.txt" || true); OOPS=${OOPS:-0}
    WARNS=$(grep -c "WARNING:" "$OUT/dmesg.txt" || true); WARNS=${WARNS:-0}
    lea_gate_fact dmesg_oops "$OOPS"
    lea_gate_fact dmesg_warnings "$WARNS"
    if [[ $keep -eq 1 ]]; then
        # Reported as a pass with a fact rather than silently: the checks
        # below are ABOUT the rig being gone.
        lea_gate_fact kept 1
        lea_gate_fact leftovers "not checked (--keep-vm)"
        [[ $OOPS -eq 0 ]] || abort teardown "$OOPS oops/BUG in the guest log -- see $OUT/dmesg.txt"
        pass teardown "kept up on request; no oops, $WARNS WARNING(s) in the guest log"
    else
        lea_rig_down "$NAME" >>"$OUT/rig.log" 2>&1 || true
        sleep 1
        # By PIDFILE and by file, never by `pgrep -f`.
        local LEFT="" f dir
        dir=$(lea_inst_dir "$NAME")
        for f in ch nvrm input; do
            lea_running "$dir/$f.pid" && LEFT="$LEFT $f.pid(pid $(cat "$dir/$f.pid"))"
        done
        for f in nvrm.sock input.sock input.fifo; do
            [[ -e $dir/$f ]] && LEFT="$LEFT $f"
        done
        LEFT=${LEFT# }
        lea_gate_fact leftovers "${LEFT:-none}"
        [[ -z $LEFT ]] || abort teardown "still there after down: $LEFT"
        [[ $OOPS -eq 0 ]] || abort teardown "$OOPS oops/BUG in the guest log -- see $OUT/dmesg.txt"
        # WARNINGs are a FACT, not a verdict: the display path is known to
        # produce one per page flip that carries an event
        # (nvidia-drm-crtc.h:355), so a count is not a constant. The display
        # gate's `kernel` stage separates the known one from a new one.
        pass teardown "instance $NAME gone (no pidfile, no socket, no fifo); no oops, $WARNS WARNING(s)"
    fi
    lea_gate_finish "${W}x${H} on $CARD: tables v$T_VER checksum $T_SUM, EDID read by an independent parser, frame $FRAME_HASH round-tripped; details in $OUT/"
}

# ============================================================================
# display -- the whole desktop path
# ============================================================================
gate_display() {
    local keep=0 NAME=desktop INDEX=5
    while [[ $# -gt 0 ]]; do
        case $1 in
            --keep-vm)  keep=1; shift ;;
            --instance) NAME=$2; shift 2 ;;
            --index)    INDEX=$2; shift 2 ;;
            -h|--help)  usage 0 ;;
            *) echo "unknown argument: $1" >&2; exit 2 ;;
        esac
    done
    lea_gate_begin display
    say "binaries"
    lea_require_built "$LEA_BIN_DIR/vhost-user-nvrm" "$LEA_BIN_DIR/vhost-user-input" "$LEA_BIN_DIR/nvrm-genhdr" \
        || lea_gate_skip "$LEA_BUILD_PROBLEM"
    # XN: the X display number the capture and swapchain stages use. Not
    # :0, which gdm3 owns -- a gate that reused a human's server would
    # measure somebody's leftover.
    local IP OUT XN=7
    IP=$(lea_ip "$INDEX"); OUT=$LEA_VM_DIR/out-display
    mkdir -p "$OUT"
    g() { lea_ssh "$IP" "$@"; }
    export LEA_KEEP_VM=$keep
    lea_rig_down_on_exit "$NAME"
    lea_hold_pidfile "$LEA_VM_DIR/test-display.pid"

    say "layout guard and helpers"
    "$LEA_BIN_DIR/nvrm-genhdr" --check guest-module/virtio_nvrm/nvrm_wire.h \
        || { fail helpers "the generated C header is out of date (nvrm-genhdr --check)"; lea_gate_finish "nvrm_wire.h is stale"; }
    # The reference EDID comes from the module's OWN builder compiled for
    # userspace -- the same translation unit, so the comparison below cannot
    # drift from what the guest hands out. A helper that does not compile is
    # a FAIL with a reason: the run reached the gate, so it owes a verdict.
    cc -O2 -Wall -Wextra -Werror -o "$OUT/edidcheck" guest-module/virtio_nvrm/test/edidcheck.c || {
        fail helpers "guest-module/virtio_nvrm/test/edidcheck.c does not compile on this host"
        lea_gate_finish "the EDID reference helper does not build"
    }

    # ---- rig ----
    say "rig"
    local foreign
    if foreign=$(lea_foreign_rigs "$NAME"); then
        lea_gate_skip "instances of another rig are running: $(tr '\n' ' ' <<<"$foreign")"
    fi
    if foreign=$(lea_inst_owner "$NAME"); then
        lea_gate_skip "$NAME is in use by pid $foreign -- not taking it over"
    fi
    lea_rig_down "$NAME" >/dev/null 2>&1
    lea_rig_up "$NAME" --index "$INDEX" --input >"$OUT/rig.log" 2>&1 \
        || { fail rig "the rig did not come up -- see $OUT/rig.log"; lea_gate_finish "rig up failed"; }
    lea_display_stage "$NAME" >"$OUT/stage.log" 2>&1 \
        || { fail rig "staging failed -- see $OUT/stage.log"; lea_gate_finish "display staging failed"; }
    lea_display_modules "$NAME" >"$OUT/modules.log" 2>&1 \
        || { fail rig "module load failed -- see $OUT/modules.log"; lea_gate_finish "display modules failed"; }
    g 'sudo dmesg -C' >/dev/null 2>&1
    echo "  rig up at $IP"
    local W H VD
    W=$(g 'cat /sys/module/virtio_nvrm/parameters/vdisplay_width' 2>/dev/null | tr -d '\r')
    H=$(g 'cat /sys/module/virtio_nvrm/parameters/vdisplay_height' 2>/dev/null | tr -d '\r')
    VD=$(g 'cat /sys/module/virtio_nvrm/parameters/vdisplay' 2>/dev/null | tr -d '\r')
    lea_gate_fact width "$W"; lea_gate_fact height "$H"; lea_gate_fact vdisplay "$VD"
    if [[ $VD == 1 && -n $W && -n $H ]]; then
        pass rig "vdisplay=1, ${W}x${H}"
    else
        fail rig "vdisplay=$VD width=$W height=$H -- the module has no virtual display"
        lea_gate_finish "the guest module does not offer a virtual display"
    fi

    # ---- card ----
    say "card"
    local NODES NODE CARD
    NODES=$(g 'ls /dev/dri' 2>/dev/null | tr '\n' ' ')
    g 'gcc -O2 -Wall -Wextra -o ~/drm-modeset ~/drm-modeset.c' >/dev/null 2>&1 || {
        fail card "drm-modeset.c does not build in the guest"; lea_gate_finish "the guest cannot build the probes"; }
    NODE=$(g 'sudo ~/drm-modeset --find' 2>/dev/null | tr -d '\r')
    CARD=${NODE##*/}
    lea_gate_fact card "$CARD"
    if [[ -n $NODE ]]; then
        pass card "$NODE is the nvidia-drm node (/dev/dri: $NODES)"
    else
        fail card "no node with driver nvidia-drm -- /dev/dri: ${NODES:-<nothing>}"
        lea_gate_finish "nvidia-drm created no DRM node"
    fi

    # ---- connector ----
    say "connector"
    local CONN STATUS MODES
    CONN=$(g "ls -d /sys/class/drm/$CARD-*/ 2>/dev/null | head -1" | tr -d '\r')
    if [[ -z $CONN ]]; then
        fail connector "$CARD has no connector at all"
    else
        STATUS=$(g "cat ${CONN}status" 2>/dev/null | tr -d '\r')
        MODES=$(g "cat ${CONN}modes" 2>/dev/null | tr '\n' ' ')
        if [[ $STATUS == connected && $MODES == *"${W}x${H}"* ]]; then
            pass connector "$(basename "$CONN") connected, modes: $MODES"
        else
            fail connector "status=$STATUS modes=$MODES (wanted ${W}x${H})"
        fi
    fi

    # ---- edid: byte for byte. A connector that reports `connected` while
    # carrying a different EDID means NVKMS answered from somewhere other
    # than this module. What this proves is that the bytes ARRIVED INTACT,
    # not that they are correct -- vdisplay's edid stage and check's edid
    # step do that with independent readers.
    say "edid"
    if [[ -n $CONN ]]; then
        g "sudo cat ${CONN}edid" > "$OUT/guest-edid.bin" 2>/dev/null
        "$OUT/edidcheck" "$W" "$H" "$OUT/ours.bin"
        if cmp -s "$OUT/ours.bin" "$OUT/guest-edid.bin"; then
            pass edid "$(stat -c%s "$OUT/ours.bin") bytes, identical to the module's own builder"
        else
            fail edid "connector EDID differs from the module's -- see $OUT/{ours,guest-edid}.bin"
        fi
    else
        fail edid "no connector to read an EDID from"
    fi

    # ---- modeset ----
    # 30 flips, not one. Measured 2026-08-16: a single flip completes on a
    # configuration whose compositor then stops dead a few seconds later. The
    # condition insists that EVERY issued flip completed and none timed out.
    say "modeset"
    g 'sudo systemctl stop gdm3 2>/dev/null || true; sleep 2; sudo pkill -9 -x gnome-shell 2>/dev/null || true; sleep 1' >/dev/null 2>&1
    local MS
    MS=$(g "sudo timeout 45 ~/drm-modeset --flips 30 $NODE" 2>&1)
    echo "$MS" > "$OUT/modeset.txt"; echo "$MS" | sed 's/^/    /'
    if grep -q "mode_valid 1" <<<"$MS" &&
       grep -qE "PAGE_FLIP: ([0-9]+) issued, \1 completed, 0 timed out" <<<"$MS" &&
       grep -qE "after flip: fb ([0-9]+) \(expected \1\)" <<<"$MS"; then
        pass modeset "mode taken, 30 flips all completed, CRTC names the new framebuffer"
    else
        fail modeset "see $OUT/modeset.txt"
    fi

    # ---- capture ----
    # A colour goes in and has to come out through three readers, then a
    # SEQUENCE has to arrive in order. On 2026-08-13 x11grab read black on
    # this path while xwd read the right pixels, and it did not reproduce
    # the next day. The X server is the one the project SHIPS: the NVIDIA
    # driver, started by lea_display_x. The colour comes from a client that
    # DRAWS, not from xsetroot -- on the NVIDIA driver `xsetroot -solid` sets
    # the root background pixmap and, with no window manager, nothing ever
    # repaints it. paintprobe owns its pixels.
    say "capture"
    g 'gcc -O2 -Wall -Wextra -o ~/shmprobe ~/shmprobe.c -lX11 -lXext
       gcc -O2 -Wall -Wextra -o ~/paintprobe ~/paintprobe.c -lX11' >/dev/null 2>&1
    lea_display_x "$NAME" --display ":$XN" --virtual "${W}x${H}" >"$OUT/xorg.log" 2>&1 || true
    local CAP WANTC
    CAP=$(g "export DISPLAY=:$XN
     xrandr >/dev/null 2>&1 || { echo 'X-DOWN'; exit 0; }
     pkill -x paintprobe 2>/dev/null; sleep 1
     setsid nohup ~/paintprobe 20a060 --geometry ${W}x${H}+0+0 >/tmp/pp.log 2>&1 &
     sleep 3
     ~/shmprobe 400 300 | grep -E 'XGetImage|XShmGetImage'
     rm -f /tmp/gate*.png
     timeout 40 ffmpeg -hide_banner -loglevel error -f x11grab -framerate 4 \
        -video_size ${W}x${H} -i :$XN -vframes 4 -y /tmp/gate%d.png >/dev/null 2>&1
     for i in 1 2 3 4; do printf 'grab%d %s\n' \$i \"\$(convert /tmp/gate\$i.png -format '%[pixel:p{400,300}]' info: 2>/dev/null)\"; done
     pkill -x paintprobe 2>/dev/null; sleep 1
     rm -f /tmp/seq*.png
     ( sleep 1; ~/paintprobe --seq c05020,3060c0,20a060 --hold 900 --geometry ${W}x${H}+0+0 >/dev/null 2>&1 ) &
     timeout 40 ffmpeg -hide_banner -loglevel error -f x11grab -framerate 4 \
        -video_size ${W}x${H} -i :$XN -vframes 12 -y /tmp/seq%d.png >/dev/null 2>&1
     wait
     printf 'seq'; for i in 1 2 3 4 5 6 7 8 9 10 11 12; do printf ' %s' \"\$(convert /tmp/seq\$i.png -format '%[pixel:p{400,300}]' info: 2>/dev/null)\"; done; echo" 2>&1)
    echo "$CAP" > "$OUT/capture.txt"; echo "$CAP" | sed 's/^/    /'
    WANTC="srgb(32,160,96)"
    if grep -q X-DOWN <<<"$CAP"; then
        fail capture "no X server on :$XN -- see $OUT/xorg.log and the guest's /tmp/lea-xorg.log"
    elif ! grep -q "XGetImage.*0x20a060" <<<"$CAP"; then
        fail capture "XGetImage did not read back the colour a drawing client put up"
    elif ! grep -q "XShmGetImage.*0x20a060" <<<"$CAP"; then
        fail capture "XGetImage is right and XShmGetImage is not -- the shared-memory path"
    elif [[ $(grep -c "grab[0-9] $WANTC" <<<"$CAP") -ne 4 ]]; then
        fail capture "x11grab did not read the colour in all 4 frames"
    elif [[ $(grep '^seq' <<<"$CAP" | tr ' ' '\n' | grep -c 'srgb(') -lt 12 ]]; then
        fail capture "the colour sequence did not arrive in full"
    elif [[ $(grep '^seq' <<<"$CAP" | tr ' ' '\n' | sort -u | grep -c 'srgb(') -lt 3 ]]; then
        fail capture "the sequence arrived, but too few distinct colours -- nothing moved"
    else
        pass capture "three readers agree on the NVIDIA X, and a colour sequence arrives in order"
    fi

    # ---- swapchain: the stage the capture stages cannot stand in for. Every
    # one of them can be green while vkCreateSwapchainKHR still refuses --
    # measured 2026-08-14/15. This insists on the VkResult, not the exit code.
    say "swapchain"
    g 'gcc -O2 -Wall -Wextra -o ~/vkprobe ~/vkprobe.c -lvulkan -lX11' >/dev/null 2>&1
    local SWAP SWAPLINE
    SWAP=$(g "export DISPLAY=:$XN
     xrandr >/dev/null 2>&1 || { echo 'X-DOWN'; exit 0; }
     [ -x ~/vkprobe ] || { echo 'NO-PROBE'; exit 0; }
     ~/vkprobe 2>&1" 2>&1)
    echo "$SWAP" > "$OUT/swapchain.txt"; echo "$SWAP" | sed 's/^/    /'
    SWAPLINE=$(grep 'vkCreateSwapchainKHR' <<<"$SWAP" | head -1)
    if grep -q X-DOWN <<<"$SWAP"; then
        fail swapchain "no X server on :$XN -- the capture stage above says why"
    elif grep -q NO-PROBE <<<"$SWAP"; then
        fail swapchain "vkprobe did not build -- see $OUT/swapchain.txt"
    elif [[ -z $SWAPLINE ]]; then
        fail swapchain "vkprobe stopped before the swapchain -- $(grep -c VK_ <<<"$SWAP") step(s) reported, see $OUT/swapchain.txt"
    elif ! grep -q VK_SUCCESS <<<"$SWAPLINE"; then
        fail swapchain "$(sed 's/  */ /g;s/^ //' <<<"$SWAPLINE")"
    else
        pass swapchain "vkCreateSwapchainKHR returned VK_SUCCESS on the virtual display"
    fi

    # ---- present: CREATING a swapchain and PRESENTING through it are two
    # questions, and for weeks they had two different answers (OPEN-QUESTIONS
    # nr 10). 120 frames, all VK_SUCCESS AND at this display's 60 Hz -- a
    # present path returning VK_SUCCESS at 900 FPS is not presenting here.
    say "present"
    local PRES PRESLINE PFPS
    PRES=$(g "export DISPLAY=:$XN
     xrandr >/dev/null 2>&1 || { echo 'X-DOWN'; exit 0; }
     [ -x ~/vkprobe ] || { echo 'NO-PROBE'; exit 0; }
     ~/vkprobe --present 120 2>&1 | tail -3" 2>&1)
    echo "$PRES" > "$OUT/present.txt"; echo "$PRES" | sed 's/^/    /'
    PRESLINE=$(grep 'presented' <<<"$PRES" | head -1)
    PFPS=$(sed -n 's/.*, \([0-9.]*\) FPS.*/\1/p' <<<"$PRESLINE")
    lea_gate_fact present_fps "${PFPS:-unknown}"
    if grep -q X-DOWN <<<"$PRES"; then
        fail present "no X server on :$XN -- the capture stage above says why"
    elif grep -q NO-PROBE <<<"$PRES"; then
        fail present "vkprobe did not build -- see $OUT/present.txt"
    elif [[ -z $PRESLINE ]]; then
        fail present "vkprobe presented nothing -- see $OUT/present.txt"
    elif ! grep -q 'all VK_SUCCESS' <<<"$PRESLINE"; then
        fail present "$(sed 's/  */ /g;s/^ //' <<<"$PRESLINE")"
    elif [[ -z $PFPS ]] || awk "BEGIN{exit !($PFPS < 50 || $PFPS > 75)}"; then
        fail present "120 frames presented but at ${PFPS:-?} FPS -- not this display's 60 Hz"
    else
        pass present "120 frames, all VK_SUCCESS, $PFPS FPS at the display's 60 Hz cap"
    fi

    # ---- rt: raytracing initialisation, the deepest thing the guest's
    # Vulkan driver does, and it failed for FOUR independent reasons at once
    # (OPEN-QUESTIONS nr 11), every one invisible to every stage above. This
    # asks the question CS2 asks.
    say "rt"
    local RT RTLINE
    RT=$(g "export DISPLAY=:$XN
     xrandr >/dev/null 2>&1 || { echo 'X-DOWN'; exit 0; }
     [ -x ~/vkprobe ] || { echo 'NO-PROBE'; exit 0; }
     ~/vkprobe --rt 2>&1 | tail -4" 2>&1)
    echo "$RT" > "$OUT/rt.txt"; echo "$RT" | sed 's/^/    /'
    RTLINE=$(grep 'vkCreateDevice' <<<"$RT" | head -1)
    if grep -q X-DOWN <<<"$RT"; then
        fail rt "no X server on :$XN"
    elif grep -q NO-PROBE <<<"$RT"; then
        fail rt "vkprobe did not build -- see $OUT/rt.txt"
    elif [[ -z $RTLINE ]]; then
        fail rt "vkprobe stopped before vkCreateDevice -- see $OUT/rt.txt"
    elif ! grep -q VK_SUCCESS <<<"$RTLINE"; then
        fail rt "$(sed 's/  */ /g;s/^ //' <<<"$RTLINE")"
    else
        pass rt "vkCreateDevice with VK_KHR_acceleration_structure returned VK_SUCCESS"
    fi

    # ---- events: the event back-channel's own reader, and the only one.
    # Before the second virtqueue every wait fell back to a 10 ms poll. The
    # threshold separates WOKEN from POLLING rather than naming a target:
    # 10.10 ms was the poll, 0.12 ms is the wake, and 1 ms sits between them.
    # Read the MEDIAN, not the worst: the tail (5.94, 6.86 ms in one run) is
    # real and is not what this stage asks about; a POLLING guest has no
    # tail. ISOLATED fallbacks are counted and reported, not failed on
    # (OPEN-QUESTIONS nr 14); a MAJORITY at the timer does fail.
    say "events"
    local FT FTVALS FTN FTMED FTMAX FTPOLL FIRED
    FT=$(g "export DISPLAY=:$XN
     xrandr >/dev/null 2>&1 || { echo 'X-DOWN'; exit 0; }
     [ -x ~/fencetime ] || gcc -O2 -o ~/fencetime ~/fencetime.c -lvulkan 2>/dev/null
     [ -x ~/fencetime ] || { echo 'NO-PROBE'; exit 0; }
     ~/fencetime 2>&1 | tail -2" 2>&1)
    echo "$FT" > "$OUT/events.txt"; echo "$FT" | sed 's/^/    /'
    FTVALS=$(grep -oE '^[0-9. ]+' <<<"$FT" | tr ' ' '\n' | grep -E '^[0-9]+\.[0-9]+$' | sort -g)
    FTN=$(wc -l <<<"$FTVALS")
    FTMED=$(sed -n "$(( (FTN + 1) / 2 ))p" <<<"$FTVALS")
    FTMAX=$(tail -1 <<<"$FTVALS")
    # 9 ms rather than 10.07 exactly: the timer fires a little late under load.
    FTPOLL=$(awk '$1 >= 9.0' <<<"$FTVALS" | wc -l)
    FIRED=$(g 'cat /sys/module/virtio_nvrm/parameters/stat_vblank_fired 2>/dev/null' 2>/dev/null | tr -d '\r')
    lea_gate_fact fence_median_ms "${FTMED:-unknown}"
    lea_gate_fact fence_poll_hits "${FTPOLL:-unknown}"
    lea_gate_fact vblank_fired "${FIRED:-unknown}"
    if grep -q X-DOWN <<<"$FT"; then
        fail events "no X server on :$XN"
    elif grep -q NO-PROBE <<<"$FT"; then
        fail events "fencetime did not build -- see $OUT/events.txt"
    elif [[ -z $FTMED ]]; then
        fail events "fencetime printed no timings -- see $OUT/events.txt"
    elif awk "BEGIN{exit !($FTMED >= 1.0)}"; then
        fail events "median wait $FTMED ms over $FTN samples -- that is the 10 ms poll, not a wakeup"
    elif [[ $FTPOLL -gt $((FTN / 2)) ]]; then
        fail events "$FTPOLL of $FTN waits at the 10 ms poll -- a polling guest with a few lucky wakeups"
    elif [[ $FTPOLL -gt 0 ]]; then
        pass events "median $FTMED ms over $FTN samples, but $FTPOLL went to the poll (OPEN-QUESTIONS nr 14), vblank fired ${FIRED:-?}"
    else
        pass events "median wait $FTMED ms over $FTN samples (worst $FTMAX, the poll was 10.10), vblank fired ${FIRED:-?}"
    fi

    # ---- stream: the shipping pipeline end to end -- Sunshine captures the
    # NVIDIA X, NVENC encodes, Moonlight on THIS host receives -- and the
    # verdict comes from Moonlight's own session summary. Content is a STATIC
    # paintprobe, which keeps the non-black count meaningful; that a MOVING
    # picture arrives is the capture stage's sequence check. A host without
    # moonlight fails this stage rather than skipping it: a skipped stage
    # inside a green gate is a success claim without a reader. bench.sh
    # stream owns the mechanics -- the one deliberate cross-call here.
    say "stream"
    if ! command -v moonlight >/dev/null 2>&1; then
        fail stream "no moonlight on this host -- pacman -S moonlight-qt"
    elif ! g 'command -v sunshine >/dev/null' 2>/dev/null; then
        fail stream "no sunshine in the guest image"
    else
        g "export DISPLAY=:$XN; pkill -x paintprobe 2>/dev/null; sleep 1
           setsid nohup ~/paintprobe 20a060 --geometry ${W}x${H}+0+0 \
               >/tmp/pp-stream.log 2>&1 </dev/null &" >/dev/null 2>&1
        "$LEA_ROOT/scripts/bench.sh" stream --session desktop --capture x11 --encoder nvenc \
            --display ":$XN" --name "$NAME" --seconds 20 --content none \
            --res "${W}x${H}" --out "$OUT/stream" >"$OUT/stream.log" 2>&1
        local STREAM_RC=$? STREAM_SUMMARY NONBLACK_OK
        g 'pkill -x paintprobe 2>/dev/null' >/dev/null 2>&1
        STREAM_SUMMARY=$(python3 - "$OUT/stream/stats.json" <<'PY' 2>/dev/null
import json, sys
s = json.load(open(sys.argv[1]))
fps = (s.get("incoming_fps") or [None])[0]
lat = s.get("host_latency") or []
drop = (s.get("drop_network") or [None])[0]
print(f"valid={s.get('valid')} encoder={s.get('encoder')} fps={fps} "
      f"host_latency={'/'.join(lat) if lat else '-'}ms net_drop={drop}% "
      f"nonblack={s.get('nonblack_pixels')}")
PY
)
        echo "    ${STREAM_SUMMARY:-<no stats.json>}"
        NONBLACK_OK=$(python3 - "$OUT/stream/stats.json" <<'PY' 2>/dev/null
import json, sys
s = json.load(open(sys.argv[1]))
print(1 if (s.get("nonblack_pixels") or 0) >= 2000 else 0)
PY
)
        if [[ $STREAM_RC -ne 0 ]]; then
            fail stream "bench.sh stream exit $STREAM_RC -- see $OUT/stream.log and $OUT/stream/"
        elif [[ ${NONBLACK_OK:-0} -ne 1 ]]; then
            fail stream "frames arrived but the captured root was (near) black -- an encoder number, not a stream"
        else
            pass stream "Moonlight received frames from the NVIDIA X ($STREAM_SUMMARY)"
        fi
    fi

    # ---- kernel: a gate that only reads its own output misses the oops that
    # happened beside it. The known WARN is counted by its signature rather
    # than ignored: nv_drm_crtc_dequeue_flip, nvidia-drm-crtc.h:355,
    # WARN_ON(nv_flip == NULL) -- one more flip completion arrives than
    # nvidia-drm queued flips (measured 2026-08-13). Anything that is a
    # WARNING and is NOT this is new, and new is what a gate is for.
    say "kernel"
    g 'sudo dmesg' > "$OUT/dmesg.txt" 2>/dev/null
    local OOPS REFUSED KNOWN WARNS UNKNOWN
    OOPS=$(grep -cE "BUG:|Oops:|kernel NULL pointer" "$OUT/dmesg.txt")
    REFUSED=$(grep -cE "is not implemented -- refused|MAP_MEMORY refused" "$OUT/dmesg.txt")
    KNOWN='nvidia-drm-crtc\.h:355 __nv_drm_handle_flip_event'
    WARNS=$(grep -c "WARNING:" "$OUT/dmesg.txt")
    UNKNOWN=$(grep "WARNING:" "$OUT/dmesg.txt" | grep -vcE "$KNOWN")
    lea_gate_fact dmesg_warnings "$WARNS"
    lea_gate_fact dmesg_warnings_unknown "$UNKNOWN"
    lea_gate_fact dmesg_oops "$OOPS"
    if [[ $OOPS -gt 0 ]]; then
        fail kernel "$OOPS oops/BUG in the guest log -- see $OUT/dmesg.txt"
    elif [[ $REFUSED -gt 0 ]]; then
        fail kernel "$REFUSED refused RM op(s) -- see $OUT/dmesg.txt"
    elif [[ $UNKNOWN -gt 0 ]]; then
        grep "WARNING:" "$OUT/dmesg.txt" | grep -vE "$KNOWN" | sed 's/^/    /'
        fail kernel "$UNKNOWN WARNING(s) that are not the known flip one"
    else
        pass kernel "no oops, no refused op, $WARNS WARNING(s), all the known flip one"
    fi

    # The detail describes what HAPPENED, not what the gate hoped for -- a
    # failing run that still claims "EDID identical" is worse than no detail.
    #
    # PFPS and FTMED are quoted with a fallback like every other reader of
    # them: today only the `pass` arms of those two stages reach this line
    # and both have already refused an empty value, but the summary must not
    # depend on that -- an added branch would otherwise print " FPS
    # presented" with a hole in it, and this script runs with set -u off.
    if [[ ${#_LEA_GATE_FAILED[@]} -eq 0 ]]; then
        lea_gate_finish "$CARD ${W}x${H}, EDID identical, modeset+flip read back, capture through three readers, swapchain created, ${PFPS:-?} FPS presented, RT device created, median wait ${FTMED:-?} ms, Moonlight received the stream"
    else
        lea_gate_finish "failed: ${_LEA_GATE_FAILED[*]} ($CARD ${W}x${H}); logs in $OUT/"
    fi
}

# ============================================================================
# gates -- the aggregator
# ============================================================================
do_gates() {
    local -a ALL=(gpu vdisplay display) WANTED=("$@") LINES=() SUMMARY=()
    # No argument and the explicit group `all` mean the same thing.
    [[ $# -eq 0 || ( $# -eq 1 && ${1:-} == all ) ]] && WANTED=("${ALL[@]}")
    local OUT=$LEA_VM_DIR/out-gates failed=0 skipped=0 g log json rc line result dur
    mkdir -p "$OUT"
    # Its own pidfile; each gate holds vm/test-<gate>.pid -- DIFFERENT names,
    # so the nesting is not a collision.
    #   until ! lea_running vm/test-gates.pid; do sleep 15; done
    lea_hold_pidfile "$LEA_VM_DIR/test-gates.pid"
    # One value out of the gate's JSON line, with sed rather than jq or
    # python: we own the producer, the shape is fixed, and a band that cannot
    # summarise itself on a machine without jq does not run where it is
    # needed. Both read only the part BEFORE `"facts":` -- inside facts a
    # gate may write any key it likes, including one called `result`.
    _head()   { printf '%s' "${1%%,\"facts\":*}"; }
    _field()  { sed -n "s/.*\"$1\":\"\\([^\"]*\\)\".*/\\1/p" <<<"$(_head "$2")"; }
    _number() { sed -n "s/.*\"$1\":\\([0-9-]*\\).*/\\1/p" <<<"$(_head "$2")"; }
    for g in "${WANTED[@]}"; do
        case $g in gpu|vdisplay|display) ;; *) echo "unknown gate: $g (known: ${ALL[*]}, or 'all')" >&2; exit 2 ;; esac
        { echo; echo "############################################################"
          echo "# $g"; echo "############################################################"; } >&2
        log=$OUT/$g.log; json=$OUT/$g.json
        # stderr into the pipe, stdout into the file. The order matters and
        # it is the opposite of the usual idiom: `2>&1` first points stderr at
        # what stdout is NOW (the pipe), and only then is stdout moved to the
        # file. The gate runs as its own PROCESS: the contract's fd-3 trick
        # and its exit codes belong to one process each.
        "$0" "$g" 2>&1 >"$json" | tee "$log" >&2
        rc=${PIPESTATUS[0]}     # not $?, which would be tee's
        line=$(tail -n1 "$json" 2>/dev/null)
        if [[ $line != '{"gate":'* ]]; then
            # A gate that printed no verdict is a failure of the gate, not a
            # missing datum: something killed it before its own EXIT trap.
            line="{\"gate\":\"$g\",\"result\":\"fail\",\"dur_s\":0,\"reason\":\"no result line on stdout (exit $rc)\",\"facts\":{}}"
            rc=1
        fi
        result=$(_field result "$line"); dur=$(_number dur_s "$line")
        # The exit code and the verdict are cross-checked rather than one of
        # them believed: before the JSON contract a gate that printed
        # status=fail while exiting 0 counted as a pass.
        case "$result:$rc" in
            pass:0|fail:1|skip:2) ;;
            *)  echo "  $g: verdict '$result' but exit $rc -- treating as fail" >&2
                line=${line/\"result\":\"$result\"/\"result\":\"fail\"}; result=fail ;;
        esac
        if command -v python3 >/dev/null 2>&1; then
            python3 -c 'import json,sys; json.loads(sys.argv[1])' "$line" 2>/dev/null \
                || echo "  $g: WARNING -- the result line is not valid JSON" >&2
        fi
        case $result in fail) failed=$((failed + 1)) ;; skip) skipped=$((skipped + 1)) ;; esac
        LINES+=("$line")
        SUMMARY+=("$(printf '%-10s %-6s %ss' "$g" "$result" "${dur:-?}")")
    done
    { echo; echo "== summary =="; printf '%s\n' "${SUMMARY[@]}"; echo
      if [[ $failed -gt 0 ]]; then echo "GATES: ${LEA_RED}FAIL${LEA_R} -- $failed failed, logs in $OUT/"
      elif [[ $skipped -gt 0 ]]; then echo "GATES: INCOMPLETE -- $skipped skipped, none failed, logs in $OUT/"
      else echo "ALL GATES: ${LEA_GRN}PASS${LEA_R} (${WANTED[*]})"; fi
    } >&2
    # Written first, printed second, and NOT through `tee`: a caller that
    # pipes this stdout into something which exits early leaves tee with
    # EPIPE before it has written, and the summary file is then truncated.
    printf '%s\n' "${LINES[@]}" > "$OUT/summary.jsonl"
    printf '%s\n' "${LINES[@]}"
    [[ $failed -gt 0 ]] && exit 1
    [[ $skipped -gt 0 ]] && exit 2
    exit 0
}

case $CMD in
    check)    do_check "$@" ;;
    gpu)      gate_gpu "$@" ;;
    vdisplay) gate_vdisplay "$@" ;;
    display)  gate_display "$@" ;;
    gates)    do_gates "$@" ;;
esac
