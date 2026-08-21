#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""Compare a guest's traces against the native ones and say what moved.

Called by ``scripts/ioctl-matrix.sh guest``; not meant to be run by hand.

WHAT THIS IS FOR. The catalogue predicts: every signature a probe emitted is
governed or passthrough, so a guest *should* carry it. A prediction is not a
measurement, and this is the file that tells them apart. It reads the two
traces the same tracer took on the two sides of the boundary and reports
where they disagree.

WHAT IS COMPARED

  * THE SIGNATURE SET. A signature the guest never emits is as much a
    finding as one it emits and the host does not: the first says the guest
    took a different path, the second says it took an extra one. Neither is
    visible in an exit code.

  * THE ``rm_status`` FINGERPRINT per signature -- which statuses that
    signature can return, including the deliberate non-zero ones. This is
    the comparison that has teeth. ``0x56`` (NOT_SUPPORTED) on the ECC,
    InfoROM and BBX paths is the RIGHT answer on this card, and a guest that
    answers ``0x0`` there has not carried the call, it has invented an
    answer. A gate that only asks "did anything fail" reads that as success.

WHAT IS NOT COMPARED, and why each would cry wolf

  * CALL COUNTS. A workload may allocate one surface more in one run than in
    the next -- the native phase already retries around exactly that -- so a
    count difference is not evidence about the boundary. What must not move
    is WHICH statuses a signature can return, and that is what is checked.

  * HANDLES, gpuIds AND ADDRESSES. They are translated on purpose: a guest
    handle IS a different number, by design. They do not enter the key --
    ``NV_ESC_RM_MAP_MEMORY`` puts a handle in ``sub`` and the key
    normalisation collapses it, the same way the catalogue does.

  * THE ANSWER BYTES. Nothing here reads them; that is the differential
    harness (OPEN-QUESTIONS number 50), and until it exists a signature that
    passes this comparison is `guest-validated`, not `verified`. The
    distinction is the whole point of both names.
"""

import argparse
import collections
import json
import pathlib
import re
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
# The key normalisation lives in ONE place. Importing it rather than
# repeating it is not tidiness: a second copy that drifted would compare two
# differently-keyed sets and report the drift as a finding about the guest.
from ioctlmatrix import MAP_MEMORY_NR, provenance_fields  # noqa: E402
import traceread                                            # noqa: E402


def read_probes(tsv):
    """The runner's own row per probe, out of a `#`-commented TSV."""
    rows, head = [], None
    if not tsv.is_file():
        return rows
    for ln in tsv.read_text(errors="replace").splitlines():
        if ln.startswith("#probe\t"):
            head = ln[1:].split("\t")
            continue
        if ln.startswith("#") or not ln.strip():
            continue
        if head:
            rows.append(dict(zip(head, ln.split("\t"))))
    return rows


def signatures(trace):
    """(device, nr, sub) -> {'calls': n, 'status': Counter, 'psize': set}

    Read through `traceread`, so this compares the two sides of the boundary
    and not the two formats the tracer can write them in.
    """
    sigs = collections.defaultdict(
        lambda: {"calls": 0, "status": collections.Counter(), "psize": set()})
    for r in traceread.read(trace):
        if r["t"] != "ioctl":
            continue
        sub = r.get("sub")
        key = (r["dev"], r["nr"], "-" if r["nr"] == MAP_MEMORY_NR else (sub or "-"))
        e = sigs[key]
        e["calls"] += 1
        if r.get("psize") is not None:
            e["psize"].add(r["psize"])
        if r.get("status") is not None:
            e["status"][r["status"]] += 1
    return sigs


