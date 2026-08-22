<!-- SPDX-License-Identifier: MIT -->
# Two games on one card (OPEN-QUESTIONS 70, 71)

Two `RTX2070-4Q` guests, 3072 MiB each, running Shadow of the Tomb Raider's
built-in benchmark AT THE SAME TIME on one RTX 2070, streamed out through
Sunshine/Moonlight. The first workload on this rig from the class that can
adapt -- a game with texture streaming -- which is what numbers 70 and 46
had no data for.

509 host samples at 1 Hz. `MARK-*.txt` cut the session into windows.

|  | 720p | 1080p |
|---|---|---|
| card used, max | 7411 MiB | 7771 MiB |
| card free, MIN | 361 MiB | **1 MiB** |
| .15 charged | 2919 | **3130** |
| .17 charged | 2928 | **3184** |
| combined | 5840 / 6144 | **6243 / 6144** |
| verdict | ran through | second scene would not load |

**720p ran through on both.** **1080p did not** -- and the reason is not
what it looks like. `refusals.csv` is EMPTY: the ledger never answered
`NV_ERR_NO_MEMORY` once, in either guest, in either run. What ran out was
the CARD, at 1 MiB free, because each guest cost ~110 MiB more than its
profile promised (number 71).

Two things this run also settled, recorded so they are not re-chased:

  * **The frozen guest was not frozen.** `.15` showed a still picture while
    its Sunshine log kept emitting frames (`Frame 50853`, IDR keyframes,
    14.5 ms average latency) and its GPU sat at 77-86 %. A loading screen
    that could not finish, not a wedge -- so this is NOT a reproduction of
    number 67, and NVKMS reported zero allocation failures in both guests.
  * **The host ran out of RAM, not the guests.** 415 MiB free and 6.3 GB of
    swap in use, load 13 on 16 threads, with two 8 GiB VMs and two qcow2
    overlays in page cache. Asset streaming for a new scene is exactly the
    load that finds that.

`sample.sh` is the sampler; columns are in its header.
