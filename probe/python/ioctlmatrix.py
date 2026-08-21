#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""Resolve traced ioctl signatures against the vendor headers and emit the
catalogue, the matrix and the task list.

Called by ``scripts/ioctl-matrix.sh catalog``; not meant to be run by hand.

WHAT MAKES THIS HONEST, and it is the whole design:

  * Every name, every struct and every size is derived at run time from
    ``vendor/`` -- the headers of the driver that was measured. Nothing is
    copied out of ``crates/nvrm-abi/src/xlate.rs``.
  * The result is then DIFFED against xlate's own serialised tables, and any
    disagreement is reported rather than resolved. Either side can be the
    bug; a generator that silently preferred one would hide it.
  * Sizes are not parsed. They are COMPILED: a C file per header, including
    the header the command comes from, printing ``sizeof`` for each params
    struct. A regex that reads a struct layout is a regex that is wrong
    about padding sooner or later.
  * Where the headers say nothing, the row says ``unknown -- not in public
    headers`` and carries the raw number. No semantics are ever invented.

The key of a signature is ``(device, ioctl nr, sub)``, where sub is the
RM_CONTROL command, the hClass of an allocation, or ``-``. That is exactly
the key ``probe/run/trace.sh analyse`` counts, so the two reports are
comparable by construction.
"""

import argparse
import collections
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile

# The one reader of a trace, whatever format it is in.
import nvkmsdecode
import traceread

# ---------------------------------------------------------------------------
# the tables the guest module is handed (scripts/ioctl-matrix.sh dumps them)
# ---------------------------------------------------------------------------
# Row shapes come from nvrm_abi::table::expect_dump(); the field names below
# are that function's, in its order.
NONE_U32 = 0xFFFFFFFF
DEV_NAME = {0: "ctl", 1: "gpu", 2: "uvm", 3: "uvmtools"}
DEV_CODE = {v: k for k, v in DEV_NAME.items()}

CF_BLOCK = 1 << 0
CF_FD = 1 << 1
KF_UNVERIFIED = 1 << 0
F_OSDESC = 1 << 1


class Governance:
    """What the backend and the guest module agree they can carry."""

    def __init__(self, path):
        self.ioctls = {}    # (dev, nr) -> dict
        self.classes = {}   # hclass -> dict
        self.ctrls = {}     # cmd -> dict
        self.nested = []
        self.header = []
        for line in pathlib.Path(path).read_text().splitlines():
            f = line.split()
            if not f:
                continue
            if f[0] == "hdr":
                self.header = [int(x) for x in f[1:]]
            elif f[0] == "ioctl":
                d = dict(zip(
                    "dev nr size fd_off emb_ptr_off emb_len_kind emb_len_off "
                    "cmd_off rights_off rights_if_size handle_off flags".split(),
                    [int(x) for x in f[1:]]))
                self.ioctls[(d["dev"], d["nr"])] = d
            elif f[0] == "class":
                d = dict(zip("hclass param_size fd_off flags fd_if_off fd_if_val".split(),
                             [int(x) for x in f[1:]]))
                self.classes[d["hclass"]] = d
            elif f[0] == "ctrl":
                d = dict(zip("cmd first count flags fd_off".split(),
                             [int(x) for x in f[1:]]))
                self.ctrls[d["cmd"]] = d
            elif f[0] == "nested":
                self.nested.append([int(x) for x in f[1:]])

    def selfcheck(self):
        """A dump that was not understood is empty and looks like a backend
        that governs nothing -- which would report every signature as
        missing. Empty is an error, loudly, for the same reason
        drmtrace.sh's decoder checks itself."""
        bad = [n for n, v in (("ioctl", self.ioctls), ("class", self.classes),
                              ("ctrl", self.ctrls)) if not v]
        if bad:
            sys.exit(f"tables.txt: no {', '.join(bad)} rows were parsed -- "
                     "the dump format moved and every signature would be "
                     "reported as missing. Fix the reader first.")


# ---------------------------------------------------------------------------
# the vendor headers
# ---------------------------------------------------------------------------
# The include set is crates/nvrm-sys/build.rs's, in the order that matters.
#
# WARNING: src/common/sdk/nvidia/inc MUST come before kernel-open/common/inc.
# Both hold an rs_access.h, they are different files, and the other order
# makes the two definitions collide. build.rs documents the same hazard for
# nvtypes.h and nv-ioctl.h.
INCLUDE_DIRS = [
    "src/common/sdk/nvidia/inc",
    "kernel-open/common/inc",
    "src/nvidia/arch/nvalloc/unix/include",
    "kernel-open/nvidia-uvm",
]

CTRL_DEF = re.compile(
    r"^#define\s+(NV[0-9A-Za-z_]*_CTRL_CMD_[A-Z0-9_]+)\s+\(?(0x[0-9a-fA-F]+)U?\)?(.*)$", re.M)
FINN_STRUCT = re.compile(r"\|\s*([A-Za-z0-9_]+)_MESSAGE_ID\"")
CLASS_PREFIX = re.compile(r"^NV([0-9A-Fa-f]{4})_CTRL_CMD_")
ESC_DEF = re.compile(r"^#define\s+(NV_ESC_[A-Z0-9_]+)\s+(.+?)\s*$", re.M)
UVM_DEF = re.compile(r"^#define\s+(UVM_[A-Z0-9_]+)\s+UVM_IOCTL_BASE\((\d+)\)", re.M)
ALLCLASS_DEF = re.compile(r"^#define\s+([A-Z][A-Z0-9_]+)\s+\((0x[0-9a-fA-F]+)\)(.*)$", re.M)
TYPEDEF = re.compile(r"typedef\s+struct\s+([A-Za-z0-9_]+)\s*\{", re.M)
ALLOC_PARAMS = re.compile(r"^[A-Z0-9_]*(ALLOC_PARAM|ALLOCATION_PARAM)[A-Z0-9_]*$")


def comment_above(text, pos):
    """The block comment immediately above an offset, as one terse line.

    NVIDIA's headers put a prose block over each command, whose first line
    repeats the symbol. That repetition is dropped and the first real
    sentence kept -- the description has to say something the name does not.
    """
    head = text[:pos]
    end = head.rfind("*/")
    if end < 0:
        return ""
    # Only if the comment really is adjacent: nothing but whitespace between.
    if head[end + 2:].strip():
        return ""
    start = head.rfind("/*", 0, end)
    if start < 0:
        return ""
    body = head[start + 2:end]
    lines = []
    for ln in body.splitlines():
        ln = ln.strip().lstrip("*").strip()
        if not ln:
            continue
        if re.fullmatch(r"[A-Z0-9_]+", ln):        # the repeated symbol
            continue
        if ln.startswith("Possible status values"):
            break
        lines.append(ln)
        if len(" ".join(lines)) > 200:
            break
    out = " ".join(lines)
    out = re.sub(r"\s+", " ", out).strip()
    # One sentence is enough; these are one-line descriptions.
    m = re.match(r"(.{20,140}?\.)(\s|$)", out)
    if m:
        out = m.group(1)
    return out[:160].strip()


class Headers:
    def __init__(self, vendor):
        self.root = pathlib.Path(vendor)
        if not self.root.is_dir():
            sys.exit(f"{vendor} missing -- ./scripts/build.sh vendor")
        self.ctrl = {}          # cmd -> {name, file, line, params, desc}
        self.esc = {}           # nr  -> {name, file, line, desc}
        self.uvm = {}           # nr  -> {name, file, line, params, desc}
        self.klass = {}         # hclass -> {name, file, line, params, desc}
        self._load_ctrl()
        self._load_esc()
        self._load_uvm()
        self._load_classes()

    def rel(self, p):
        return str(pathlib.Path(p).relative_to(self.root))

    def cmd_by_name(self, sym):
        """Command number for a header symbol. The backend names some of the
        commands it mediates through bindgen (`sys::NV_..._CTRL_CMD_...`)
        rather than as a literal, and those have to resolve to the same
        number the trace carries."""
        for val, c in self.ctrl.items():
            if c["name"] == sym:
                return val
        return None

    # -- RM_CONTROL ---------------------------------------------------------
    def _load_ctrl(self):
        base = self.root / "src/common/sdk/nvidia/inc/ctrl"
        cand = collections.defaultdict(list)
        for f in sorted(base.rglob("*.h")):
            text = f.read_text(errors="replace")
            for m in CTRL_DEF.finditer(text):
                sym, val, tail = m.group(1), int(m.group(2), 16), m.group(3)
                fm = FINN_STRUCT.search(tail)
                cm = CLASS_PREFIX.match(sym)
                cand[val].append({
                    "name": sym,
                    "file": self.rel(f),
                    "line": text[:m.start()].count("\n") + 1,
                    "params": fm.group(1) if fm else None,
                    "finn": bool(fm),
                    "cls_ok": bool(cm) and (val >> 16) == int(cm.group(1), 16),
                    "desc": comment_above(text, m.start()),
                    "incl": self._include_of(f),
                })
        for val, lst in cand.items():
            self.ctrl[val] = self._pick(lst)

    @staticmethod
    def _pick(lst):
        """Several symbols can carry the same number: a real command and the
        enum-like values defined beside it (``..._SYSMEM`` = 0x1 next to a
        command whose own value is 0xd01). Rank rather than guess:

          1. a FINN-evaluated define is a real RM command by construction,
          2. otherwise one whose upper 16 bits are the class in its own name,
          3. and a symbol that merely EXTENDS another candidate's name is a
             value of that command, never a command.

        A tie that survives all three is reported as ambiguous instead of
        being silently resolved.
        """
        names = {c["name"] for c in lst}
        keep = [c for c in lst
                if not any(c["name"].startswith(n + "_") for n in names - {c["name"]})]
        keep = keep or lst
        for key in ("finn", "cls_ok"):
            best = [c for c in keep if c[key]]
            if best:
                keep = best
                break
        out = dict(keep[0])
        if len(keep) > 1:
            out["ambiguous"] = sorted(c["name"] for c in keep)
        return out

    def _include_of(self, f):
        """The path to #include this header by, against INCLUDE_DIRS."""
        for d in INCLUDE_DIRS:
            root = self.root / d
            try:
                return str(pathlib.Path(f).relative_to(root))
            except ValueError:
                continue
        return None

    # -- frontend escapes ---------------------------------------------------
    def _load_esc(self):
        # Two headers, two shapes. nv_escape.h gives literal numbers;
        # nv-ioctl-numbers.h gives NV_IOCTL_BASE + n, and the base is read
        # from the same file rather than assumed.
        f1 = self.root / "src/nvidia/arch/nvalloc/unix/include/nv_escape.h"
        t1 = f1.read_text(errors="replace")
        for m in re.finditer(r"^#define\s+(NV_ESC_[A-Z0-9_]+)\s+(0x[0-9a-fA-F]+)", t1, re.M):
            self.esc[int(m.group(2), 16)] = {
                "name": m.group(1), "file": self.rel(f1),
                "line": t1[:m.start()].count("\n") + 1,
                "desc": comment_above(t1, m.start()),
            }
        f2 = self.root / "kernel-open/common/inc/nv-ioctl-numbers.h"
        t2 = f2.read_text(errors="replace")
        bm = re.search(r"^#define\s+NV_IOCTL_BASE\s+(\d+)", t2, re.M)
        base = int(bm.group(1)) if bm else None
        if base is None:
            return
        for m in re.finditer(r"^#define\s+(NV_ESC_[A-Z0-9_]+)\s+\(NV_IOCTL_BASE\s*\+\s*(\d+)\)",
                             t2, re.M):
            self.esc[base + int(m.group(2))] = {
                "name": m.group(1), "file": self.rel(f2),
                "line": t2[:m.start()].count("\n") + 1,
                "desc": comment_above(t2, m.start()),
            }

    # -- UVM ----------------------------------------------------------------
    def _load_uvm(self):
        # UVM applies NO _IOC encoding: UVM_IOCTL_BASE(i) = i, so these are
        # raw numbers. Applying _IOC_SIZE to them silently yields 0.
        for name in ("kernel-open/nvidia-uvm/uvm_ioctl.h",
                     "kernel-open/nvidia-uvm/uvm_linux_ioctl.h"):
            f = self.root / name
            if not f.is_file():
                continue
            t = f.read_text(errors="replace")
            for m in UVM_DEF.finditer(t):
                sym, nr = m.group(1), int(m.group(2))
                self.uvm[nr] = {
                    "name": sym, "file": self.rel(f),
                    "line": t[:m.start()].count("\n") + 1,
                    "params": sym + "_PARAMS" if f"{sym}_PARAMS" in t else None,
                    "desc": comment_above(t, m.start()),
                    "incl": self._include_of(f),
                }
            # The two 0x3000_000x values are literals, not UVM_IOCTL_BASE.
            for m in re.finditer(r"^#define\s+(UVM_[A-Z0-9_]+)\s+(0x[0-9a-fA-F]+)", t, re.M):
                nr = int(m.group(2), 16)
                if nr < 0x1000:
                    continue
                sym = m.group(1)
                self.uvm[nr] = {
                    "name": sym, "file": self.rel(f),
                    "line": t[:m.start()].count("\n") + 1,
                    "params": sym + "_PARAMS" if f"{sym}_PARAMS" in t else None,
                    "desc": comment_above(t, m.start()),
                    "incl": self._include_of(f),
                }

    # -- RM_ALLOC classes ---------------------------------------------------
    def _load_classes(self):
        f = self.root / "src/nvidia/generated/g_allclasses.h"
        t = f.read_text(errors="replace")
        for m in ALLCLASS_DEF.finditer(t):
            sym, val, tail = m.group(1), int(m.group(2), 16), m.group(3)
            if "alias" in tail:
                continue
            self.klass.setdefault(val, {
                "name": sym, "file": self.rel(f),
                "line": t[:m.start()].count("\n") + 1,
                "desc": comment_above(t, m.start()),
                "params": None, "incl": None,
            })
        # The alloc params struct lives in the class header, cl<hclass>.h.
        # Its NAME is not derivable from the class name (NV_CHANNEL_ALLOC_PARAMS
        # for 0xc46f), so it is read out of the header instead of constructed.
        cdir = self.root / "src/common/sdk/nvidia/inc/class"
        for hc, info in self.klass.items():
            cf = cdir / f"cl{hc:04x}.h"
            if not cf.is_file():
                continue
            ct = cf.read_text(errors="replace")
            names = [n for n in TYPEDEF.findall(ct) if ALLOC_PARAMS.match(n)]
            if names:
                info["params"] = names[0]
                info["params_file"] = self.rel(cf)
                info["incl"] = self._include_of(cf)