class Names:
    """Names for the two numbers a finding is made of, read from where they
    are defined and nowhere else: the signature name from the catalogue this
    pipeline just generated, the status name from the vendor's own status
    table. A finding that says `ctl nr=0x2a sub=0x20800802 -> 0x1e` is a
    finding nobody can act on without three lookups."""

    def __init__(self, outdir, driver, vendor):
        self.sig, self.status = {}, {}
        cat = pathlib.Path(outdir) / f"catalog-{driver}.json"
        if cat.is_file():
            try:
                for r in json.loads(cat.read_text()).get("signatures", []):
                    self.sig[(r["device"], r["nr"], r["sub"])] = r["name"]
            except Exception:                               # noqa: BLE001
                pass
        # nvstatuscodes.h is a list of NV_STATUS_CODE(name, value, "text")
        # macro invocations -- the same table the driver builds its own from.
        h = (pathlib.Path(vendor) / "src/common/sdk/nvidia/inc/nvstatuscodes.h")
        if h.is_file():
            for m in re.finditer(r"NV_STATUS_CODE\(\s*(\w+)\s*,\s*(0x[0-9A-Fa-f]+)",
                                 h.read_text(errors="replace")):
                self.status[int(m.group(2), 16)] = m.group(1)

    def of(self, k):
        n = self.sig.get(k)
        return f"{n} ({k[0]} nr={k[1]} sub={k[2]})" if n else f"{k[0]} nr={k[1]} sub={k[2]}"

    def st(self, v):
        try:
            n = self.status.get(int(v, 16))
        except ValueError:
            n = None
        return f"{v} {n}" if n else v

    def sts(self, vs):
        return "[" + ", ".join(self.st(v) for v in sorted(vs)) + "]" if vs else "[none]"


# DRM is not compared, and that is the same decision the catalogue makes for
# it: `/dev/dri` is a different namespace, a different driver instance and a
# different report (probe/run/drmtrace.sh). The guest's card and render nodes
# belong to a nvidia-drm that is not the host's, so a difference there is
# expected by construction and would drown every real finding. The delta is
# still COUNTED, per probe, so that "not compared" cannot be read as "no
# difference".
DRM_DEVICES = ("drm", "render")


