#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""Compare the ANSWER BYTES of forwarded controls, guest against native.

Called by ``scripts/ioctl-matrix.sh verify``; not meant to be run by hand.

This is the first slice of the differential harness that OPEN-QUESTIONS
number 50 asks for. Everything else in this tree compares status codes and
workload results, and the failure class those cannot see has a name here --
"an answer that looks valid and is wrong" (number 32), which is what number
44's crash turned out to be.

WHAT IT COMPARES, AND WHAT THAT IS WORTH. The tracer already dumps the
first 32 bytes of the params buffer AFTER the call for root-client (0x2xx)
and subdevice (0x2080xxxx) controls -- the ``ctrlout`` line in
``crates/nvrm-trace/src/log.rs``, which exists precisely so an enumeration
answer can be diffed native against guest. Nothing new is logged here: this
reads the traces the two phases already took. The claim it can therefore
support is exact and deliberately narrow:

    the first 32 bytes of this control's answer are the same bytes in a
    guest as they are natively, in every call the two runs made.

Not "the answer is correct" -- 32 bytes of a 384-byte answer is 32 bytes.
The evidence file records the extent per signature so that nothing
downstream can round it up.

THE MASK IS DERIVED, NOT DECLARED. Handles, gpuIds and addresses are
translated on purpose, so a comparison that flagged them would cry wolf on
every call; that is the hard half of number 50. A hand-written list of
"fields that may differ" would be a table nobody can check, and this
pipeline refuses to have those. So each masked word has to prove itself out
of the two traces:

  * gpuId -- the word equals THIS side's gpu_id, which each trace states in
    its own ``cardinfo`` line (host 0x2d00, guest 0x5 on this rig). A word
    that is the host's id natively and the guest's id in the guest is not a
    difference, it is the translation working, and it is the only kind of
    "difference" that is positive evidence.
  * handle -- the word is an RM handle that THIS side allocated, i.e. it
    appears as one in this trace's own allocation lines.
  * nested pointer -- the word is part of a pointer field that the
    DESCRIPTOR TABLE itself declares for this command. The table is dumped
    beside the traces (``tables.txt``), so the mask comes from the same
    stream the guest module was handed and covers exactly the offsets the
    mediation walks. A pointer into the caller's address space is a
    different number on the two sides by construction.

Anything else that differs is a MISMATCH and is reported. A signature with
one unexplained differing word is not verified, however small the word.