# ---------------------------------------------------------------------------
# DRM: a different namespace, decoded so the rows are not bare numbers
# ---------------------------------------------------------------------------
# The same two header sources probe/run/drmtrace.sh parses, and for the same
# reason: strace names NVIDIA's private DRM numbers after whatever driver
# happens to sit at that number in its own table -- 0x0e prints as
# AMDGPU_FENCE_TO_HANDLE. The names come from the headers that apply.
#
# These rows are NOT part of the RM catalogue and carry no RM status. They
# are here so that a signature seen in a GL or Vulkan trace is not a bare
# number nobody can look up.
class DrmNames:
    def __init__(self, vendor):
        self.private, self.core = {}, {}
        self.base, self.end = 0x40, 0xA0
        p = pathlib.Path(vendor) / "kernel-open/nvidia-drm/nv_drm_common_ioctl.h"
        if p.is_file():
            for m in re.finditer(r"#define\s+DRM_NVIDIA_(\w+)\s+0x([0-9a-fA-F]+)", p.read_text()):
                self.private[int(m[2], 16)] = m[1]
        for c in ("/usr/include/libdrm/drm.h", "/usr/include/drm/drm.h"):
            f = pathlib.Path(c)
            if not f.is_file():
                continue
            t = f.read_text()
            for m in re.finditer(r"#define\s+DRM_IOCTL_(\w+)\s+DRM_IO\w*\s*\(\s*0x([0-9a-fA-F]+)", t):
                self.core.setdefault(int(m[2], 16), m[1])
            for name, attr in (("DRM_COMMAND_BASE", "base"), ("DRM_COMMAND_END", "end")):
                mm = re.search(rf"#define\s+{name}\s+0x([0-9a-fA-F]+)", t)
                if mm:
                    setattr(self, attr, int(mm[1], 16))
            break

    def get(self, nr):
        # The private window is a RANGE, not a floor: with "nr >= 0x40" the
        # SYNCOBJ ioctls land inside it and are reported as invented private
        # numbers. drmtrace.sh got that wrong once and it cost five real
        # core ioctls.
        if self.base <= nr < self.end:
            idx = nr - self.base
            return ("NVIDIA_" + self.private[idx]) if idx in self.private \
                else f"NVIDIA_UNKNOWN_0x{idx:02x}"
        return self.core.get(nr)


# ---------------------------------------------------------------------------
# the commands the backend answers itself
# ---------------------------------------------------------------------------
BACKEND_CONST = re.compile(
    r"^\s*(?:pub\s+)?const\s+(CMD_[A-Z0-9_]+|CTRL_[A-Z0-9_]+)\s*:\s*u32\s*=\s*"
    r"(0x[0-9a-fA-F_]+|sys::[A-Za-z0-9_]+)\s*;", re.M)


def manifest_answered(path):
    """cmd -> why, for the commands the backend ANSWERS ITSELF, read out of
    the mediation manifest beside the traces.

    This is the authoritative half. The constants that used to be scanned
    for out of the backend source now live in `crates/nvrm-abi/src/mediate.rs`
    -- one table, so the guest module's generated header, the manifest
    `verify` masks with and this catalogue cannot disagree about which field
    carries what. A reader that kept grepping the backend crate for them
    found two commands where there are seven, which is how this function
    came to exist.

    Only `backend-answered` and `identity-string` count here. The gpuId,
    pointer and fd kinds are mediation too, but they are the DESCRIPTOR
    TABLE's and the guest module's, and the catalogue already classifies
    those from the table itself.
    """
    out = {}
    p = pathlib.Path(path)
    if not p.is_file():
        return out
    for ln in p.read_text(errors="replace").splitlines():
        f = ln.split()
        # `mediated <device> <nr> <sub> <off> <len> <stride> <count> <kind>
        # <field>`. A manifest written before the escape namespace existed
        # has `<cmd>` where the signature is now and four columns fewer; it
        # is skipped rather than misread, so an old trace directory produces
        # weaker wording and never a wrong verdict.
        if f[:1] != ["mediated"] or len(f) < 10:
            continue
        if f[8] not in ("backend-answered", "identity-string"):
            continue
        # This map is keyed by CONTROL COMMAND, because that is what the row
        # it annotates is keyed by. Escapes carry their mediation under `sub`
        # of "-" and are read by manifest_mediated_escapes instead -- which
        # exists because this comment used to end "NV_ESC_CARD_INFO is
        # passthrough and mediated at once", stating a contradiction between
        # two artefacts as though it were a property of the world. It was
        # OPEN-QUESTIONS number 62, and it is fixed.
        if f[1] != "ctl" or f[2] != "0x2a" or f[3] == "-":
            continue
        cmd = int(f[3], 16)
        what = f"{' '.join(f[9:])} @{f[4]} ({f[8]}, mediation.txt)"
        out[cmd] = f"{out[cmd]}; {what}" if cmd in out else what
    return out


def manifest_mediated_escapes(path):
    """nr -> what is mediated, for ESCAPES the manifest declares fields on.

    THE GAP THIS CLOSES, and the code above used to state it as a fact rather
    than fix it: an escape was classified from the DESCRIPTOR TABLE alone, so
    `NV_ESC_CARD_INFO` -- whose BDF and gpuId the guest module rewrites in
    every entry of its inline block -- came out `passthrough`, while the
    evidence file called it mediated over the whole of its answer. Both files
    were right as each defined its words, and a reader who took `passthrough`
    to mean "carried unchanged" was misled by which table had been asked
    (OPEN-QUESTIONS number 62).

    So ask the other table. The mediation manifest is generated from
    `crates/nvrm-abi/src/mediate.rs`, the SAME table the guest module's BDF
    header comes from -- so a field the module rewrites and the manifest does
    not know about cannot exist. That makes this a classification derived
    from a table, which is what the descriptor-table route was wanted for;
    the descriptor table simply is not the table that knows this fact.

    Descriptor-table kinds are deliberately NOT counted here: a pointer or an
    fd field is the descriptor table's business and `gi` already classifies
    from it. Counting them twice would say nothing new and would make this
    reader disagree with that one.
    """
    out = {}
    p = pathlib.Path(path)
    if not p.is_file():
        return out
    for ln in p.read_text(errors="replace").splitlines():
        f = ln.split()
        if f[:1] != ["mediated"] or len(f) < 10:
            continue
        # An escape: sub is "-". Anything else is a control and belongs to
        # manifest_answered above.
        if f[3] != "-":
            continue
        if f[8] not in ("bdf-address", "bdf-scalar", "identity-string",
                        "backend-answered"):
            continue
        nr = int(f[2], 16)
        what = f"{' '.join(f[9:])} @{f[4]} ({f[8]})"
        out[nr] = f"{out[nr]}; {what}" if nr in out else what
    return out


def backend_mediated(crate_dir, hdr):
    """Controls the backend does not merely forward.

    The descriptor tables govern what can be CARRIED. They do not describe
    the calls the backend answers or rewrites itself -- the VRAM ledger
    rewrites FB_GET_INFO, the semaphore-surface waiters are intercepted, and
    a guest asking for the GPU's name can be given a different one. Those are
    implemented commands, and reporting them as `passthrough` would be
    wrong in the direction that matters: it would understate what has been
    built.

    Read from the backend source rather than listed here: every one of them
    is a `const CMD_*`/`CTRL_*: u32` beside the code that uses it. The rule
    finds constants, not arbitrary inline literals -- a command mediated
    without a named constant would be missed, which is why the count is
    printed and self-checked.
    """
    out = {}
    for f in sorted(pathlib.Path(crate_dir).glob("*.rs")):
        t = f.read_text(errors="replace")
        for m in BACKEND_CONST.finditer(t):
            name, val = m.group(1), m.group(2)
            if val.startswith("sys::"):
                num = hdr.cmd_by_name(val[5:])
            else:
                num = int(val.replace("_", ""), 16)
            if num is not None:
                out[num] = f"{name} ({f.name}:{t[:m.start()].count(chr(10)) + 1})"
    return out


