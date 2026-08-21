<!-- SPDX-License-Identifier: MIT -->
# Notes for working on this code

Context worth having before changing something here, and worth handing to
an assistant before a bug hunt: the rules the code follows, the traps that
have already cost someone a day, the numbers that are measured rather than
assumed, and the hypotheses that were tested and turned out wrong.

None of this is required reading. If a change needs a fact from here, the
better fix is usually to put that fact in the code, next to what it
explains. Treat a growing section in this file as a sign that a comment is
missing somewhere.

Question numbers below refer to [`OPEN-QUESTIONS.md`](OPEN-QUESTIONS.md).

---

## 1. Rules this code follows

**One number lives in one place.** The guest kernel module contains no
NVIDIA constant at all — no escape number (an escape is an RM ioctl), no
struct size, no field
offset. The host serialises a descriptor table out of
`crates/nvrm-abi/src/xlate.rs` at startup and the module interprets it.
`xlate.rs` is the source of truth; `table.rs` queries it across its whole
key space rather than keeping a second list that could go stale.

**Generated files are generated, not maintained.**
`guest-module/virtio_nvrm/nvrm_wire.h` comes out of `nvrm-genhdr` with a
`_Static_assert` per field offset, so the module cannot be compiled
against a stale layout. Regenerate and commit it with the change:

    cargo run --release --bin nvrm-genhdr -- guest-module/virtio_nvrm/nvrm_wire.h

**A comment says why, not where.** This code carries a lot of prose
deliberately. Shorten what is wrong or duplicated, not what is thorough. If
a comment states a number, the number belongs in the comment together with
what measured it.

**Question numbers are permanent.** They appear in about fifty code
comments and in commit messages. Never renumber, never reuse.

**`vendor/` is not ours.** It is fetched by `scripts/build.sh vendor` at the
version in `DRIVER_VERSION`, and nothing in it is edited. A change made
there is invisible to every gate and disappears on the next fetch.

**Everything is English** — prose, code, comments, commit messages.

**Say what is measured.** Where a statement rests on a run, write
"Measured 2026-08-18:" and the number. Where it is a conjecture, write
"Unverified:". The distinction is the whole reason this project's notes
are worth anything.

## 2. Gates