def compare(native, guest, names):
    """One list of findings for one probe. Empty means the two sides agree
    about every signature and every status either of them can return."""
    out = []
    nat = {k: v for k, v in native.items() if k[0] not in DRM_DEVICES}
    gst = {k: v for k, v in guest.items() if k[0] not in DRM_DEVICES}
    for k in sorted(set(nat) - set(gst)):
        out.append({
            "kind": "signature-absent-in-guest",
            "device": k[0], "nr": k[1], "sub": k[2], "name": names.sig.get(k, ""),
            "detail": f"{names.of(k)}: {nat[k]['calls']} call(s) natively, none in the guest",
        })
    for k in sorted(set(gst) - set(nat)):
        out.append({
            "kind": "signature-only-in-guest",
            "device": k[0], "nr": k[1], "sub": k[2], "name": names.sig.get(k, ""),
            "detail": f"{names.of(k)}: {gst[k]['calls']} call(s) in the guest, none natively",
        })
    for k in sorted(set(nat) & set(gst)):
        ns, gs = set(nat[k]["status"]), set(gst[k]["status"])
        if ns != gs:
            out.append({
                "kind": "rm-status-fingerprint",
                "device": k[0], "nr": k[1], "sub": k[2], "name": names.sig.get(k, ""),
                "native_status": sorted(ns), "guest_status": sorted(gs),
                "detail": (f"{names.of(k)}: native answers {names.sts(ns)}, "
                           f"guest answers {names.sts(gs)}"),
            })
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--native", required=True)
    ap.add_argument("--guest", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--driver", required=True)
    ap.add_argument("--vendor", required=True)
    ap.add_argument("--provenance", default="")
    a = ap.parse_args()

    ndir, gdir = pathlib.Path(a.native), pathlib.Path(a.guest)
    outdir = pathlib.Path(a.out)
    nrows = {r["probe"]: r for r in read_probes(ndir / "probes.tsv")}
    grows = read_probes(gdir / "probes.tsv")
    names = Names(outdir, a.driver, a.vendor)

    probes, counts = [], collections.Counter()
    for g in grows:
        p = g["probe"]
        nat = signatures(traceread.trace_file(ndir, p))
        gst = signatures(traceread.trace_file(gdir, p))
        findings = compare(nat, gst, names) if gst else []
        drm = (sum(v["calls"] for k, v in nat.items() if k[0] in DRM_DEVICES),
               sum(v["calls"] for k, v in gst.items() if k[0] in DRM_DEVICES))
        gres = g.get("result", "")

        if not gres.startswith("PASS"):
            # The guest run did not stand on its own -- it failed its own
            # criterion, or its trace could not be gated. Comparing an
            # incomplete trace would produce findings about the instrument
            # and file them against the boundary.
            verdict = "FAIL" if gres.startswith("FAIL") else "blocked"
            findings = [{"kind": "guest-run", "detail": gres}] + findings
        elif findings:
            verdict = "FAIL"
        elif not gst:
            verdict = "blocked"
            findings = [{"kind": "guest-run",
                         "detail": "the guest run passed but left no trace to compare"}]
        else:
            verdict = "guest-validated"
        counts[verdict] += 1

        probes.append({
            "probe": p,
            "verdict": verdict,
            "guest_result": gres,
            "criterion": g.get("criterion", ""),
            "native_result": nrows.get(p, {}).get("result", "not measured natively"),
            "native_signatures": len(nat),
            "guest_signatures": len(gst),
            "native_ioctls": nrows.get(p, {}).get("tracer", ""),
            "guest_ioctls": g.get("tracer", ""),
            "native_modeset": nrows.get(p, {}).get("modeset", ""),
            "guest_modeset": g.get("modeset", ""),
            "drm_calls_not_compared": {"native": drm[0], "guest": drm[1]},
            "findings": findings,
        })

    # Probes that were measured natively and that this sweep never ran. Named
    # rather than omitted: a sweep that silently skipped a probe reads as a
    # sweep that covered everything.
    ran = {p["probe"] for p in probes}
    notrun = sorted(n for n, r in nrows.items()
                    if r.get("result", "").startswith("PASS") and n not in ran)

    js = {
        "provenance": [x for x in a.provenance.split("|") if x],
        "provenance_fields": provenance_fields(
            [x for x in a.provenance.split("|") if x]),
        "generated_by": "scripts/ioctl-matrix.sh guest",
        "compared": ("the signature set and the rm_status fingerprint per "
                     "signature, native against guest, both traced by the same "
                     "interposer"),
        "not_compared": [
            "call counts -- a workload is allowed to allocate one surface more",
            "handles, gpuIds and addresses -- translated on purpose",
            "DRM (/dev/dri) -- a different namespace and a different report; "
            "counted per probe as drm_calls_not_compared",
            "answer bytes -- that is the differential harness, OPEN-QUESTIONS 50",
        ],
        "summary": dict(counts),
        "passed_natively_but_not_run_here": notrun,
        "probes": sorted(probes, key=lambda x: x["probe"]),
    }
    (outdir / f"guest-{a.driver}.json").write_text(json.dumps(js, indent=2) + "\n")

    for p in js["probes"]:
        print(f"{p['probe']:<16} {p['verdict']:<16} "
              f"{p['native_signatures']:>4} -> {p['guest_signatures']:<4} signatures"
              f"{'  ' + str(len(p['findings'])) + ' finding(s)' if p['findings'] else ''}")
        for f in p["findings"]:
            print(f"    {f['kind']}: {f['detail']}")
    if notrun:
        print(f"\nnot run in this sweep: {', '.join(notrun)}")
    print("\n" + ", ".join(f"{v} {k}" for k, v in sorted(counts.items())))
    print(f"wrote {outdir / f'guest-{a.driver}.json'}")


if __name__ == "__main__":
    main()