# ---------------------------------------------------------------------------
# sizeof, compiled
# ---------------------------------------------------------------------------
class Sizeof:
    """Layout for a set of structs, one translation unit per header.

    Two things come out of it and they have DIFFERENT provenance, which is
    the whole reason this class exists rather than a regex:

      * ``sizeof(S)``, ``offsetof(S, m)`` and ``sizeof(((S *)0)->m)`` are
        COMPILED. No number here is parsed out of a header, ever. A layout
        read with a regex is a layout that is wrong the first time NVIDIA
        wraps a member in an alignment macro -- which they do, constantly.
      * the member NAME and its declared TYPE are parsed, because they are
        text and there is nothing to compile about them.

    The two cannot drift apart in the dangerous direction. A misparsed
    member name produces a line the COMPILER REFUSES, so it is dropped and
    counted; it cannot produce a wrong offset. That asymmetry is what makes
    the textual half safe.

    Per header rather than one big file on purpose: the SDK headers are not
    all mutually includable, and a single failure would take every size with
    it. Per header, a header that does not compile costs exactly its own
    structs and says so.
    """

    def __init__(self, vendor):
        self.root = pathlib.Path(vendor)
        self.cache = {}
        self.fields = {}    # struct -> [{name, type, offset, size}]
        self.dropped = {}   # struct -> [member, ...] the compiler refused
        self.failed = {}
        self.cc = shutil.which("cc") or shutil.which("gcc")

    def _flags(self):
        out = ["-std=gnu11", "-DNV_LINUX", "-D__linux__", "-w"]
        for d in INCLUDE_DIRS:
            out.append(f"-I{self.root / d}")
        return out

    def batch(self, wants, members_of=None):
        """wants: {include_path: {struct, ...}}.

        Fills ``cache`` with sizes and, when ``members_of`` is given (a
        callable ``(incl, struct) -> [(type, name), ...]``), ``fields`` with
        one compiled offset and member size per member.
        """
        if not self.cc:
            self.failed["*"] = "no C compiler"
            return
        for incl, structs in wants.items():
            structs = sorted(s for s in structs if s and s not in self.cache)
            if not incl or not structs:
                continue
            self._one(incl, structs, members_of)

    def _one(self, incl, structs, members_of):
        with tempfile.TemporaryDirectory() as td:
            src = pathlib.Path(td) / "s.c"
            head = ["#include <stdio.h>", "#include <stddef.h>",
                    "#include <nvtypes.h>", "#include <nvos.h>"]
            if incl not in ("nvos.h",):
                head.append(f'#include "{incl}"')
            head.append("int main(void){")
            # One PROBE per line, so a line number out of the compiler's
            # error maps back to exactly one struct or one member and
            # nothing else has to be guessed.
            probes = []
            for st in structs:
                probes.append(("S", st, None,
                               f'    printf("S\\t%s\\t%zu\\n", "{st}", sizeof({st}));'))
                for ty, mem in (members_of(incl, st) if members_of else ()):
                    probes.append(("M", st, mem,
                                   f'    printf("M\\t%s\\t%s\\t%s\\t%zu\\t%zu\\n", '
                                   f'"{st}", "{mem}", "{ty}", '
                                   f'(size_t)offsetof({st}, {mem}), '
                                   f'sizeof(((({st} *)0)->{mem})));'))
            exe = pathlib.Path(td) / "s"
            # DROP THE LINE THE COMPILER NAMES, then try again. A member the
            # textual scan invented (a bitfield, a member of an anonymous
            # union, a macro that did not expand to a declaration) is one
            # refused line, and losing the header for it would throw away
            # every good offset beside it. Bounded, because a compiler that
            # keeps failing on new lines is a header that does not build and
            # that is a different answer, reported as one.
            dead = set()
            for _ in range(24):
                lines = head + [t[3] for i, t in enumerate(probes) if i not in dead] \
                        + ["    return 0;", "}"]
                src.write_text("\n".join(lines) + "\n")
                r = subprocess.run([self.cc, *self._flags(), "-o", str(exe), str(src)],
                                   capture_output=True, text=True)
                if r.returncode == 0:
                    break
                # Map the reported line numbers back onto the probe list.
                live = [i for i in range(len(probes)) if i not in dead]
                hit = set()
                for m in re.finditer(r"^[^:\n]*s\.c:(\d+):", r.stderr, re.M):
                    k = int(m.group(1)) - len(head) - 1
                    if 0 <= k < len(live):
                        hit.add(live[k])
                if not hit:
                    # The compiler is unhappy about something that is not one
                    # of our lines -- the header itself. Fall back to the old
                    # behaviour: one struct at a time, so the loss is named.
                    if len(structs) > 1:
                        for st in structs:
                            self._one(incl, [st], members_of)
                    else:
                        self.failed[structs[0]] = \
                            r.stderr.strip().splitlines()[-1:] or ["?"]
                    return
                dead |= hit
            else:
                self.failed[incl] = ["the compiler kept refusing new lines"]
                return
            for i in sorted(dead):
                kind, st, mem, _ = probes[i]
                if kind == "M":
                    self.dropped.setdefault(st, []).append(mem)
                else:
                    self.failed[st] = ["the sizeof probe itself did not compile"]
            out = subprocess.run([str(exe)], capture_output=True, text=True)
            for ln in out.stdout.splitlines():
                f = ln.split("\t")
                if f[0] == "S" and len(f) == 3 and f[2].isdigit():
                    self.cache[f[1]] = int(f[2])
                elif f[0] == "M" and len(f) == 6 and f[4].isdigit() and f[5].isdigit():
                    self.fields.setdefault(f[1], []).append({
                        "name": f[2], "type": f[3],
                        "offset": int(f[4]), "size": int(f[5]),
                    })

    def members(self, name):
        """The compiled field list of one struct, offset order."""
        return sorted(self.fields.get(name, []), key=lambda m: (m["offset"], m["name"]))

    def field_at(self, name, off):
        """The member covering byte `off`, or None. Innermost wins: a member
        of size 0 or one that merely starts there loses to one that actually
        spans the byte."""
        best = None
        for m in self.fields.get(name, []):
            if m["offset"] <= off < m["offset"] + max(m["size"], 1):
                if best is None or m["size"] < best["size"]:
                    best = m
        return best

    def get(self, name):
        return self.cache.get(name)


# ---------------------------------------------------------------------------
# mediation flags
# ---------------------------------------------------------------------------
# WARNING: not anchored at the start of a line, and that is the whole point.
# NVIDIA wraps aligned members in a macro --
#
#     NV_DECLARE_ALIGNED(NvP64 pChannelHandleList, 8);
#
# -- so a `^\s*NvP64` rule matches none of them. Anchored, this scan found
# zero pointers in 1787 control structs, including the ones this project
# already mediates BECAUSE they carry pointers. A heuristic that finds
# nothing looks exactly like a clean result.
FD_MEMBER = re.compile(
    r"\b(?:NvS32|NvU32|int|NvHandle)\s+\**\s*([A-Za-z0-9_]*[Ff][Dd][A-Za-z0-9_]*)\s*[;\[,)]")
P64_MEMBER = re.compile(r"\bNvP64\s+\**\s*([A-Za-z0-9_]+)")


# One member declaration inside a struct body. The TYPE and the NAME are all
# this takes from the text -- the offset and the member's size are compiled
# (class Sizeof), so a wrong guess here becomes a line the compiler refuses
# and never a wrong number.
# Read from the RIGHT, which is the only way that is not ambiguous: a
# declaration is <type words and stars> <name> <array suffix>, and a
# left-to-right type pattern is greedy over the name -- `NvU32 subdeviceMask`
# parses as type `NvU32 subdeviceMas` and member `k`, which compiles into
# nothing and silently empties the field map.
MEMBER_ARR = re.compile(r"(?:\s*\[[^\]]*\])+$")
MEMBER_NAME = re.compile(r"(?:^|[\s*])([A-Za-z_][A-Za-z0-9_]*)$")
# NVIDIA wraps aligned members in a macro. Unwrapping it is not optional:
# every NvP64 in these headers is inside one.
ALIGNED = re.compile(r"NV_DECLARE_ALIGNED\s*\(\s*(.*?)\s*,\s*\d+\s*\)", re.S)
BLOCK = re.compile(r"\{[^{}]*\}", re.S)


def struct_members(headers, incl, name):
    """[(type, member), ...] for one struct, from its text.

    Anonymous inner blocks are collapsed rather than descended into: their
    members are addressable in C, but naming them needs the union's own
    rules and the compiler will refuse anything this gets wrong anyway. A
    named inner struct survives as its own member, which is what a caller
    reading an offset wants.
    """
    body = struct_body(headers, incl, name)
    if not body:
        return []
    body = re.sub(r"/\*.*?\*/", " ", body, flags=re.S)
    body = re.sub(r"//[^\n]*", " ", body)
    body = ALIGNED.sub(r"\1", body)
    # Collapse inner {...} until none are left, so `struct { ... } foo;`
    # still yields `foo` and the members inside do not leak out as members
    # of the outer struct at offsets they do not have.
    prev = None
    while prev != body:
        prev, body = body, BLOCK.sub(" @ ", body)
    body = body.strip()
    if body.startswith("{"):
        body = body[1:]
    out, seen = [], set()
    for decl in body.split(";"):
        decl = " ".join(decl.split())
        if not decl or ":" in decl or "(" in decl:
            continue          # bitfield or function pointer
        if "@" in decl:
            # A collapsed inner block. `union { ... } gpuNameString;` becomes
            # `union @ gpuNameString`, and that member IS addressable --
            # offsetof takes it and the compiler checks the name. Keeping it
            # matters: NV2080_CTRL_GPU_GET_NAME_STRING_PARAMS puts the whole
            # name in one, so dropping it left the mediated identity of all
            # things reported as a bare offset. An ANONYMOUS block leaves
            # nothing after the `@` and is skipped -- its members belong to
            # the outer struct at offsets this text cannot attribute.
            head_kw = decl.split("@")[0].strip() or "struct"
            rest = decl.split("@", 1)[1].strip()
            if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", rest):
                continue
            if rest not in seen:
                seen.add(rest)
                out.append((head_kw, rest))
            continue
        arr = MEMBER_ARR.search(decl)
        head = decl[:arr.start()].rstrip() if arr else decl
        m = MEMBER_NAME.search(head)
        if not m:
            continue
        mem = m.group(1)
        ty = " ".join(head[:m.start(1)].split())
        if not ty or mem in seen:
            continue
        seen.add(mem)
        out.append((ty + ("".join(arr.group(0).split()) if arr else ""), mem))
    return out


def params_by_convention(headers, info):
    """The params struct of a command whose `finn:` comment does not name one.

    NVIDIA's newer commands carry the struct in that comment and the reader
    mines it. The older ones evaluate a bare number instead --
    `NV2080_CTRL_CMD_BIOS_GET_INFO` is one, and it is the control number 51
    had just fixed, so the undercount lands on exactly the rows this
    pipeline cares most about.

    The name is not invented. It is PROPOSED by the two conventions these
    headers actually follow and accepted only when TWO independent things
    agree: the typedef is present in the command's own header, and the
    compiler answers `sizeof` for it (which the caller checks, because it
    compiles it anyway). A proposal that either check refuses is dropped
    and counted, never written down.

    There are two conventions and both are real, which is why the candidates
    are a list:

      * drop `_CMD`, append `_PARAMS` -- the common one, and the one that
        closes `NV2080_CTRL_CMD_BIOS_GET_INFO`;
      * append `_PARAMS` to the symbol verbatim, `_CMD` and all -- which is
        how `NV_CONF_COMPUTE_CTRL_CMD_SYSTEM_GET_CAPABILITIES_PARAMS` and
        `NV2080_CTRL_CMD_BIOS_GET_POST_TIME_PARAMS` are spelled.

    A command whose header holds BOTH is ambiguous and is left alone: two
    candidates that both pass every check is not a fact, it is a coin toss,
    and this pipeline does not have those.
    """
    incl, sym = info.get("incl"), info.get("name") or ""
    if not incl or "_CMD_" not in sym:
        return None
    cands = [sym.replace("_CMD_", "_", 1) + "_PARAMS", sym + "_PARAMS"]
    for d in INCLUDE_DIRS:
        f = headers.root / d / incl
        if not f.is_file():
            continue
        have = set(TYPEDEF.findall(f.read_text(errors="replace")))
        hit = [c for c in cands if c in have]
        return hit[0] if len(hit) == 1 else None
    return None


def struct_body(headers, incl, name):
    """The text of one typedef struct, for the member scan. Textual on
    purpose: this looks for the PRESENCE of a member kind, never at a
    layout -- layout is what the compiler answers."""
    if not incl or not name:
        return ""
    for d in INCLUDE_DIRS:
        f = headers.root / d / incl
        if f.is_file():
            t = f.read_text(errors="replace")
            m = re.search(r"typedef\s+struct\s+" + re.escape(name) + r"\s*\{", t)
            if not m:
                return ""
            depth, i = 0, m.end() - 1
            for j in range(i, len(t)):
                if t[j] == "{":
                    depth += 1
                elif t[j] == "}":
                    depth -= 1
                    if depth == 0:
                        return t[i:j]
            return t[i:]
    return ""


# ---------------------------------------------------------------------------
# the run
# ---------------------------------------------------------------------------
def read_probes(tdir):
    rows = []
    for ln in (tdir / "probes.tsv").read_text().splitlines():
        if ln.startswith("#") or not ln.strip():
            continue
        f = ln.split("\t")
        f += [""] * (13 - len(f))
        rows.append(dict(zip(
            "probe result tracer strace delta ioctls sigs modeset drm "
            "group libs libs_seen criterion".split(),
            f)))
    return rows


def passed(pr):
    """A probe whose trace may be catalogued.

    `PASS (matched on attempt N)` is a pass: the gate's claim is that a run
    exists in which the tracer and the kernel agree exactly, and one does.
    `ungated: ...` is NOT -- its criterion held, but nothing counted the
    calls, and an uncounted trace could be short without anything saying so.
    """
    return pr["result"].startswith("PASS")


def read_inventory(tdir):
    inv = collections.defaultdict(list)
    p = tdir / "inventory.tsv"
    if p.is_file():
        for ln in p.read_text().splitlines():
            f = ln.split("\t")
            if len(f) >= 2:
                inv[f[0]].append(tuple(f[1:]))
    return inv


def collect_signatures(tdir, probes):
    """(dev, nr, sub) -> {'probes': {probe: calls}, 'psize': set, 'status': set}"""
    sigs = collections.defaultdict(
        lambda: {"probes": collections.Counter(), "psize": set(), "rmstatus": collections.Counter()})
    for pr in probes:
        if not passed(pr):
            continue
        for r in traceread.read(traceread.trace_file(tdir, pr["probe"])):
            if r["t"] != "ioctl":
                continue
            # NORMALISE THE KEY. The tracer's `sub` field is the second
            # DISPATCH level for RM_CONTROL (the cmd), RM_ALLOC and
            # RM_ALLOC_MEMORY (the hClass) -- and for NV_ESC_RM_MAP_MEMORY
            # (0x4e) it is hMemory, a runtime HANDLE, put there so mappings
            # can be matched to their allocations (log.rs says so). A handle
            # is not a dimension of the surface: keyed on it, one escape
            # became 409 catalogue rows in the Vulkan trace, all of them the
            # same call. It collapses to one row (OPEN-QUESTIONS number 47).
            sub = r.get("sub")
            key = (r["dev"], r["nr"], "-" if r["nr"] == MAP_MEMORY_NR else (sub or "-"))
            e = sigs[key]
            e["probes"][pr["probe"]] += 1
            if r.get("psize") is not None:
                e["psize"].add(r["psize"])
            if r.get("status") is not None:
                e["rmstatus"][r["status"]] += 1
    return sigs


