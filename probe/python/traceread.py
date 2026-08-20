#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""The one reader of a tracer trace, on the Python side.

The tracer writes two formats (``crates/nvrm-trace/src/log.rs``): the legacy
TSV and JSONL, both rendered from the same record by two renderers, so they
cannot carry different information. This module reads either and yields the
same records, and every Python consumer -- ``ioctlmatrix``, ``guestdiff``,
``answerdiff`` -- goes through it instead of knowing which format it got.

WHAT A RECORD IS. A dict with ``t`` (the kind), optionally ``phase`` (``in``
for the sample taken before the driver overwrote the buffer, ``out`` for the
one after), and the record's fields under their own names:

    {"t": "ioctl", "dev": "gpu", "nr": "0xd6", "sub": None, "size": "8",
     "psize": None, "ret": "0", "status": None, "fd": "9"}

Values are STRINGS, spelled as the legacy TSV spells them -- hex stays
``0x...``, because that is how the descriptor tables and the catalogue spell
it and every consumer reads it with ``int(x, 16)``. An absent field is
``None`` and never the string ``-``: the TSV writes ``-`` for absent, and a
reader that passed that through would make "no sub-command" and "a
sub-command spelled -" the same thing.

The one exception to "as the TSV spells it" is ``dump``, which is contiguous
hex here (``002d0000``) rather than space-separated. Spaces are a rendering
of the old format; ``bytes.fromhex`` is what a caller wants.

