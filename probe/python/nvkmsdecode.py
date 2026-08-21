#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""Name the NVKMS commands, and check the names against what was measured.

OPEN-QUESTIONS number 64. NVKMS carries its whole interface under a single
ioctl number -- ``_IOWR('m', 0, struct NvKmsIoctlParams)`` -- so ``nr`` is 0
on every line and the real command is a field of that struct. The tracer
already reads it and puts it in ``sub``; this turns the number into a name
and a parameter struct, the way an RM_CONTROL row gets one out of
``ctrl*.h``.

WHAT MAKES THIS HONEST is the same rule ``ioctlmatrix.py`` follows, and one
extra check that this interface happens to make possible:

  * The command numbers are DERIVED, not transcribed: ``enum
    NvKmsIoctlCommand`` in ``nvkms-api.h`` has no explicit initialisers, so
    a command's number is its position, and the position is read from the
    header at run time. A renumbering upstream moves these with it.
  * The parameter struct is resolved by the header's own naming convention
    -- ``NVKMS_IOCTL_FOO_BAR`` -> ``struct NvKmsFooBarParams`` -- and then
    CHECKED against the structs the header actually declares. The match is
    case-insensitive because the casing of acronyms is not a rule anyone
    wrote down (``FrameLock``, ``CRC32``, ``3DVision``), while the sequence
    of words is. A command whose struct is not declared is reported as
    unresolved; nothing is invented for it.
  * Sizes are COMPILED, never parsed: one C file per run, including the
    header, printing ``sizeof`` for each struct.
  * And then the size is compared against ``psize`` -- the size of the block
    each call actually pointed at, measured per call by the tracer. That is
    the check number 64 named as the one a decoder can be tested against
    before it is trusted, and it is what makes this a measurement rather
    than a mapping.

Usage:
    probe/python/nvkmsdecode.py [--vendor DIR] [--traces DIR] [--json]

