#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""vramcap - drive the VM's VRAM cap red, then prove it lets go again.

Three questions, in order:

  1. Does the allocation stop at all? Blocks of NVCAP_BLOCK_MIB are
     allocated until something refuses.
  2. Is the refusal an ordinary CUDA OOM? A cap that surfaces as anything
     other than torch.cuda.OutOfMemoryError is not a cap but a crash --
     torch raises AcceleratorError for a foreign error, and that is not
     something a workload can catch and adapt to.
  3. Does the counter come back down? Everything is freed and two blocks
     are allocated again. If the host only ever counted up, this fails --
     which is the failure mode a VRAM cap has to be tested against.

Runs unchanged natively and in the guest. Without a cap set it simply
measures the card, which is the counter-check.

  NVCAP_BLOCK_MIB  size of one block, default 256
  NVCAP_MAX_BLOCKS bound, so a run without a cap ends too, default 64
"""
import gc
import os

import torch

BLOCK_MIB = int(os.environ.get("NVCAP_BLOCK_MIB", "256"))
MAX_BLOCKS = int(os.environ.get("NVCAP_MAX_BLOCKS", "64"))

dev = torch.device("cuda")
elems = BLOCK_MIB * (1 << 20) // 4          # float32


def grab(n):
    """n blocks, or as many as go in. Returns (blocks, how it ended)."""
    out = []
    for _ in range(n):
        try:
            t = torch.empty(elems, dtype=torch.float32, device=dev)
            t.fill_(1.0)
            out.append(t)
        except torch.cuda.OutOfMemoryError:
            return out, "oom"
        except Exception as e:                      # noqa: BLE001
            # Deliberately caught and named: this is the interesting
            # failure. Anything that is not an OutOfMemoryError means the
            # refusal did not arrive as a CUDA OOM.
            return out, f"WRONG-ERROR {type(e).__name__}: {e}"
    return out, "limit"


torch.cuda.init()
blocks, how = grab(MAX_BLOCKS)
first = len(blocks)
print(f"vramcap: phase1 blocks={first} ({first * BLOCK_MIB} MiB) end={how}", flush=True)

if how.startswith("WRONG-ERROR"):
    print(f"vramcap: FAIL {how}")
    raise SystemExit(1)
if how == "limit":
    print(f"vramcap: no limit hit within {MAX_BLOCKS} blocks "
          f"({MAX_BLOCKS * BLOCK_MIB} MiB) -- nothing refused")

# Give it all back. If the host's books only ever went up, the next
# allocation fails even though the card is empty.
blocks.clear()
gc.collect()
torch.cuda.empty_cache()

again, how2 = grab(2)
got = len(again)
print(f"vramcap: phase2 after free blocks={got} end={how2}", flush=True)

ok = first > 0 and got == 2
print(f"vramcap: first={first} again={got} block_mib={BLOCK_MIB} "
      f"ok={1 if ok else 0}")
raise SystemExit(0 if ok else 1)