THE PROJECTION, and why it exists. ``project()`` renders a record back into
the legacy TSV line, and ``--check`` asserts that the JSONL of a run carries
exactly the records its TSV does. `scripts/ioctl-matrix.sh trace` runs it
per probe and `guest` runs it over the traces the VM sends back, so no sweep
can pass without it. It is a binary outcome on real traces rather than a
judgement about whether two parsers agree -- and it is the reason the tracer
writes both formats from ONE run instead of the sweep being run twice. Two
runs would compare two runs, and this pipeline has measured values that are
not stable between two runs of one binary on one machine.
"""

import json
import pathlib

# The measurement kinds. Their TSV form is positional, and these are the
# column names, in order. `log.rs` renders exactly this order; the counting
# rule in scripts/lib/matrix.sh selects on the first two of `ioctl`.
POSITIONAL = {
    "open": ["dev", "fd"],
    "ioctl": ["dev", "nr", "sub", "size", "psize", "ret", "status", "fd"],
    "mmap": ["dev", "fd", "len", "off", "addr"],
    "read": ["dev", "fd", "ret"],
    "poll": ["dev", "fd", "revents"],
    "eventreg": ["fd", "prev"],
}
MEASUREMENT = frozenset(POSITIONAL)

# The kinds that have an IN and an OUT sample, which the TSV spells by
# appending `in` to the kind (`nvos64in` beside `nvos64`) and JSONL spells
# as a field. `size` and `attr` of an NVOS32 are IN/OUT -- the caller asks
# and RM writes back what it really did -- so both samples are the point.
PHASED = frozenset({"nvos02", "nvos33", "nvos32", "nvos46", "nvos64",
                    "memparams", "ctrlout", "uvmout", "allocout", "escout"})

# Three kinds mix positional and keyed fields, and this is the whole of that
# irregularity:
#   cardinfo  [0]     gpu_id=... pci=... ...      -- `i` is positional, as [n]
#   ctrlout   0x214   len=384 status=0x0  <dump>  -- `cmd` and `dump` are
#   uvmout    0x25    len=32   <dump>             -- `nr` and `dump` are
MIXED = {"cardinfo": ("i",), "ctrlout": ("cmd", "dump"),
         "uvmout": ("nr", "dump"),
         "allocout": ("dev", "class", "dump"),
         "escout": ("dev", "nr", "sub", "dump")}


def _split_phase(kind):
    """`nvos64in` -> (nvos64, in); `nvos64` -> (nvos64, out)."""
    if kind.endswith("in") and kind[:-2] in PHASED:
        return kind[:-2], "in"
    if kind in PHASED:
        return kind, "out"
    return kind, None


def _from_tsv(line):
    f = line.split("\t")
    kind, phase = _split_phase(f[0])
    rec = {"t": kind}
    if phase:
        rec["phase"] = phase

    names = POSITIONAL.get(kind)
    if names is not None:
        for name, v in zip(names, f[1:]):
            rec[name] = None if v == "-" else v
        return rec

    rest = f[1:]
    if kind == "cardinfo":
        rec["i"] = rest[0].strip("[]") if rest else ""
        rest = rest[1:]
    elif kind == "ctrlout":
        rec["cmd"] = rest[0] if rest else ""
        rest = rest[1:]
    for x in rest:
        if "=" in x:
            k, _, v = x.partition("=")
            rec[k] = v
        elif kind == "ctrlout":
            # The trailing dump, the one field of the old format that is
            # neither positional-by-index nor named.
            rec["dump"] = x.replace(" ", "")
    return rec


def _from_json(line):
    o = json.loads(line)
    rec = {}
    for k, v in o.items():
        if v is None:
            rec[k] = None
        elif isinstance(v, bool):          # nothing writes one today
            rec[k] = "true" if v else "false"
        elif isinstance(v, (int, float)):
            rec[k] = str(v)
        else:
            rec[k] = v
    return rec


def read(path):
    """Every record of a trace, in file order, whatever format it is in.

    The provenance header is skipped in both formats -- `#` lines in the
    TSV, a `{"t":"meta"}` record in the JSONL. Callers that want it ask
    `meta()`.
    """
    path = pathlib.Path(path)
    if not path.is_file():
        return
    for ln in path.read_text(errors="replace").splitlines():
        if not ln or ln.startswith("#"):
            continue
        if ln.startswith("{"):
            try:
                rec = _from_json(ln)
            except json.JSONDecodeError:
                # A truncated last line is the shape a killed process
                # leaves. Everything before it is still a measurement, and
                # dropping the whole trace over the tail would lose it.
                continue
            if rec.get("t") == "meta":
                continue
            yield rec
        else:
            yield _from_tsv(ln)


def meta(path):
    """The provenance header, as a dict. Empty if the trace has none."""
    path = pathlib.Path(path)
    if not path.is_file():
        return {}
    out = {}
    for ln in path.read_text(errors="replace").splitlines():
        if ln.startswith("{"):
            try:
                o = json.loads(ln)
            except json.JSONDecodeError:
                continue
            if o.get("t") == "meta":
                return {k: v for k, v in o.items() if k != "t"}
            return out
        if ln.startswith("#"):
            k, _, v = ln[1:].partition(":")
            out[k.strip()] = v.strip()
        else:
            return out
    return out


def trace_file(tdir, probe):
    """The trace to read for one probe, JSONL first.

    Both formats are written during the migration and the equivalence gate
    proves they agree, so preferring the new one is what actually exercises
    it. A trace directory cut before the migration has no `.jsonl` and
    falls back silently, because there is nothing wrong with it.
    """
    tdir = pathlib.Path(tdir)
    j = tdir / f"{probe}.jsonl"
    return j if j.is_file() else tdir / f"{probe}.tsv"


def all_traces(tdir):
    """Every probe trace in a directory, JSONL first, index files excluded.

    `probes.tsv` and `inventory.tsv` are the run's own index files, written
    by the shell and not by the tracer. They carry a `.tsv` name and are not
    traces, and a glob that swept them in would parse a probe row as a
    record.
    """
    tdir = pathlib.Path(tdir)
    index = {"probes", "inventory"}
    stems = sorted({f.stem for f in tdir.glob("*.tsv") if f.stem not in index}
                   | {f.stem for f in tdir.glob("*.jsonl") if f.stem not in index})
    return [trace_file(tdir, stem) for stem in stems]


def handles_of(rec):
    """Every RM handle this record names.

    The mask that says "this word is a handle THIS side allocated" is built
    out of these. Reading them by FIELD NAME rather than by a regex over the
    line is what makes the mask independent of the format: the old reader
    matched `\\bh[A-Z][A-Za-z]*=(0x...)` against raw text, which finds
    nothing at all in a JSON line.
    """
    for k, v in rec.items():
        if len(k) > 1 and k[0] == "h" and k[1].isupper() and v:
            try:
                n = int(v, 16)
            except ValueError:
                continue
            if n:
                yield n


# ---------------------------------------------------------------------------
# the projection -- the migration's gate
# ---------------------------------------------------------------------------
def _spaced(hexstr):
    return " ".join(hexstr[i:i + 2] for i in range(0, len(hexstr), 2))


def project(rec):
    """One record as the line the legacy TSV format writes for it."""
    kind = rec["t"]
    phase = rec.get("phase")
    out = [kind + "in" if phase == "in" else kind]

    names = POSITIONAL.get(kind)
    if names is not None:
        for name in names:
            v = rec.get(name)
            out.append("-" if v is None else v)
        return "\t".join(out)

    mixed = MIXED.get(kind, ())
    for k, v in rec.items():
        if k in ("t", "phase"):
            continue
        if k == "i" and kind == "cardinfo":
            out.append(f"[{v}]")
        elif k == "dump":
            out.append(_spaced(v))
        elif k in mixed:
            # A positional field that is absent is `-` in the old format,
            # the same as in the measurement lines. `escout.sub` is the
            # common case: most escapes have no second dispatch level.
            out.append("-" if v is None else v)
        else:
            out.append(f"{k}={'-' if v is None else v}")
    return "\t".join(out)


def _main():
    import argparse
    import sys

    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--project", metavar="TRACE",
                    help="print the legacy TSV projection of a trace")
    ap.add_argument("--check", nargs=2, metavar=("JSONL", "TSV"),
                    help="the migration gate: the projection of JSONL must "
                         "reproduce TSV byte for byte")
    a = ap.parse_args()

    if a.project:
        for rec in read(a.project):
            print(project(rec))
        return 0

    if a.check:
        jsonl, tsv = pathlib.Path(a.check[0]), pathlib.Path(a.check[1])
        if not jsonl.is_file():
            print(f"{jsonl}: no JSONL trace", file=sys.stderr)
            return 1
        if not tsv.is_file():
            print(f"{tsv}: no TSV trace", file=sys.stderr)
            return 1
        got = [project(r) for r in read(jsonl)]
        # The TSV carries the `#` provenance header the JSONL carries as a
        # meta record; the comparison is of the records, not of the headers.
        want = [ln for ln in tsv.read_text(errors="replace").splitlines()
                if ln and not ln.startswith("#")]

        # COMPARED AS A MULTISET, and that is not a weakening -- it is the
        # difference between what the renderers control and what they do
        # not. One record is rendered into both formats by two consecutive
        # write() calls, and the pair is not atomic: a second thread can
        # write both of ITS lines in between, so the two files hold the same
        # records in a different INTERLEAVING. Measured 2026-08-20: 7 of 20
        # probes, all of them the concurrent ones (nvenc, opencl, the Vulkan
        # set); the single-threaded probes match in order too.
        #
        # No renderer can reorder records -- only the scheduler can -- so an
        # ordered comparison here tests the thread scheduler and calls it a
        # format defect. What the gate is for is the claim that every record
        # carries the same information in both formats, and that is exactly
        # a multiset identity: a record that rendered differently, a record
        # written to one file and not the other, and a dropped write all
        # fail it.
        #
        # It costs nothing after the cutover either: once only the JSONL is
        # written, its order is the scheduler's order, which is what the
        # TSV's order was.
        if got == want:
            print(f"{jsonl.name}: {len(got)} records project to the TSV exactly")
            return 0

        import collections
        gc, wc = collections.Counter(got), collections.Counter(want)
        if gc == wc:
            moved = sum(1 for g, w in zip(got, want) if g != w)
            print(f"{jsonl.name}: {len(got)} records project to the TSV exactly "
                  f"({moved} in a different order -- a concurrent workload "
                  f"interleaves the two files)")
            return 0

        if len(got) != len(want):
            print(f"{jsonl.name}: {len(got)} records against {len(want)} TSV lines",
                  file=sys.stderr)
        only_json = list((gc - wc).elements())
        only_tsv = list((wc - gc).elements())
        for label, rows in (("in the JSONL and not the TSV", only_json),
                            ("in the TSV and not the JSONL", only_tsv)):
            for r in rows[:3]:
                print(f"{jsonl.name}: {label}: {r!r}", file=sys.stderr)
            if len(rows) > 3:
                print(f"{jsonl.name}: ... and {len(rows) - 3} more {label}",
                      file=sys.stderr)
        return 1

    ap.error("nothing to do -- pass --project or --check")


if __name__ == "__main__":
    raise SystemExit(_main())
