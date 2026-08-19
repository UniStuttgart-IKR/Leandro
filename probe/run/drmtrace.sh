#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# The DRM ioctl surface of real clients, natively, decoded honestly.
#
#   probe/run/drmtrace.sh [--out DIR] [--list] [name ...]
#
# An early floor measurement put the DRM surface at **14 distinct ioctls**
# -- with a warning attached: three enumerating clients, no compositor,
# no game, no frame. The display path is costed against that floor. This
# script takes the rest of the curve -- clients that actually render,
# present, export a buffer and run CUDA against a GL texture.
#
# WHY IT DOES ITS OWN DECODING, and this is the whole point:
#
#   strace lies about driver-private DRM numbers. NVIDIA's 0x0e prints as
#   DRM_IOCTL_AMDGPU_FENCE_TO_HANDLE and 0x0d as DRM_IOCTL_I915_GEM_PIN --
#   strace picks the first driver in its own table with that number. So the
#   trace is taken with `-X raw`, which prints the bare request number, and
#   the names come from the headers that actually apply:
#
#     private (nr >= 0x40)  vendor/.../nvidia-drm/nv_drm_common_ioctl.h
#     core    (nr <  0x40)  /usr/include/libdrm/drm.h
#
#   Both are PARSED, not transcribed -- a hand-copied table is a table that
#   drifts from the driver it claims to describe.
#
# Runs on the HOST, against the real driver. It does not touch the
# operator's compositor: every workload is a CLIENT, which is also the
# only side the display path would have to serve (it builds a render
# node, not KMS -- kernel modesetting).
set -uo pipefail
cd "$(dirname "$0")/../.."      # repository root

source ./scripts/lib/config.sh
source ./scripts/lib/common.sh

NVHDR=vendor/open-gpu-kernel-modules/kernel-open/nvidia-drm/nv_drm_common_ioctl.h
DRMHDR=/usr/include/libdrm/drm.h
[[ -f $DRMHDR ]] || DRMHDR=/usr/include/drm/drm.h

usage() {
    # Skip the shebang and the SPDX block: licence and copyright are
    # metadata for a tool, not the first three lines of help.
    awk 'NR>1 { if ($0 !~ /^#/) exit; sub(/^# ?/, ""); if ($0 ~ /^SPDX-/) next; print }' "$0"
    exit "${1:-0}"
}

# Workload -> command. Each one answers a different question, and the last
# three are the ones the 14-ioctl floor is missing.
declare -A WL=(
    # the floor's three enumerating clients, repeated so it is re-measured in the same
    # session rather than quoted across weeks
    [vulkaninfo]='vulkaninfo --summary'
    [glxinfo]='glxinfo -B'
    [eglinfo]='eglinfo'
    # a Vulkan OFFSCREEN render with readback: upload into device memory,
    # run a compute shader over it, read it back. No window, no swapchain.
    [vkoffscreen]='ffmpeg -hide_banner -loglevel error -init_hw_device vulkan=vk -filter_hw_device vk -f lavfi -i testsrc2=size=1280x720:rate=1:duration=2 -vf format=nv12,hwupload,gblur_vulkan,hwdownload,format=nv12 -f null -'
    # Vulkan derived FROM a CUDA device: the external-memory interop
    # path -- the substantive row of the display-path cost table.
    [vkcuda]='ffmpeg -hide_banner -loglevel error -init_hw_device cuda=cu -init_hw_device vulkan=vk@cu -filter_hw_device vk -f lavfi -i testsrc2=size=1280x720:rate=1:duration=2 -vf format=nv12,hwupload,gblur_vulkan,hwdownload,format=nv12 -f null -'
    # real frames, presented through the operator's compositor -- the client
    # half of "a compositor with a presented frame"
    [vkmark]='vkmark --winsys wayland -s 640x480 -b clear'
    # real frames, GL, no presentation at all
    [glmark2off]='glmark2 --off-screen --size 640x480 -b build'
)
ORDER=(vulkaninfo glxinfo eglinfo vkoffscreen vkcuda vkmark glmark2off)

OUT=
SELECTED=()
while [[ $# -gt 0 ]]; do
    case $1 in
        --out)  OUT=$2; shift 2 ;;
        --list) printf '%s\n' "${ORDER[@]}"; exit 0 ;;
        -h|--help) usage 0 ;;
        -*) echo "unknown option: $1" >&2; usage 2 ;;
        *) SELECTED+=("$1"); shift ;;
    esac