def provenance_fields(lines):
    """The provenance header as FIELDS, beside the human-readable lines.

    WHY BOTH, and why this function exists at all (OPEN-QUESTIONS number 56):
    every artefact carries its provenance as `driver:  610.57.04` and so on,
    one string per line. That is right for a person and useless for a MERGE --
    the question people ask is per ioctl across the drivers and cards it was
    measured on, and answering it means joining catalogues on architecture and
    driver. Joining on prose means parsing prose later, in a reader that does
    not exist yet, against files written months apart. Number 56 says to make
    the provenance structured NOW and merge later, for exactly that reason.

    The strings stay. They are what a reader reads, several artefacts print
    them verbatim, and dropping them to save a duplicate would break those for
    no gain. These are DERIVED from the same lines, in one place, so the two
    cannot drift.

    `arch` is split from the compute capability, which the line bundles as
    "Turing (compute 7.5)": an index wants to select on either.
    """
    out = {}
    for ln in lines:
        k, _, v = ln.partition(":")
        k, v = k.strip(), v.strip()
        if not k or not v:
            continue
        if k == "arch":
            m = re.match(r"(.*?)\s*\(compute\s*([0-9.]+)\)\s*$", v)
            if m:
                out["arch"], out["compute_cap"] = m.group(1).strip(), m.group(2)
                continue
        if k == "commit":
            # "8c0c627 (working tree modified)" -- the flag is a fact about the
            # run and belongs in its own field, not in the middle of a sha.
            out["tree_modified"] = "working tree modified" in v
            out["commit"] = v.split()[0]
            continue
        out[k] = v
    return out


def hexint(s):
    try:
        return int(s, 16) if s.startswith("0x") else int(s)
    except ValueError:
        return None


UNKNOWN = "unknown -- not in public headers"

# NV_ESC_RM_MAP_MEMORY. Read from the headers below for the resolution, but
# needed as a literal here because the key has to be normalised BEFORE
# anything is resolved. Checked against the header in the decoder self-test.
MAP_MEMORY_NR = "0x4e"


_NVKMS_TABLE = None


def nvkms_table(root):
    """The NVKMS command table, compiled once per run.

    OPEN-QUESTIONS number 64. Cached because resolving it compiles a sizeof
    probe, and there is one modeset row per command. A failure here is
    REPORTED and then tolerated: the catalogue's other 270-odd rows do not
    depend on NVKMS, and a vendor tree without `nvkms-api.h` should still
    produce a catalogue -- with the raw numbers it used to carry.
    """
    global _NVKMS_TABLE
    if _NVKMS_TABLE is None:
        try:
            _NVKMS_TABLE = nvkmsdecode.table(str(root))
        except (SystemExit, OSError) as e:
            print(f"  nvkms: no decoder ({e}) -- modeset rows keep their raw numbers",
                  file=sys.stderr)
            _NVKMS_TABLE = {}
    return _NVKMS_TABLE


def resolve(sigs, gov, hdr, sizes, mediated, drm, esc_mediated=None):
    """One catalogue row per signature. Every field is either read from a
    header or explicitly unknown."""
    esc_mediated = esc_mediated or {}
    rows = []
    # First pass: work out which struct sizes are wanted, then compile them
    # all in one go rather than one process per row.
    wants = collections.defaultdict(set)
    plan = []
    for (dev, nrs, subs), info in sigs.items():
        nr, sub = hexint(nrs), hexint(subs)
        kind, meta = None, None
        if dev in ("uvm", "uvmtools"):
            kind, meta = "uvm", hdr.uvm.get(nr)
        elif dev in ("ctl", "gpu"):
            esc = hdr.esc.get(nr)
            if esc and esc["name"] == "NV_ESC_RM_CONTROL" and sub is not None:
                kind, meta = "ctrl", hdr.ctrl.get(sub)
            elif esc and esc["name"] in ("NV_ESC_RM_ALLOC", "NV_ESC_RM_ALLOC_MEMORY") \
                    and sub is not None:
                # RM_ALLOC_MEMORY's sub is an hClass too (log.rs: "the column
                # that answers whether libcuda allocates NV01_MEMORY_SYSTEM
                # here"), and its length comes from that class the same way
                # -- the descriptor row says EMB_LEN_CLASS. So it is resolved
                # and classified as an allocation, not as a flat escape.
                kind, meta = "alloc", hdr.klass.get(sub)
            else:
                kind, meta = "escape", esc
        else:
            kind, meta = "other", None
        if meta and meta.get("params") and meta.get("incl"):
            wants[meta["incl"]].add(meta["params"])
        plan.append(((dev, nrs, subs), info, kind, meta, nr, sub))
    sizes.batch(wants, members_of=lambda i, st: struct_members(hdr, i, st))

    for (dev, nrs, subs), info, kind, meta, nr, sub in plan:
        devcode = DEV_CODE.get(dev)
        row = {
            "device": dev, "nr": nrs, "sub": subs, "kind": kind,
            "name": meta["name"] if meta else UNKNOWN,
            "description": (meta.get("desc") or "") if meta else "",
            "header": f"{meta['file']}:{meta['line']}" if meta else "",
            "params_struct": (meta.get("params") or "") if meta else "",
            "params_size": None,
            "observed_params_size": sorted(info["psize"]),
            "seen_in": sorted(info["probes"]),
            "calls": sum(info["probes"].values()),
            "rm_status": dict(info["rmstatus"]),
            "flags": [],
            "notes": [],
        }
        if meta and meta.get("ambiguous"):
            row["notes"].append("ambiguous in the headers: " + ", ".join(meta["ambiguous"]))
        if not meta:
            row["description"] = UNKNOWN
        if meta and meta.get("params"):
            row["params_size"] = sizes.get(meta["params"])
            # The COMPILED field list. Every offset here came from offsetof
            # in a translation unit that included the pinned header, so a row
            # can name the member a difference lands in instead of a word
            # index -- which is the whole point of the field map.
            row["fields"] = sizes.members(meta["params"])
            if meta.get("params_by"):
                row["notes"].append(
                    f"params struct name not in the finn: comment; "
                    f"{meta['params_by']}")
        elif meta and kind == "ctrl" and info["psize"] and set(info["psize"]) == {"0x0"}:
            # NOT an undercount: the header names no params struct AND every
            # observed call passed a zero-length params buffer. Two
            # independent statements that this command takes no arguments,
            # which is a closed answer rather than a missing one.
            row["notes"].append(
                f"argument-less: no params struct in the header, and all "
                f"{sum(info['probes'].values())} observed call(s) passed "
                f"paramsSize 0")

        # ---- governance -> status ----------------------------------------
        gi = gov.ioctls.get((devcode, nr)) if devcode is not None else None
        if kind == "ctrl":
            gc = gov.ctrls.get(sub)
            if gi is None:
                row["status"] = "missing"
                row["notes"].append("RM_CONTROL itself has no ioctl row -- nothing can carry it")
            elif sub in mediated:
                # Not carried but ANSWERED: the backend rewrites or
                # intercepts this one in code of its own.
                row["status"] = "implemented-unverified"
                row["notes"].append(f"answered by the backend itself: {mediated[sub]}")
            elif gc is None:
                # RM_CONTROL is self-describing: params pointer at 16, length
                # at 24. A command with no ctrl row is forwarded verbatim,
                # which is passthrough and NOT missing.
                row["status"] = "passthrough"
            elif gc["flags"] & CF_BLOCK:
                row["status"] = "implemented-unverified"
                row["notes"].append("BLOCKED: never forwarded from a guest (xlate::blocked_ctrls)")
            else:
                row["status"] = "implemented-unverified"
        elif kind == "alloc":
            gk = gov.classes.get(sub)
            needs_params = bool(meta and meta.get("params"))
            if gi is None:
                row["status"] = "missing"
            elif gk is None and needs_params:
                # Alloc params are NOT self-describing: the length comes from
                # the hClass. No row means no size, and the honest answer is
                # ENOTSUP -- never a guess, which would be an out-of-bounds
                # read in the host driver's copy_from_user.
                row["status"] = "missing"
                row["notes"].append("no hClass size-table entry, and this class has alloc params")
            elif gk is None:
                row["status"] = "passthrough"
                row["notes"].append("class allocates with a NULL params pointer -- no size needed")
            else:
                row["status"] = "implemented-unverified"
                if gk["flags"] & KF_UNVERIFIED:
                    row["notes"].append("class carries KF_UNVERIFIED: never allocated on real silicon")
        elif kind == "uvm":
            if gi is None:
                # UVM applies no _IOC encoding, so the size is not in the
                # command and there is nowhere else to get it. The guest
                # module answers EOPNOTSUPP rather than guessing -- a wrong
                # size is an out-of-bounds read in the host driver's
                # copy_from_user.
                row["status"] = "missing"
                row["notes"].append("no UVM size entry -- the command cannot be carried at all")
            else:
                row["status"] = "implemented-unverified"
        elif kind == "escape":
            # A frontend escape carries its own size in the _IOC encoding, so
            # a descriptor row is needed only where something has to be
            # TRANSLATED (an fd field, an embedded pointer, an XFER wrapper).
            # Read out of the guest module rather than assumed: the ioctl
            # path takes nr and size from _IOC_NR/_IOC_SIZE, looks the
            # descriptor up, and runs the call whether or not it found one
            # (virtio_nvrm.c, the else branch of ctx_is_uvm). Only the UVM
            # branch refuses on a missing row, because UVM numbers carry no
            # size at all.
            #
            # So an escape with no row is passthrough, not missing. Ten of
            # the 35 escapes in the headers have a row; the rest are flat
            # blocks that need nobody's help.
            # The descriptor table OR the mediation manifest. An escape the
            # guest module rewrites is implemented whether or not it needed a
            # descriptor row to do it -- CARD_INFO carries its answer inline
            # and needs no translation entry, and is mediated all the same.
            me = esc_mediated.get(nr)
            if gi:
                row["status"] = "implemented-unverified"
            elif me:
                row["status"] = "implemented-unverified"
                row["notes"].append(
                    f"no descriptor row and mediated anyway: the guest module "
                    f"rewrites {me} in the inline block (mediation.txt, "
                    f"generated from mediate.rs)")
            else:
                row["status"] = "passthrough"
        else:
            row["status"] = "not-governed"
            if dev == "modeset":
                # NVKMS carries its whole interface under ONE ioctl number
                # (_IOWR('m', 0, struct NvKmsIoctlParams), nvkms-ioctl.h), so
                # `nr` is 0 on every row and the command is the `sub` column
                # -- read by the tracer out of that struct, whose offsets are
                # a compiled layout guard in nvrm-sys.
                #
                # The command namespace IS resolved now (number 64):
                # `nvkmsdecode` reads `enum NvKmsIoctlCommand` out of
                # nvkms-api.h, derives each command's parameter struct by the
                # header's own naming convention, and COMPILES its size. The
                # size is then checked against `psize` -- the block size the
                # tracer measured per call -- so a wrong name is caught by a
                # measurement rather than believed.
                row["header"] = "kernel-open/nvidia-modeset/nvkms-ioctl.h"
                row["params_struct"] = "NvKmsIoctlParams"
                nk = nvkms_table(hdr.root).get(sub) if sub is not None else None
                if nk and nk["params_struct"]:
                    row["name"] = nk["name"]
                    row["params_struct"] = nk["params_struct"]
                    row["params_size"] = nk["params_size"]
                    row["header"] = "src/nvidia-modeset/interface/nvkms-api.h"
                    row["description"] = (
                        "NVKMS, not RM: a second userspace boundary, one ioctl number "
                        "for the whole interface, and the command in the `sub` column")
                    # The check number 64 named. `observed_params_size` is the
                    # measured psize; disagreeing with the compiled size means
                    # the name is wrong, and that is worth a note on the row
                    # rather than a silent mapping.
                    obs = [hexint(o) for o in row.get("observed_params_size", [])]
                    if obs and any(o != nk["params_size"] for o in obs):
                        row["notes"].append(
                            f"NVKMS name unverified: measured params size "
                            f"{row['observed_params_size']} against "
                            f"sizeof({nk['params_struct']}) = {nk['params_size']}")
                    elif obs:
                        row["notes"].append(
                            f"NVKMS name checked against the measured params size "
                            f"({nk['params_size']} bytes)")
                else:
                    why = nk["unresolved"] if nk else "command not in enum NvKmsIoctlCommand"
                    row["description"] = (
                        "NVKMS, not RM: a second userspace boundary, one ioctl number "
                        f"for the whole interface. Not named: {why}")
            if dev in ("drm", "render") and nr is not None:
                n = drm.get(nr)
                if n:
                    row["name"] = "DRM_" + n
                    row["header"] = ("kernel-open/nvidia-drm/nv_drm_common_ioctl.h"
                                     if n.startswith("NVIDIA_") else "libdrm drm.h")
                    row["description"] = ("DRM, not RM: a different namespace and a "
                                          "different boundary (probe/run/drmtrace.sh)")

        # ---- mediation flags ---------------------------------------------
        body = struct_body(hdr, meta.get("incl") if meta else None,
                           meta.get("params") if meta else None)
        # The escape row's own offsets describe the ESCAPE, not the command
        # inside it: NVOS54 always carries a params pointer at 16 and NVOS64
        # always carries pAllocParms at 16. Inheriting those onto every
        # control and every class flagged all 172 passthrough controls as
        # `embedded-ptr`, including a four-byte struct that cannot hold one.
        # For a control or an allocation the mediation cost is what is inside
        # ITS params, which is the ctrl/class row and the struct scan below.
        generic = kind in ("escape", "uvm")
        if generic and gi and gi["fd_off"] != NONE_U32:
            row["flags"].append("fd-field")
        if kind == "ctrl" and gov.ctrls.get(sub, {}).get("flags", 0) & CF_FD:
            row["flags"].append("fd-field")
        if kind == "alloc" and gov.classes.get(sub, {}).get("fd_off", NONE_U32) != NONE_U32:
            row["flags"].append("fd-field")
        if body and FD_MEMBER.search(body) and "fd-field" not in row["flags"]:
            m = FD_MEMBER.search(body)
            row["flags"].append("fd-field")
            row["notes"].append(f"params member '{m.group(1)}' looks like a process-local fd")
        if body and P64_MEMBER.search(body):
            row["flags"].append("embedded-ptr")
            # WHICH pointer, and where. The regex above answers "is there
            # one" off the header text; the offset comes from the compiled
            # field map, never from counting members in that text -- these
            # structs wrap every NvP64 in NV_DECLARE_ALIGNED, so a counted
            # offset is wrong exactly where it matters most.
            ptrs = [f"{m['name']} @{m['offset']}"
                    for m in (row.get("fields") or []) if m["type"] == "NvP64"]
            if ptrs:
                row["notes"].append("pointer field(s), offsets compiled: "
                                    + ", ".join(ptrs))
        if generic and gi and gi["emb_ptr_off"] != NONE_U32 \
                and "embedded-ptr" not in row["flags"]:
            row["flags"].append("embedded-ptr")
        if kind == "ctrl" and gov.ctrls.get(sub, {}).get("count", 0):
            # Second-level pointers: the expensive case. The params buffer
            # holds a P64 whose target is itself a struct that has to be
            # walked, and that is what the nested table describes.
            row["flags"].append("embedded-ptr(second-level)")
        if generic and gi and gi["flags"] & F_OSDESC:
            row["flags"].append("process-local-va")
        if kind == "alloc" and meta and "OS_DESCRIPTOR" in (meta.get("name") or ""):
            row["flags"].append("process-local-va")
        if kind == "alloc" and meta and meta.get("params"):
            row["flags"].append("size-table")
        if not row["flags"]:
            row["flags"].append("none")
        row["flags"] = sorted(set(row["flags"]))

        # ---- the diff against xlate ---------------------------------------
        # Generated here from the headers, then compared with xlate's own
        # serialised answer. A disagreement is reported, never resolved.
        row["xlate_size"] = None
        if kind == "alloc" and sub in gov.classes:
            row["xlate_size"] = gov.classes[sub]["param_size"]
        elif kind == "uvm" and gi:
            row["xlate_size"] = gi["size"]
        if row["params_size"] is not None and row["xlate_size"] is not None \
           and row["params_size"] != row["xlate_size"]:
            row["notes"].append(
                f"DISAGREEMENT: headers say sizeof({row['params_struct']}) = "
                f"{row['params_size']}, xlate's table says {row['xlate_size']}")
        rows.append(row)
    return rows