With ``--traces`` it verifies against measured ``psize`` and exits non-zero
if any observed command disagrees. Without it, it just prints the table.
"""
import json
import os
import re
import subprocess
import sys
import tempfile
from glob import glob

HEADER = "src/nvidia-modeset/interface/nvkms-api.h"

# Resolved by following the #include chain out of nvkms-api.h until it
# compiles. Ordered as found; the set is small because this header pulls in
# far less than the RM control headers do.
INCLUDE_DIRS = [
    "src/common/sdk/nvidia/inc",
    "kernel-open/common/inc",
    "src/common/inc",
    "src/nvidia-modeset/interface",
    "src/common/unix/common/inc",
]

PREFIX = "NVKMS_IOCTL_"


def commands(text):
    """-> [(number, NVKMS_IOCTL_NAME)], numbered by POSITION.

    The enum carries no explicit values -- checked, not assumed: an entry
    with an ``=`` makes this raise rather than silently misnumber everything
    after it.
    """
    m = re.search(r"enum NvKmsIoctlCommand\s*\{(.*?)\n\};", text, re.S)
    if not m:
        raise SystemExit("nvkms-api.h: no enum NvKmsIoctlCommand")
    out = []
    for line in m.group(1).split("\n"):
        line = re.sub(r"/\*.*?\*/", "", line).strip().rstrip(",")
        if not line or line.startswith("//"):
            continue
        if "=" in line:
            raise SystemExit(
                f"nvkms-api.h: '{line}' has an explicit value -- this tool numbers "
                "by position and would be wrong. Teach it the new form."
            )
        if not line.startswith(PREFIX):
            raise SystemExit(f"nvkms-api.h: enum member '{line}' lacks {PREFIX}")
        out.append((len(out), line))
    return out


def declared_params(text):
    """-> {lowercased struct name: [as declared]} for every NvKms*Params."""
    d = {}
    for name in re.findall(r"struct\s+(NvKms\w+Params)\s*\{", text):
        d.setdefault(name.lower(), []).append(name)
    return d


def resolve(cmds, declared):
    """-> [(n, cmd, struct or None, why)]."""
    out = []
    for n, cmd in cmds:
        words = cmd[len(PREFIX):].split("_")
        # Title case only for the message a human reads. The MATCH below is
        # case-insensitive, because the casing of acronyms is nobody's rule.
        want = "NvKms" + "".join(w.capitalize() for w in words) + "Params"
        hits = declared.get(want.lower())
        if not hits:
            out.append((n, cmd, None, f"no struct {want} (any casing) in the header"))
        elif len(hits) > 1:
            out.append((n, cmd, None, f"ambiguous: {hits}"))
        else:
            out.append((n, cmd, hits[0], ""))
    return out


def sizes(root, structs):
    """-> {struct: sizeof}. Compiled, because a regex is wrong about padding."""
    if not structs:
        return {}
    src = ['#include <stdio.h>', f'#include "{os.path.basename(HEADER)}"', "int main(void){"]
    for s in structs:
        src.append(f'    printf("{s} %zu\\n", sizeof(struct {s}));')
    src.append("    return 0;\n}")
    with tempfile.TemporaryDirectory() as td:
        c, exe = os.path.join(td, "s.c"), os.path.join(td, "s")
        open(c, "w").write("\n".join(src))
        cmd = ["gcc", "-o", exe, c] + [f"-I{os.path.join(root, d)}" for d in INCLUDE_DIRS]
        r = subprocess.run(cmd, capture_output=True, text=True)
        if r.returncode:
            raise SystemExit("nvkmsdecode: sizeof probe did not compile:\n" + r.stderr[:2000])
        out = subprocess.run([exe], capture_output=True, text=True).stdout
    return {ln.split()[0]: int(ln.split()[1]) for ln in out.strip().split("\n")}


def observed(tracedir):
    """-> {cmd number: {psize: count}} from recorded NVKMS calls."""
    seen = {}
    for f in glob(os.path.join(tracedir, "**", "*.jsonl"), recursive=True):
        for ln in open(f):
            if '"modeset"' not in ln:
                continue
            try:
                r = json.loads(ln)
            except ValueError:
                continue
            if r.get("t") != "ioctl" or r.get("dev") != "modeset":
                continue
            try:
                n = int(r.get("sub") or "-1", 16)
            except ValueError:
                continue
            ps = r.get("psize")
            seen.setdefault(n, {}).setdefault(ps, 0)
            seen[n][ps] += 1
    return seen


def table(root="vendor/open-gpu-kernel-modules"):
    """-> {command number: {name, params_struct, params_size}}.

    The entry point for other tools in this directory -- ``ioctlmatrix.py``
    calls it so the catalogue's NVKMS rows carry a name. Commands whose
    struct the header does not declare are present with
    ``params_struct = None``: a row that says "not named" is the honest
    output, and dropping them would hide that two exist.
    """
    text = open(os.path.join(root, HEADER)).read()
    rows = resolve(commands(text), declared_params(text))
    sz = sizes(root, [s for _, _, s, _ in rows if s])
    return {n: {"name": cmd, "params_struct": st,
                "params_size": sz.get(st) if st else None,
                "unresolved": why or None}
            for n, cmd, st, why in rows}


def main(argv):
    root, tracedir, as_json = "vendor/open-gpu-kernel-modules", None, False
    i = 1
    while i < len(argv):
        if argv[i] == "--vendor":
            root = argv[i + 1]; i += 2
        elif argv[i] == "--traces":
            tracedir = argv[i + 1]; i += 2
        elif argv[i] == "--json":
            as_json = True; i += 1
        else:
            raise SystemExit(f"unknown argument {argv[i]}")

    text = open(os.path.join(root, HEADER)).read()
    rows = resolve(commands(text), declared_params(text))
    sz = sizes(root, [s for _, _, s, _ in rows if s])
    obs = observed(tracedir) if tracedir else {}

    result, bad = [], 0
    for n, cmd, struct, why in rows:
        e = {"cmd": f"{n:#04x}", "n": n, "name": cmd, "params_struct": struct,
             "params_size": sz.get(struct), "unresolved": why or None}
        if n in obs:
            e["calls"] = sum(obs[n].values())
            e["observed_psize"] = sorted(obs[n])
            want = sz.get(struct)
            got = [int(p, 16) for p in obs[n] if p]
            e["agrees"] = want is not None and bool(got) and all(g == want for g in got)
            if not e["agrees"]:
                bad += 1
        result.append(e)

    if as_json:
        json.dump(result, sys.stdout, indent=1); print()
        return 1 if bad else 0

    print(f"enum NvKmsIoctlCommand: {len(rows)} commands, "
          f"{sum(1 for r in rows if r[2])} with a params struct in the header")
    if obs:
        print(f"observed in traces: {len(obs)} commands, "
              f"{sum(sum(v.values()) for v in obs.values())} calls")
    print()
    print(f"{'cmd':>5} {'name':<46} {'params struct':<44} {'size':>5} {'calls':>6} check")
    for e in result:
        if tracedir and "calls" not in e:
            continue
        chk = "" if "agrees" not in e else ("psize agrees" if e["agrees"] else
                                            f"MISMATCH: psize {e['observed_psize']}")
        print(f"{e['cmd']:>5} {e['name']:<46} {(e['params_struct'] or '--'):<44} "
              f"{(e['params_size'] if e['params_size'] is not None else '--'):>5} "
              f"{e.get('calls','--'):>6} {chk}")
    un = [e for e in result if e["unresolved"]]
    if un:
        print()
        print("NOT NAMED, and nothing is invented for them:")
        for e in un:
            print(f"  {e['cmd']:>5} {e['name']:<46} {e['unresolved']}")
    if bad:
        print()
        print(f"{bad} observed command(s) disagree with the compiled size -- the decoder is NOT trustworthy")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
