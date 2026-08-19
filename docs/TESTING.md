<!-- SPDX-License-Identifier: MIT -->
# Testing and Measuring

What checks exist, how to run them, and — the longer part — **what you
fall for when measuring**. Confidence markers as elsewhere in this
repository: "Measured:" marks something measured or read in source,
"Unverified:"/"presumably" marks conjecture, "WARNING:" marks a trap that
has already bitten someone.

---

## 1. Two tiers: with GPU and without

| | Command | Duration | Needs |
|---|---|---|---|
| GPU-free | `./scripts/test.sh check` | ~2 s warm | `cc`, `edid-decode` |
| The gates | `./scripts/test.sh gates` | the sum of the three | GPU, VM |
| — compute | `./scripts/test.sh gpu` | ~100 s | GPU, VM, persistence mode |
| — virtual display | `./scripts/test.sh vdisplay` | minutes warm; a fresh instance first builds NVKMS in the guest | GPU, VM |
| — desktop | `./scripts/test.sh display` | ~10 min | GPU, VM, X, Sunshine, `moonlight` |
| The suites | `./probe/run/suites.sh` | ~10 min | GPU, VM, guest venv |

`test.sh vdisplay` is the fast one, and it exists because `test.sh display`
is the wrong gate to run during a refactor: six stages (`setup`, `tables`,
`device`, `edid`, `pixel`, `teardown`) with no X server, no Vulkan and no
streaming. Its `edid` stage is the one thing it does that the desktop gate
cannot: the bytes are read by `probe/c/edid-verify.c`, which shares **no**
code with the module's builder — see the EDID note below. Its `pixel` stage
writes a deterministic frame into a dumb buffer, puts it on the CRTC and
reads it back through `GETFB2` + PRIME export (DRM's cross-driver
buffer handoff), comparing against a hash
committed in `probe/data/vdisp-frame.ref`. The full catalogue, with what it
does not cover, is in [`../DEVELOPMENT.md`](../DEVELOPMENT.md) section 4.

`test.sh display` is the virtual screen: the EDID the module invents against
the bytes the guest's DRM connector carries, a modeset and a page flip read
back off the CRTC rather than trusted, and a known colour that has to survive
three readers (XGetImage, XShmGetImage, ffmpeg's x11grab) plus a colour
SEQUENCE that has to arrive in order. It does not cover glamor: EGL is
off on this path (`eglQueryDevicesEXT` reports zero devices), so the capture
stages measure software rendering. When glamor returns, it needs a stage of
its own rather than being implied by this one.

The suites are NOT a third tier of the gate. They are breadth tests over
real CUDA APIs and deliberately contain a known failure; the gate must stay
binary. `probe/suites/README.md` sets out how gates, probes and suites
relate.

WARNING: **There is no fast GPU-only smoke gate.** The former two-second
smoke gate ran over the LD_PRELOAD path (guest process and host daemon in
the same kernel), which was the only GPU path that needed no VM; it was
removed together with that path on 2026-08-04. The module path
needs a guest kernel by definition, so a "module smoke test" would only be
a smaller GPU gate at nearly the same price — the VM boot dominates. The
fastest GPU gate is therefore `gpu` at about 100 s; the fast GPU-free band stays
`test.sh check`.

`./scripts/test.sh check` runs fourteen steps, one PASS/FAIL line each, one
exit code:

1. `cargo test` over the whole workspace, **debug profile**.
2. `cargo test` again, **release profile**.
3. Doctests (`cargo test --doc`).
4. `cargo doc` with `-D warnings` — rustdoc's own lints (broken intra-doc
   links, unclosed HTML in doc comments), which no compiler step sees.