STATUS_ORDER = ["missing", "passthrough", "implemented-unverified",
                "implemented-verified", "implemented-verified-mediated",
                "not-governed"]


def verified_evidence(outdir, driver):
    """The Phase 0.6 evidence file, if a differential harness ever writes
    one. Never written here: this pipeline reads it or reports its absence."""
    p = pathlib.Path(outdir) / f"verified-{driver}.json"
    # Repo-relative: the artefacts are read on machines other than the one
    # that wrote them, and an absolute path from somebody's home directory
    # is not a location anybody else can act on.
    shown = f"matrix/verified-{driver}.json"
    if not p.is_file():
        return None, shown
    try:
        return json.loads(p.read_text()), shown
    except Exception as e:                                  # noqa: BLE001
        return None, f"{shown} (unreadable: {e})"


def guest_evidence(outdir, driver):
    """What `scripts/ioctl-matrix.sh guest` found, if it has ever run.

    Read, never written here, for the same reason as the file above: this
    generator describes the HOST and predicts the guest. Only a run inside a
    guest may write down what a guest did.
    """
    p = pathlib.Path(outdir) / f"guest-{driver}.json"
    shown = f"matrix/guest-{driver}.json"
    if not p.is_file():
        return None, shown
    try:
        js = json.loads(p.read_text())
        return {e["probe"]: e for e in js.get("probes", [])}, shown
    except Exception as e:                                  # noqa: BLE001
        return None, f"{shown} (unreadable: {e})"


def apply_convention_params(hdr, sizes):
    """Fill in the params struct of every command whose `finn:` comment does
    not name one, where the convention proposes a name that the header AND
    the compiler both accept. Returns (closed, still_open).

    Run BEFORE the batch, because the compiler's answer is the second of the
    two checks: a proposal `sizeof` refuses is withdrawn here, not recorded.
    """
    proposals = {}
    for cmd, info in hdr.ctrl.items():
        if info.get("params"):
            continue
        cand = params_by_convention(hdr, info)
        if cand:
            proposals[cmd] = cand
    wants = collections.defaultdict(set)
    for cmd, cand in proposals.items():
        wants[hdr.ctrl[cmd]["incl"]].add(cand)
    sizes.batch(wants, members_of=lambda i, st: struct_members(hdr, i, st))
    closed = 0
    for cmd, cand in proposals.items():
        if sizes.get(cand) is None:
            continue
        hdr.ctrl[cmd]["params"] = cand
        hdr.ctrl[cmd]["params_by"] = "convention (_CMD_ dropped, _PARAMS " \
                                     "appended), typedef present and sizeof compiled"
        closed += 1
    still = sum(1 for c in hdr.ctrl.values() if not c.get("params"))
    return closed, still