done
[[ ${#SELECTED[@]} -eq 0 ]] && SELECTED=("${ORDER[@]}")

command -v strace >/dev/null || { echo "ERROR: strace missing" >&2; exit 1; }
[[ -f $NVHDR ]]  || { echo "ERROR: $NVHDR missing -- ./scripts/build.sh vendor" >&2; exit 1; }
[[ -f $DRMHDR ]] || { echo "ERROR: no drm.h (libdrm headers)" >&2; exit 1; }

: "${OUT:=probe/traces/drm-$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$OUT"

# Seven traced workloads, up to 180 s each. Wait on the FILE, never on
# `pgrep -f` -- that pattern stands in the waiting shell's own command
# line (lea_hold_pidfile in scripts/lib/common.sh):
#   until ! lea_running vm/drmtrace.pid; do sleep 15; done
mkdir -p "$LEA_VM_DIR"
lea_hold_pidfile "$LEA_VM_DIR/drmtrace.pid"

echo "== tracing natively, -X raw (strace's DRM names are wrong for private numbers) =="
for name in "${SELECTED[@]}"; do
    [[ -v WL[$name] ]] || { echo "unknown workload '$name' -- try --list" >&2; exit 2; }
    printf '  %-12s ' "$name"
    # -y prints the path behind each fd, which is how a /dev/dri node is
    # told from every other ioctl in the process.
    # -f follows children: ffmpeg and glmark2 both fork.
    timeout 180 strace -f -y -X raw -e trace=ioctl -o "$OUT/$name.strace" \
        bash -c "${WL[$name]}" >"$OUT/$name.out" 2>&1
    rc=$?
    n=$(grep -c '/dev/dri/' "$OUT/$name.strace" 2>/dev/null || echo 0)
    echo "rc=$rc  ${n} DRM ioctls"
done

python3 - "$OUT" "$NVHDR" "$DRMHDR" "${SELECTED[@]}" <<'PY'
import re, sys, json, pathlib, collections

out = pathlib.Path(sys.argv[1])
nvhdr, drmhdr = pathlib.Path(sys.argv[2]), pathlib.Path(sys.argv[3])
names = sys.argv[4:]

# ---- the two name tables, parsed from the headers that apply -------------
# private: #define DRM_NVIDIA_<NAME>  0x<nr>, numbered from DRM_COMMAND_BASE
private = {}
for m in re.finditer(r"#define\s+DRM_NVIDIA_(\w+)\s+0x([0-9a-fA-F]+)", nvhdr.read_text()):
    private[int(m[2], 16)] = m[1]

# core: #define DRM_IOCTL_<NAME>  DRM_IO*(0x<nr>[, ...])
# WARNING: `\s*` before the paren is load-bearing, not tidiness. drm.h aligns
# its column with a space -- `DRM_IOW (0x09, struct drm_gem_close)` -- and a
# regex that demanded `DRM_IO\w*\(` silently lost GEM_CLOSE and every other
# spaced entry. It did not fail; it produced a table with holes, and the
# holes came out of the far end as CORE_UNKNOWN_0x09.
drmtext = drmhdr.read_text()
core = {}
for m in re.finditer(r"#define\s+DRM_IOCTL_(\w+)\s+DRM_IO\w*\s*\(\s*0x([0-9a-fA-F]+)", drmtext):
    core.setdefault(int(m[2], 16), m[1])

# The private WINDOW, both ends, taken from the same header. Driver-private
# is 0x40..0xA0 -- it is a RANGE, not a floor.
# WARNING: this is what the first run of this script got wrong. With
# "private = nr >= 0x40" the five SYNCOBJ ioctls (0xBF, 0xC0, 0xC1, 0xC2,
# 0xCC) landed in NVIDIA's private space and were reported as
# NVIDIA_UNKNOWN_0x7f..0x8c -- five invented private ioctls, and five real
# core ones missing from the count. Exactly the kind of number the display
# path would
# have been costed against.
def hdrnum(name, default):
    m = re.search(rf"#define\s+{name}\s+0x([0-9a-fA-F]+)", drmtext)
    return int(m[1], 16) if m else default

DRM_COMMAND_BASE = hdrnum("DRM_COMMAND_BASE", 0x40)
DRM_COMMAND_END  = hdrnum("DRM_COMMAND_END", 0xA0)
IOC_TYPE_DRM = 0x64          # 'd'


def decode(req):
    """A raw ioctl request number -> (node-independent) name, or None if
    it is not a DRM request at all."""
    typ = (req >> 8) & 0xFF
    if typ != IOC_TYPE_DRM:
        return None
    nr = req & 0xFF
    if DRM_COMMAND_BASE <= nr < DRM_COMMAND_END:
        idx = nr - DRM_COMMAND_BASE
        return f"NVIDIA_{private[idx]}" if idx in private else f"NVIDIA_UNKNOWN_0x{idx:02x}"
    return core.get(nr, f"CORE_UNKNOWN_0x{nr:02x}")


# ---- the decoder checks itself, out loud --------------------------------
# A table parsed from a header can be empty, half-parsed or off by an offset
# and still produce a plausible-looking report. These five are known from
# the headers above; if any of them stops resolving, the run says so and
# stops rather than publishing a number.
def _req(nr):
    return (0x3 << 30) | (0x20 << 16) | (IOC_TYPE_DRM << 8) | nr


SELFTEST = [
    (0x00, "VERSION"),                              # core, DRM_IOWR
    (0x09, "GEM_CLOSE"),                            # core, DRM_IOW with a SPACE
    (0xBF, "SYNCOBJ_CREATE"),                       # core ABOVE the private window
    (0x40 + 0x0d, "NVIDIA_GEM_EXPORT_DMABUF_MEMORY"),   # strace calls this I915_GEM_PIN
    (0x40 + 0x0e, "NVIDIA_GEM_IDENTIFY_OBJECT"),        # ... and this AMDGPU_FENCE_TO_HANDLE
]
bad = [(hex(nr), want, decode(_req(nr))) for nr, want in SELFTEST if decode(_req(nr)) != want]
print(f"  decoder: {len(private)} private + {len(core)} core names parsed, "
      f"private window 0x{DRM_COMMAND_BASE:02x}..0x{DRM_COMMAND_END:02x}, "
      f"{len(SELFTEST) - len(bad)}/{len(SELFTEST)} self-checks pass")
if bad:
    for nr, want, got in bad:
        print(f"  DECODER BROKEN: nr {nr} should be {want}, decoded as {got}")
    sys.exit(1)


# strace -X raw, -y:  ioctl(4</dev/dri/renderD128>, 0xc0206440, 0x7ffc...) = 0
LINE = re.compile(r"ioctl\((\d+)<([^>]+)>,\s*(0x[0-9a-fA-F]+)")

report, union = {}, collections.Counter()
for name in names:
    p = out / f"{name}.strace"
    if not p.exists():
        continue
    per_node = collections.defaultdict(collections.Counter)
    unknown = collections.Counter()
    for line in p.read_text(errors="replace").splitlines():
        m = LINE.search(line)
        if not m or "/dev/dri/" not in m[2]:
            continue
        who = decode(int(m[3], 16))
        if who is None:
            continue
        node = m[2].rsplit("/", 1)[-1]
        per_node[node][who] += 1
        union[who] += 1
        if "UNKNOWN" in who:
            unknown[f"{who} on {node}"] += 1
    report[name] = {n: dict(c) for n, c in per_node.items()}
    if unknown:
        report[name]["_unknown"] = dict(unknown)

# ---- the 14-ioctl floor, so the delta is visible and not asserted --------
FLOOR = {
    "NVIDIA_GEM_EXPORT_DMABUF_MEMORY", "NVIDIA_GEM_IDENTIFY_OBJECT",
    "NVIDIA_GEM_IMPORT_NVKMS_MEMORY", "NVIDIA_GET_DEV_INFO",
    "NVIDIA_GET_DRM_FILE_UNIQUE_ID",
    "GEM_CLOSE", "GET_CAP", "PRIME_HANDLE_TO_FD", "SYNCOBJ_CREATE",
    "SYNCOBJ_DESTROY", "SYNCOBJ_FD_TO_HANDLE", "SYNCOBJ_HANDLE_TO_FD",
    "SYNCOBJ_TRANSFER", "VERSION",
}

print()
print("== DRM ioctls per workload (native) ==")
print(f"  {'workload':<12} {'calls':>7} {'distinct':>9}  nodes")
for name in names:
    if name not in report:
        continue
    nodes = {k: v for k, v in report[name].items() if not k.startswith("_")}
    calls = sum(sum(c.values()) for c in nodes.values())
    distinct = len({k for c in nodes.values() for k in c})
    print(f"  {name:<12} {calls:>7} {distinct:>9}  {' '.join(nodes) or '—'}")

print()
print(f"== union: {len(union)} distinct ==")
new = sorted(k for k in union if k not in FLOOR)
kept = sorted(k for k in union if k in FLOOR)
print(f"  already in the DISPLAY.md 1.4 floor ({len(kept)} of 14): {', '.join(kept) or '—'}")
print(f"  NOT in the floor ({len(new)}):")
for k in new:
    print(f"    {k:<40} {union[k]:>7} calls")
missing = sorted(FLOOR - set(union))
if missing:
    print(f"  in the floor but not seen here ({len(missing)}): {', '.join(missing)}")

(out / "summary.json").write_text(json.dumps(
    {"per_workload": report, "union": dict(union),
     "floor": sorted(FLOOR), "new_vs_floor": new}, indent=2))
print()
print(f"  raw: {out}/  summary: {out}/summary.json")
PY