5. `clippy -D warnings` with a documented allow list.
6. `nvrm-genhdr --check` — the layout guard for `nvrm_wire.h`.
7. `c-interpreter` — the kernel module's table interpreter
   (`nvrm_tables.c`) compiled for userspace via
   `guest-module/virtio_nvrm/test/tabcheck.c`, reading the very byte
   stream that the Rust table builder writes, field by field against what
   the builder says it wrote. Catches struct-layout drift, wrong section
   pointers and broken lookups without a guest kernel and without a VM.
   The same step then runs `test/tabreject.c` over damaged copies of that
   stream — wrong magic, format version, length, counts, checksum, too
   many nested slots — and expects every one refused with a reason: the
   interpreter's defences against an unexpected host, exercised.
8. `edid` — the EDID the virtual display hands out, read by a parser that
   knows the spec. `guest-module/virtio_nvrm/test/edidcheck.c` runs the
   module's own builder (`nvrm_edid.c`, the same translation unit) and
   `edid-decode --check` reads the bytes; any non-conformance is a
   failure. Before the sweep, `test/edidclamp.c` checks the clamp
   arithmetic itself over a matrix up to 8K/240 Hz — the invariants that
   hold when a requested mode exceeds what the EDID can encode. The block is DERIVED from the requested size rather than
   tabulated, so the step sweeps six resolutions -- a fixed range limit is
   right at 1080p and a contradiction at 4K. This found three defects on
   a block nothing had ever parsed: 6 bpc where the comment said 8 bpc, a
   max dotclock ten times too high, and GTF claimed without the
   continuous-frequency bit.
9. `class-sizes` — compares **every**
   alloc-param size in the class table against a `sizeof()` compiled from
   the vendor headers themselves. Most of those sizes are transcribed from
   a header by hand, and a transcribed number is exactly what goes stale
   when the driver version moves. A wrong size is not a failed allocation;
   it is an out-of-bounds read in the driver's `copy_from_user`.