def write_fieldmap(path, hdr, sizes, prov):
    """The compiled field map, beside tables.txt in the trace artefacts.

    RAW MATERIAL, not a claim: it is a property of the vendor headers this
    tree is pinned to, it is regenerated from them in seconds, and nothing
    in it is a decision anybody took. That is why it lives with the traces
    and not under matrix/ (the versioning rule).

    Header-wide rather than run-wide on purpose. A consumer that only has
    the run's own signatures cannot name a field of a command this run did
    not make, and the next run's set is different -- so the artefact would
    change for reasons that have nothing to do with what it describes.
    """
    closed, still = apply_convention_params(hdr, sizes)
    wants = collections.defaultdict(set)
    for d in (hdr.ctrl, hdr.klass, hdr.uvm):
        for c in d.values():
            if c.get("params") and c.get("incl"):
                wants[c["incl"]].add(c["params"])
    sizes.batch(wants, members_of=lambda i, st: struct_members(hdr, i, st))

    def commands(d, kind):
        out = {}
        for k, c in d.items():
            if not c.get("params"):
                continue
            out[f"{k:#x}"] = {
                "name": c["name"], "kind": kind, "params_struct": c["params"],
                "header": f"{c['file']}:{c['line']}",
                "params_name_from": c.get("params_by", "finn: comment"),
            }
        return out

    structs = {}
    for name, size in sorted(sizes.cache.items()):
        structs[name] = {"size": size, "members": sizes.members(name)}
    js = {
        "provenance": prov,
        "provenance_fields": provenance_fields(prov),
        "generated_by": "scripts/ioctl-matrix.sh trace (probe/python/ioctlmatrix.py --fieldmap-only)",
        "method": (
            "sizeof(S), offsetof(S, m) and sizeof(((S *)0)->m) are COMPILED, one "
            "translation unit per header -- no number here is parsed. The member "
            "NAME and TYPE are read from the header text, and a misparse becomes a "
            "line the compiler refuses rather than a wrong offset"),
        "params_name": (
            "from the command's finn: comment where it carries one. Where it does "
            "not, PROPOSED by convention (drop _CMD, append _PARAMS) and accepted "
            "only when the typedef is in the command's own header and sizeof "
            "compiles -- two checks, neither of them a list somebody maintains"),
        "counts": {
            "structs": len(structs),
            "members": sum(len(v["members"]) for v in structs.values()),
            "params_named_by_convention": closed,
            "controls_still_without_params": still,
            "structs_that_did_not_compile": len(sizes.failed),
            "members_the_compiler_refused":
                sum(len(v) for v in sizes.dropped.values()),
        },
        "commands": {
            "ctrl": commands(hdr.ctrl, "ctrl"),
            "uvm": commands(hdr.uvm, "uvm"),
            "class": commands(hdr.klass, "alloc"),
        },
        "structs": structs,
        "did_not_compile": {k: v for k, v in sorted(sizes.failed.items())},
        "members_refused": {k: sorted(v) for k, v in sorted(sizes.dropped.items())},
    }
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(js, indent=2) + "\n")
    c = js["counts"]
    print(f"  field map: {c['structs']} struct(s), {c['members']} member(s), "
          f"offsets compiled; {c['params_named_by_convention']} params struct(s) "
          f"named by convention, {c['controls_still_without_params']} control(s) "
          f"still without one")
    if c["structs_that_did_not_compile"] or c["members_the_compiler_refused"]:
        print(f"  field map: {c['structs_that_did_not_compile']} struct(s) did not "
              f"compile, {c['members_the_compiler_refused']} member(s) refused")
    print(f"wrote {path}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--traces", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--driver", required=True)
    ap.add_argument("--vendor", required=True)
    ap.add_argument("--xlate", required=True)
    ap.add_argument("--provenance", default="")
    ap.add_argument("--fieldmap", default="",
                    help="also write the compiled field map here")
    ap.add_argument("--fieldmap-only", action="store_true",
                    help="write only the field map and stop -- needs no traces")
    a = ap.parse_args()

    tdir, outdir = pathlib.Path(a.traces), pathlib.Path(a.out)
    prov = [x for x in a.provenance.split("|") if x.strip()]

    if a.fieldmap_only:
        hdr = Headers(a.vendor)
        sizes = Sizeof(a.vendor)
        write_fieldmap(pathlib.Path(a.fieldmap), hdr, sizes, prov)
        return 0

    gov = Governance(tdir / "tables.txt")
    gov.selfcheck()
    hdr = Headers(a.vendor)
    if not hdr.ctrl or not hdr.esc or not hdr.klass:
        sys.exit("the vendor header readers came back empty -- ./scripts/build.sh vendor")
    sizes = Sizeof(a.vendor)
    drm = DrmNames(a.vendor)
    # BOTH halves, and neither is redundant. The manifest names every command
    # whose params buffer the backend WRITES INTO; the source scan still
    # catches the ones it intercepts without rewriting a field -- the two
    # semaphore-surface waiter controls are handled entirely in the backend
    # and change no byte of the answer, so no manifest record describes them
    # and only a constant in the code says they exist.
    mediated = backend_mediated(
        pathlib.Path(a.xlate).parent.parent.parent / "vhost-user-nvrm/src", hdr)
    mediated.update(manifest_answered(tdir / "mediation.txt"))

    # The same closure the field map applies, so the catalogue and the field
    # map name the same struct for the same command. Without it a row like
    # NV2080_CTRL_CMD_BIOS_GET_INFO -- whose finn: comment evaluates a bare
    # number instead of naming its params -- carries no struct here while
    # fields.json has one, and the two artefacts would disagree about the
    # same header.
    closed, still = apply_convention_params(hdr, sizes)
    print(f"  params struct names: {closed} taken from the naming convention "
          f"and checked twice, {still} control(s) still without one")

    probes = read_probes(tdir)
    inv = read_inventory(tdir)
    sigs = collect_signatures(tdir, probes)
    rows = resolve(sigs, gov, hdr, sizes, mediated, drm,
                   manifest_mediated_escapes(tdir / "mediation.txt"))
    if a.fieldmap:
        write_fieldmap(pathlib.Path(a.fieldmap), hdr, sizes, prov)

    ev, evpath = verified_evidence(outdir, a.driver)
    if ev:
        keys = {tuple(k.split()) for k in ev.get("verified", [])}
        # TWO CLASSES, AND THEY DO NOT SHARE A ROW UNLABELLED. `verified` is
        # "the same bytes on both sides"; `verified_mediated` is "different
        # bytes, in exactly the fields the mediation declares and nowhere
        # else". Both are answer evidence, so both get the note and both can
        # promote -- but the STATUS says which, because a reader who cannot
        # tell them apart has been told the boundary carried something
        # unchanged when it deliberately did not.
        medkeys = {tuple(k.split()) for k in ev.get("verified_mediated", [])}
        detail = ev.get("evidence", {})
        for r in rows:
            k = (r["device"], r["nr"], r["sub"])
            if k not in keys and k not in medkeys:
                continue
            # The note goes on EVERY row the evidence names, whatever its
            # status. Byte evidence for a passthrough command is evidence --
            # it is simply not evidence about the descriptor tables, which is
            # what the `implemented-*` classes are about. Keeping the two
            # apart is the difference between a status and a fact.
            d = detail.get(" ".join(k), {})
            if d:
                r["notes"].append(
                    f"answer bytes compared against a native run: "
                    f"{d.get('calls_compared', '?')} call(s), "
                    f"{d.get('bytes_compared', '?')} of {d.get('answer_size', '?')} bytes"
                    + (f", masked {d['words_masked']}" if d.get("words_masked") else "")
                    + ("" if d.get("fully_paired", True) else
                       "; NOT paired in " + ", ".join(d.get("probes_not_comparable", []))
                       + ", which judged nothing either way"))
            if k in medkeys:
                r["notes"].append(
                    "MEDIATED: the answer differs from the native one in exactly "
                    "the fields the mediation manifest declares ("
                    + "; ".join(d.get("mediation", [])) + ") and in no other "
                    "byte. That is a different claim from byte equality")
                if r["status"] == "implemented-unverified":
                    r["status"] = "implemented-verified-mediated"
            elif r["status"] == "implemented-unverified":
                r["status"] = "implemented-verified"

    # A self-check with teeth: the decoder is only trustworthy if the
    # commands everybody knows resolve. If RM_CONTROL, RM_ALLOC and
    # UVM_INITIALIZE do not come back by name, the header readers are broken
    # and every row below is decoration.
    checks = [
        (hdr.esc.get(0x2A, {}).get("name"), "NV_ESC_RM_CONTROL"),
        (hdr.esc.get(0x2B, {}).get("name"), "NV_ESC_RM_ALLOC"),
        (hdr.esc.get(200 + 1, {}).get("name"), "NV_ESC_REGISTER_FD"),
        (hdr.uvm.get(0x30000001, {}).get("name"), "UVM_INITIALIZE"),
        (hdr.esc.get(int(MAP_MEMORY_NR, 16), {}).get("name"), "NV_ESC_RM_MAP_MEMORY"),
        # The two strace names wrongly after other vendors' drivers, and one
        # core ioctl ABOVE the private window -- the case a "nr >= base" rule
        # gets wrong.
        (drm.get(0x40 + 0x0D), "NVIDIA_GEM_EXPORT_DMABUF_MEMORY"),
        (drm.get(0x40 + 0x0E), "NVIDIA_GEM_IDENTIFY_OBJECT"),
        (drm.get(0xBF), "SYNCOBJ_CREATE"),
        (hdr.klass.get(0xC46F, {}).get("name"), "TURING_CHANNEL_GPFIFO_A"),
    ]
    bad = [(g, w) for g, w in checks if g != w]
    if bad:
        for g, w in bad:
            print(f"  DECODER BROKEN: expected {w}, got {g}", file=sys.stderr)
        sys.exit("the header decoder failed its own self-check")
    if not mediated:
        sys.exit("no CMD_*/CTRL_* constants were found in the backend source -- "
                 "the reader for backend-answered commands no longer understands "
                 "it, and every one of them would be reported as plain passthrough")
    print(f"  backend answers {len(mediated)} command(s) itself: "
          + ", ".join(f"{c:#x}" for c in sorted(mediated)))
    print(f"  decoder: {len(hdr.esc)} escapes, {len(hdr.ctrl)} controls, "
          f"{len(hdr.uvm)} UVM commands, {len(hdr.klass)} classes parsed, "
          f"{len(checks)}/{len(checks)} self-checks pass")

    guest, guestpath = guest_evidence(outdir, a.driver)
    if guest:
        print(f"  guest evidence: {len(guest)} probe(s) from {guestpath}")

    write_catalog(outdir, a.driver, prov, rows, probes, inv, gov, sizes, ev, evpath)
    write_matrix(outdir, a.driver, prov, rows, probes, inv, guest, guestpath)
    write_tasks(outdir, a.driver, prov, rows, probes, inv, ev, evpath)


# ---------------------------------------------------------------------------
# outputs
# ---------------------------------------------------------------------------
def md_head(fh, prov, title, cmd):
    fh.write("<!-- SPDX-License-Identifier: MIT -->\n")
    fh.write(f"<!-- GENERATED by {cmd} -- do not edit. -->\n")
    fh.write(f"# {title}\n\n```\n")
    for ln in prov:
        fh.write(ln + "\n")
    fh.write("```\n\n")


def flagstr(r):
    return ", ".join(r["flags"])


def sortkey(r):
    return (STATUS_ORDER.index(r["status"]) if r["status"] in STATUS_ORDER else 9,
            r["seen_in"][0] if r["seen_in"] else "", r["device"], r["nr"], r["sub"])


def write_catalog(outdir, driver, prov, rows, probes, inv, gov, sizes, ev, evpath):
    js = {
        "provenance": prov,
        "provenance_fields": provenance_fields(prov),
        "generated_by": "scripts/ioctl-matrix.sh catalog",
        "signature_key": "(device, ioctl nr, sub) -- sub is the RM_CONTROL cmd, "
                         "the RM_ALLOC hClass, the NVKMS command on device "
                         "'modeset', or '-'",
        "answer_verification_evidence": evpath if ev else None,
        "signatures": sorted(rows, key=sortkey),
        "not_staged": [{"library": l[0], "where": l[1]} for l in inv.get("notstaged", [])],
        "probes": probes,
    }
    (outdir / f"catalog-{driver}.json").write_text(json.dumps(js, indent=2) + "\n")

    with (outdir / f"catalog-{driver}.md").open("w") as fh:
        md_head(fh, prov, f"ioctl catalogue, driver {driver}",
                "scripts/ioctl-matrix.sh catalog")
        fh.write(
            "One row per signature `(device, ioctl nr, sub)`. Names, descriptions,\n"
            "header references and parameter sizes come from `vendor/` only -- the\n"
            "sizes are compiled, not parsed. Where a header says nothing the row says\n"
            f"`{UNKNOWN}` and carries the raw number.\n\n"
            "`missing` rows come first, because they are the ones that carry work.\n\n"
            "## Status\n\n"
            "| status | meaning |\n|---|---|\n"
            "| `missing` | observed in a trace and the backend has no entry that could carry it |\n"
            "| `passthrough` | forwarded without interpretation (RM_CONTROL is self-describing) |\n"
            "| `implemented-unverified` | governed by the descriptor table; the response bytes have NEVER been compared against a native run |\n"
            "| `implemented-verified` | as above AND the first bytes of its answer are the SAME in a guest as natively |\n"
            "| `implemented-verified-mediated` | as above, but the answer DIFFERS -- in exactly the fields the mediation manifest declares and in no other byte. A different claim, and deliberately not the same row |\n"
            "| `not-governed` | a different namespace (DRM, NVKMS), carried here for completeness |\n\n")
        nver = sum(1 for r in rows if r["status"] == "implemented-verified")
        if not ev:
            fh.write(
                "**`implemented-verified` is empty in this run, and that is a correct\n"
                "output.** No answer-verification evidence file exists "
                f"(`{evpath}`), because no differential harness has been built yet.\n"
                "Nothing in this tree compares the answer BYTES of a forwarded control\n"
                "against the bytes the same call returns natively -- the gates compare\n"
                "status codes and workload results. Until that harness exists, every\n"
                "governed signature is honestly `implemented-unverified`. TASKS carries\n"
                "the standing task that would change it.\n\n")
        else:
            nev = len(ev.get("verified", []))
            fh.write(
                f"**Answer evidence exists** (`{evpath}`): {nev} signature(s) had the\n"
                "first bytes of their answer compared, call by call, against the same\n"
                "call in a native run, and matched. Rows that carry it say so in a\n"
                "note, with how many bytes of how large an answer -- 32 bytes of a\n"
                "384-byte answer is 32 bytes, and the note is there so that\n"
                "`verified` cannot be read as more than what was compared.\n\n"
                f"Of those, **{nver} are `implemented-verified`**, i.e. governed by the\n"
                "descriptor tables AND matched. ")
            if nver == 0:
                fh.write(
                    "That the count is zero is a finding rather than an omission, and\n"
                    "it has two halves.\n\n"
                    "**Reach.** The evidence comes from the tracer's `ctrlout` line,\n"
                    "which dumps root-client (0x2xx) and subdevice (0x2080xxxx)\n"
                    "controls and nothing else. Allocations and UVM commands -- where\n"
                    "most of the governed class lives, because those are the two places\n"
                    "a size is not self-describing -- have no answer dump at all and are\n"
                    "out of reach of this slice entirely.\n\n"
                    "**Criterion.** The governed CONTROLS it does reach are the ones the\n"
                    "backend answers itself, and their answers differ from the native\n"
                    "ones deliberately: that is what mediation IS. Byte equality is the\n"
                    "wrong test for them. The right one -- differs in exactly the fields\n"
                    "the mediation rewrites, and nowhere else -- needs the mediation to\n"
                    "name its own fields, and it does not yet. `not_verified` carries\n"
                    "each of them with the catalogue's own words about why it is\n"
                    "mediated, so the list is a work list rather than a complaint.\n\n")
            else:
                named = [r["name"] for r in rows
                         if r["status"] == "implemented-verified"]
                fh.write(
                    "They are: " + ", ".join(f"`{n}`" for n in sorted(named)) + ".\n\n"
                    "That the number is small has two causes, and neither is an\n"
                    "omission.\n\n"
                    "**Reach.** The evidence comes from the tracer's `ctrlout` line,\n"
                    "which dumps root-client (0x2xx) and subdevice (0x2080xxxx)\n"
                    "controls and nothing else. Allocations and UVM commands -- where\n"
                    "most of the governed class lives, because those are the two places\n"
                    "a size is not self-describing -- have no answer dump at all and are\n"
                    "out of reach of this comparison entirely.\n\n"
                    "**Criterion.** Several of the governed controls it does reach are\n"
                    "the ones the backend answers ITSELF, and their answers differ from\n"
                    "the native ones deliberately: that is what mediation IS. Byte\n"
                    "equality is the wrong test for them, and the right one -- differs\n"
                    "in exactly the fields the mediation rewrites, and nowhere else --\n"
                    "needs the mediation to name its own fields. `not_verified` in the\n"
                    "evidence file carries each of them with the catalogue's own words\n"
                    "about why it is mediated, so the list is a work list rather than a\n"
                    "complaint.\n\n")

        # The diff against xlate, stated rather than implied. Silence here
        # would read as "the comparison was not run", which is a different
        # claim from "it was run and agreed".
        cmp_rows = [r for r in rows
                    if r["params_size"] is not None and r["xlate_size"] is not None]
        dis = [r for r in cmp_rows if r["params_size"] != r["xlate_size"]]
        fh.write(
            "## The diff against xlate.rs\n\n"
            "Every size above was compiled from the vendor headers, independently of\n"
            "`crates/nvrm-abi/src/xlate.rs`, and then compared with the number xlate's\n"
            "own serialised table carries. Either side can be the bug, so a\n"
            "disagreement is reported and never resolved.\n\n"
            f"**{len(cmp_rows)} sizes compared, {len(dis)} disagreements.**\n\n")
        if dis:
            for r in dis:
                fh.write(f"- `{r['name']}`: headers say sizeof({r['params_struct']}) = "
                         f"{r['params_size']}, xlate says {r['xlate_size']}\n")
            fh.write("\n")
        else:
            fh.write("Where the two can be compared -- RM_ALLOC classes and UVM commands,\n"
                     "the two places a size is not self-describing -- they agree.\n\n")

        counts = collections.Counter(r["status"] for r in rows)
        fh.write("| status | signatures |\n|---|---:|\n")
        for s in STATUS_ORDER:
            if counts.get(s):
                fh.write(f"| `{s}` | {counts[s]} |\n")
        fh.write(f"| **total** | **{len(rows)}** |\n\n")

        fh.write("## Mediation flags\n\n"
                 "| flag | what it costs an implementation |\n|---|---|\n"
                 "| `fd-field` | the params carry a process-local file descriptor that has to be translated |\n"
                 "| `embedded-ptr` | the params carry an `NvP64` into the caller's address space |\n"
                 "| `embedded-ptr(second-level)` | the pointed-to buffer holds further pointers -- the expensive case |\n"
                 "| `process-local-va` | an OS_DESCRIPTOR-style user range: pages have to be pinned and described |\n"
                 "| `size-table` | RM_ALLOC is not self-describing; this hClass needs a size entry |\n"
                 "| `none` | flat struct, self-describing length -- likely pure passthrough |\n\n")

        cur = None
        for r in sorted(rows, key=sortkey):
            if r["status"] != cur:
                cur = r["status"]
                fh.write(f"\n## {cur}\n\n")
                fh.write("| device | nr | sub | name | description | header | params | size | flags | seen in |\n")
                fh.write("|---|---|---|---|---|---|---|---:|---|---|\n")
            size = r["params_size"]
            if size is None and r["xlate_size"] is not None:
                size = f"{r['xlate_size']} (table)"
            fh.write("| {device} | `{nr}` | `{sub}` | `{name}` | {desc} | `{hdr}` | `{ps}` | {size} | {flags} | {seen} |\n".format(
                device=r["device"], nr=r["nr"], sub=r["sub"], name=r["name"],
                desc=(r["description"] or "").replace("|", "\\|") or "&mdash;",
                hdr=r["header"] or "&mdash;", ps=r["params_struct"] or "&mdash;",
                size=size if size is not None else "&mdash;",
                flags=flagstr(r), seen=", ".join(r["seen_in"])))
            for n in r["notes"]:
                fh.write(f"| | | | | *{n}* | | | | | |\n")

        ms = [pr for pr in probes if (pr["modeset"] or "0") not in ("", "0")]
        km = [r for r in rows if r["device"] == "modeset"]
        fh.write("\n## The NVKMS userspace node\n\n")
        if ms:
            fh.write(
                "`/dev/nvidia-modeset` is a userspace boundary of its own. NVKMS is an\n"
                "in-kernel RM client, and that part of it no interposer can see -- but\n"
                "the node itself is opened by the GL and Vulkan libraries directly, and\n"
                "those calls are as much a part of what a guest has to carry as any\n"
                "escape. They were found by counting the two instruments against each\n"
                "other and asking what the difference was made of, and until\n"
                "2026-08-20 the tracer had no tag for the node and could see none of\n"
                "them. It has one now, and this node passes the same counter-check\n"
                "against strace that the RM nodes do.\n\n"
                "One ioctl number carries the whole interface\n"
                "(`_IOWR('m', 0, struct NvKmsIoctlParams)`, nvkms-ioctl.h), so `nr` is\n"
                "0 in every row and the command is the `sub` column, read out of that\n"
                "struct. The command NAMESPACE is decoded (OPEN-QUESTIONS 64): these are\n"
                "not RM_CONTROL commands and resolve against no `ctrl*.h`, so the names\n"
                "come from `enum NvKmsIoctlCommand` in `nvkms-api.h`, which carries no\n"
                "explicit initialisers -- a command's number IS its position, read at\n"
                "generation time. The parameter struct follows the header's own naming\n"
                "convention and is then checked against the structs the header actually\n"
                "declares; a command whose struct is not declared is left unnamed rather\n"
                "than guessed at.\n\n"
                "| probe | ioctls on /dev/nvidia-modeset |\n|---|---:|\n")
            for pr in sorted(ms, key=lambda x: -int(x["modeset"])):
                fh.write(f"| `{pr['probe']}` | {pr['modeset']} |\n")
            fh.write(
                "\nThe size of the Vulkan number is the finding. An enumerating Vulkan\n"
                "client makes more calls to NVKMS from userspace than the entire NVML\n"
                "path makes to RM.\n\n"
                f"### The {len(km)} command(s) behind that count\n\n"
                "| command (`sub`) | name | calls | measured | `sizeof` | seen in |\n"
                "|---|---|---:|---|---:|---|\n")
            agree = 0
            for r in sorted(km, key=lambda x: -x["calls"]):
                meas = ", ".join(r["observed_params_size"]) or "&mdash;"
                want = r["params_size"]
                ok = (want is not None
                      and all(hexint(o) == want for o in r["observed_params_size"]))
                agree += 1 if ok else 0
                fh.write("| `{sub}` | `{name}` | {calls} | {meas} | {want} | {seen} |\n".format(
                    sub=r["sub"], name=r["name"], calls=r["calls"], meas=meas,
                    want=want if want is not None else "&mdash;",
                    seen=", ".join(r["seen_in"])))
            fh.write(
                "\n`measured` is the size NVKMS was handed for the block the command\n"
                "points at, taken per call by the tracer -- not the size of the 16-byte\n"
                "indirection struct. `sizeof` is that command's parameter struct compiled\n"
                "out of `nvkms-api.h`. **They are independent**, which is what makes the\n"
                "comparison worth anything: the first comes from a run, the second from a\n"
                "header, and a wrong name shows up as a disagreement rather than as a\n"
                f"plausible label. Here **{agree} of {len(km)}** agree.\n")
        else:
            fh.write("No probe issued an ioctl on `/dev/nvidia-modeset` in this run.\n")

        fh.write("\n## Libraries that are not staged into the guest\n\n"
                 "These are in the host driver payload and never reach the guest, so no\n"
                 "trace of them can exist. They are carried at library granularity: for a\n"
                 "feature guarantee, what is absent is the line that decides it.\n\n"
                 "| library | where it is | status |\n|---|---|---|\n")
        for l in inv.get("notstaged", []):
            fh.write(f"| `{l[0]}` | {l[1]} | `not-staged` |\n")
        if not inv.get("notstaged"):
            fh.write("| &mdash; | the inventories agree, which would be surprising | |\n")

        if sizes.failed:
            fh.write("\n## Sizes that could not be compiled\n\n")
            for k, v in sorted(sizes.failed.items()):
                fh.write(f"- `{k}`: {v}\n")


def write_matrix(outdir, driver, prov, rows, probes, inv, guest, guestpath):
    by_probe = collections.defaultdict(collections.Counter)
    for r in rows:
        for p in r["seen_in"]:
            by_probe[p][r["status"]] += 1

    with (outdir / f"MATRIX-{driver}.md").open("w") as fh:
        md_head(fh, prov, f"Coverage matrix, driver {driver}",
                "scripts/ioctl-matrix.sh catalog")
        fh.write("Probes against status. This is the compatibility statement: a probe\n"
                 "with no `missing` commands is one whose feature path the backend can\n"
                 "carry today. Whether it DID is a second column, and the two are not\n"
                 "the same claim -- `predicted-green` is read off the descriptor table,\n"
                 "`guest-validated` is a run inside a VM whose signature set and status\n"
                 "fingerprint matched the native trace. A probe with `missing` commands\n"
                 "names the work.\n\n")
        if guest:
            fh.write(f"Guest evidence: `{guestpath}`, "
                     + ", ".join(f"{v} {k}" for k, v in sorted(
                         collections.Counter(g["verdict"] for g in guest.values()).items()))
                     + ".\n\n")
        else:
            fh.write(f"**No guest evidence exists** (`{guestpath}`), so every verdict\n"
                     "below is a prediction and says so. `scripts/ioctl-matrix.sh guest`\n"
                     "is what turns one into the other.\n\n")
        fh.write("| probe | group | result | guest | ioctls | catalogued | missing | passthrough | governed | NVKMS | libraries seen |\n")
        fh.write("|---|---|---|---|---:|---:|---:|---:|---:|---:|---|\n")
        notes = []
        for pr in probes:
            c = by_probe.get(pr["probe"], collections.Counter())
            miss = c.get("missing", 0)
            gv = (c.get("implemented-unverified", 0) + c.get("implemented-verified", 0)
                  + c.get("implemented-verified-mediated", 0))
            cat = miss + gv + c.get("passthrough", 0) + c.get("not-governed", 0)
            if passed(pr):
                verdict = "predicted-green" if miss == 0 else f"{miss} missing"
                if "attempt" in pr["result"]:
                    verdict += " *"
                    notes.append(f"`{pr['probe']}`: {pr['result']}")
            else:
                # The full reason is a paragraph for the ungated and blocked
                # rows. A table cell holds the first sentence; the rest goes
                # under the table, where it can be read.
                verdict = pr["result"].split(".")[0]
                if verdict != pr["result"].rstrip("."):
                    verdict += " *"
                    notes.append(f"`{pr['probe']}`: {pr['result']}")
            # The guest column. A probe with no guest row is not "green in
            # the guest" and not "broken in the guest" -- it is unmeasured
            # there, and the cell says so.
            ge = (guest or {}).get(pr["probe"])
            if ge is None:
                gcell = "not run"
            elif ge["verdict"] == "guest-validated":
                gcell = "**guest-validated**"
            else:
                gcell = f"{ge['verdict']} *"
                notes.append(f"`{pr['probe']}` in the guest: "
                             + "; ".join(f["detail"] for f in ge["findings"][:4])
                             + (" ..." if len(ge["findings"]) > 4 else ""))
            fh.write("| `{p}` | {g} | {v} | {gc} | {i} | {s} | {m} | {pt} | {gv} | {ms} | {libs} |\n".format(
                p=pr["probe"], g=pr["group"], v=verdict, gc=gcell,
                i=pr["ioctls"] or "&mdash;", s=cat or "&mdash;",
                m=miss, pt=c.get("passthrough", 0), gv=gv,
                ms=pr["modeset"] or "&mdash;",
                libs=(pr["libs_seen"] or "&mdash;").replace(" ", "<br>")))
        if notes:
            fh.write("\n")
            for n in notes:
                fh.write(f"- \\* {n}\n")
        fh.write(
            "\n`catalogued` is the number of signatures from this probe that reached\n"
            "the catalogue, which is smaller than the raw count in its trace: the\n"
            "tracer's `sub` column carries a runtime HANDLE for NV_ESC_RM_MAP_MEMORY,\n"
            "and those collapse to one row.\n")

        fh.write("\n## What each verdict means\n\n"
                 "| verdict | meaning |\n|---|---|\n"
                 "| `predicted-green` | every signature this probe emitted is governed or passthrough. A prediction from the descriptor table, not a gate result. |\n"
                 "| `guest-validated` (guest column) | the same probe ran in a guest, met its own criterion there, its trace passed the same counter-check, and every signature and every rm_status the native run produced came back identical. Answer BYTES are still uncompared -- that is `implemented-verified`, and it is a different claim. |\n"
                 "| `FAIL` (guest column) | it ran in a guest and something moved. The findings are under the table and in the evidence file. |\n"
                 "| `blocked` (guest column) | the guest run could not be gated or left no trace -- an unmeasured row, not a passing one. |\n"
                 "| `not run` (guest column) | this sweep did not run it in a guest. |\n"
                 "| `N missing` | N signatures have no entry that could carry them. TASKS groups them. |\n"
                 "| `declared-unsupported: no workload exists` | a standing decision -- there is nothing to run, and there will not be |\n"
                 "| `declared-unsupported: workload not procurable in this environment` | an invitation: a human with the SDK or the right host can turn this row green |\n"
                 "| `blocked: needs kernel-side trace point` | measurable in principle, not by any instrument in this tree |\n"
                 "| `FAIL: ...` | the probe ran and did not meet its own criterion, or failed the strace gate. Its signatures are NOT in the catalogue. |\n\n")

        fh.write("## Libraries with no measurement\n\n"
                 "| library | why |\n|---|---|\n")
        for l in inv.get("notstaged", []):
            fh.write(f"| `{l[0]}` | not staged into the guest ({l[1]}) |\n")
        for pr in probes:
            if pr["result"].startswith(("declared-unsupported", "blocked")):
                fh.write(f"| {pr['libs'].replace(' ', ', ')} | `{pr['probe']}`: {pr['result']} |\n")


def write_tasks(outdir, driver, prov, rows, probes, inv, ev, evpath):
    missing = [r for r in rows if r["status"] == "missing"]
    # One task per GROUP, not per number: the work is shaped by the probe
    # that validates it and by the mediation the commands share, and a task
    # per number would be a list of forty tickets nobody can schedule.
    groups = collections.defaultdict(list)
    for r in missing:
        key = (r["seen_in"][0] if r["seen_in"] else "?", flagstr(r))
        groups[key].append(r)

    with (outdir / f"TASKS-{driver}.md").open("w") as fh:
        md_head(fh, prov, f"Implementation tasks, driver {driver}",
                "scripts/ioctl-matrix.sh catalog")
        fh.write("Generated from the catalogue. One task per missing-command GROUP,\n"
                 "clustered by the probe that validates it and by the mediation the\n"
                 "commands share -- a task per number would be a list nobody can\n"
                 "schedule, and commands with the same mediation are one piece of work.\n\n"
                 "Every task's validation criterion is the same shape, and it is the one\n"
                 "this repository already trusts: run the probe in a guest and against\n"
                 "the host natively, and the status diff between the two traces must be\n"
                 "**0**. That is a necessary condition and not a sufficient one -- see\n"
                 "the standing task at the end.\n\n")

        if not groups:
            fh.write("No missing commands in this run: every signature the probes\n"
                     "emitted is governed or passthrough.\n\n")
        n = 0
        for (probe, flags), rs in sorted(groups.items(), key=lambda kv: -len(kv[1])):
            n += 1
            fh.write(f"## Task {n}: {len(rs)} command(s) from `{probe}`, mediation `{flags}`\n\n")
            fh.write("| device | nr | sub | name | params | size |\n|---|---|---|---|---|---:|\n")
            for r in rs:
                fh.write(f"| {r['device']} | `{r['nr']}` | `{r['sub']}` | `{r['name']}` | "
                         f"`{r['params_struct'] or '?'}` | "
                         f"{r['params_size'] if r['params_size'] is not None else '?'} |\n")
            fh.write(f"\n- **Mediation:** `{flags}`\n")
            if "size-table" in flags:
                fh.write("  - RM_ALLOC is not self-describing. Each hClass needs a size entry;\n"
                         "    a guessed size is an out-of-bounds read in the host driver's\n"
                         "    `copy_from_user`, while a clean ENOTSUP is an understood error.\n")
            if "fd-field" in flags:
                fh.write("  - A process-local fd travels in the params and has to be translated\n"
                         "    to the host's. Four bytes for a control (`NvS32`), eight for an\n"
                         "    alloc (`NvP64`) -- writing the wrong width overwrites the neighbour.\n")
            if "second-level" in flags:
                fh.write("  - Second-level pointers: the pointed-to buffer holds further\n"
                         "    pointers. This is the expensive case and the one the nested\n"
                         "    descriptor table exists for.\n")
            if "process-local-va" in flags:
                fh.write("  - A user address range: pages have to be pinned and described as\n"
                         "    GPA runs, and the pin has to be charged against a quota.\n")
            fh.write(f"- **Validated by:** `probe/matrix/{probe}.sh`\n")
            fh.write("- **Criterion:** the probe passes in a guest, its own `CRITERION:` line\n"
                     "  holds, and `sig()`-keyed status diff against the native trace is 0.\n\n")

        # ---- tasks the catalogue produces that are not missing commands ----
        # Everything below is derived from the same rows. None of it is a
        # judgement typed in by hand: each one is a query over the
        # catalogue that came back non-empty.
        leaky = [r for r in rows if r["status"] == "passthrough"
                 and any(f != "none" for f in r["flags"])]
        if leaky:
            n += 1
            fh.write(f"## Task {n}: {len(leaky)} forwarded command(s) carry something "
                     "that is not a plain value\n\n")
            fh.write("| device | nr | sub | name | params | flags | description |\n"
                     "|---|---|---|---|---|---|---|\n")
            for r in leaky:
                fh.write(f"| {r['device']} | `{r['nr']}` | `{r['sub']}` | `{r['name']}` | "
                         f"`{r['params_struct'] or '?'}` | {flagstr(r)} | "
                         f"{(r['description'] or '&mdash;')} |\n")
            fh.write(
                "\nThese are forwarded verbatim -- they are not on any mediation list --\n"
                "and their parameter struct holds a pointer or a file descriptor. A\n"
                "pointer forwarded verbatim is a GUEST address handed to the host\n"
                "driver. It may be harmless: a deprecated field the driver never reads\n"
                "is still a pointer in the struct, and the scan cannot tell those apart.\n"
                "It is worth one look each, and it is exactly the shape of defect that\n"
                "produces an answer that looks valid.\n\n"
                "- **Validated by:** the probes named in the catalogue rows\n"
                "- **Criterion:** either the field is shown to be unread (and the row\n"
                "  gets a note saying so), or the command joins the mediation table and\n"
                "  the status diff against the native trace stays 0.\n\n")

        ms = [pr for pr in probes if (pr["modeset"] or "0") not in ("", "0")]
        kmds = [r for r in rows if r["device"] == "modeset"]
        # A command counts as DONE when it has a name out of nvkms-api.h AND
        # the compiled size of its params struct matches every params size
        # the tracer measured for it. Naming without that check would be a
        # label; the check is what makes it a decode. OPEN-QUESTIONS 64.
        unnamed = [r for r in kmds
                   if not (r["name"] or "").startswith("NVKMS_IOCTL_")
                   or r["params_size"] is None
                   or any(hexint(o) != r["params_size"]
                          for o in r["observed_params_size"])]
        if ms and unnamed:
            n += 1
            total = sum(int(pr["modeset"]) for pr in ms)
            fh.write(f"## Task {n}: {total} ioctls on /dev/nvidia-modeset, "
                     f"{len(unnamed)} of {len(kmds)} command(s) still unnamed\n\n")
            fh.write("| probe | calls |\n|---|---:|\n")
            for pr in sorted(ms, key=lambda x: -int(x["modeset"])):
                fh.write(f"| `{pr['probe']}` | {pr['modeset']} |\n")
            fh.write(
                f"\nThe node is traced since 2026-08-20 and gated against strace like "
                f"every\nother, so the count above is the tracer's own and the "
                f"{len(kmds)} command(s)\nbehind it are catalogue rows. What is left "
                "is the second half of the work:\n\n"
                "The NVKMS decoder EXISTS (`probe/python/nvkmsdecode.py`) and names "
                "the\nrest, so what is listed here is the remainder it cannot account "
                "for:\n\n")
            for r in sorted(unnamed, key=lambda x: -x["calls"]):
                why = (r["description"] or "").split("Not named: ")[-1]
                fh.write(f"- `{r['sub']}`, {r['calls']} call(s): {why}\n")
            fh.write(
                "\n- **Criterion:** every command in the catalogue's NVKMS section "
                "carries a\n  name and a params struct out of `nvkms-api.h`, the way "
                "an RM_CONTROL row\n  carries one out of `ctrl*.h`, AND the compiled "
                "size of that struct equals\n  every params size the tracer measured "
                "for the command. A name that fails\n  the second half is a label, not "
                "a decode.\n\n")

        ns = inv.get("notstaged", [])
        if ns:
            n += 1
            fh.write(f"## Task {n}: decide {len(ns)} staging question(s)\n\n")
            fh.write("| library | where it is |\n|---|---|\n")
            for l in ns:
                fh.write(f"| `{l[0]}` | {l[1]} |\n")
            only32 = sum(1 for l in ns if "32-bit only" in l[1])
            fh.write(
                "\nEach is a decision, not a defect: the host driver ships it and the "
                "guest\nnever gets it."
                # The trailing space belongs to the sentence that may not be
                # written: without it the paragraph ended in "gets it. " and
                # ran straight into the criterion with no blank line, which
                # is what an empty branch looks like in the artefact.
                + (f" {only32} of them are staged for 32-bit clients and for "
                   "nobody else,\nso a 64-bit client looks for the name and does not "
                   "find it -- the case most\nlikely to be an oversight rather than a "
                   "choice.\n\n" if only32 else "\n\n")
                + "- **Criterion:** each library either enters a staging array in\n"
                "  `scripts/lib/provision.sh` with a probe that exercises it, or gets a "
                "line\n  saying why it is deliberately absent. Either way the row stops "
                "being open.\n\n")

        blocked = [pr for pr in probes if pr["result"].startswith(("declared-unsupported",
                                                                   "blocked", "ungated"))]
        if blocked:
            n += 1
            fh.write(f"## Task {n}: {len(blocked)} library group(s) with no measurement\n\n")
            fh.write("| probe | libraries | reason |\n|---|---|---|\n")
            for pr in blocked:
                fh.write(f"| `{pr['probe']}` | {pr['libs'].replace(' ', ', ')} | "
                         f"{pr['result'].split('.')[0]} |\n")
            fh.write(
                "\nRead the reasons closely, because they are not the same kind of "
                "thing.\n`no workload exists` is a standing decision and needs no "
                "follow-up.\n`workload not procurable in this environment` is an "
                "invitation: somebody\nwith the SDK download or the right host turns "
                "that row green without\nwriting any code here. `blocked: needs "
                "kernel-side trace point` and\n`ungated` are one piece of work -- an "
                "instrument on the kernel side would\nsettle the 32-bit set, the NVKMS "
                "axis and the debugger probe together.\n\n")

        fh.write("## Standing task: finish the differential answer-verification harness\n\n")
        fh.write(
            "The gates compare status codes and workload results. What they cannot\n"
            "see is the failure class this project has a name for -- \"an answer that\n"
            "looks valid and is wrong\" (number 32), which is what number 44's crash\n"
            "turned out to be: a returned object that a NULL check waved through and\n"
            "whose leading fields were never filled. **A status comparison cannot see\n"
            "that. Only the bytes can.** That is OPEN-QUESTIONS number 50.\n\n")
        if not ev:
            fh.write(
                "Nothing compares those bytes yet. What it has to do:\n\n"
                "1. Run the same probe twice, guest and native, with the answers\n"
                "   recorded per call. The tracer's `ctrlout` line already dumps the\n"
                "   first bytes of a control's answer; `LEA_DEBUG=2` logs every\n"
                "   forwarded one.\n"
                "2. Compare the answer bytes per signature, with the fields that are\n"
                "   ALLOWED to differ derived rather than declared -- handles, gpuIds\n"
                "   and addresses are translated on purpose, and a harness that\n"
                "   flagged them would cry wolf on every call.\n"
                f"3. Emit `{evpath}` mapping signature -> verified-against-native.\n\n"
                "This pipeline reads that file the moment it exists. It is never\n"
                "written by hand: a hand-written verification record is not evidence.\n")
        else:
            nver = sum(1 for r in rows if r["status"] == "implemented-verified")
            fh.write(
                f"**A first slice exists** (`scripts/ioctl-matrix.sh verify`, "
                f"`{evpath}`): {len(ev.get('verified', []))} signature(s) had the first\n"
                "bytes of their answer compared against a native run, call by call and\n"
                "word by word, with a mask derived from the two traces rather than\n"
                f"declared. {nver} of them are `implemented-verified`.\n\n"
                "What is left is the reason that number is what it is, and it is two\n"
                "specific pieces of work rather than a standing wish:\n\n"
                "1. **An answer dump for allocations and UVM.** `ctrlout` covers 0x2xx\n"
                "   and 0x2080xxxx controls only, and the governed class mostly lives in\n"
                "   the two places a size is not self-describing -- RM_ALLOC classes and\n"
                "   UVM commands. They are out of reach of the comparison entirely.\n"
                "2. **A field mask the mediation declares itself.** The governed controls\n"
                "   the slice does reach are the ones the backend answers ITSELF, and\n"
                "   those differ from the native answer on purpose. Byte equality is the\n"
                "   wrong test for them; \"differs in exactly the fields the mediation\n"
                "   rewrites, and nowhere else\" is the right one, and it needs each\n"
                "   mediated command to name its own fields.\n\n"
                "The evidence file's `not_verified` list carries every one of them with\n"
                "the catalogue's own words about why it is mediated, so it reads as a\n"
                "work list rather than as a complaint.\n")


if __name__ == "__main__":
    main()