| Gate | Needs | What it proves |
|---|---|---|
| `scripts/test.sh check` | nothing special | Builds, tests, clippy, licence headers, the generated header, the module's table interpreter against the real stream, EDID conformance, shell syntax, no dangling references. Runs anywhere. |
| `scripts/test.sh gpu` | a real card | The compute path end to end: tables, `nvidia-smi`, RM mmap (RM: NVIDIA's Resource Manager, the forwarded driver), UVM pool (UVM: its unified-memory driver), PyTorch bit-identical to a native run, robustness. |
| `scripts/test.sh display` | a card and a guest desktop | The display path, twelve stages. |

`test.sh check` runs `cargo test` in **both** debug and release. That is not
redundancy: `u64` arithmetic panics in debug and wraps in release, so they
are not the same program. A memory-corruption hole in `Arena::build` was
only visible as itself in a release run; in debug it was a panic, which is
a different bug.

## 3. Traps

Each of these cost real time.

**An empty `LEA_DEBUG` switches debug output on.** `env::var_os(...)
.is_some()` is true for `LEA_DEBUG=`, because an empty value is still a set
variable. The careful-looking shell idiom `LEA_DEBUG="${LEA_DEBUG:-}"`
therefore always sets it. The rig ran with full debug output on a
per-frame path — 86 645 waiter lines and 35 759 ioctl status lines in one
session — and every measurement taken before the fix included that cost.
Test with `is_some_and(|v| !v.is_empty())`.

**And it was in a second switch until 2026-08-21.** `LEA_FD_CENSUS` was read
with `var_os(..).is_none()`, so the fd census ran on **every rig anybody ever
brought up** — `rig.sh` passes `LEA_FD_CENSUS="${LEA_FD_CENSUS:-}"` like the
other seven, and the census hangs off `PROC_GONE`, once per guest process
exit, doing a `read_dir` of `/proc/self/fd` and a `read_link` per fd. Its own
doc says the switch exists because that path "must not be free either".
Measured in both directions before it was believed: old binary with the
variable unset, 70 census lines on a desktop rig; new binary unset, 0; new
binary with `LEA_FD_CENSUS=1`, 6 on the same workload — the third row is the
one that proves the path still runs.

When you find this trap, **sweep the neighbours**: of the eight `LEA_`
variables `rig.sh` passes with that idiom, that was the only remaining one.
The others compare against `"1"`, parse a value, or already test for
emptiness — `LEA_OBJLOG`, the nearest neighbour in intent, always had it
right. The lesson is not "the idiom is banned"; it is that the shell's
`${X:-}` and the reader's `is_some()` are a matched pair of mistakes and you
have to check the reader, one switch at a time.

**Never wait on `pgrep -f`.** The pattern stands in the waiting shell's own
command line, so the loop waits for itself. Wait on the pidfile:

    until ! lea_running vm/test-check.pid; do sleep 15; done

The same trap bites `pkill -f`, and harder: it matches the killing command's
own line and kills the shell that ran it. Measured twice on 2026-08-21, once
against a soak script and once against a sampler. Kill by the pid you
recorded when you started the thing, never by a pattern.

**`pgrep -x cloud-hypervisor` never matches, and answers 0 forever.** A
process's `comm` is capped at 15 characters (`TASK_COMM_LEN`), so the name
the kernel stores is `cloud-hyperviso` and an exact-match query for the
16-character spelling cannot hit it. On 2026-08-21 that check was used
repeatedly to report "0 rigs up"; it was right by luck every time, because
the rigs really were down, and it would have said exactly the same thing
with two VMs running -- as it eventually did. Ask
`scripts/showcase.sh status`, which reads the pidfiles, or match the
truncated name deliberately. The same applies to any binary whose name is
15 characters or longer.

**A gate can kill another gate's backend.** A run once sent `SIGTERM` to a
desktop instance's backend. cloud-hypervisor stayed up, so it did not look
like a kill at all — the guest simply hung forever in the next RM call.
`lea_foreign_rigs` in `scripts/lib/rig.sh` exists to prevent this.
If a guest hangs with a healthy-looking VM, check whether its backend is
still alive before anything else.

**The host prints one `dmesg` line per ioctl.** At `ResmanDebugLevel: 0`
the driver still prints `NV_DBG_INFO`. The line looks like a rejection and
is not. It costs time on a per-frame path, and 2734 of them displaced every
other diagnosis from the ring buffer. Check the debug level before reading
host `dmesg` as evidence.

**A client's FPS counter is not evidence of presentation.** `glxgears`
reported 58.3–58.8 FPS while a person watching the screen saw the gears
standing still. A client counts swaps; whether a frame reaches the screen
is not something it can know. Any presentation claim needs a reader that
looks at pixels, or a human.

**An Xwayland instance is a consumable.** The crash in number 23 depends on
process state and gets worse over an instance's life. Take a fresh
instance per measurement point, or you are measuring its age.

**Do not decode DRM ioctls in the tracer.** Decoding one reads a foreign,
usually smaller struct at NVIDIA offsets — an over-read that has happened
(88 bytes past a foreign struct). The full contract sits on the decoder
itself, `crates/nvrm-trace/src/log.rs`; DRM lines carry `nr`, `size` and
`ret`, and stop.

**Check the params pointer before the class lookup.** The first version of
the module looked `hClass` (the RM class id of the object being
allocated) up in the table before looking at the params
pointer. `NV01_ROOT_CLIENT` (hClass 0) has no alloc params and is
therefore in no table, so `cuInit` got `EOPNOTSUPP` on its very first
allocation. The order is semantics, not style.

**Charge guest pages, do not just allocate them.** The UVM pool path
allocates real guest pages. With plain `GFP_USER` it did so until the
machine was empty and the OOM killer fired. It now uses
`__GFP_RETRY_MAYFAIL|__GFP_NOWARN` plus a quota (`nvrm_charge`, capped by
`max_pin_mib`), so asking for too much returns an honest `ENOMEM` and the
neighbour is left alone.

**A cleanup can delete the deadline it is waiting on.** The frame limiter
once appeared not to work at all, because a tidy-up path removed the
deadline before it could be observed. Symptom: a mechanism that is clearly
built, clearly reached, and has no effect.

## 4. Numbers that are measured

These are results, not targets. Where one of them constrains the code, the
code says so at the point it constrains.

### Limits

| Limit | Where | Default | Shape |
|---|---|---|---|
| `max_pin_mib` | guest module | 1024 MiB | total across all pins |
| `LEA_MAX_PIN_MIB` | host backend | 256 MiB | **one** pin |
| `LEA_VRAM_LIMIT_MIB` | host backend | off | per VM, device memory only |

The smaller limit is the host one, and its error reads like a refusal:
`cudaHostRegister` past either returns 304 (`cudaErrorOperatingSystem`).
Measured 2026-08-16: a single 512 MiB pin fails with the guest limit at
its 1024 MiB default and only 10 MiB pinned, because the host refuses one
arena over 256 MiB. Raising the guest limit alone changes nothing; the
backend log is the only place the two are told apart.

### The VRAM ledger

Under a 4096 MiB cap the books track the card to within **214 MiB**, and
that remainder is device memory RM allocates itself behind a channel
(context buffers, USERD — a channel's doorbell page) which never crosses
the boundary as a request.

What the cap mostly does is not refuse: CS2 on an 8 GiB card takes 4.8 GB
uncapped and 3.1 GB under a 4 GiB cap **without a single allocation being
refused**. Told the truth about what is left, a streaming engine sizes
itself to it. The refusal path exists and no measured workload has reached
it.

### The event back-channel

Building the second virtqueue was the single largest improvement to the
display path:

| | polling | woken |
|---|---|---|
| `fencetime` | 10.10 ms | **0.12 ms** |
| Sunshine frame time | 62 ms | **6 ms** |

Under load — a game and a live stream together — 43 000 events per second
were delivered without a single ring-full drop. The four drop reasons each
have a counter under `/sys/module/virtio_nvrm/parameters/stat_events_drop_*`.

### Sharing one card

- Four VMs, `convburn` in each: 95.4 ms/it each against 24.4 ms/it alone.
  That is 3.9x at four VMs, i.e. fair time-sharing, and all four results
  bit-identical.
- Two guests of *different* kinds: one played CS2 on the virtual display
  while the other ran PyTorch on the same RTX 2070. Both stayed correct,
  the display path unmoved at 60.1 FPS, and the price of sharing was
  15–20 % of throughput — not correctness.

### Rendering and presentation

- `glmark2 --off-screen`: host native 21 486, one guest 21 277, i.e. 99.0 %.
  Off-screen renders into an FBO, so this is render cost with no
  compositor, no presentation and no X server in the path.
- Presentation cost model:
  `cost/frame = 3.49 ns × window area + 1.57 ns × output area + 0.13 ms`.
  At 1080p full screen that is 7.24 ms for the client present plus 3.26 ms
  for compositor and scanout.
- The frame limiter lands close to its target across the range: at 30, 60
  and 120 Hz, `vkcube-wayland` measured 29.0, 53.9 and 99.5 FPS. X11
  `vkcube` escapes it sometimes, not always.
- With the late-unregister race fixed, `Failed to acquire the EGL Image`
  went from 624 occurrences to zero in a comparable session, and CS2 ran
  at about 58 FPS against a 60 Hz target.

### The descriptor table

As of 2026-08-19 (`nvrm-genhdr --dump-tables`): 6932 bytes, 66 ioctl
entries, 128 classes (18 of them verified on Turing silicon, the rest
derived from `resource_list.h` and flagged), 17 controls, 16 nested
pointers, checksum `0xf21e2edb`. The `tables` stage of the gpu gate checks
guest and host agree on the checksum at every run; when it was first
measured the stream was 3632 bytes with 64/18/4/7 and `0x89d45e9c`.

### Protocol version

`PROTO_VERSION` is **6**. The bump rule — meaning-changes bump, moved
offsets are caught by size checks, purely additive kinds do not bump —
lives next to the constant, in `crates/nvrm-wire/src/lib.rs` and that
crate's README.

## 5. Hypotheses that were tested and are wrong

Recorded so nobody spends a night re-deriving them.

**crosvm's GPU knobs did not change presentation cost** (measured while
the interim virtio-gpu display existed; crosvm left the tree on
2026-08-18). `udmabuf`, `external_blob`, `system_blob`,
`fixed_blob_mapping` and `wsi=vk` were swept over six configurations. The
slope was 3.58 ns/pixel before and 3.45–3.67 after — inside the
run-to-run spread. None of them did anything.

**The "field diff" correlation for the GLX crash was a coincidence.** A
difference between native and guest parameter fields looked like the
cause and was withdrawn after a counter-test with equal load on both
sides. The allocation is in fact **identical** between native and guest —
a negative result that closes a whole direction.

**The vblank thread is not the cause of defect 22-A.** Excluded by an A/B
test with and without the patch.

**The black stream is not a counter problem.** `fbprobe` established that
the framebuffer content is carried, CUDA included. The blocker is the
compositor not repainting.

**The GLX crash is not a wrongly mapped address.** Both faulting
instructions dereference offset 8 of a base pointer that is exactly zero.
It is a NULL pointer, and it depends on process state rather than on the
client.

**`GET_SURFACE_PHYS_PAGES` is not why CS2's window was empty.** The
timestamps of the two error kinds do not coincide, and no RM call failed
while the FBO errors were occurring.

**The three controls before the SIGFPE are not the zero.** `ZCULL_INFO`
and the two beside it answer completely, with plausible values.

**FBConfig 0 was not a GLX bug.** Every "broken GL" measurement of that
day was taken after the SHMEM channel had died; FBConfig 0, "no available
drivers", llvmpipe compositing and the laggy desktop were one bug.

**Sessions per VM is wrong for pool state.** Per-VM is right for tokens
and GPU addresses, but `PoolState.pools` is keyed on GPU virtual address
and `libcuda` places every process's semaphore pool at the same address
(`0x204a00000`). Per-VM sessions therefore wrote the second process's
semaphore into the first process's dead arena — the second CUDA process
hung in a user-space loop while the first ran fine. Sessions are per guest
process now.

## 6. Display path: derivations worth keeping

The short version of this half is in [`DISPLAY.md`](DISPLAY.md). These are
the derivations behind it — the parts that took measurement to get right
and would otherwise be re-derived.

### Event ring sizing, measured twice

128 entries overflowed under CS2: 106 000 ring-full drops in one
deathmatch. 1024 held a running desktop but not the session start —
sampling the counter every two seconds across a fresh desktop `up` put
every one of 18 310 drops into a single 26-second window about two minutes
after boot, around the gdm handover and the X restart, and none before or
after. 8192 holds both, counter-measured at 43 000 events/s with CS2 and a
live Moonlight stream together, with an 80 415/s spike as the game starts
and `drop_ringfull` at zero throughout.

The rate matters more than it looks. The stream alone is only about
2900 events/s and an idle desktop about 100/s. It takes the game **and**
the stream together to reach a rate that breaks a ring — which is why a
burst test without a connected client under-tests this path.

### Raytracing initialisation: four causes, only the last fatal

`vkCreateDevice` with `VK_KHR_acceleration_structure` needed all four
fixed:

1. `GET_ACTIVE_DEVICE_IDS` (0x288) — a host `gpuId` leaked through a
   12-byte stride the array mediation could not express.
2. `GET_P2P_CAPS_MATRIX` (0x13a) — two `gpuId` arrays in the *question*,
   so the host answered `INVALID_ARGUMENT` for the guest's id.
3. `UVM_INIT_FLAGS_MULTI_PROCESS_SHARING_MODE`, forced on every
   `UVM_INITIALIZE`, cost pageable memory access — which the RT
   initialisation requires. It is not forced any more.
4. `libnvidia-rtcore.so` was never staged into the guest. The driver
   `dlopen`s it right after the 4 GiB VA reservation, and only with the RT
   extension enabled: twelve `ENOENT`s in `strace`, and invisible to every
   RM trace, because no RM call is involved.

Number 4 is the useful lesson: a missing **file** produces no failing RM
call, so an RM trace cannot see it. When every forwarded call succeeds and
the thing still does not work, trace `openat`.

### The host-visible window

1 GiB was never sized for a game. CS2 mapped 938 MiB into it and its 129th
mapping failed. It is 8 GiB now, and a full one says so in the guest's
own log (`window full`).

### Why there is no copy-based fallback for the window

Asked seriously, because it would remove the need for the shmem patch
(`patches/0001-generic-vhost-user-shmem.patch`): could mappings be
**copied over the data path**
instead of mapped? It is buildable and the price disqualifies it.

The obstacle is not bandwidth. It is that **there is no flush point**: a
mapping is used by the CPU with ordinary loads and stores, and the RM ABI
has no "I am done writing" call to hook. A copying implementation could
not copy on demand — it would have to *trap* writes, i.e. write-protect
the guest's view and take a fault per page (`userfaultfd`, or `mprotect`
plus `SIGSEGV`) at 4 KiB granularity.

Order of magnitude, unverified: a fault round trip is single-digit
microseconds, so roughly 5 µs per 4 KiB is about 1.2 s per GiB touched —
against a mapping, where the same access is a load. Three orders of
magnitude on the hottest path there is, plus a wire that carries 16 KiB
per message into a window that is 8 GiB today.

That is worth recording for what it says about the design rather than the
feature: **the shared window is not an optimisation that could be traded
for portability.** A transport that carries ioctls but cannot map is a
different architecture, not a degraded mode of this one.
