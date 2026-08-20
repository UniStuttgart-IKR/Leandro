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
    of one run is the same question as call i of the other -- and it costs a
    run, which is why it is not this.
    """
    rep = collections.defaultdict(int)
    unstable = collections.defaultdict(set)
    for path in paths:
        if not path.is_file():
            continue
        calls = collections.defaultdict(list)
        for r in traceread.read(path):
            if r["t"] == "ctrlout":
                calls[r["cmd"]].append(list(bytes.fromhex(r["dump"])))
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
    """cmd -> [ {off, len, stride, count, kind, field}, ... ] out of the
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
        if f[:1] != ["mediated"] or len(f) < 8:
            continue
        out[f[1]].append({
            "off": int(f[2]), "len": int(f[3]), "stride": int(f[4]),
            "count": int(f[5]), "kind": f[6], "field": " ".join(f[7:]),
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
    gpuids, handles = set(), set()
    if not pathlib.Path(path).is_file():
        return None
    for r in traceread.read(path):
        if r["t"] == "ctrlout":
            calls[r["cmd"]].append({
                "len": int(r["len"]), "status": r["status"],
                "bytes": list(bytes.fromhex(r["dump"])),
            })
            continue
        if r["t"] == "cardinfo":
            if r.get("gpu_id"):
                gpuids.add(int(r["gpu_id"], 16))
            continue
        handles.update(traceread.handles_of(r))
    return {"calls": calls, "gpuids": gpuids, "handles": handles}


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


def compare_cmd(cmd, ncalls, gcalls, native, guest, ptrs=(), fm=None, med=None,
                stab=None):
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
    for i, (n, g) in enumerate(zip(ncalls, gcalls)):
        if n["status"] != g["status"]:
            return None, (f"call {i}: status {n['status']} natively, "
                          f"{g['status']} in the guest"), {}
        if n["len"] != g["len"]:
            return None, (f"call {i}: paramsSize {n['len']} natively, "
                          f"{g['len']} in the guest"), {}
        if len(n["bytes"]) != len(g["bytes"]):
            return None, f"call {i}: the two dumps are of different length", {}
        nws, gws = words_of(n["bytes"]), words_of(g["bytes"])
        for j, (nw, gw) in enumerate(zip(nws, gws)):
            if nw == gw:
                continue
            why = explain(nw, gw, j * 4, ptrs, native, guest)
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
            if why is None and stab and (j * 4) in stab["unstable"]:
                return "unstable", (
                    f"call {i}, {name_at(fm, j * 4)}: {nw:#010x} natively, "
                    f"{gw:#010x} in the guest -- and this word is NOT STABLE "
                    f"between two calls of the native run itself, so it is "
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
                hits = [mediated_hit(med, j * 4 + b)
                        for b in range(4) if nb[b] != gb[b]]
                if hits and all(h is not None for h in hits):
                    why = "mediated:" + hits[0]["kind"]
            if why is None:
                extra = (" -- this command IS mediated, and this byte is in "
                         "none of the fields the mediation declares"
                         if med else "")
                return None, (f"call {i}, {name_at(fm, j * 4)}: "
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
    stability = read_stability(traceread.all_traces(ndir))
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
    abstained, unstable_by_sig = {}, {}
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
        for cmd in sorted(set(n["calls"]) | set(g["calls"])):
            key = f"ctl 0x2a {cmd}"
            fm = fields.get(cmd)
            med = mediation.get(cmd)
            st = stability.get(cmd)
            ok, why, masked = compare_cmd(cmd, n["calls"].get(cmd, []),
                                          g["calls"].get(cmd, []), n, g,
                                          declared.get(cmd, ()), fm, med, st)
            row = cat.get(("ctl", "0x2a", cmd), {})
            if ok == "abstain":
                abstained.setdefault(key, []).append({"probe": p, "reason": why})
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
            if ok:
                nb = len(n["calls"][cmd][0]["bytes"])
                e = ok_by_sig.setdefault(key, {
                    "name": row.get("name", ""),
                    "status_before": row.get("status", ""),
                    "probes": [], "calls_compared": 0,
                    "bytes_compared": nb,
                    "answer_size": n["calls"][cmd][0]["len"],
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
                e["calls_compared"] += len(n["calls"].get(cmd, []))
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
                    if k not in bad_by_sig and k not in unstable_by_sig)
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
        cmd = k.split()[-1]
        st = stability.get(cmd, {})
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
        cmd = x["signature"].split()[-1]
        if stability.get(cmd, {}).get("repeats", 0) < 2:
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

    js = {
        "provenance": [x for x in a.provenance.split("|") if x],
        "generated_by": "scripts/ioctl-matrix.sh verify",
        "method": (
            "the tracer's ctrlout line (first 32 bytes of the params buffer "
            "after the call) from the native trace against the guest trace, "
            "call by call, word by word"),
        "extent": (
            "TWO extents, and both are per signature so that nothing "
            "downstream can round either up. In BYTES: the first bytes of the "
            "answer, not the whole answer -- bytes_compared against "
            "answer_size. In CALLS: only the calls that could be paired -- "
            "calls_compared, with probes_not_comparable naming any probe whose "
            "two runs made different numbers of calls and which therefore "
            "judged nothing. A signature with a non-empty "
            "probes_not_comparable is a weaker claim than one without, and "
            "fully_paired says which it is"),
        "mask": (
            "derived, never hand-written: a differing word is allowed only if "
            "it is this side's own gpu_id (cardinfo line), a handle this side "
            "allocated, or part of a pointer field the descriptor table "
            "declares for that command (tables.txt, the stream the guest "
            "module was handed). Anything else is a mismatch"),
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
        "unstable": unstable,
        "stability_unknown": stability_unknown,
        "verdicts": {
            "mismatch": "the bytes differ, at a word that was constant across "
                        "the calls one native run made. THE ONLY POTENTIAL "
                        "DEFECT in this file",
            "unstable": "the bytes differ, at a word that moves between two "
                        "calls of the native run itself -- evidence for "
                        "nothing, in either direction",
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
              f"word(s) {x['unstable_words']} move within one native run")
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
    print(f"not verified splits three ways: {len(mismatch)} MISMATCH (the only "
          f"potential defects), {len(unstable)} unstable (differ at a word that "
          f"moves within one native run), {len(stability_unknown)} of unknown "
          f"stability (no native trace called them twice)")
    print(f"of the {full + part} verified: {full} paired call for call in every "
          f"probe, {part} had at least one probe whose two runs made different "
          f"numbers of calls and which therefore judged nothing "
          f"(probes_not_comparable per signature says which)")
    print(f"wrote {outdir / f'verified-{a.driver}.json'}")
    return 0 if (verified or verified_mediated) else 1


if __name__ == "__main__":
    sys.exit(main())