10. `kapi-abi` — the guest module's entry points for kernel-side RM
   calls (RM: NVIDIA's Resource Manager, the driver being forwarded)
   against the driver's own `nv-modeset-interface.h`.
11. `bash -n` over every script. (ShellCheck is deliberately *not* here:
    `test.sh check` runs on laptops that do not have it, and a step that
    silently skips itself is a green claim without a reader. It runs in
    CI instead — [`../DEVELOPMENT.md`](../DEVELOPMENT.md) section 4,
    "Shell hygiene", and by hand with
    `shellcheck -x scripts/*.sh scripts/lib/*.sh`.)
12. `licence` — every tracked file carries an `SPDX-License-Identifier`,
    and the right one: GPL-2.0-only under `guest-module/` (that code links
    against the guest kernel), MIT everywhere else.
13. `dangling-refs` — nothing points at material that is not in this tree.
    Comments used to cite a lab log for the reason behind a magic number,
    and every such citation became a dangling pointer — which is how a
    workaround gets deleted by the next person. They were resolved by
    writing the evidence into the comment instead of a location.
14. `no-markers` — no marker glyphs in comments or prose. This repository
    used to mark every statement with a glyph, one for "measured", one for
    "conjecture", one for "trap"; 846 were removed for publication. Write
    "Measured 2026-08-18:", "Unverified:" or "WARNING:" instead.

WARNING: **Debug and release are not the same program.** `u64` arithmetic
**panics** in the debug profile and **wraps** in the release profile. The
memory corruption found in `Arena::build` was a panic in the debug run
(that is, a different bug) and only showed itself for what it was in the
release run. Release is what ships — which is why `test.sh check` tests both.

`./scripts/test.sh gates [gpu|vdisplay|display|all]` runs the gates one after
another (each wants the GPU exclusively) and summarizes. Every gate ends in
exactly **one** machine-readable line, and stdout carries nothing else:

```
{"gate":"gpu","result":"pass","dur_s":100,"facts":{"stages":[...],"failed":[],...}}
```

Exit 0 pass, 1 fail, **2 skip** — a precondition that was not met, which is
a different statement from a failed measurement. The contract is written out
in full in [`../DEVELOPMENT.md`](../DEVELOPMENT.md) section 4; the
implementation is `lea_gate_*` in `scripts/lib/common.sh`.

WARNING: the `gpu` and `display` gates run without `set -e` (they clean up
after themselves and want every stage to run). `lea_gate_finish` therefore
terminates explicitly via `exit` — a mere return value would be lost there
and the gate would report PASS although a stage had failed. The `vdisplay`
gate is fail-fast instead, because each of its stages is a
precondition of the next.

### What the gate covers

- **gpu** — stage `tables` (table checksum guest == host) through stage
  `torch` (PyTorch bit-identical to the **native** reference on the host,
  same torch version from `vendor/hostvenv`), plus `robustness` (two CUDA
  processes in parallel, `kill -9` without a leak, `rmmod` refused while an
  FD is open, counter-check with the module unloaded). Stage `smi` compares
  `nvidia-smi` masked against the native host run; stage `own` checks the
  VM's own process list; stage `encode` encodes 1080p h264 with NVENC in
  the guest and hardware-decodes it back, against the native run's
  bitstream size.
  WARNING: The gate enforces the rig check (`showcase.sh state --check`) up front: the native
  reference depends on persistence mode (§3.2).
  WARNING: `smi` and `own` are separate stages because `smi` **cannot** see
  the process list -- its mask drops it (`/| Processes:/,$d`), for the good
  reason that host PIDs do not exist in the guest. The entire guest-visible
  process list could be dead and `smi` would stay green.

- **vdisplay** — the virtual display without the desktop on top of it.
  Stage `tables` asserts the descriptor tables against named constants AND
  against the backend's own log line, so a translation surface that moved
  during a refactor is a finding rather than a silent difference. Stage
  `edid` reads the connector bytes with `probe/c/edid-verify.c`, which
  shares no source with `nvrm_edid.c` — the gap the `display` gate's byte
  comparison cannot close, measured by mutating `e[10]` and watching it
  pass. Stage `pixel` proves the buffer round trip; it does **not** prove
  anything about rendering or scanout.
  WARNING: the FIRST run on a fresh instance builds `nvidia-modeset.ko` and
  `nvidia-drm.ko` in the guest and takes minutes. The overlay disk is kept
  between runs for exactly that reason; `--fresh` pays for it again.

`gpu` is the only GPU *compute* gate. The gates that existed alongside it until
2026-08-04 tested the LD_PRELOAD path and the virtio-gpu carrier, and fell
with those carriers. The correctness statements they made
(`nvidia-smi` across the boundary, kernel execution including the semaphore
pool, PyTorch bit-identical) are all checked by the `gpu` gate over the one
remaining carrier.

Measured: after changes to the module, to the host or to `session.rs`, run
the **gpu** gate.

The numbers in this document come from runs recorded in
[`OPEN-QUESTIONS.md`](OPEN-QUESTIONS.md), with the measurements behind them
in [`llm.md`](llm.md).

---

## 2. The fuzz corpus

`crates/vhost-user-nvrm/fuzz/` — target `handle_msg`, and the attacker
model is **the guest**. The session is given two tokens on memfds so that
the path runs through *all* checks up to immediately before the real
ioctl; the ioctl itself is not executed (it would otherwise test the
NVIDIA driver instead of this code).

```
cargo +nightly fuzz run handle_msg -- -max_total_time=3600 -max_len=131072
```

Measured: 136,520,904 executions in 3601 s, 587 coverage edges,
**0 crashes, 0 artifacts**.

The corpus is **real, not synthetic**: a run of the GPU gate with
`LEA_CAPTURE_DIR=<dir>` captured 9287 messages, `cargo fuzz cmin` boiled
them down to coverage-equivalent representatives, and the fuzzer added
more.

Measured: **it also carries without nightly.** The test
`the_fuzz_corpus_still_goes_through` replays the corpus in every `cargo
test`. The 126 coverage-minimised representatives (`cargo fuzz cmin` over
the 9287 captured messages, plus what the fuzzer found) are committed
under `fuzz/corpus/handle_msg/`, so the replay runs in every checkout,
costs milliseconds, and catches exactly the regression a restructuring
makes likely — a message that used to go through now panics. A larger
corpus is captured on a rig with `LEA_CAPTURE_DIR` and minimised the same
way.

WARNING: The capture hook sits in the production path but is off: the
environment variable is read **once**, not per message. `std::env::var_os`
scans `environ` linearly and takes a lock — at 12 µs per forwarded ioctl
that would distort precisely the number this rig measures.

---

## 3. Measuring — and the traps

### 3.1 The rig state is part of the result

```
./scripts/showcase.sh state            # state as one line
./scripts/showcase.sh state --check    # additionally: is the rig ready to measure?
```

Every `bench.sh` measurement calls the check **before** measuring and
writes the state to `<outdir>/rig.txt`; `bench.sh transport` additionally
writes persistence **per line** into the CSV (column 12). `bench.sh
summary` **aborts** when a run mixes states, and prints the rig state
above every summary.

WARNING: **Why this is enforced mechanically instead of being left to
care** — three mismeasurements, all from the same family, have already
gone through in a single day:

1. **`strace -c -w` over *all* syscalls** instead of filtered. strace's own
   per-call surcharge inflates the numbers: it looked like "2 ms per
   ioctl"; filtered with `-e trace=ioctl` it is **51 µs** median. A factor
   of 40.
2. **Persistence mode was `Disabled`.** That alone moved native `cuInit`
   from 132 to 209 ms (**+58 %**). The wrong number almost made it into
   the documentation as a target figure.
3. **Two measurement frames mixed**: guest numbers from the bench harness
   against a native number measured freehand in a shell. That produced a
   "correction" which had to be withdrawn.

The countermeasure is not an admonition but mechanics: carry the state
along, reject mixtures loudly, print the state above every output.

### 3.2 Persistence mode, concretely

Measured, native `cuInit`:

| State | Wall clock | ioctl total | Largest call |
|---|---|---|---|
| Persistence off | 209 ms | 145.8 ms | 104.9 ms |
| `nvidia-smi -pm 1` | 174 ms | 99 ms | 82 ms |
| plus `nvidia-persistenced` | **132 ms** | **33 ms** | **26 ms** |

The reason: without persistence the driver tears the GPU state down after
the last client, and every `cuInit` pays to rebuild it. The host daemon
holds RM clients open anyway — **so the guest path gets that warmth for
free and the native run does not.** Measuring without persistence compares
a cold native run against a warm guest run.

Measured: persistence shifts the **absolute values**, not the **gap**: the
module path's surcharge over native was +26 ms with persistence off and
+29 ms with persistence on.

Unverified: the CPU governor on this machine is `powersave` and is **not**
pinned — it goes into the rig line so that it is visible, but whether it
shifts the numbers is unmeasured.

### 3.3 How to measure correctly

- **Interleaved and rotating.** `bench.sh transport` runs all variants per round,
  in changing order. Otherwise one variant would always land on the cold
  card and another on the warm one (the card warms from 62 to 72 °C over a
  measurement session).
- **Median and p10/p90, never the mean.** The distribution is skewed; a
  compaction run or a second of desktop load shoots a single value up.
- **Correctness columns next to every time column.** `convburn acc`,
  `rlprobe mean10`, `mmsweep chk` must be identical across all variants. A
  fast wrong answer would be no answer.
- **Count counts, measure times.** `bench.sh diag` counts what crosses
  the boundary and deliberately prints **no** wall clock: it runs with
  `LEA_DEBUG=2`, and the log is itself expensive.

### 3.4 The probes

| | measures |
|---|---|
| `probe/c/ioctlping.c` | Round trip per ioctl. Third argument `pause_us` — hot loop versus scattered calls |
| `probe/c/ctrlping.c` | The same with growing payload |
| `crates/nvrm-client/src/bin/mmapping.rs` | Round trip per **window mapping** (`RM_MAP_MEMORY` + `mmap` + `munmap`) |
| `probe/c/managedprobe.c` | Managed memory in stages, with a correctness check |
| `probe/python/vramcap.py` | Drives the VM's VRAM cap red and allocates again afterwards. Checks the *kind* of refusal too: anything other than `torch.cuda.OutOfMemoryError` is reported as `WRONG-ERROR` |
| `crates/nvrm-client/src/bin/smipids` | The two controls `nvidia-smi` builds its process list from, asked directly and printed. Runs on the host and in the guest, so the two answers are comparable. WARNING: `GET_PIDS` needs `id = NV20_SUBDEVICE_0`; with `id = 0` the table comes back empty, which looks exactly like "nothing is running" |
| `scripts/bench.sh transport` | The measurement track, `--loads` selects, `--minutes`/`--hours` |
| `scripts/bench.sh fleet` | The same discipline on 1/2/4 VMs, `--load convburn\|managed\|ping` |
| `scripts/bench.sh render` / `stream` / `diag` / `vk` | GL under N-way contention; one Moonlight session; what crosses the boundary at setup; vkmark on the virtual display (new, unmeasured) |

WARNING: `--hours` wants a **whole** number. `--hours 0.25` used to yield a
deadline of *now*, silently: the rig booted, measured zero rounds and shut
down again. Fractions are now rejected loudly — use `--minutes` for
anything shorter.

Measured: **the cold queue, quantified:** the same ioctl costs 13.5 µs
through the guest module in a hot loop and 52.7 µs with a 5 ms pause
between calls. Natively the effect is small (0.75 → 1.55 µs). Extrapolating
transport numbers from a hot loop to a real program extrapolates too
favourably.

---

## 4. Traps in the rig itself

WARNING: **`pkill -x`, never `pkill -f`.**

WARNING: One `vhost-user-nvrm` backend serves exactly **one** VM
connection. Killing it while the VM runs makes cloud-hypervisor exit
immediately — which looks like a guest crash and is not one.

WARNING: Always shut the VM down via `./scripts/showcase.sh down`, otherwise
empty SSH host keys are left behind.

WARNING: `LEA_MANAGED_COMPAT=1` is a **host** switch (read in
`session.rs`), not a guest switch. Set in the guest it does nothing.
`showcase.sh up` passes it through to the backends.

WARNING: **Fleet VMs 1–3 do not automatically carry the current probes.**
`showcase.sh up --count N` provisions every member from the current tree
(the payload is re-sent only when it changed). Whoever measures a new probe provisions
all fleet VMs — otherwise it looks as if three of four VMs had failed.

WARNING: Do not edit a running script (bash keeps reading it
incrementally). A process that is killed takes its stdio buffer to the
grave — use `stdbuf -o0`, and `--line-buffered` for `grep` in a pipe.

WARNING: `VAR=$(function)` is a subshell: assignments to arrays inside it
are gone on return. And writing markers into a log file that another
process holds open does not work — that process writes over them at its
own offset.

WARNING: **`mv file.bak file` restores the content and the OLD mtime.**
cargo then sees a source file older than its own artifact and does not
rebuild -- the next `cargo test` silently runs the **previous, mutated**
binary. Found while doing §5 below: after restoring the source, a test kept
failing that the source could not make fail. `touch` after every restore,
and prove the tree is green again between two mutations, or the whole
exercise reports on a binary nobody has.

WARNING: `grep -oP '…\K…'` needs `\K` inside **single** quotes and `\\K`
inside double quotes. With the wrong one grep finds **nothing**, and the
target column stays empty while the raw data was there all along.

---

## 5. What a new test must satisfy

Measured: **a test that was never red is not a test.** Every new one is
demonstrably broken once — revert the line it checks, watch the test fail,
put the line back. Twice this exercise left something green, and both times
*that* was the finding: unreachable lines of defence, which are marked as
such in the source ever since.

Where a barrier is unreachable by construction, that belongs on the line as
an honesty note — together with the condition under which it becomes
effective again. "Cleaning it up" is exactly the change a refactor makes.