WHAT IS STILL REPORTED THAT PROBABLY SHOULD NOT BE. Some answers are not
stable between two runs of the same binary on the same machine -- a
timestamp (TIMER_GET_GPU_CPU_TIME_CORRELATION_INFO), a counter
(BUS_GET_PEX_UTIL_COUNTERS), the current P-state, a PID. Byte equality is
the wrong test for those, and nothing here can tell them from a real
difference, because the trace of ONE run cannot say which of its words
would have moved in a second one. The honest fix is a control test: the
same probe traced twice NATIVELY, where every word that differs native
against native is unstable and can be evidence for nothing. Until that
exists they stay in `not_verified`, which is the safe direction to be
wrong in.
"""

import argparse
import collections
import json
import pathlib
import sys

# The one reader of a trace, whatever format it is in.
import traceread
# The key normalisation lives in ONE place, for the reason guestdiff imports
# it too: a second copy that drifted would key the evidence differently from
# the catalogue it is judged against.
from ioctlmatrix import MAP_MEMORY_NR

# The three things this file reads out of a trace -- the answer dumps, the
# card's gpuId and the handles this side allocated -- used to be three
# regexes over raw text. They are fields of a record now, read through
# `traceread`, because a regex anchored on `^ctrlout\t` finds nothing at all
# in a JSONL trace and would have reported "no answers to compare" rather
# than an error.
#
# `handles_of` keeps the field-name pattern the old HANDLE regex had
# (`h[A-Z][A-Za-z]*`), which also matches `hClass` -- a class number, not a
# handle. That over-inclusion is deliberately preserved here: it is the
# behaviour every number in the current baseline was measured under, and
# narrowing it is a change to the MASK, which belongs in its own run with
# its own re-measurement.


def read_declared_pointers(tables):
    """cmd -> the byte offsets of every pointer the descriptor table declares
    inside that command's params, read out of the stream the guest module was
    handed rather than out of a second list here.

    Row shapes are `table::expect_dump()`'s own:
        ctrl   <cmd> <first> <count> <flags> <fd_off>
        nested <ptr_off> <len_kind> <len_off> <elem>
    where a control's `first`/`count` index into the nested list.
    """
    ctrls, nested = [], []
    if not tables.is_file():
        return {}
    for ln in tables.read_text(errors="replace").splitlines():
        f = ln.split()
        if f[:1] == ["ctrl"] and len(f) >= 4:
            ctrls.append((int(f[1]), int(f[2]), int(f[3])))
        elif f[:1] == ["nested"] and len(f) >= 2:
            nested.append(int(f[1]))
    out = {}
    for cmd, first, count in ctrls:
        if count and first + count <= len(nested):
            out[f"{cmd:#x}"] = [nested[i] for i in range(first, first + count)]
    return out


def read_control(ndir, probes):
    """THE CONTROL TEST, second-native-run variant: which words of which
    signature are not constant between two native runs of the same probe.

    This is the variant OPEN-QUESTIONS number 55 asked for and the one the
    within-trace test below could not be. Call i of run A is the SAME
    QUESTION as call i of run B -- same binary, same machine, same
    arguments, minutes apart -- so a word that differs is volatile and
    nothing else. The cheap variant compares the several calls one trace
    made of one command, and cannot tell "this value moved" from "this was a
    different question": GPU_GET_INFO_V2, GR_GET_INFO and FB_GET_INFO are
    index lists whose successive calls ask for different indices, and most
    of what that variant flagged was of that shape.

    The control trace is written by `scripts/ioctl-matrix.sh trace` into
    `<traces>/control/`. It is not gated and not counted -- it measures
    nothing about the surface, so a short one costs precision and can never
    fail a probe.

    Returns the same shape as the within-trace test, so the caller cannot
    tell which produced it: signature -> {"repeats", "unstable"}.
    """
    cdir = pathlib.Path(ndir) / "control"
    rep = collections.defaultdict(int)
    unstable = collections.defaultdict(set)
    for p in probes:
        a, b = traceread.trace_file(ndir, p), traceread.trace_file(cdir, p)
        if not pathlib.Path(b).is_file():
            continue
        ra, rb = read_side(a), read_side(b)
        if not ra or not rb:
            continue
        for sig in set(ra["calls"]) & set(rb["calls"]):
            ca, cb = ra["calls"][sig], rb["calls"][sig]
            # Pair by index, over as many calls as BOTH runs made. Unequal
            # counts are not a finding here -- this is a control, and a
            # workload that allocated one surface more in one run says
            # nothing about whether a word is volatile.
            n = min(len(ca), len(cb))
            rep[sig] = max(rep[sig], n)
            # ONLY WORDS BOTH RUNS ACTUALLY WROTE. A word neither run wrote
            # holds the caller's leftovers, and two runs of one program have
            # different leftovers -- so comparing them reports the stack as
            # volatile. That is not a harmless over-report here: `unstable`
            # is pooled per SIGNATURE across probes, so one probe's garbage
            # would mark the word volatile for every probe and suppress a
            # real finding elsewhere. Measured 2026-08-21: it did exactly
            # that to GR_GET_CAPS_V2, which nvdec answers and nvenc leaves
            # untouched.
            wra = written_words(ra["asked"].get(sig, []), ca)
            wrb = written_words(rb["asked"].get(sig, []), cb)
            for i in range(n):
                wa, wb = words_of(ca[i]["bytes"]), words_of(cb[i]["bytes"])
                for j, (x, y) in enumerate(zip(wa, wb)):
                    off = j * 4
                    if wra is not None and off not in wra[i]:
                        continue
                    if wrb is not None and off not in wrb[i]:
                        continue
                    if x != y:
                        unstable[sig].add(off)
    return {c: {"repeats": rep[c], "unstable": sorted(unstable.get(c, ()))}
            for c in rep}


def read_stability(paths):
    """THE CONTROL TEST, within-trace variant: which words of which command
    are not constant across the calls ONE NATIVE RUN made.

    A word that differs native-against-native can be evidence for nothing.
    Without this, a timer, a counter or a free-memory figure is reported as
    a native/guest MISMATCH -- a potential defect -- and the real findings
    sit in a list beside them where nobody can see them.

    Returns cmd -> {"repeats": most calls seen in one trace,
                    "unstable": {word offsets that moved}}.

    WHAT THIS VARIANT CANNOT TELL APART, and it is the honest limit of doing
    it without a second run: two calls of the same command inside one trace
    are not necessarily the same QUESTION. `GPU_GET_INFO_V2`, `GR_GET_INFO`
    and `FB_GET_INFO` are index lists -- successive calls ask for different
    indices, so their answers differ because the question differed and not
    because anything moved. Measured 2026-08-20: of 19 verified signatures
    with a word flagged here, most are of that shape.

    That is why the flag is used in ONE DIRECTION ONLY. It can stop a
    difference from being called a defect; it can never turn one into a
    pass, and it never promotes anything. The variant that does not have
    this weakness is the second native trace of the same probe, where call i
    of one run is the same question as call i of the other. That one exists
    now -- `read_control` above -- and the two are UNIONED rather than one
    replacing the other, because neither proves stability: each sees
    volatility the other cannot, and "this word moved" is a positive
    observation where "this word did not move" is the absence of one.
    """
    rep = collections.defaultdict(int)
    unstable = collections.defaultdict(set)
    for path in paths:
        if not path.is_file():
            continue
        calls = collections.defaultdict(list)
        for r in traceread.read(path):
            # The OUT sample only. The tracer takes both now (the `in` one
            # is what number 60 needs), and pooling them would compare a
            # command's question with its answer and call the difference
            # instability.
            if r.get("phase") == "in":
                continue
            if r["t"] == "ctrlout":
                calls[f"ctl 0x2a {r['cmd']}"].append(list(bytes.fromhex(r["dump"])))
            elif r["t"] == "uvmout":
                calls[f"uvm {r['nr']} -"].append(list(bytes.fromhex(r["dump"])))
            elif r["t"] == "allocout":
                calls[f"{r['dev']} 0x2b {r['class']}"].append(
                    list(bytes.fromhex(r["dump"])))
            elif r["t"] == "escout":
                sub = r.get("sub")
                if sub is None or r["nr"] == MAP_MEMORY_NR:
                    sub = "-"
                calls[f"{r['dev']} {r['nr']} {sub}"].append(
                    list(bytes.fromhex(r["dump"])))
        for cmd, cs in calls.items():
            rep[cmd] = max(rep[cmd], len(cs))
            if len(cs) < 2:
                continue
            n = min(len(c) for c in cs)
            for i in range(0, n - n % 4, 4):
                if any(c[i:i + 4] != cs[0][i:i + 4] for c in cs[1:]):
                    unstable[cmd].add(i)
    return {c: {"repeats": rep[c], "unstable": sorted(unstable.get(c, ()))}
            for c in rep}


def read_mediation(path):
    """signature -> [ {off, len, stride, count, kind, field}, ... ] out of the
    manifest the trace phase wrote beside ``tables.txt``.

    THE FOURTH MASK, and its semantics are INVERTED against the other three.
    The first three answer "this word may differ, here is the proof". This
    one answers "this command is MEDIATED, so it MUST differ inside these
    fields and nowhere else". A mediated command that matches byte for byte
    outside them is a stronger statement than one that merely matches, and a
    mediated command that differs OUTSIDE them is a finding on a row where
    byte equality could previously only shrug -- `GPU_GET_NAME_STRING`
    answering `NVID` natively and `Lean` in a guest was reported as a
    mismatch, which is the mediation working exactly as designed.

    Derived like the rest: `crates/nvrm-abi/src/mediate.rs` is the code the
    backend and the guest module rewrite FROM, and the manifest is generated
    out of it. It is not a list of bytes to ignore maintained beside the
    code.
    """
    out = collections.defaultdict(list)
    if not path.is_file():
        return {}
    for ln in path.read_text(errors="replace").splitlines():
        f = ln.split()
        if f[:1] != ["mediated"]:
            continue
        # `mediated <device> <nr> <sub> <off> <len> <stride> <count> <kind>
        # <field>`: the first three columns are the catalogue's own signature
        # key, so this joins against the evidence without either side
        # knowing anything about the other's namespace. A manifest written
        # before the escape namespace existed had `<cmd>` where the
        # signature is now, and is skipped rather than misread -- an old
        # trace directory produces weaker wording and never a wrong verdict.
        if len(f) < 10:
            continue
        out[" ".join(f[1:4])].append({
            "off": int(f[4]), "len": int(f[5]), "stride": int(f[6]),
            "count": int(f[7]), "kind": f[8], "field": " ".join(f[9:]),
        })
    return dict(out)


def mediated_hit(recs, off):
    """The manifest record covering byte `off`, or None.

    An array record covers element starts only: `stride`-spaced windows of
    `len` bytes. The GAP between elements is NOT covered, deliberately --
    NV2080_CTRL_FB_INFO is {index, data} and only `data` is rewritten, so a
    changed `index` has to stay visible. Masking a whole array because part
    of it moves is the looseness this mask exists to avoid.
    """
    for r in recs or ():
        if r["count"] > 1 and r["stride"] > 0:
            if off < r["off"]:
                continue
            k, rem = divmod(off - r["off"], r["stride"])
            if k < r["count"] and rem < r["len"]:
                return r
        elif r["off"] <= off < r["off"] + r["len"]:
            return r
    return None


def read_fieldmap(path):
    """cmd -> {"struct": name, "size": n, "members": [...]}, out of the
    compiled field map the trace phase wrote beside ``tables.txt``.

    Every offset in it came from ``offsetof`` in a translation unit that
    included the pinned vendor header; nothing was parsed. That is what
    makes it safe to turn "word 2" into a field NAME here -- the alternative,
    counting members in header text, is wrong the first time NVIDIA wraps
    one in ``NV_DECLARE_ALIGNED``, which is every NvP64 in these headers.

    Absent map = no names, and every message falls back to the offset. A
    missing artefact must degrade the wording, never the verdict.
    """
    if not path.is_file():
        return {}
    js = json.loads(path.read_text())
    structs = js.get("structs", {})
    out = {}
    for cmd, c in js.get("commands", {}).get("ctrl", {}).items():
        st = structs.get(c.get("params_struct", ""))
        if st:
            out[cmd] = {"struct": c["params_struct"], "size": st["size"],
                        "members": st["members"],
                        "params_name_from": c.get("params_name_from", "")}
    return out


def field_at(fm, off):
    """The member covering byte `off`. Innermost wins, so a big array does
    not shadow the element the offset is actually in."""
    best = None
    for m in (fm or {}).get("members", ()):
        if m["offset"] <= off < m["offset"] + max(m["size"], 1):
            if best is None or m["size"] < best["size"]:
                best = m
    return best


def name_at(fm, off):
    """`biosInfoList (NvP64) at offset 8` -- or just the offset when the
    field map cannot say. Never a guess."""
    m = field_at(fm, off)
    if not m:
        return f"offset {off}"
    where = "" if m["offset"] == off else f", byte {off - m['offset']} of it"
    return f"{m['name']} ({m['type']}) at offset {m['offset']}{where}"


def read_side(path):
    """One side of the comparison: its answers, its gpuId, its handles."""
    calls = collections.defaultdict(list)
    asked = collections.defaultdict(list)
    gpuids, handles = set(), set()
    if not pathlib.Path(path).is_file():
        return None
    for r in traceread.read(path):
        # KEYED BY THE SIGNATURE THE CATALOGUE USES, "<device> <nr> <sub>",
        # so that a control and a UVM command are the same kind of thing to
        # everything downstream. They are: both are one entry of the surface
        # with an answer that either survives the boundary or does not.
        sig = None
        if r["t"] == "ctrlout":
            sig, status = f"ctl 0x2a {r['cmd']}", r["status"]
        elif r["t"] == "uvmout":
            # UVM has no status field in the ioctl. Every UVM command carries
            # its rmStatus INSIDE the parameter block, so it is compared as
            # part of the answer rather than beside it.
            sig, status = f"uvm {r['nr']} -", "-"
        elif r["t"] == "escout":
            # The escape's own parameter block. `sub` is None for the escapes
            # that have no second dispatch level, and the catalogue spells
            # that "-".
            #
            # NORMALISE THE KEY THE WAY THE CATALOGUE DOES, by importing the
            # rule rather than repeating it. For NV_ESC_RM_MAP_MEMORY the
            # tracer's `sub` is hMemory -- a runtime HANDLE, put there so
            # mappings can be matched to their allocations. Keyed on it, one
            # escape becomes hundreds of rows that are all the same call, and
            # none of them would match anything in the catalogue.
            sub = r.get("sub")
            if sub is None or r["nr"] == MAP_MEMORY_NR:
                sub = "-"
            sig, status = f"{r['dev']} {r['nr']} {sub}", "-"
        elif r["t"] == "allocout":
            # RM_ALLOC. The sub-dispatch is the CLASS, and the device
            # matters: the same class allocated through the control node and
            # through a GPU node are two rows of the catalogue. The escape's
            # own status travels on the nvos64 line, not here; what this
            # compares is the parameter block RM wrote back.
            sig, status = f"{r['dev']} 0x2b {r['class']}", "-"
        if sig is not None:
            # The params buffer AND what its NvP64 points at, concatenated.
            # For a list control the params are the QUESTION -- a count and a
            # pointer -- and the ANSWER is behind the pointer, so comparing
            # the params alone compares the question. `params_bytes` records
            # where one ends and the other begins, so a finding can say which
            # of the two an offset is in.
            body = bytes.fromhex(r["dump"])
            nested = bytes.fromhex(r.get("nested") or "")
            # The two samples go into two dicts. `calls` is the ANSWER, and
            # is what every existing comparison is about; `asked` is the
            # buffer as the caller handed it over, which is the only thing
            # that can tell an OUT pointer the boundary dropped from an IN
            # pointer the caller never supplied (number 60).
            where = asked if r.get("phase") == "in" else calls
            where[sig].append({
                "len": int(r["len"]) + int(r.get("nlen") or 0),
                "status": status,
                "bytes": list(body + nested),
                "params_bytes": len(body),
                "has_nested": bool(nested),
            })
            continue
        if r["t"] == "cardinfo":
            if r.get("gpu_id"):
                gpuids.add(int(r["gpu_id"], 16))
            continue
        handles.update(traceread.handles_of(r))
    return {"calls": calls, "asked": asked, "gpuids": gpuids, "handles": handles}


def words_of(byts):
    """The dump as 4-byte little-endian words, which is the granularity
    every field in these structs has. A trailing partial word is compared as
    bytes and never split."""
    return [int.from_bytes(bytes(byts[i:i + 4]), "little")
            for i in range(0, len(byts) - len(byts) % 4, 4)]


def explain(nw, gw, off, ptrs, native, guest):
    """Why these two words may differ, or None if they may not."""
    if nw in native["gpuids"] and gw in guest["gpuids"]:
        return "gpuId"
    if nw in native["handles"] and gw in guest["handles"]:
        return "handle"
    # An NvP64 is eight bytes, so both of its words are covered by one offset.
    if any(p <= off < p + 8 for p in ptrs):
        return "nested-ptr"
    return None


def written_words(asked, calls):
    """Per call, the set of word offsets the driver actually WROTE.

    A word whose OUT sample equals its IN sample was left as the caller had
    it. That matters because `ctrlout` dumps a buffer, not an answer: the
    bytes past the end of what a control fills are the caller's stack or
    heap, and comparing THOSE native against guest compares two programs'
    leftovers. Measured 2026-08-21, that is exactly what the two surviving
    mismatches were -- `0x20809064` agreed perfectly in the two words it
    answers (`0x1` at 16, `0x64` at 20) and was reported as a defect over an
    address at offset 24 that neither side ever wrote.

    WHAT IT IS NOT PROOF OF, and the limit is the same shape as the
    stability test's. A driver that writes the value already there is
    indistinguishable from one that writes nothing -- most plausibly when a
    caller zeroes its buffer and the honest answer is zero. So this is used
    in ONE DIRECTION ONLY: it can stop a difference from being called a
    defect, and it can never turn one into a pass. Nothing is promoted by it.

    Returns None when the two samples cannot be paired, which makes every
    caller fall back to comparing the whole dump, as it did before.
    """
    if not asked or len(asked) != len(calls):
        return None
    out = []
    for a, c in zip(asked, calls):
        aw, cw = words_of(a["bytes"]), words_of(c["bytes"])
        if len(aw) != len(cw):
            return None
        out.append({j * 4 for j, (x, y) in enumerate(zip(aw, cw)) if x != y})
    return out


# THE ONE DECLARED CLASSIFICATION IN THIS FILE, AND IT IS NOT A MASK.
#
# Every mask here is DERIVED -- the gpu_id out of the trace's own cardinfo
# line, the handles out of its own allocation lines, the pointer offsets out
# of the descriptor stream, the written-ness out of the before-call sample.
# That property is deliberate and this table does NOT spend it, because a mask
# and a class are different things:
#
#   a MASK says "these bytes may differ" and the signature can still be
#     VERIFIED. It is a claim about correctness.
#   this CLASS says "these bytes DO differ, and here is the category". It
#     never promotes. The signature is not verified and does not pretend to be.
#
# So the cost of being wrong here is bounded: a wrong entry moves a row from
# `mismatch` to `host_assigned` and loses an alarm. It can never make
# something count as verified that is not.
#
# WHY A DECLARATION AT ALL, when everything else is derived. For
# FIFO_GET_CHANNELLIST the differing value is a hardware channel id, and the
# only place it appears in the trace is inside the answer of the very command
# under test -- checked 2026-08-21, the channel allocation (hClass 0xc46f, 376
# bytes of answer) does not carry it. Deriving a mask from that would make the
# instrument agree with what it is measuring, which is the same circularity
# that stopped allocation dump lengths being taken from the table under test.
#
# THE CRITERION, so this cannot become a place to put anything inconvenient.
# An entry belongs here only if ALL of these hold:
#   1. the value is assigned by the HOST or by the hardware, not by the guest,
#      and a guest cannot be expected to match it;
#   2. it is STABLE on each side -- the control test has already had its say,
#      and an unstable word never reaches this check;
#   3. the difference is consistent across every probe that exercises it, not
#      occasional;
#   4. the entry names the evidence, below, in the row itself.
# A value that merely looks plausible does not qualify. If in doubt it stays
# in `mismatch`, which is the safe place for an open question.
HOST_ASSIGNED = {
    # NV0080_CTRL_CMD_FIFO_GET_CHANNELLIST. The dump behind the two NvP64s is
    # a handle followed by a channel id, 8 bytes per channel; the handle
    # matches (the handle mask covers it) and the id does not.
    #
    # Measured 2026-08-21: in nvenc the first three channels are 0x35/0x36/0x37
    # natively and 0x37/0x38/0x39 in the guest -- the native ids plus two,
    # with IDENTICAL handles -- and the first channel is 0x35 against 0x37 in
    # cuda-core, cuda-jit, nvdec, nvenc and opencl alike. A constant offset is
    # what "the host has channels of its own that the guest does not" predicts.
    ("ctl 0x2a 0x80170d", "nested", 4): (
        "the hardware channel id, which RM assigns when a channel is "
        "allocated. The host has channels of its own that the guest does not, "
        "so the ids cannot coincide: measured 2026-08-21 as the native id "
        "plus two, consistently, with the channel HANDLES identical"),
}


def host_assigned_hit(sig, off, params_bytes):
    """Is this word a declared host-assigned value? -> the reason, or None.

    `off` is into the whole dump; past the params buffer it is into what an
    NvP64 pointed at, which is the "nested" buffer here -- the same split
    `where_at` renders.
    """
    if params_bytes is not None and off >= params_bytes:
        return HOST_ASSIGNED.get((sig, "nested", off - params_bytes))
    return HOST_ASSIGNED.get((sig, "params", off))


def where_at(fm, off, params_bytes):
    """Name the field at `off`, saying which BUFFER it is in.

    Past the end of the params buffer the offset is into what an NvP64
    pointed at, and the field map has nothing to say about it -- the map
    describes the params struct, and the buffer behind the pointer is a list
    whose element type the struct does not name.
    """
    if params_bytes is not None and off >= params_bytes:
        return f"+{off - params_bytes} in the buffer behind the NvP64"
    return name_at(fm, off)


def compare_cmd(cmd, ncalls, gcalls, native, guest, ptrs=(), fm=None, med=None,
                stab=None, nasked=(), gasked=(), sig=None):
    """One signature's verdict: verified, or the reason it is not.

    Returns `(ok, why, masked)`. With a mediation manifest for this command
    the test changes shape: a differing word must fall INSIDE a manifest
    field, and `masked` records which ones actually moved. The caller then
    separates two claims that must never share a row -- bytes that matched
    outright, and bytes that differed exactly where the mediation says they
    would.
    """
    if len(ncalls) != len(gcalls):
        # NOT A MISMATCH -- NOT COMPARABLE, and the difference matters.
        #
        # A mismatch is evidence AGAINST the signature: bytes were compared
        # and they differed outside every mask. Unequal call counts are
        # evidence about NEITHER side's bytes -- call i of one run is not
        # call i of the other, so there is nothing to compare. Conflating the
        # two made a probe that merely asked once more veto the positive
        # evidence of every probe that did pair, which is how
        # GPU_GET_NAME_STRING came to be judged by nothing: it pairs in nvml,
        # cuda-core, opencl, nvdec and nvenc, and does not in gl-enum (5
        # against 4) and vk-enum (3 against 2).
        #
        # The caller keeps this apart from a mismatch. It never promotes on
        # its own: a signature that no probe could pair is judged by nothing
        # and stays out of both verified classes. Call-count differences are
        # reported where they belong -- the guest evidence file, which
        # compares signature sets and counts as its whole job.
        return "abstain", (f"{len(ncalls)} call(s) natively and {len(gcalls)} in "
                           "the guest -- not comparable, so this probe judges "
                           "nothing either way"), {}
    masked = collections.Counter()
    nwrote = written_words(nasked, ncalls)
    gwrote = written_words(gasked, gcalls)
    for i, (n, g) in enumerate(zip(ncalls, gcalls)):
        if n["status"] != g["status"]:
            return None, (f"call {i}: status {n['status']} natively, "
                          f"{g['status']} in the guest"), {}
        if n["len"] != g["len"]:
            return None, (f"call {i}: paramsSize {n['len']} natively, "
                          f"{g['len']} in the guest"), {}
        if len(n["bytes"]) != len(g["bytes"]):
            return None, f"call {i}: the two dumps are of different length", {}
        pb = n.get("params_bytes")
        nws, gws = words_of(n["bytes"]), words_of(g["bytes"])
        for j, (nw, gw) in enumerate(zip(nws, gws)):
            if nw == gw:
                continue
            off = j * 4
            # NOT WRITTEN NATIVELY: mask, ahead of everything. The other
            # masks all say something about a VALUE ("this word IS this
            # side's gpu_id"); this one says there is no value here to talk
            # about. RM left the native caller's own leftovers at this
            # offset, so there is no answer to compare the guest against,
            # whatever the guest did.
            if nwrote is not None and off not in nwrote[i]:
                masked["not-written"] += 1
                continue
            why = explain(nw, gw, off, ptrs, native, guest)
            # PRECEDENCE, and it is the whole logic of this function.
            #
            #   1. the three DERIVED masks -- each is a positive
            #      identification ("this word IS this side's gpu_id"), and a
            #      fact is not weakened by the value being volatile;
            #   2. UNSTABLE -- the word moved between two calls of one native
            #      run, so it is evidence for nothing. Not a pass and not a
            #      defect: a third answer;
            #   3. MEDIATED -- the manifest says this field may be rewritten.
            #
            # 2 before 3 deliberately. A word that is not stable cannot be
            # evidence that the mediation worked either, and the alternative
            # order would let an unstable value be reported as mediation
            # doing its job. FB_GET_INFO_V2 on an uncapped rig is exactly
            # that case: the byte that moves is HEAP_FREE.
            if why is None and stab and off in stab["unstable"]:
                return "unstable", (
                    f"call {i}, {where_at(fm, off, pb)}: {nw:#010x} natively, "
                    f"{gw:#010x} in the guest -- and this word is NOT STABLE "
                    f"between two native runs, or between two calls of one "
                    f"native run, so it is "
                    f"evidence for nothing"), {}
            if why is None and med:
                # The inverted test. Inside a manifest field this word is
                # SUPPOSED to differ, and that it does is the evidence.
                #
                # BYTE granularity, not word granularity, and that is not
                # pedantry. GET_PCI_INFO puts `bus` and `slot` in one word as
                # two NvU16; if only one of them were mediated, masking the
                # whole word would hide a real change in the other. So every
                # byte that ACTUALLY differs has to be covered -- a mediated
                # field can never shield the neighbour it shares a word with.
                nb = nw.to_bytes(4, "little")
                gb = gw.to_bytes(4, "little")
                hits = [mediated_hit(med, off + b)
                        for b in range(4) if nb[b] != gb[b]]
                if hits and all(h is not None for h in hits):
                    why = "mediated:" + hits[0]["kind"]
            if why is None:
                # A defect, and now -- and only now -- ask which KIND.
                #
                # WRITTEN-NESS IS A LABEL ON A DEFECT, NEVER A BYPASS OF THE
                # MASKS, and getting that order wrong is not academic: with
                # this test in front of the mediation check,
                # GPU_GET_NAME_STRING was reported as "the guest did not
                # answer" on its first run. It is a mediated command whose
                # guest buffer already held the mediated name from an earlier
                # call, so its before and after samples agreed -- exactly the
                # case `written_words` documents as indistinguishable. Every
                # mask gets its say first; what reaches here is a difference
                # nothing explains, and the only question left is whether the
                # guest wrote anything at all.
                if gwrote is not None and off not in gwrote[i]:
                    # HOW OFTEN, not just this once. A word the guest never
                    # writes and a word it writes on the second call but not
                    # the first are different findings, and the second is
                    # what GR_GET_CAPS_V2 turned out to be: natively answered
                    # on both calls, in the guest on the second only, with
                    # the answer byte-identical when it does come. Reporting
                    # one call index would have read as "never answered".
                    #
                    # The usual caveat on pairing applies and is why the
                    # counts are given rather than a verdict: call i of one
                    # side is call i of the other only because both runs made
                    # the same number of calls, which is all that has been
                    # checked.
                    tot = len(ncalls)
                    nn = sum(1 for c in range(tot) if off in nwrote[c])
                    gg = sum(1 for c in range(tot) if off in gwrote[c])
                    return "unwritten", (
                        f"call {i}, {where_at(fm, off, pb)}: RM wrote {nw:#010x} "
                        f"natively and the guest left the caller's own "
                        f"{gw:#010x} in place, with NV_OK on both sides. Over "
                        f"the {tot} paired call(s) of this probe, this word "
                        f"was written natively on {nn} and in the guest on "
                        f"{gg}. NOT by itself a defect -- RM declines to "
                        f"populate some answers depending on the caller's "
                        f"state -- but the two sides reached this call in "
                        f"different states"), {}
                # LAST, and only here. Everything above has had its say:
                # the derived masks, the control test, the mediation manifest
                # and the written-ness label. What reaches this point is a
                # word both sides wrote, that is stable, that no mask
                # explains -- i.e. a mismatch -- and the only question left is
                # whether it is a value the guest could ever have matched.
                ha = host_assigned_hit(sig, off, pb)
                if ha is not None:
                    return "host-assigned", (
                        f"call {i}, {where_at(fm, off, pb)}: {nw:#010x} "
                        f"natively, {gw:#010x} in the guest -- {ha}. NOT "
                        f"masked and NOT verified: the bytes differ and this "
                        f"says which category the difference is in"), {}
                extra = (" -- this command IS mediated, and this byte is in "
                         "none of the fields the mediation declares"
                         if med else "")
                return None, (f"call {i}, {where_at(fm, off, pb)}: "
                              f"{nw:#010x} natively, {gw:#010x} in the guest"
                              f"{extra}"), {}
            masked[why] += 1
        # The tail the word view does not cover, compared as bytes.
        tail = len(n["bytes"]) - len(n["bytes"]) % 4
        if n["bytes"][tail:] != g["bytes"][tail:]:
            return None, f"call {i}: the trailing bytes differ", {}
    return True, None, dict(masked)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--native", required=True)
    ap.add_argument("--guest", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--driver", required=True)
    ap.add_argument("--probes", nargs="*", default=[])
    ap.add_argument("--provenance", default="")
    a = ap.parse_args()

    ndir, gdir, outdir = (pathlib.Path(a.native), pathlib.Path(a.guest),
                          pathlib.Path(a.out))
    declared = read_declared_pointers(ndir / "tables.txt")
    fields = read_fieldmap(ndir / "fields.json")
    mediation = read_mediation(ndir / "mediation.txt")
    # Every native trace, not just the probes named on the command line: a
    # command's stability is a property of the command, and the more calls
    # the evidence rests on the fewer values are wrongly called stable.
    # TWO CONTROL TESTS, UNIONED -- never one replacing the other.
    #
    # Each finds volatility the other cannot. The second-native-run variant
    # sees a command that a single trace calls only ONCE, which the
    # within-trace variant has nothing to say about at all (that is the whole
    # `stability_unknown` class). The within-trace variant sees a value that
    # moves between two calls minutes apart inside one run, which two runs
    # taken a minute apart can easily agree about by luck -- a GPU
    # temperature reads the same in both and differs in the guest.
    #
    # Neither PROVES stability, and that asymmetry is the reason for the
    # union rather than a preference. "This word moved" is a positive
    # observation; "this word did not move" is the absence of one. Replacing
    # the cheap test with the strict one was tried first and moved five
    # signatures into MISMATCH -- a thermal reading, a work-submit token, two
    # addresses and a PCI bus number -- none of them defects, all of them
    # words the cheap test had correctly refused to treat as evidence.
    #
    # Used in one direction only, like each of them alone: it can stop a
    # difference from being called a defect and it never promotes anything.
    stability = read_stability(traceread.all_traces(ndir))
    control_seen = read_control(ndir, a.probes)
    for sig, st in control_seen.items():
        cur = stability.get(sig, {"repeats": 0, "unstable": []})
        stability[sig] = {
            "repeats": max(cur["repeats"], st["repeats"]),
            "unstable": sorted(set(cur["unstable"]) | set(st["unstable"])),
        }
    cat = {}
    catf = outdir / f"catalog-{a.driver}.json"
    if catf.is_file():
        for r in json.loads(catf.read_text()).get("signatures", []):
            cat[(r["device"], r["nr"], r["sub"])] = r

    # Per SIGNATURE and not per probe. A signature that matched under one
    # probe and mismatched under another is NOT verified: the claim is about
    # the command, and one unexplained word anywhere falsifies it. The first
    # version of this file appended to a list as it went and would have kept
    # the good half of exactly that case.
    ok_by_sig, bad_by_sig, evidence, sides, skipped = {}, {}, {}, {}, []
    abstained, unstable_by_sig, unwritten_by_sig = {}, {}, {}
    host_assigned_by_sig = {}
    behind = {}
    for p in a.probes:
        n = read_side(traceread.trace_file(ndir, p))
        g = read_side(traceread.trace_file(gdir, p))
        if not n or not g:
            skipped.append({"probe": p, "reason": "one of the two traces is missing"})
            continue
        if not g["calls"]:
            # The guest run made no control that carries an answer dump --
            # it failed before it got that far. Comparing anyway would turn
            # one failed run into sixty "cannot be paired" rows about
            # commands nothing is wrong with. The probe is named as skipped;
            # its failure is a finding in the GUEST evidence file, which is
            # where a failed guest run belongs.
            skipped.append({"probe": p,
                            "reason": "the guest run produced no answer dumps"})
            continue
        sides[p] = {
            "native_gpuid": [hex(x) for x in sorted(n["gpuids"])],
            "guest_gpuid": [hex(x) for x in sorted(g["gpuids"])],
            "native_handles": len(n["handles"]), "guest_handles": len(g["handles"]),
            "commands_with_an_answer_dump": len(n["calls"]),
        }
        for key in sorted(set(n["calls"]) | set(g["calls"])):
            # The field map, the mediation manifest and the declared-pointer
            # list are all keyed by the CONTROL command, because that is the
            # only namespace they cover. A UVM signature simply has none of
            # them, and the comparison falls back to offsets and byte
            # equality -- which is what it had for every command before the
            # masks existed.
            cmd = key.split()[-1] if key.startswith("ctl 0x2a ") else None
            fm = fields.get(cmd)
            # Keyed by the signature, so an escape that is mediated -- and
            # NV_ESC_CARD_INFO is -- gets its manifest the same way a control
            # does.
            med = mediation.get(key)
            st = stability.get(key)
            ok, why, masked = compare_cmd(key, n["calls"].get(key, []),
                                          g["calls"].get(key, []), n, g,
                                          declared.get(cmd, ()), fm, med, st,
                                          n["asked"].get(key, []),
                                          g["asked"].get(key, []), sig=key)
            row = cat.get(tuple(key.split()), {})
            if ok == "abstain":
                abstained.setdefault(key, []).append({"probe": p, "reason": why})
                continue
            if ok == "unwritten":
                # ITS OWN CLASS, and the reason is the same reason
                # `not_verified` was split three ways: this is not "the
                # answers differ". It is "the guest returned NV_OK and did
                # not answer", which a reader can act on directly and which
                # no status fingerprint can see, because both sides say
                # NV_OK. Measured 2026-08-21 on NV0080_CTRL_CMD_GR_GET_CAPS_V2.
                unwritten_by_sig.setdefault(key, {
                    "signature": key, "name": row.get("name", ""),
                    "probes": [], "reason": why,
                    "catalogue_status": row.get("status", ""),
                    "catalogue_notes": row.get("notes", []),
                })["probes"].append(p)
                continue
            if ok == "host-assigned":
                # Beside `unstable`, and for the same structural reason: not a
                # pass and not a defect, so it disqualifies the signature from
                # both verified classes without counting as evidence against
                # the boundary. What it buys is that `mismatch` goes back to
                # meaning "look at this".
                host_assigned_by_sig.setdefault(key, {
                    "signature": key, "name": row.get("name", ""),
                    "probes": [], "reason": why,
                    "catalogue_status": row.get("status", ""),
                })["probes"].append(p)
                continue
            if ok == "unstable":
                # Not a pass and not a defect. It joins neither verified class
                # and it does NOT disqualify the signature the way a mismatch
                # does -- a word that can be evidence for nothing is also not
                # evidence against.
                unstable_by_sig.setdefault(key, {
                    "signature": key, "name": row.get("name", ""),
                    "probes": [], "reason": why,
                    "unstable_words": (st or {}).get("unstable", []),
                    "native_calls_compared": (st or {}).get("repeats", 0),
                })["probes"].append(p)
                continue
            followed = any(c.get("has_nested")
                           for c in n["calls"].get(key, []))
            if ok is True and declared.get(cmd) and not followed:
                # THE ANSWER IS NOT IN WHAT WAS COMPARED.
                #
                # `ctrlout` dumps the params BUFFER. For a command whose
                # descriptor row declares an NvP64, the params buffer holds
                # the QUESTION -- a count and a pointer -- and the answer is
                # behind the pointer, which nothing dumps. Measured
                # 2026-08-21: thirteen signatures were reported `verified` on
                # "16 of 16 bytes", which reads as the whole answer and is
                # the whole question. GPU_GET_CLASSLIST, GR_GET_INFO,
                # FB_GET_INFO, BIOS_GET_INFO and GPU_GET_ENGINES are among
                # them.
                #
                # Matching on the question is worth something -- the count,
                # the flags and the shape of the request survived the
                # boundary -- but it is not the claim `verified` makes, so it
                # is a class of its own and it does not promote.
                behind.setdefault(key, {
                    "signature": key, "name": row.get("name", ""),
                    "probes": [], "params_bytes_compared": len(n["calls"][key][0]["bytes"]),
                    "pointer_offsets": sorted(declared.get(cmd, ())),
                    "reason": ("the params buffer matched on both sides, but "
                               "this command's descriptor row declares an "
                               "NvP64 and the answer is behind it -- nothing "
                               "dumps what it points at, so the answer itself "
                               "was not compared"),
                    "catalogue_status": row.get("status", ""),
                })["probes"].append(p)
                continue
            if ok:
                nb = len(n["calls"][key][0]["bytes"])
                e = ok_by_sig.setdefault(key, {
                    "name": row.get("name", ""),
                    "status_before": row.get("status", ""),
                    "probes": [], "calls_compared": 0,
                    "bytes_compared": nb,
                    "answer_size": n["calls"][key][0]["len"],
                    # WHICH FIELDS the compared bytes actually cover, by
                    # name and type out of the compiled field map. "32 of 384
                    # bytes" says how much; this says WHAT, which is the
                    # question a reader of a verification claim has.
                    "params_struct": (fm or {}).get("struct", ""),
                    "fields_covered": [
                        f"{m['name']} ({m['type']}) @{m['offset']}"
                        for m in (fm or {}).get("members", [])
                        if m["offset"] < nb],
                    "fields_not_covered": [
                        f"{m['name']} ({m['type']}) @{m['offset']}"
                        for m in (fm or {}).get("members", [])
                        if m["offset"] >= nb],
                    "words_masked": {},
                    "mediation": [
                        f"{r['field']} ({r['kind']}) @{r['off']}"
                        + (f" x{r['count']} stride {r['stride']}"
                           if r["count"] > 1 else "")
                        for r in (med or ())],
                })
                e["probes"].append(p)
                e["calls_compared"] += len(n["calls"].get(key, []))
                for k2, v2 in masked.items():
                    e["words_masked"][k2] = e["words_masked"].get(k2, 0) + v2
            else:
                # The catalogue's own words about this row travel with the
                # reason. Several of these commands are answered by the
                # backend on purpose, and a reader who has to look that up
                # elsewhere will read "not verified" as "wrong".
                b = bad_by_sig.setdefault(key, {
                    "signature": key, "name": row.get("name", ""),
                    "probes": [], "reason": why,
                    "catalogue_status": row.get("status", ""),
                    "catalogue_notes": row.get("notes", []),
                })
                b["probes"].append(p)

    # The disqualification: matched somewhere, mismatched somewhere else.
    # A signature is judged only where a probe actually compared bytes. One
    # that no probe could pair appears in `abstained` alone and is in neither
    # verified class -- unjudged, which is a third answer and not a pass.
    # Filled after the sweep, not during it: a probe that abstains may come
    # after the ones that judged, and a row written as we went would name
    # only the abstentions that happened to be seen first.
    for k, e in ok_by_sig.items():
        e["probes_not_comparable"] = [x["probe"] for x in abstained.get(k, [])]
        e["fully_paired"] = not e["probes_not_comparable"]

    # DISQUALIFICATION IS THE SAME FOR BOTH KINDS OF NEGATIVE OUTCOME, and
    # getting this wrong is how a stability classification PROMOTES.
    #
    # A signature that mismatches under one probe is not verified, however
    # well it matched under another -- the claim is about the command. The
    # same has to hold for `unstable`: if one probe's comparison landed on a
    # word that moves within a native run, that probe learned nothing, and a
    # signature cannot be verified on the strength of the probes that were
    # luckier. Leaving `unstable` out of this test raised the verified count
    # by two the first time it ran, which is the gate this package is
    # measured against catching its own logic inverted.
    ok_all = sorted(k for k in ok_by_sig
                    if k not in bad_by_sig and k not in unstable_by_sig
                    and k not in unwritten_by_sig)
    evidence = {k: ok_by_sig[k] for k in ok_all}
    disputed = sorted(set(ok_by_sig) & set(bad_by_sig))
    notv = [bad_by_sig[k] for k in sorted(bad_by_sig)]


    # TWO CLASSES, AND THEY MUST NEVER SHARE AN UNLABELLED ROW.
    #
    #   verified          the answer bytes are the SAME on both sides, once
    #                     the three derived masks are applied.
    #   verified-mediated the answer bytes DIFFER, in exactly the fields the
    #                     mediation declares and in no other byte.
    #
    # The second is a different claim, and in one respect a stronger one: it
    # required the rewriting to actually happen. A command whose manifest
    # fields all happened to match is NOT here -- it is in `verified`, where
    # it belongs, because nothing about the mediation was exercised. That is
    # why membership is decided on `words_masked` (what moved in this run)
    # and never on the manifest alone (what could move).
    def moved(k):
        return any(w.startswith("mediated:") for w in evidence[k]["words_masked"])

    verified = [k for k in ok_all if not moved(k)]
    verified_mediated = [k for k in ok_all if moved(k)]

    # `verified` rows KEEP their status whatever the control test says, and
    # that is deliberate rather than lenient. Their claim is "these bytes
    # were the same in every call compared", which is true whether or not the
    # value is volatile. What the control test adds to such a row is a
    # CAVEAT, not a demotion: a match on a word that moves within a native
    # run is luck, and a reader deciding what to trust should be told.
    for k in ok_all:
        st = stability.get(k, {})
        nb = evidence[k]["bytes_compared"]
        evidence[k]["native_calls_seen"] = st.get("repeats", 0)
        evidence[k]["unstable_words_in_extent"] = [
            o for o in st.get("unstable", ()) if o < nb]
        evidence[k]["stability"] = (
            "unknown -- no native trace called it twice, so nothing here says "
            "whether these bytes are stable at all"
            if st.get("repeats", 0) < 2 else
            "some compared words are not constant across the calls one native "
            "run made; a match on those is not proof they are stable"
            if evidence[k]["unstable_words_in_extent"] else
            "every compared word was constant across the calls one native run "
            "made")

    # not_verified splits three ways, and only ONE of them is a defect
    # candidate. Before this, a timer and a genuinely wrong answer sat in the
    # same list and the list was read as "45 things that do not match".
    notv_all = [bad_by_sig[k] for k in sorted(bad_by_sig)]
    mismatch, stability_unknown = [], []
    for x in notv_all:
        if stability.get(x["signature"], {}).get("repeats", 0) < 2:
            x["stability"] = ("unknown -- no native trace called this command "
                              "twice, so the control test cannot say whether "
                              "the differing word is stable. NOT the same as "
                              "stable")
            stability_unknown.append(x)
        else:
            x["stability"] = ("every word that differs was constant across the "
                              "calls one native run made, so the difference is "
                              "not volatility")
            mismatch.append(x)
    # A signature that BOTH mismatches somewhere and is unstable somewhere is
    # reported as the mismatch: that is the outcome that could be a defect,
    # and the one a reader has to look at.
    unstable = [unstable_by_sig[k] for k in sorted(unstable_by_sig)
                if k not in bad_by_sig]
    # Reported ahead of a plain mismatch where a signature is both: "did not
    # answer" is the more specific statement and the more actionable one.
    unwritten = [unwritten_by_sig[k] for k in sorted(unwritten_by_sig)]
    # Same precedence rule as `unstable`: a signature that also mismatches
    # somewhere is reported as the mismatch, because that is the outcome a
    # reader has to look at.
    host_assigned = [host_assigned_by_sig[k] for k in sorted(host_assigned_by_sig)
                     if k not in bad_by_sig]

    js = {
        "provenance": [x for x in a.provenance.split("|") if x],
        "generated_by": "scripts/ioctl-matrix.sh verify",
        "method": (
            "the tracer's ctrlout lines -- the params buffer sampled BEFORE "
            "and AFTER each call -- from the native trace against the guest "
            "trace, call by call, word by word"),
        "extent": (
            "TWO extents, and both are per signature so that nothing "
            "downstream can round either up. In BYTES: bytes_compared against "
            "answer_size. The dump cap is 65536 (LEA_TRACE_DUMP), which is "
            "the whole answer for every signature in the current trace set "
            "but is still a cap -- a truncated row is a claim about the "
            "bytes compared and never about the struct. In CALLS: only the calls that could be paired -- "
            "calls_compared, with probes_not_comparable naming any probe whose "
            "two runs made different numbers of calls and which therefore "
            "judged nothing. A signature with a non-empty "
            "probes_not_comparable is a weaker claim than one without, and "
            "fully_paired says which it is"),
        "mask": (
            "derived, never hand-written: a differing word is allowed only if "
            "RM never wrote it natively (its before-call sample equals its "
            "after-call one, so the value is the caller's leftovers and not "
            "an answer), or it is this side's own gpu_id (cardinfo line), a "
            "handle this side allocated, or part of a pointer field the "
            "descriptor table declares for that command (tables.txt, the "
            "stream the guest module was handed). Anything else is a "
            "mismatch -- except a word RM DID write natively and the guest "
            "did not touch, which is its own class, answer_not_written"),
        "declared_pointer_commands": len(declared),
        "mediation_manifest": (
            f"{len(mediation)} command(s) and "
            f"{sum(len(v) for v in mediation.values())} field(s) "
            "(matrix/traces/<drv>/mediation.txt, generated by nvrm-genhdr "
            "--mediation-dump from crates/nvrm-abi/src/mediate.rs -- the same "
            "table the guest module's BDF header is generated from)"
            if mediation else
            "absent -- mediated commands are judged by byte equality, which is "
            "the wrong test for them. scripts/ioctl-matrix.sh trace writes it"),
        "classes": {
            "verified": "the answer bytes are the same on both sides under the "
                        "three derived masks",
            "verified-mediated": "the answer bytes DIFFER, in exactly the fields "
                                 "the mediation manifest declares and nowhere "
                                 "else. A different claim from `verified` and it "
                                 "must never share a row with it unlabelled",
            "verified-mediated, what it does NOT prove": (
                "that the mediation is what moved those bytes. The manifest "
                "declares what MAY be rewritten; whether it WAS depends on "
                "runtime configuration, and a declared field can also be a "
                "value that is simply not stable between two runs. Measured "
                "2026-08-20: NV2080_CTRL_CMD_FB_GET_INFO_V2 lands here on an "
                "UNCAPPED rig, where rewrite_fb_info returns without touching "
                "anything -- the byte that moved is index 0x16 HEAP_FREE, free "
                "memory, which moves between two native calls as well, while "
                "0x08 TOTAL_RAM_SIZE and 0x09 HEAP_SIZE are byte-identical on "
                "both sides. Separating the two needs the control test, not "
                "this mask"),
        },
        "field_map": (
            f"{len(fields)} command(s) have a compiled field map "
            f"(matrix/traces/<drv>/fields.json); offsets and member sizes come "
            f"from offsetof/sizeof in a translation unit that included the "
            f"pinned vendor header, never from parsing"
            if fields else
            "absent -- messages fall back to byte offsets. "
            "scripts/ioctl-matrix.sh trace writes it"),
        "probes": sides,
        "control_test": (
            "within-trace variant: for every command a native trace called "
            "more than once, the answer words are compared across those "
            "calls. A word that differs native-against-native is not stable "
            "and can be evidence for nothing. It is used in ONE DIRECTION -- "
            "it moves a difference out of `mismatch`, and never into a "
            "verified class. LIMIT: two calls of one command in one trace are "
            "not always the same QUESTION (index-list commands such as "
            "GPU_GET_INFO_V2 and GR_GET_INFO ask for a different index each "
            "call), so this flags more words than are genuinely volatile. The "
            "variant without that weakness is a second native trace of the "
            "same probe, where call i is the same question on both sides"),
        "verified": verified,
        "verified_mediated": verified_mediated,
        "evidence": evidence,
        "not_verified": notv,
        "mismatch": mismatch,
        "answer_not_written": unwritten,
        "answer_behind_a_pointer": [behind[k] for k in sorted(behind)],
        "unstable": unstable,
        "host_assigned": host_assigned,
        "stability_unknown": stability_unknown,
        "verdicts": {
            "mismatch": "the bytes differ, at a word that was constant across "
                        "the calls one native run made, and that BOTH sides "
                        "wrote. A potential defect",
            "answer-behind-a-pointer": "the params buffer matched on both "
                                       "sides and the answer is not in it: "
                                       "the command declares an NvP64 and "
                                       "nothing dumps what it points at. NOT "
                                       "verified -- matching on the question "
                                       "is not the claim `verified` makes",
            "answer-not-written": "RM wrote this word natively and the guest "
                                  "left the caller's own value in place, with "
                                  "NV_OK on both sides. A DIFFERENCE, and no "
                                  "status fingerprint can see it because "
                                  "nothing failed -- but not by itself a "
                                  "defect: measured 2026-08-21, RM declines "
                                  "to populate some answers depending on the "
                                  "caller's state, and does so identically on "
                                  "both sides when asked identically. What "
                                  "this class says is that the two sides "
                                  "reached the call in different states; "
                                  "whether the boundary caused that is a "
                                  "separate question and needs a reproducer",
            "host-assigned": "the bytes differ, at a word the HOST or the "
                             "hardware assigns and a guest cannot be expected "
                             "to match -- a channel id, and nothing else so "
                             "far. NOT a mask and NOT verified: the signature "
                             "joins neither verified class. It is the one "
                             "DECLARED classification in answerdiff.py, and "
                             "the criterion it has to meet is stated there. "
                             "It exists so `mismatch` can go back to meaning "
                             "look at this",
            "unstable": "the bytes differ, at a word that moves between two "
                        "calls of one native run, or between two native "
                        "runs of the same probe -- evidence for nothing, in "
                        "either direction",
            "stability-unknown": "the bytes differ, and no native trace called "
                                 "the command twice, so the control test has "
                                 "nothing to say. Never to be read as stable",
        },
        "matched_under_one_probe_and_not_another": disputed,
        "probes_skipped": skipped,
        "not_comparable": {
            k: v for k, v in sorted(abstained.items())},
        "not_comparable_note": (
            "per (signature, probe): the two runs made a different NUMBER of "
            "calls, so call i of one is not call i of the other and no byte "
            "comparison is possible. This is not evidence against the "
            "signature and does not disqualify it -- a signature no probe "
            "could pair is judged by nothing and appears in neither verified "
            "class. Call-count differences are a finding of the GUEST "
            "comparison, which is where they are reported"),
        "unjudged": sorted(k for k in abstained
                           if k not in ok_by_sig and k not in bad_by_sig),
    }
    (outdir / f"verified-{a.driver}.json").write_text(json.dumps(js, indent=2) + "\n")

    for k in verified:
        e = evidence[k]
        print(f"  verified {k:<22} {e['name'][:44]:<44} "
              f"{e['calls_compared']} call(s), {e['bytes_compared']}/{e['answer_size']} bytes"
              + (f", masked {e['words_masked']}" if e["words_masked"] else ""))
    for k in verified_mediated:
        e = evidence[k]
        moved_in = sorted({w.split(":", 1)[1] for w in e["words_masked"]
                           if w.startswith("mediated:")})
        print(f"  MEDIATED {k:<22} {e['name'][:44]:<44} "
              f"{e['calls_compared']} call(s), {e['bytes_compared']}/{e['answer_size']} "
              f"bytes, differs only in {', '.join(moved_in)}")
    for x in mismatch[:14]:
        print(f"  MISMATCH {x['signature']:<22} {x['reason']}")
    if len(mismatch) > 14:
        print(f"  ... and {len(mismatch) - 14} more mismatching")
    for x in stability_unknown[:8]:
        print(f"  UNKNOWN? {x['signature']:<22} {x['reason'][:96]}")
    if len(stability_unknown) > 8:
        print(f"  ... and {len(stability_unknown) - 8} more of unknown stability")
    for x in unstable[:8]:
        print(f"  UNSTABLE {x['signature']:<22} {x['name'][:44]:<44} "
              f"word(s) {x['unstable_words']} move between two native runs")
    if len(unstable) > 8:
        print(f"  ... and {len(unstable) - 8} more unstable")
    for x in skipped:
        print(f"  skipped  {x['probe']:<22} {x['reason']}")
    full = sum(1 for k in verified + verified_mediated if evidence[k]["fully_paired"])
    part = len(verified) + len(verified_mediated) - full
    print(f"\n{len(verified)} signature(s) verified against a native run, "
          f"{len(verified_mediated)} verified-mediated (differ in exactly the "
          f"fields the mediation declares)"
          + (f", {len(disputed)} matched under one probe and not another"
             if disputed else ""))
    for x in unwritten:
        print(f"  NOTANSWERED {x['signature']:<18} {x['name'][:44]:<44} "
              f"{x['reason'][:90]}")
    print(f"{len(behind)} signature(s) matched on the params buffer and are "
          f"NOT verified: their answer is behind an NvP64 that nothing dumps")
    print(f"control: {len(control_seen)} signature(s) had a second native run "
          f"to compare against; the rest rest on the within-trace variant only")
    print(f"not verified splits four ways: {len(mismatch)} MISMATCH (the "
          f"potential defects), {len(unwritten)} ANSWER-NOT-WRITTEN (a "
          f"difference no status fingerprint can see, and not by itself a "
          f"defect -- RM declines to populate some answers depending on the "
          f"caller's state), "
          f"{len(unstable)} unstable (differ at a word that moves within one "
          f"native run), {len(stability_unknown)} of unknown stability (no "
          f"native trace called them twice)")
    print(f"of the {full + part} verified: {full} paired call for call in every "
          f"probe, {part} had at least one probe whose two runs made different "
          f"numbers of calls and which therefore judged nothing "
          f"(probes_not_comparable per signature says which)")
    print(f"wrote {outdir / f'verified-{a.driver}.json'}")
    return 0 if (verified or verified_mediated) else 1


if __name__ == "__main__":
    sys.exit(main())
