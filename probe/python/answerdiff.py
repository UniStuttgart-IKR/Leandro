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
import re
import sys

CTRLOUT = re.compile(r"^ctrlout\t(0x[0-9a-f]+)\tlen=(\d+)\tstatus=(0x[0-9a-f]+)\t(.*)$")
CARDINFO = re.compile(r"\bgpu_id=(0x[0-9a-f]+)")
# Every handle field the detail lines carry, whatever the escape: hRoot,
# hParent, hNew, hClient, hDevice, hMemory, hObjectParent, hDma, hVASpace.
HANDLE = re.compile(r"\bh[A-Z][A-Za-z]*=(0x[0-9a-f]+)")


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


def read_side(path):
    """One side of the comparison: its answers, its gpuId, its handles."""
    calls = collections.defaultdict(list)
    gpuids, handles = set(), set()
    if not path.is_file():
        return None
    for ln in path.read_text(errors="replace").splitlines():
        m = CTRLOUT.match(ln)
        if m:
            words = [int(x, 16) for x in m.group(4).split()]
            calls[m.group(1)].append({
                "len": int(m.group(2)), "status": m.group(3), "bytes": words,
            })
            continue
        if ln.startswith("cardinfo\t"):
            g = CARDINFO.search(ln)
            if g:
                gpuids.add(int(g.group(1), 16))
            continue
        for h in HANDLE.findall(ln):
            v = int(h, 16)
            if v:
                handles.add(v)
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


def compare_cmd(cmd, ncalls, gcalls, native, guest, ptrs=()):
    """One signature's verdict: verified, or the reason it is not."""
    if len(ncalls) != len(gcalls):
        return None, (f"{len(ncalls)} call(s) natively and {len(gcalls)} in the "
                      "guest -- the answers cannot be paired"), {}
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
            if why is None:
                return None, (f"call {i}, word {j} (offset {j * 4}): "
                              f"{nw:#010x} natively, {gw:#010x} in the guest"), {}
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
    for p in a.probes:
        n = read_side(ndir / f"{p}.tsv")
        g = read_side(gdir / f"{p}.tsv")
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
            ok, why, masked = compare_cmd(cmd, n["calls"].get(cmd, []),
                                          g["calls"].get(cmd, []), n, g,
                                          declared.get(cmd, ()))
            row = cat.get(("ctl", "0x2a", cmd), {})
            if ok:
                e = ok_by_sig.setdefault(key, {
                    "name": row.get("name", ""),
                    "status_before": row.get("status", ""),
                    "probes": [], "calls_compared": 0,
                    "bytes_compared": len(n["calls"][cmd][0]["bytes"]),
                    "answer_size": n["calls"][cmd][0]["len"],
                    "words_masked": {},
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
    verified = sorted(k for k in ok_by_sig if k not in bad_by_sig)
    evidence = {k: ok_by_sig[k] for k in verified}
    disputed = sorted(set(ok_by_sig) & set(bad_by_sig))
    notv = [bad_by_sig[k] for k in sorted(bad_by_sig)]

    js = {
        "provenance": [x for x in a.provenance.split("|") if x],
        "generated_by": "scripts/ioctl-matrix.sh verify",
        "method": (
            "the tracer's ctrlout line (first 32 bytes of the params buffer "
            "after the call) from the native trace against the guest trace, "
            "call by call, word by word"),
        "extent": (
            "the FIRST BYTES of the answer, not the whole answer -- "
            "bytes_compared against answer_size per signature says how much"),
        "mask": (
            "derived, never hand-written: a differing word is allowed only if "
            "it is this side's own gpu_id (cardinfo line), a handle this side "
            "allocated, or part of a pointer field the descriptor table "
            "declares for that command (tables.txt, the stream the guest "
            "module was handed). Anything else is a mismatch"),
        "declared_pointer_commands": len(declared),
        "probes": sides,
        "verified": verified,
        "evidence": evidence,
        "not_verified": notv,
        "matched_under_one_probe_and_not_another": disputed,
        "probes_skipped": skipped,
    }
    (outdir / f"verified-{a.driver}.json").write_text(json.dumps(js, indent=2) + "\n")

    for k in verified:
        e = evidence[k]
        print(f"  verified {k:<22} {e['name'][:44]:<44} "
              f"{e['calls_compared']} call(s), {e['bytes_compared']}/{e['answer_size']} bytes"
              + (f", masked {e['words_masked']}" if e["words_masked"] else ""))
    for x in notv[:14]:
        print(f"  NOT      {x['signature'] or x['probe']:<22} {x['reason']}")
    if len(notv) > 14:
        print(f"  ... and {len(notv) - 14} more not verified")
    for x in skipped:
        print(f"  skipped  {x['probe']:<22} {x['reason']}")
    print(f"\n{len(verified)} signature(s) verified against a native run, "
          f"{len(notv)} not"
          + (f", {len(disputed)} of them matched under another probe"
             if disputed else ""))
    print(f"wrote {outdir / f'verified-{a.driver}.json'}")
    return 0 if verified else 1


if __name__ == "__main__":
    sys.exit(main())
