<!-- SPDX-License-Identifier: MIT -->
# Questions

Every question this project has had to answer, numbered. The numbers are
cited from about fifty code comments and from commit messages, so they are
permanent: a question keeps its number forever, and numbers are never
reused. Sections are short on purpose. Where a resolved question left
measurements worth keeping, they are in [`llm.md`](llm.md).

Status is one of **Open**, **Resolved**, **Decided**, **Superseded by N**,
**Withdrawn**.

Numbers 1-13 were written in the compute phase and are all settled.
Numbers 14 onward are mostly display and rendering, which is the younger
and thinner half of the project.

---

## Open

### 9. NVKMS wedges after a killed X server plus a module reload
**Open, half fixed 2026-08-16.** After hours of X and Steam, killing the
rig's X server left the next one unable to start. The backend half is
fixed and measured — a probe mmap closes the brick cycle. Two things are
still owed: a change in cloud-hypervisor
(`virtio-devices/src/vhost_user/mod.rs:406-419`) so a failed `shmem_map`
does not kill the worker, and a taxonomy of which 512 KiB mapping Steam's
probe is attempting.

**The cloud-hypervisor half is done and MEASURED, 2026-08-21.**
`patches/0003-generic-vhost-user-refused-request.patch`.

*The mechanism, read out of the code.* `FrontendReqHandler::handle_request()`
(vhost 0.16) maps a handler failure to `Error::ReqHandlerError` and then --
before returning it -- calls `self.send_ack_message(&hdr, &res)?`. So the
backend has ALREADY been told its request failed, over the protocol, and the
two ends are still in step. cloud-hypervisor's `handle_event` then treated
that error like a dead socket: it set `disconnected` and returned
`EpollHelperError::HandleEvent`, which terminates the epoll worker. One
refused mapping, and the device is deaf for the life of the VM.

*The A/B.* `LEA_TEST_SHMEM_MAP_OOB` (a one-shot test hook in the backend, see
`on_map_prepare`) makes the FIRST `MapPrepare` ask the VMM to map one window
past the end. The VMM bounds-checks it -- patch 0001 does that -- and refuses.
Same knob, same probe, only the binary differs:

| | probe 1 (sabotaged mapping) | probe 2 (legitimate) |
|---|---|---|
| CH without 0003 | fails | **fails** |
| CH with 0003 | fails | **`stage 0/1/2 ok`, rc=0** |

and the backend log is the clearer half. Without the patch:

    SHMEM_MAP: Frontend internal error          <- the one refusal, correct
    SHMEM_MAP: socket is broken: Broken pipe    <- the NEXT, legitimate map

With it, the refusal is followed by ordinary operation resuming
(`pool @0x204a00000 ... attached to GPU VA`). One refused mapping costs one
mapping now, and not the device.

*Why the hook exists at all:* the backend's own bounds check means a
well-formed request never asks the VMM for something outside the window, so
the branch where the VMM says no is unreachable in ordinary running. That is
also why this defect survived so long -- and why the probe mmap fix above,
which stopped the backend asking for the impossible, was enough to close the
brick cycle without the VMM ever being corrected.

*Honest scope.* This is defence in depth rather than the cause of the
original wedge: with the backend half fixed, the guest no longer walks into
the refusal by itself. What the patch removes is the class -- any refusal
from the VMM, for any reason, taking the whole device with it. The window
filling up is the reachable case that is not a bug in anybody's code, and
number 31 measures 1015 window mappings held for dead clients, so it is not
hypothetical.

**Still owed, and it is the whole of what is left here:** the taxonomy of
which 512 KiB mapping Steam's probe is attempting.
### 14. Fence waits sometimes fall back to the polling timer
**Open, rarer since an unrelated fix.** A woken guest answers a fence in
tenths of a millisecond. A polling guest answers in exactly 10.07 ms,
because that is the fallback timer. The `events` stage of the display gate
counts how many waits took the fallback, so a regression is visible rather
than merely slow. It has not been traced to a cause.

**REPRODUCED 2026-08-21, and it is a minority of waits rather than a mode.**
One instrumented soak, shared with numbers 15 and 31 so all three carry the
same session: a GNOME **Wayland** desktop guest, up ~30 minutes, under
repeated `glmark2` + `vkmark` + `vkcube` load on the compositor's own
Xwayland, `LEA_DEBUG` deliberately UNSET (it sits on a per-frame path and
would be measuring itself).

`fencetime`, four runs of ten waits, **40 samples**:

    3.03  1.52  0.22  0.30  0.06  0.06  0.05  0.05  0.05  0.05
   10.09  0.33  0.25  0.08  0.07  0.13  0.07  0.06  0.06  0.06
   10.08  0.22  1.03  0.22  4.57  0.23  1.94  0.23  0.08  4.49
    4.78  0.21  1.82  0.22  4.73  0.22  0.07 10.07  0.22  3.35

- **median 0.220 ms**, and 27 of 40 waits under 1 ms — the guest is woken,
  not polling;
- **3 of 40 (7.5%) at the fallback timer**, and they land on it exactly:
  10.07, 10.08, 10.09 ms against the documented 10.07;
- `stat_vblank_fired` 88652, `stat_semsurf_fired` 426710,
  `stat_semsurf_waiters` 0 at the end.

Two things this session adds. The fallback is **not** merely a warm-up
artefact: it is the first sample in two runs and the **eighth** in the third,
after seven sub-millisecond waits. And it is not a majority in any run, so
the `events` gate's own criterion — median under 1 ms, fewer than half at the
poll — passes while the defect is present, which is what that gate was
designed to do (an isolated fallback is counted and reported, not failed on).

Still not traced to a cause. What this measurement adds to the next attempt
is that the fallback is reachable within a 30-minute session under ordinary
compositor load, so reproducing it does not need a long soak — 40 samples
found three.
### 15. Concurrent CUDA processes failed on a long-running guest
**Open, seen once on 2026-08-16, not reproduced since.** On a desktop
guest after a long probing session, two simultaneous `nvprobe 3` runs
became unreliable and four were hopeless (`cuCtxCreate: out of memory`,
`cuInit: no CUDA-capable device`); one alone still worked, and thirty-two
at once got one through. A fresh guest does not show it, which points at
accumulated state rather than at concurrency itself. Possibly the same
root as number 31.

**NOT REPRODUCED 2026-08-21, under conditions that include the suspected
root.** Same single instrumented session as numbers 14 and 31: GNOME Wayland
desktop guest, up ~30 minutes, after repeated `glmark2`/`vkmark`/`vkcube`
load and dozens of GL client lifecycles.

`nvprobe 3` run concurrently, all launched together and waited on:

| concurrent | succeeded | errors |
|---|---|---|
| 1 | 1/1 | none |
| 2 | 2/2 | none |
| 4 | 4/4 | none |
| 8 | 8/8 | none |
| **32** | **32/32** | none |

Not one `cuCtxCreate: out of memory` and not one `cuInit: no CUDA-capable
device`. This entry records two becoming unreliable, four "hopeless", and
thirty-two getting exactly one through; here thirty-two all completed and
verified their result.

**The suspected shared root was PRESENT and did not reproduce it.** This
entry says number 31 is "a strong candidate for the root". At the time of
this measurement the backend held **442** open `/dev/nvidiactl` descriptors —
the number-31 condition, well advanced (it starts at 68 on a fresh boot). The
32-way run drove it to a peak of **1152** and it fell back to its baseline
exactly. So a high descriptor count does not by itself make concurrent CUDA
unreliable, which weakens the shared-root hypothesis rather than settling it:
the original observation stood at 2003 descriptors, and this session reached
442.

What is still owed, and it is now specific: the same concurrency ladder on a
session that has reached four figures of descriptors. Number 31's measured
rate makes that arrangeable on purpose rather than by waiting — see there.

**AND THE OWED TEST WAS TAKEN THE SAME DAY, AT FOUR FIGURES.** Number 31's
measured rate (2.0 descriptors per GL client lifecycle) makes the descriptor
count something to arrange rather than wait for. 750 `glxinfo` lifecycles
against the compositor's Xwayland took the backend to **2010** open
`/dev/nvidiactl` descriptors — the level of this entry's own suspected root,
where it was first observed at 2003 — and the ladder was re-run there:

| concurrent | succeeded at 2010 descriptors |
|---|---|
| 1 | 1/1 |
| 2 | 2/2 |
| 4 | 4/4 |
| 8 | 8/8 |
| **32** | **32/32** |

No `cuCtxCreate: out of memory`, no `cuInit: no CUDA-capable device`, at any
level.

**So the shared-root hypothesis is falsified at the descriptor count that
motivated it.** This entry says number 31 "is a strong candidate for the root"
and number 31 says this one is a strong candidate for its consequence. At 2010
descriptors — 30× a fresh boot's 68, and the same figure as the original
observation — thirty-two concurrent CUDA processes all completed and verified
their results. Whatever made two unreliable on 2026-08-16, it is not the
descriptor count.

What is left is genuinely accumulated state of some OTHER kind, and the entry's
own wording is the right one: "a fresh guest does not show it, which points at
accumulated state rather than at concurrency itself". The descriptor count is
now excluded from what that state can be.
### 16. Connector detect breaks after a session that really drew
**Open, bounded, workaround holds.** After a compositor that was DRM
master exits abnormally, the next `drmModeGetConnector` probe reports the
virtual connector as disconnected, and nvidia-modeset logs `Failed
detecting connected displays for displayless HW`. `sysfs` still says
`connected` because it returns the last known state; the probe asks NVKMS
again and that is what fails. Reloading `nvidia_drm` and `nvidia_modeset` (the display module load,
`lea_display_modules`) always recovers it, and has carried a full
working day across six compositor sessions.

### 17. Sunshine `capture = kms` under Wayland shows a black stream
**Open; mostly answered, and one hypothesis withdrawn.** Sunshine
initialises cleanly and grabs 60 frames per second, but the receiver sees
black — once black with a live mouse pointer, meaning the cursor plane
arrives and the main plane does not. The path demonstrably *can* carry
content: the same chain showed a working desktop earlier the same day.
`fbprobe` later established that the framebuffer content is there, so the
counter hypothesis is **withdrawn**; the real blocker turned out to be the
compositor not repainting (see 35).

### 18. Flip completions arrive in excess
**Open, low priority.** Every compositor start produces two to five kernel
warnings from `nv_drm_crtc_dequeue_flip` — nvidia-drm receives more flip
completions than it has flips outstanding. It is most likely our invented
vblank path reporting modeset commits as flips, or counting per plane so
the cursor counts twice. It never occurs in steady state, and the
dangerous inverse (a *lost* completion, which would freeze a compositor)
has never been observed. Since 2026-08-18 it is understood as a signature
of the same root as 22-A.

### 19. `GET_SURFACE_PHYS_PAGES` is refused with `INSUFFICIENT_PERMISSIONS`
**Open, not a blocker.** Under four concurrent Vulkan clients, RM answers
control `0x3e0102` on an `NV01_MEMORY_SYSTEM` object with
`NV_ERR_INSUFFICIENT_PERMISSIONS`, and nvidia-drm logs `Failed to get
memory pages for NvKmsKapiMemory`. Do not confuse this with number 20: the
theory that it caused CS2's empty window was checked and rejected — the
timestamps of the two error kinds do not coincide, and no RM call failed
while the FBO errors were occurring.

### 31. The backend holds thousands of `nvidiactl` file descriptors
**Open, and substantially narrowed 2026-08-21: it has an owner, a rate and
a mechanism now, and it is not what this entry called it.** Measured on a
running rig: after 4 h 20 min the backend held
**2003** open `/dev/nvidiactl` descriptors while exactly one guest process
(`gnome-shell`) had the GPU open. It is the first hard, monotonically
growing resource leak on our own side. It is not the leak number 26 was
looking for, and it is a strong candidate for the root of number 15.

**MEASURED 2026-08-21. The count has an owner, a rate, and a composition,
and "monotonically growing leak" is the wrong description.** One instrumented
soak, shared with numbers 14 and 15: GNOME Wayland desktop guest, backend
from a fresh boot at **68** `/dev/nvidiactl` descriptors.

**It is not monotonic, and it is not per client.** Over 91 fd-census samples
the count fell in 14 of 90 steps. The decisive test is a controlled cycle:

| workload | baseline | peak | settled | residual |
|---|---|---|---|---|
| 16 concurrent `nvprobe` (CUDA) | 338 | **1152** | **338** | **0** |
| 10 sequential `nvprobe` (CUDA) | 442 | — | 442 | **0** |
| 10 `glxinfo` (GL, via Xwayland) | 362 | — | 382 | **+20** |
| 10 `glxinfo` again | 382 | — | 402 | **+20** |
| 20 `glxinfo` | 402 | — | 442 | **+40** |

So **a CUDA client lifecycle retains nothing** — sixteen at once took it to
1152 and it returned to its baseline exactly — and **a GL client lifecycle
retains exactly 2.0 descriptors**, reproduced three times at two batch sizes.

**What the two are.** The fd census decomposes it exactly. Twenty `glxinfo`
runs, before against after:

    sessions hold   280 -> 300   +20
    mirror          275 -> 295   +20      (mirror_ever 1219 -> 1359, +140)
    window maps     230 -> 250   +20
    outside session 247 -> 267   +20
    nvidiactl       442 -> 482   +40

One descriptor per client in a **session's handle mirror**, and one per
client for a **window mapping**, which is what the "outside every session"
figure counts — window and outside move together, +20 and +20. Seven mirror
entries are created per client and one is kept.

**And the owner is the X server, proved by killing one.** A private
`Xwayland :3` was started in the same session (number 29's other arm), ten
`glxinfo` were run against it, and it was killed:

    before Xwayland :3 started        482
    after it started                  528
    after 11 glxinfo on :3            580
    after killing Xwayland :3         506     -- 74 descriptors released at once

**So this is not a leak in the backend.** `release_window_of` works and is
keyed on the guest process; the process it is keyed on is the X server, and
on a desktop the X server never exits. Every GL client hands its mapping and
its mirrored handle to a session that outlives it, and they are correctly
held for exactly as long as that session lives — which is the whole session.
That is why the original observation found 2003 descriptors while "exactly
one guest process (`gnome-shell`) had the GPU open": the one process was the
owner, not a bystander.

**Why it still matters, and it matters more than a descriptor count.** The
comment on `release_window_of` says it: each leftover costs a duplicated
`/dev/nvidia*` FD *and its slice of the host-visible window*, and running out
of holes in that window is what killed CS2 at its 129th mapping. At **one
window mapping per GL client lifecycle** against an 8 GiB window, this is the
same wall with a slow fuse, and the fuse is lit by ordinary desktop use.

**What is now decidable rather than open**, and it is a design question, not
a measurement:

1. Is a mapping placed by a client that has exited still the X server's, or
   should it be released when the CLIENT goes rather than when its owner
   does? The answer decides whether this is correct behaviour or a defect,
   and the code cannot decide it — RM's ownership is the X server's.
2. If it is correct, the window needs reclamation that is not keyed on
   process exit, because on a desktop the owner never exits.

**And it makes number 15 arrangeable.** At 2.0 descriptors per GL client, a
session can be driven to four figures on purpose in minutes instead of
waited for over hours — which is exactly what number 15's remaining test
needs.

**The rate holds at scale, and it is the lever.** 750 `glxinfo` lifecycles
against the compositor's Xwayland moved the backend from 506 to **2010**
descriptors — **2.01 per client**, the same figure as the 10- and 20-client
batches, with zero client failures and the GL stack healthy throughout. 300 of
them took 46 seconds.

Two things follow. The 2003 of the original observation is **reachable on
purpose in about two minutes** rather than over 4 h 20 min, which is what made
number 15's owed test possible the same day (it is there, and it falsified the
shared root). And the linearity across three orders of batch size says this is
a per-lifecycle retention with no cliff and no saturation — nothing reclaims,
and nothing gets worse either.

**And the window occupancy is the number that matters.** At 2012 descriptors
the census reads `5 sessions hold 1065 (mirror 1060 of 6704 ever) | window
**1015 maps** | 2097 fds, 2012 of them nvidiactl | outside every session
1032`. So **1015 window mappings are held for clients that exited long ago**,
one per GL client lifecycle, against the 8 GiB window — and it grew from 13 at
boot in step with the descriptor count throughout.

No `window full` was reached and none is claimed: the window is 8 GiB now and
CS2 hit the wall at 129 mappings when it was 1 GiB. The point is the shape, not
an imminent failure — the mappings are never reclaimed, so the headroom is
consumed by how many GL clients a desktop has ever run rather than by how many
are running.
### 46. Sound continues while the picture hangs, and it is the CPU
**Open, but named, and it is NOT a GPU question.** Under a real game the
stream stalls: audio keeps playing, video stops. Measured 2026-08-20 while
Shadow of the Tomb Raider ran in a `--session gnome --wayland` guest with
Sunshine on `capture=kms encoder=nvenc`:

- `fbprobe` says **READER GREEN** -- all three readers (mmap, GL, CUDA)
  see moving, nonzero content, frame changed in 9 of 9 polls, peak
  997-1024/1024. So the guest IS drawing and the scanout IS advancing.
  This is the opposite of the reading number 44 records for the crash
  session (`STATIC, 0/9 polls`), and it is why that reader exists.
- The game holds **438 %** CPU of **8** vCPUs, load average **10.97**, and
  `sunshine` gets **11.5 %**.
- Sunshine cannot raise its own threads: `setpriority failed for nice -15:
  Permission denied` and `RTKit: Could not set priority ... AccessDenied`,
  repeatedly, in `/tmp/lea-sunshine.out`. NVENC itself initialises cleanly
  (`Nvenc version 13.1, Nvenc initialized successfully`) and logs no
  encoder error at all.

So the encode path is starved by the workload it is supposed to capture,
and audio survives because it is cheap. Two levers, neither tried yet:
give the guest more of the host's 16 cores (`--cpus`, the default is 4 and
this guest had 8), and let Sunshine have the priority it asks for
(`cap_sys_nice` on the binary, or a limits drop-in). Which of the two
carries it is a measurement, not a guess.

Recorded because nothing in this tree mentioned `setpriority` or RTKit
before, and because a stall with sound is exactly the shape a reader would
otherwise file against numbers 10, 20 or 44.

### 52. The guest's graphics stack asks a different set of questions
**Half answered 2026-08-21: the boundary carries them; the userspace stopped
asking.** The direct probe this entry called for exists --
`probe/matrix/rm-direct.sh` over `probe/c/rmdirect.c`, raw RM ioctls with no
driver userspace at all: it opens the control node, builds the object
hierarchy by hand and issues each command once. In a guest it is
**guest-validated, 11 signatures against 11** (re-read from
`matrix/guest-610.57.04.json` on 2026-08-21; this entry said 10), and every
command answers `NV_OK`:

| command | in the guest |
|---|---|
| `NV0000_CTRL_CMD_GPU_GET_PROBED_IDS` | `NV_OK`, one probed GPU |
| `NV0000_CTRL_CMD_GPU_ATTACH_IDS` | `NV_OK` |
| `NV0000_CTRL_CMD_GPU_DETACH_IDS` | `NV_OK` |
| `NV2080_CTRL_CMD_TIMER_GET_TIME` | `NV_OK` |
| `NV0073_CTRL_CMD_SYSTEM_GET_CAPS_V2` | `NV_OK`, caps `81 2f` |

So the first candidate explanation is the one that survives: **the guest's
userspace takes a different discovery branch**, not "the mediated identity
ends the enumeration early" and not "the boundary cannot carry these". The
carrying is now measured rather than predicted -- four of the five are in
`verified` or `verified-mediated` in the evidence file, and `TIMER_GET_TIME`
is `unstable`, which is what a timer should be.

`NV_ESC_RM_IDLE_CHANNELS`, the sixth command, is NOT in the direct probe: it
needs a channel, which needs a GPFIFO allocation, a pushbuffer and a VA
space, and building those by hand is a different program. It is verified
anyway, through `egl-gbm`, which does issue it in a guest.

WHICH PROBES STILL SKIP WHAT, re-measured on 2026-08-21 and narrower than
this entry first stated. It is not "every graphics probe":

  * `gl-enum`, `gl-render`, `vk-enum`, `vk-offscreen` skip the three GPU id
    controls; `vk-rt` skips `DETACH_IDS` only;
  * `TIMER_GET_TIME` and `SYSTEM_GET_CAPS_V2` are skipped more widely, by
    those and by `egl-xlib` and `gles`;
  * `egl-gbm`, and the whole CUDA and NVML set, ask them in a guest exactly
    as they do natively.

What is still open is WHY the branch differs, and that is now a question
about NVIDIA's userspace rather than about this boundary. The probe that
would narrow it further compares the guest's and the host's `openat`/`stat`
sets around the discovery path, not their ioctls.

**Original entry, measured 2026-08-20.** Every graphics probe in the guest sweep skips
the SAME six commands, natively issued by all of them and by none of them
in a guest:

`NV0000_CTRL_CMD_GPU_GET_PROBED_IDS`, `..._ATTACH_IDS`, `..._DETACH_IDS`,
`NV0073_CTRL_CMD_SYSTEM_GET_CAPS_V2`, `NV2080_CTRL_CMD_TIMER_GET_TIME`,
`NV_ESC_RM_IDLE_CHANNELS` (and `NV_ESC_RM_DUP_OBJECT` in the two Vulkan
probes).

The same statement in the other namespace, and much larger: `vulkaninfo`
issues **405 NVKMS ioctls natively and 4 in the guest** — none of the eight
commands that make up the native enumeration, and two the host never
issues. The GL and EGL probes go the other way: 6 natively, 14 in the
guest.

**Still open on 2026-08-20 after a second full sweep, which reproduced it
exactly** — 9 FAIL, 3 blocked, 8 guest-validated, and six of those nine FAIL
are this entry and nothing else. It is now the largest single cause of FAIL
rows in the sweep, and it is not a defect: `egl-xlib`, `gl-enum`, `gles`,
`vk-enum`, `vk-offscreen` and `vk-rt` all meet their own criteria in the
guest and fail only on this signature-set difference.

The direct probe that would settle it (issue the six commands in a guest and
compare each status fingerprint against the native one) was scoped for this
session and NOT built, because it was gated on every earlier package's gate
being met and two were only partly met. It remains the next concrete step
for this entry.

Unverified, and the two candidate explanations are testable: either the
guest's userspace takes a different discovery branch (the guest module
enumerates GPUs in-kernel, so a probe may find the device already
attached), or the mediated identity answers something that ends the
enumeration early. Neither is a defect on its face — every probe still met
its criterion — but it has a consequence that is exact: **those six
signatures are predicted to be carried and are exercised by nothing in a
guest**, so for them `predicted-green` remains untested no matter how many
guest runs pass.

**THE PROBE THIS ENTRY ASKED FOR WAS RUN, 2026-08-21, AND IT CAME BACK
EMPTY -- WHICH IS THE RESULT.** The entry names it: *"The probe that would
narrow it further compares the guest's and the host's `openat`/`stat` sets
around the discovery path, not their ioctls."* The matrix straces already
carry `openat` (`strace -f -y -e trace=ioctl,openat`), so it needed no new
run.

Filtered to what discovery actually reads -- the NVIDIA nodes, `/proc/driver`
and `/sys` -- the two sides are IDENTICAL, in `gl-enum`, `vk-enum` and
`egl-gbm` alike:

    /dev/nvidia-modeset   /dev/nvidia0   /dev/nvidiactl   /proc/driver/nvidia/params

Four paths on each side, no difference in the set, and every one of them opens
SUCCESSFULLY on both. And `/proc/driver/nvidia/params` is byte-identical --
45 parameters, `diff` empty -- so the one file whose CONTENT userspace could
branch on says the same thing to both.

**So the filesystem inputs to discovery are the same, and the branch is
decided by an ioctl answer.** That eliminates a class rather than finding the
cause, which is what this probe was for.

**AND THE BRANCH POINT IS NOW NAMED, which is new.** Walking the two ioctl
sequences from the first call, `vk-enum` diverges at step 4 and the three
before it are identical:

| # | native | guest |
|---|---|---|
| 0 | `NV_ESC_CHECK_VERSION_STR` | same |
| 1 | `NV_ESC_SYS_PARAMS` | same |
| 2 | **`NV_ESC_CARD_INFO`** | same |
| 3 | alloc root client (`0x41`) | same |
| 4 | **`GPU_GET_PROBED_IDS` (0x214)** | **`GPU_GET_DEVICE_IDS` (0x204)** |

**It is not "a different question" -- it is ONE OF TWO OMITTED.** Counted
across probes:

| probe | native | guest |
|---|---|---|
| `vk-enum`, `gl-enum` | PROBED_IDS 1, DEVICE_IDS 1 | PROBED_IDS **0**, DEVICE_IDS 1 |
| `egl-gbm` | PROBED_IDS 1, DEVICE_IDS 1 | PROBED_IDS 1, DEVICE_IDS 1 |
| `nvml` | PROBED_IDS 2, DEVICE_IDS 0 | PROBED_IDS 2, DEVICE_IDS 0 |

The native GL/Vulkan path asks BOTH; the guest asks only `GET_DEVICE_IDS`.
`egl-gbm` asks both on both sides -- which is exactly why it never appeared in
the skip list -- and `nvml` asks neither differently.

**What that leaves, and it is a much smaller space.** The last thing both
sides do identically before the branch is `NV_ESC_CARD_INFO`, and that is the
one call in the sequence this project MEDIATES: the guest module rewrites its
BDF and gpuId in place (number 62). So the leading candidate is now specific
enough to test -- answer `CARD_INFO` unmediated to one run and see whether
`GET_PROBED_IDS` comes back. That is the same experiment number 65 wants for
the same reason, and one run could answer both.
### 56. The surface is tracked per run, and the question is per ioctl
**Open, raised 2026-08-20.** Everything the matrix writes is keyed by the
run that produced it: `catalog-<driver>.json` is one file per driver
version, and the architecture appears once, in the provenance header, as
prose (`arch: Turing (compute 7.5)`). That is right for a record of a
measurement and wrong for the question people actually ask, which is about
one ioctl across the versions and cards it was measured on: *did this
command change shape between drivers? does this class exist on Ampere? was
this control ever carried on anything but Turing?*

The key is already right — `(device, nr, sub)` is stable across drivers by
construction. What is missing is the other axis.

The shape that would fit this tree, sketched rather than built:

  * **Keep the per-driver catalogues as the source of truth.** They are the
    record of one run, with its provenance intact, and nothing should
    rewrite them later.
  * **Generate an index that merges them**, one row per signature with an
    observation per `(driver, architecture)`: status, call count, the
    `rm_status` fingerprint, whether a guest run validated it, and when it
    was measured. Regenerated from whatever `catalog-*.json` files are in
    the directory, so it is never a table anybody maintains — the rule this
    whole pipeline follows.
  * **Two forms of the same content**: JSON for a machine, and one
    Markdown table with a column per driver for a person. Both from one
    generator, so they cannot disagree.
  * One enabling change it needs: the provenance in the JSON is currently
    an array of strings. Architecture and compute capability should be
    FIELDS, so the merge does not parse prose.

Worth building at the second data point and not before -- with one driver
and one card the index has one column, and its value is entirely in what it
does when the second arrives. The cost of waiting is that an old catalogue
may lack a field the merge wants, which argues for making the provenance
structured now and merging later.

**THE ENABLING CHANGE IS DONE, 2026-08-21.** This entry ends by arguing for
it: *"the provenance in the JSON is currently an array of strings.
Architecture and compute capability should be FIELDS, so the merge does not
parse prose"*, and *"the cost of waiting is that an old catalogue may lack a
field the merge wants, which argues for making the provenance structured now
and merging later."* Every artefact the matrix writes now carries both:

    "provenance": ["driver:  610.57.04", "arch:    Turing (compute 7.5)", ...]
    "provenance_fields": {
        "driver": "610.57.04", "gpu": "NVIDIA GeForce RTX 2070",
        "arch": "Turing", "compute_cap": "7.5",
        "kernel": "7.1.8-arch1-3", "date": "2026-08-21T10:42:39Z",
        "commit": "aed491d", "tree_modified": true
    }

Three details, each deliberate:

  * **the strings stay.** They are what a person reads and several artefacts
    print them verbatim; dropping them to avoid a duplicate would break those
    for no gain. The fields are DERIVED from the same lines, in one function
    (`ioctlmatrix.provenance_fields`, imported by `guestdiff` and
    `answerdiff`), so the two cannot drift;
  * **`arch` and `compute_cap` are separate.** The line bundles them as
    `Turing (compute 7.5)` and an index wants to select on either;
  * **`tree_modified` is its own boolean.** It was a parenthesis inside the
    commit string, which made the sha unparseable without knowing to strip
    it.

**THE SECOND DATA POINT HAS ARRIVED, BUT NOT FOR THIS AXIS, AND THE
DISTINCTION IS THE WHOLE POINT OF THIS ENTRY.** A Blackwell card (RTX 5060
Ti, compute 12.0, same driver 610.57.04) ran the GATES on 2026-08-21 and
passed `gpu` 8/8 and `vdisplay` 6/6 — the README carries the matrix. That is
a second `(architecture, driver)` observation about the BOUNDARY.

It is not a second catalogue. Nobody has run `ioctl-matrix.sh` on that card,
so `matrix/` still holds exactly one `catalog-*.json` and the index this
entry describes would still have one column. **Building it remains premature
for the reason stated above**, and the trigger is unchanged: a second
`catalog-<driver>.json`, from any card.

What the gate results do establish, and it is worth having before the index
exists: the descriptor table's checksum and the virtual display's frame hash
are IDENTICAL on the two architectures (`0xad009afd`,
`0xb6a79817d7f4a5c3`), and PyTorch is bit-identical to native on both. So the
first cross-architecture evidence says the surface did not move — which is a
prediction the per-ioctl index would be able to check properly, and cannot
yet.
### 57. A vendor manifest is registered nowhere, and a knob depends on it
**Open, measured 2026-08-20.** Found while generalising number 53's rule.
Of the ten host files that name an NVIDIA library, eight are written into
the guest now. The ninth and tenth are the two halves of
`/usr/share/vulkan/implicit_layer.d/nvidia_layers.json`, and neither is
written:

  * `VK_LAYER_NV_optimus` names `libGLX_nvidia.so.0`, which IS staged. The
    library is reachable anyway — the Vulkan ICD manifest names it — so
    this is not the "absent library that costs disk" shape. What is absent
    is the LAYER.
  * `VK_LAYER_NV_present` names `libnvidia-present.so.610.57.04`, which is
    NOT staged and is one of the seven libraries whose staging is a
    person's decision (`TASKS-<drv>.md`, task 3). Writing the file whole
    would point the loader at a library that is not there, which is the
    failure mode number 53 is about, in the other direction.

The consequence is small and exact, and it is why this is written down
rather than fixed: `/usr/local/bin/nvidia-run`, which `lea_gl_stage
--system` installs, runs its program with `__NV_PRIME_RENDER_OFFLOAD=1` and
`__VK_LAYER_NV_optimus=NVIDIA_only`. The first of those is precisely the
`enable_environment` of a layer that is registered nowhere in the guest, so
**the variable is inert** — the wrapper does on a guest a strictly smaller
thing than its name and its comment claim.

Not fixed, and the reason is that it cannot be fixed by half without a
measurement: registering the optimus half alone changes ICD selection on a
guest that has exactly one GPU, and nothing in this tree exercises a layer
today. Unverified, and testable: register the optimus half only, run
`vk-enum` in a guest, and compare the signature set against this baseline.
If it does not move, the layer is inert in both directions and the honest
fix is to drop the two variables from `nvidia-run` instead.

### 58. NVIDIA's xcb EGL platform declines in a guest, and its xlib platform does not
**Open, measured 2026-08-20.** Split out of number 53, whose hypothesis for
this half — a missing registration, "the same shape one platform further
on" — is FALSIFIED.

`eglplat xcb` in the guest resolves to vendor `Mesa Project`; natively it
resolves to `NVIDIA`. Read out of the guest run's own strace, every step of
the registration chain is intact: `/usr/share/glvnd/egl_vendor.d/10_nvidia.
json` is read, `/usr/share/egl/egl_external_platform.d/20_nvidia_xcb.json`
is read, and `libnvidia-egl-xcb.so.1` is `dlopen`ed and returns a
descriptor. The vendor library is found, loaded, and declines.

What makes it sharp is the neighbour: `eglplat xlib` in the SAME guest, in
the same sweep, against the same X server, resolves to `NVIDIA` and reads
its pixel back correctly (`3377bbff`). Two external-platform libraries over
one X connection, and only one of them accepts. So this is not "EGL is
broken in a guest" and not a staging question; it is one platform module's
own acceptance test.

The environment difference is known and is the obvious suspect: the guest's
X server runs on the virtio-gpu (Mesa's own diagnostic in the same trace
reads `pci id for fd 6: 1af4:107c`), while the host's runs on the NVIDIA
card. Unverified, and the counter-test is cheap and does not need a guest:
run `eglplat xcb` and `eglplat xlib` on the HOST against an X server that
is NOT on the NVIDIA card. If xcb answers Mesa there too, this is a
property of `libnvidia-egl-xcb` and the X server's device, the boundary is
not involved, and the honest row for `egl-xcb` in a guest is an environment
row like `cuda-managed`. If xcb answers NVIDIA there, the boundary IS
involved and this becomes a real finding about what the guest answers to
whatever that library probes.

No probe failed for it in a way that hides anything: `egl-xcb` is FAIL in
the guest column with its reason on the row.

### 59. The probes are entry paths, and the class that can be missing is the one they do not reach
**Raised 2026-08-20**, out of a design conversation rather than a run, and
recorded because it is the direction with the largest measurable target in
this tree.

The 27 matrix probes are entry paths — an enumeration, a vector add, two
seconds of video. That is a deliberate property (they are cheap, they gate,
they are deterministic) and it has an exact consequence, which number 55's
section on "0 missing" already states: of 290 signatures only **58** can EVER
be reported `missing` — 39 allocation classes and 19 UVM commands, the two
places a length is not self-describing. Everything else is forwarded whether
or not any table names it.

And the descriptor table holds **128 classes while the probes touch 39**. So
the yield of any new-workload effort is not a matter of taste: it is *which
of the untouched classes did we reach*, and that is a diff that can be
computed BEFORE choosing a workload rather than discovered afterwards.

**The gates and the matrix are not substitutes for each other**, and the
question was asked directly, so the answer belongs here. A gate produces a
VERDICT — pass/fail, bisectable, "is the tree still good". The matrix
produces a DIFFERENTIAL — what surface did this workload touch and did the
boundary carry it identically. The matrix's verdicts are deliberately not
binary (`predicted-green` / `guest-validated` / `FAIL` / `blocked` are four
claims), so gating on them would fail the tree for things like number 52
which are not defects, and replacing the probes with gate workloads would
lose the native reference run the comparison rests on.

What DOES compose, cheapest first:

  1. **Feed the gates' existing workloads through the matrix.** The tracer is
     an `LD_PRELOAD` interposer; any binary goes under it, and traces from
     the bench work already exist and have never been through this pipeline.
     No new probe code.
  2. **Let a gate stage emit a trace as a by-product** — one guest run, a
     verdict for the gate and a signature set for the catalogue.
  3. Only then new probes, aimed by the coverage diff.

High-value targets, by what they allocate rather than by what they are:
`libnvoptix` (OptiX allocates classes of its own) and `libnvidia-ngx`
(DLSS) — both are staging decisions first (`TASKS-<drv>.md` task 3); CUDA
IPC, whose `DUP_OBJECT` is one of number 52's six commands; CUDA dynamic
parallelism, whose `MAP_DYNAMIC_PARALLELISM_REGION` (UVM 65) is in `xlate`
and reached by nothing; fine-grained SVM in OpenCL, now that number 53's ICD
makes that library reachable at all; Vulkan sparse residency and external
memory; EGLStreams.

Two constraints this tree already imposes and which a workload day must
keep. **Every probe verifies a RESULT, never the absence of a crash** — the
pixel-readback and element-compare rule; `oclprobe.c` says why, and number 53
is what happens when it is ignored. And **the strace counter-check is the
trust anchor**, which does not survive every workload: `cuda-gdb` is already
ungated because strace cannot follow a process that is itself ptracing, and
the tracer sits on a per-frame path measured at 86 645 lines in one session.

Score such a day by class-coverage delta, not by probes added. That is the
number that makes "did we find non-carryable ioctls" answerable instead of
hopeful.

### 64. The NVKMS commands are recorded and none of them has a name
**Open, split out of number 48 on 2026-08-21**, which is resolved: the node
is traced, counted and gated, and this is the half that was never anything
but a decoder.

NVKMS carries its whole interface under a single ioctl number —
`_IOWR('m', 0, struct NvKmsIoctlParams)` (`nvkms-ioctl.h`) — so `nr` is 0 on
every line and the real command is a field of that 16-byte struct. The
tracer reads it there and puts it in `sub`, with the size of the block it
points at in `psize`. Across the matrix probes that is **451 calls in 14
commands**, 405 of them from `vulkaninfo --summary` alone.

**Nothing is named.** NVKMS command numbers are their own namespace and
resolve against no `ctrl*.h`, so the raw number is the honest catalogue
entry until a reader for `nvkms-api.h` exists. `matrix/catalog-<drv>.md`
carries the count and the node and invents nothing, which is the right
behaviour and not a workaround.

**The criterion**, unchanged from 48 and from `matrix/TASKS-<drv>.md` task
2: every row in the catalogue's NVKMS section carries a name and a params
struct out of `nvkms-api.h`, the way an RM_CONTROL row carries one out of
`ctrl*.h`. The one number a decoder can be checked against before it is
trusted is already recorded — `psize`, the size of the block each command
points at, measured per call.

**Deliberately not next.** The raw numbers cost nobody anything today: the
node is gated, the counts are honest, and no verdict anywhere rests on
knowing what command 7 is. This is a reader to be written when something
needs the names, not a gap that is currently misleading anyone.

### 65. NVML allocates an SMC monitor session natively and never in a guest
**Open, split out of number 51 on 2026-08-21**, which is resolved. It was
never downstream of the two controls that entry fixed, and keeping it there
made a closed entry look open.

`AMPERE_SMC_MONITOR_SESSION` (class `0xc640`, `ctl nr=0x2b sub=0xc640`) is
allocated **once** in a native `nvidia-smi -q` run and **never** in a guest
one. It is the only finding the `nvml` probe has left:
`matrix/guest-610.57.04.json` records 105 native signatures against 104,
179 ioctls against 179, and this single `signature-absent-in-guest` row.

**The first hypothesis is falsified, and that is the useful part.** It was
that the absence was downstream of the two controls number 51 fixed
(`GPUACCT_GET_ACCOUNTING_STATE` and `BIOS_GET_INFO`). Both answer `NV_OK` in
a guest since the fix and the allocation is still absent, so it is not that.

**What is known about when it happens.** Early — right after the GPU node is
opened and `GET_PROBED_IDS` answers, and before UVM opens. So it belongs to
NVML's initialisation and not to its process accounting, which is what the
class name would suggest.

**The candidate left is the mediated identity**, and it is a decision NVML
makes from what the card says it is: the guest is told it is a "Leandro RTX
2070". Weakened rather than confirmed by a measurement taken since:
`NV2080_CTRL_CMD_GPU_GET_NAME_STRING` is `verified-mediated` over its 68
bytes and differs from the native answer in `gpuNameString` **and in no
other byte**, so the mediation is not writing anywhere it should not. That
removes "the mediation corrupts something adjacent" and leaves "NVML reads
the name and branches on it", which is not the same claim and is not
measured.

**Not a defect on its face.** `nvidia-smi -q` meets its criterion in the
guest and prints a plausible report; nothing fails. What it costs is
coverage: the class is allocated by nothing in a guest, so whatever the
boundary would do with it is exercised by no sweep — the same shape as
number 52's six commands, and the reason that entry states the consequence
in those terms.

**What would settle it**, cheapest first:

1. `strace` the guest and native `nvidia-smi` around the allocation point
   and diff what each read before deciding — the same `openat`/`stat`
   comparison number 52's other half needs, on a different path.
2. Answer the un-mediated product name to one run and see whether the
   allocation appears. That is a measurement, not a change: if it does, the
   branch is the name and the question becomes what SMC monitoring costs a
   guest that has no MIG.
3. Allocate the class directly from `probe/c/rmdirect.c`, which already
   builds a hierarchy by hand, and find out whether the boundary carries it
   at all. That answers the coverage question regardless of why NVML skips
   it, and it is the one step that does not depend on guessing NVML's
   reasoning.

## Resolved and decided

### 1. Does the descriptor table warrant a protocol change?
**Decided.** Neither — the table does not travel in `Hello` at all. A
separate `GetTables` request, sent by the module after `Hello`, carrying a
table version and a checksum and paginating if the table outgrows one
message.

### 2. Who owns `/proc/driver/nvidia` when both modules are loaded?
**Decided.** `nvrm_nodes.ko` keeps it; `virtio_nvrm.ko` does not touch
`/proc` at all. Both modules run side by side, `nvrm_nodes.ko` with
`create_nodes=0`. Nothing in `nvrm_nodes` had to change.

### 3. Project name, and the `nvshim-*` crate prefix
**Decided.** The project is named Leandro; the scheme is in
[`NAMING.md`](NAMING.md). `nvshim` is the name of an NVIDIA Windows
service, and confusability with a vendor name was wanted neither in a
title nor in a search.

### 4. One host session per VM, or per guest process?
**Decided and built: per guest process.** Per-VM is right for tokens and
for GPU addresses, but wrong for the pool state, which is keyed on GPU
virtual address — and `libcuda` places every process's semaphore pool at
the same address. Per-VM sessions therefore wrote one process's semaphore
into another's dead arena. See [`llm.md`](llm.md).

### 5. Where does the seam run in `session.rs`?
**Decided and built, in two steps.** First a `NvSyscalls` trait, which
enabled eighteen guest-lies tests; then `prepare() -> Plan` /
`execute(Plan)`, carried by those tests. Buffer ownership is handled by a
documented invariant rather than by moving ownership: translated buffers
stay session state, the `Plan` carries control data only, and nothing
touches the buffers between the two calls.

### 6. How does the kernel path resolve a guest file descriptor?
**Resolved 2026-08-15.** Two halves. In the guest, the refusal tested the
wrong thing (`c->kern`); NVKMS runs `IMPORT_OBJECT_FROM_FD` inside the
caller's own ioctl, so the current process *is* that process, and the
check now tests `PF_KTHREAD`. On the host, the token resolved and then
had to be accepted on the other side.

### 6a. The same question as first written
**Resolved with 6.** Kept because it records what was known before the
answer: `EXPORT_OBJECT_TO_FD` arrives on the user path and works,
`IMPORT_OBJECT_FROM_FD` arrives on the kernel path and was refused with
`EBADF`. This is what stopped `vkCreateSwapchainKHR`.

### 7. A vblank callback is a guest kernel function pointer
**Resolved 2026-08-15.** Forwarding `pProc` would hand the host a guest
kernel address, so the class is deliberately absent from the tables. On
the kernel path the caller is NVKMS and the pointer is live *in the
guest*, so the module became the raster generator: an hrtimer at the
virtual display's refresh rate. `stat_vblank_fired` ticks at 60/s under a
running GNOME session.

### 8. GLX clients get FBConfig id 0
**Superseded by 9.** Not a GLX question at all — every "broken GL"
measurement that day was taken after the SHMEM channel had died. FBConfig
0, "no available drivers", llvmpipe compositing and the laggy desktop are
one bug.

### 10. Black windows under the compositor, then an assert
**Resolved 2026-08-15.** The cause was the missing event back-channel: the
guest could not be told a fence had signalled, so every wait fell back to
a 10 ms poll. With the second virtqueue built, `fencetime` went from
10.10 ms to 0.12 ms and Sunshine's frame time from 62 ms to 6 ms.

### 11. CS2 gets no Vulkan while `vkcube` runs beside it
**Resolved 2026-08-15.** Four causes, each real, only the last fatal: a
12-byte stride the array mediation could not express in
`GET_ACTIVE_DEVICE_IDS`; two unmediated arrays in `GET_P2P_CAPS_MATRIX`;
a forced UVM multi-process sharing flag; and `libnvidia-rtcore.so` never
being staged into the guest.

### 12. The VRAM cap counted one door and the card has several
**Resolved 2026-08-15.** The cap bounded allocations arriving through one
ioctl, and the graphics stack does not use that one. Both doors are hooked
now and the books match the card to within RM's own overhead. Numbers in
[`llm.md`](llm.md).

### 13. Two directories of probes, two licences
**Decided 2026-08-17.** MIT for the workspace, scripts, probes and docs;
GPL-2.0-only for `guest-module/`, which links against the guest kernel. See
[`LICENSES.md`](../LICENSES.md). `scripts/test.sh check` enforces the split per
file.

### 20. CS2 renders but its window stays empty
**Resolved on the native path 2026-08-17.** CS2 loads, computes and plays
audio, and the X window is viewable and correctly sized — but GNOME's
overview showed no thumbnail for it, meaning the compositor had no texture
at all. So it was never a stacking, focus or flip problem but the client
buffer's route to the compositor. The Xwayland remainder runs under 22.

### 21. The backend leaked `/dev/nvidiactl` descriptors and RM clients
**Fixed and verified 2026-08-17.** After a long desktop session the
backend held 3160 device descriptors and had created 2605 RM clients.
Fixed, and the fix verified by rerunning the same session shape.
Number 31 is a *different*, still open leak.

### 22. `GL_OUT_OF_MEMORY` on EGLImage import under Xwayland
**Resolved 2026-08-21, all three halves.** A is decided (number 25), B is
fixed (number 28), and C is closed with the rest of the chain. The original
reasoning is kept below. Steam's and CS2's
windows exist and are mapped but are never drawn, and glamor reports
`GL_OUT_OF_MEMORY — Failed to acquire the EGL Image memory`. Defect A is
named (see 25); B is a 32-bit gap in GBM packaging; C is the import
failure itself, which is now understood as the head of the chain in 35.

---

**ALL THREE HALVES ARE ACCOUNTED FOR, 2026-08-21:**

  * **A** -- the displayless HAL. Number 25 is DECIDED: forcing it is the
    route taken, `CAP_SYS_ADMIN` was measured to make the outcome worse, and
    what remains is a resolution ceiling rather than a choice.
  * **B** -- the 32-bit gap. Number 28 is FIXED: `nvrm-gl.conf` listed only
    the 64-bit directory, so `ldconfig` knew `libGLX_nvidia.so.0` only as
    x86-64. Its own note says *"the fix moved the failure rather than
    removing it; see 35"* -- and 35 is closed now too.
  * **C** -- the import failure itself, the head of the chain in 35.


---

**CLOSED 2026-08-21 BY THE RUN NUMBER 35 HAD BEEN WAITING FOR.** The full
account is in number 35. The short version: the configuration that produced
this defect was assembled completely for the first time, and the defect did
not appear.

  * GNOME **Wayland** session -- the path number 24 proved all of these hang
    on;
  * Sunshine `capture=kms` with `h264_nvenc`, its own log reading
    *"Screencasting with KMS"*;
  * **Moonlight connected** and decoding HEVC -- the leg that had never
    worked, blocked by a Sunshine started without a Wayland environment
    (fixed the same day);
  * Shadow of the Tomb Raider running, 49 threads, with `steamwebhelper` on
    `/dev/nvidia0` beside it -- number 44's exact pair, `WinMain` and the
    overlay's renderer.

**19 minutes 25 seconds** against the 15-20 SECONDS this defect took to
appear, sampled every 20 s across 60 samples. `segfault at 8`: **0**.
`Failed to acquire the EGL Image`: **0**, where the crashing session logged
**744**. `GL_OUT_OF_MEMORY`: **0**. Xwayland never restarted, so the instance
number 44 calls a consumable was the same one throughout.

**The cause, as far as the evidence supports one: number 45.** A signal
arriving while the guest module waited for a reply it had ALREADY submitted
made the kernel restart the whole ioctl, so a one-shot escape was issued twice
and the second was refused -- Xwayland was told an attach had failed that had
succeeded. That is the right shape for a GL stack built around an id its owner
believes invalid, it landed AFTER both sessions that crashed, and until this
run it had never been tested against them.

**What would reopen this**, stated so the closure is falsifiable: a
`segfault at 8` in `libGLX_nvidia`, or `Failed to acquire the EGL Image`
returning on a Wayland session. Both are one `dmesg` and one `journalctl`
away, and both are in the crash-watch loop this run used.
### 23. GLX clients segfault in the guest
**Resolved 2026-08-21.** Not reproduced in the configuration that produced
it, once that configuration could be assembled in full. The original reasoning
is kept below. `glxgears` and Steam's
`gldriverquery` both segfault at address 8 inside
`libGLX_nvidia.so`. Number 33 established that the faulting pointer is
exactly NULL rather than a wrongly mapped address, and that the crash
depends on process state rather than on which client runs. Number 29
narrowed it further: the same binary in a self-started Xwayland instance
does not crash. Number 44 names the object (2026-08-20): the NULL is the
`+8` field of a config object that libGLX_nvidia's list walk and glcore's
array search both dereference, both dying at address 8, and the game's
crash is the same defect rather than a neighbouring one.

---

**CLOSED 2026-08-21 BY THE RUN NUMBER 35 HAD BEEN WAITING FOR.** The full
account is in number 35. The short version: the configuration that produced
this defect was assembled completely for the first time, and the defect did
not appear.

  * GNOME **Wayland** session -- the path number 24 proved all of these hang
    on;
  * Sunshine `capture=kms` with `h264_nvenc`, its own log reading
    *"Screencasting with KMS"*;
  * **Moonlight connected** and decoding HEVC -- the leg that had never
    worked, blocked by a Sunshine started without a Wayland environment
    (fixed the same day);
  * Shadow of the Tomb Raider running, 49 threads, with `steamwebhelper` on
    `/dev/nvidia0` beside it -- number 44's exact pair, `WinMain` and the
    overlay's renderer.

**19 minutes 25 seconds** against the 15-20 SECONDS this defect took to
appear, sampled every 20 s across 60 samples. `segfault at 8`: **0**.
`Failed to acquire the EGL Image`: **0**, where the crashing session logged
**744**. `GL_OUT_OF_MEMORY`: **0**. Xwayland never restarted, so the instance
number 44 calls a consumable was the same one throughout.

**The cause, as far as the evidence supports one: number 45.** A signal
arriving while the guest module waited for a reply it had ALREADY submitted
made the kernel restart the whole ioctl, so a one-shot escape was issued twice
and the second was refused -- Xwayland was told an attach had failed that had
succeeded. That is the right shape for a GL stack built around an id its owner
believes invalid, it landed AFTER both sessions that crashed, and until this
run it had never been tested against them.

**What would reopen this**, stated so the closure is falsifiable: a
`segfault at 8` in `libGLX_nvidia`, or `Failed to acquire the EGL Image`
returning on a Wayland session. Both are one `dmesg` and one `journalctl`
away, and both are in the crash-watch loop this run used.
### 24. The X11 counter-test: all three defects hang on the Wayland path
**Resolved 2026-08-18.** One session switch answered three open questions.
On real Xorg the GLX vendor is NVIDIA rather than SGI/glamor, `glxgears`
runs instead of segfaulting, and `vkcube` under FIFO sits at 58.1 FPS
instead of 1520. That resolved a contradiction that had stood since
2026-08-17.

### 25. The displayless HAL is forced, and the EVO path is a privilege question
**Decided 2026-08-21: route 2, keep forcing the displayless HAL.** The
cause was measured long ago; what was missing was the choice, and it is made.
The original reasoning is kept below. With
`display=2` the EVO path prints exactly one line:
`NV0073_CTRL_CMD_SPECIFIC_GET_ALL_HEAD_MASK` returns
`NV_ERR_INSUFFICIENT_PERMISSIONS`, and nvidia-modeset gives up with
`Failed to get head configuration`. So the EVO path is not missing, it is
refused — a privilege question, not a capability one. Three routes out
exist and the choice is a design decision, not a measurement.

---

**DECISION MEMO, written 2026-08-21. This entry is blocked on a person, not
on a measurement, and nothing below decides it.** The routes are named here
because this entry said "three routes exist" without naming them, and a
choice cannot be made from a count.

What is measured and is not in dispute: the EVO path is REFUSED, not
missing. `NV0073_CTRL_CMD_SPECIFIC_GET_ALL_HEAD_MASK` returns
`NV_ERR_INSUFFICIENT_PERMISSIONS` and nvidia-modeset stops with *"Failed to
get head configuration"*.

**Option 1 — carry `CAP_SYS_ADMIN` (`LEA_ADMIN_PRIV=1`).** The mechanism
exists: `settle_admin_privilege` in
[`crates/vhost-user-nvrm/src/main.rs`](../crates/vhost-user-nvrm/src/main.rs)
drops the capability unless that variable says otherwise, so this is one
environment variable and a `setcap`.
*Cost:* the sentence "the only boundary the host enforces is the VM" stops
being true — this process takes guest input apart, and `CAP_SYS_ADMIN` is
the almost-root capability.
*What it changes downstream:* **measured 2026-08-08, it makes the outcome
WORSE.** With the capability NVKMS gets past the head mask and then dies on
`GET_PCLK_LIMIT`, which is kernel-privileged and admin does not reach, so
nvidia-drm answers "Failed to allocate NvKmsKapiDevice" and `/dev/dri/card1`
disappears entirely. Without it the earlier failure is harmless and the
render node is there. This option is not "more privilege, more display" —
it is measured to be strictly worse, and it would have to be paired with
something for `GET_PCLK_LIMIT` to be worth anything.

**Option 2 — keep forcing the displayless HAL.** This is the status quo and
it works: `vdisplay=1` swaps `NV04_DISPLAY_COMMON` for
`NVA083_GRID_DISPLAYLESS` in the answer to `GET_CLASSLIST` (one out, one in,
so `numClasses` does not change), and the guest module answers the class
itself — nothing reaches the host, because there is no host state behind an
invented monitor.
*Cost:* the mode is an INVENTION and the tree says so at load time; the
ceiling is NVIDIA's own displayless limit, 2560x1600 and 4096000 pixels
(`objgriddisplayless.c:38-39,54`), so 1080p fits and 4K does not. Whoever
raises one raises both.
*What it changes downstream:* nothing. The display gate is 12/12 green on
this path.

**Option 3 — virtualise the real display engine.** The road
[`DISPLAY.md`](DISPLAY.md) explicitly does not take.
*Cost:* a project, not a change, and it is the one route for which no
measurement here exists at all.
*What it changes downstream:* it is the only route that removes the
resolution ceiling and the invention, and the only one that would make
number 16's connector-detect breakage a real question rather than a property
of a display nothing backs.

**Recommendation, and it IS a recommendation.** Take **2** — that is, close
this by deciding to keep the displayless HAL, and re-scope what remains as
the resolution ceiling rather than as an open choice. Option 1 is measured
worse; option 3 has no measurement and no demand behind it. The reason to
DECIDE rather than leave it open is that the entry currently reads as though
three live routes are being weighed, and only one of them has ever produced
a working display.

**A person answers this with one word:** `1`, `2`, `3`, or `leave-open`.

---

**DECIDED 2026-08-21 by the operator: option 2.** Forcing the displayless HAL
is the answer, and this entry closes on the decision rather than on a new
measurement — which is what it had been waiting for since 2026-08-08.

Why that is the right shape of answer, restated so the decision is legible
later: **option 1 was measured to make the outcome worse**, not better. With
`CAP_SYS_ADMIN` NVKMS gets past the head mask and then dies on
`GET_PCLK_LIMIT`, which is kernel-privileged and admin does not reach, so
nvidia-drm answers "Failed to allocate NvKmsKapiDevice" and `/dev/dri/card1`
disappears. More privilege, less display, and the "the only boundary the host
enforces is the VM" sentence spent for nothing. Option 3 remains a project
with no measurement behind it and no demand in front of it.

**What is NOT closed by this, and it is deliberately a separate thing:** the
resolution ceiling. The displayless path is bounded at 2560x1600 and 4096000
pixels, NVIDIA's own limits for unlicensed passthrough
(`objgriddisplayless.c:38-39,54`), so 1080p fits and 4K does not, and whoever
raises one bound raises both. That is a property of the route now chosen, not
an open question about which route to take, and it belongs with the display
work rather than here.

`docs/DISPLAY.md` said "The choice between the three ways out is still open —
number 25". It is not open any more and that sentence is corrected there.
### 26. The EGLImage import itself is clean
**Resolved as three negative results.** Xwayland can no longer be traced,
so a one-purpose probe (`eglimport.c` — written for that session, not
kept in the tree) did exactly what glamor
does per window pixmap and nothing else. It established three things that
are *not* the cause. The import path itself is clean; see 35 for where the
failure actually sits.

### 27. `nvidia_drm vblank=0` throttles better than `vblank=1`
**Measured 2026-08-18; the design question it raises is open.** Our
`NVA083` path stands exactly where NVIDIA's own vGPU plugin stands: vsync
from a software timer, no emulated raster generator. The guest never asks
about vblank state — both relevant controls belong to a class we remove
from the class list, so it cannot. Numbers in [`llm.md`](llm.md).

### 28. 32-bit clients could not find NVIDIA's GLX
**Fixed 2026-08-18.** `/etc/ld.so.conf.d/nvrm-gl.conf` listed only the
64-bit directory, so `ldconfig` knew `libGLX_nvidia.so.0` only as x86-64
and a 32-bit process could not resolve it — although the 32-bit half had
been staged all along. The fix moved the failure rather than removing it;
see 35.

### 29. The crash depends on *whose* Xwayland instance it is
**Resolved 2026-08-18.** Xwayland can be started as an ordinary Wayland
client on a second display. Same binary, same version, same guest, same
driver — the only difference is who started it. Under the compositor's own
instance `glxgears` segfaults; under a self-started instance it runs at
59 FPS for ten runs of ninety seconds.

### 30. A client's FPS counter is not evidence of presentation
**Resolved as a rule for every further measurement.** `glxgears` reported
58.3–58.8 FPS while a human watching the screen saw the gears standing
still. A client counts *swaps*; whether an image reaches the screen is not
something it can know. This retracted an earlier conclusion of number 29.
Since then, a presentation claim needs a reader that looks at pixels.

### 32. Xwayland dies on SIGFPE inside NVIDIA's EGL core
**Resolved 2026-08-21 with the chain it belongs to.** The faulting
instruction is still named and the field still is not -- and the SIGFPE has not
recurred in the configuration that produced it. The original reasoning is kept
below. Twice, both
times at the same instruction inside `libnvidia-eglcore`, Xwayland took a
floating point exception and aborted. A division by zero means some value
we supply is zero where the driver assumes it cannot be. Number 39 cleared
three candidate controls, which answer completely and plausibly.

---

**CLOSED 2026-08-21 BY THE RUN NUMBER 35 HAD BEEN WAITING FOR.** The full
account is in number 35. The short version: the configuration that produced
this defect was assembled completely for the first time, and the defect did
not appear.

  * GNOME **Wayland** session -- the path number 24 proved all of these hang
    on;
  * Sunshine `capture=kms` with `h264_nvenc`, its own log reading
    *"Screencasting with KMS"*;
  * **Moonlight connected** and decoding HEVC -- the leg that had never
    worked, blocked by a Sunshine started without a Wayland environment
    (fixed the same day);
  * Shadow of the Tomb Raider running, 49 threads, with `steamwebhelper` on
    `/dev/nvidia0` beside it -- number 44's exact pair, `WinMain` and the
    overlay's renderer.

**19 minutes 25 seconds** against the 15-20 SECONDS this defect took to
appear, sampled every 20 s across 60 samples. `segfault at 8`: **0**.
`Failed to acquire the EGL Image`: **0**, where the crashing session logged
**744**. `GL_OUT_OF_MEMORY`: **0**. Xwayland never restarted, so the instance
number 44 calls a consumable was the same one throughout.

**The cause, as far as the evidence supports one: number 45.** A signal
arriving while the guest module waited for a reply it had ALREADY submitted
made the kernel restart the whole ioctl, so a one-shot escape was issued twice
and the second was refused -- Xwayland was told an attach had failed that had
succeeded. That is the right shape for a GL stack built around an id its owner
believes invalid, it landed AFTER both sessions that crashed, and until this
run it had never been tested against them.

**What would reopen this**, stated so the closure is falsifiable: a
`segfault at 8` in `libGLX_nvidia`, or `Failed to acquire the EGL Image`
returning on a Wayland session. Both are one `dmesg` and one `journalctl`
away, and both are in the crash-watch loop this run used.
### 33. The crash in 23 is a NULL pointer, and depends on state
**Resolved 2026-08-21 with the chain it belongs to.** The original reasoning
is kept below. All `segfault at 8` addresses of one boot were resolved back to
two instructions in `libGLX_nvidia`, both dereferencing offset 8 of a base
pointer that is exactly zero. That rules out the "wrongly mapped address"
hypothesis. The crash follows process state rather than the client, and
the instance appears to poison itself over time.

---

**CLOSED 2026-08-21 BY THE RUN NUMBER 35 HAD BEEN WAITING FOR.** The full
account is in number 35. The short version: the configuration that produced
this defect was assembled completely for the first time, and the defect did
not appear.

  * GNOME **Wayland** session -- the path number 24 proved all of these hang
    on;
  * Sunshine `capture=kms` with `h264_nvenc`, its own log reading
    *"Screencasting with KMS"*;
  * **Moonlight connected** and decoding HEVC -- the leg that had never
    worked, blocked by a Sunshine started without a Wayland environment
    (fixed the same day);
  * Shadow of the Tomb Raider running, 49 threads, with `steamwebhelper` on
    `/dev/nvidia0` beside it -- number 44's exact pair, `WinMain` and the
    overlay's renderer.

**19 minutes 25 seconds** against the 15-20 SECONDS this defect took to
appear, sampled every 20 s across 60 samples. `segfault at 8`: **0**.
`Failed to acquire the EGL Image`: **0**, where the crashing session logged
**744**. `GL_OUT_OF_MEMORY`: **0**. Xwayland never restarted, so the instance
number 44 calls a consumable was the same one throughout.

**The cause, as far as the evidence supports one: number 45.** A signal
arriving while the guest module waited for a reply it had ALREADY submitted
made the kernel restart the whole ioctl, so a one-shot escape was issued twice
and the second was refused -- Xwayland was told an attach had failed that had
succeeded. That is the right shape for a GL stack built around an id its owner
believes invalid, it landed AFTER both sessions that crashed, and until this
run it had never been tested against them.

**What would reopen this**, stated so the closure is falsifiable: a
`segfault at 8` in `libGLX_nvidia`, or `Failed to acquire the EGL Image`
returning on a Wayland session. Both are one `dmesg` and one `journalctl`
away, and both are in the crash-watch loop this run used.
### 34. The host writes one `dmesg` line per ioctl
**Named as a trap.** At `ResmanDebugLevel: 0` the driver still prints
`NV_DBG_INFO`, which is one line per ioctl. The line looks like a
rejection and is not. The damage is real twice over: a kernel log write on
a per-frame path costs time, and 2734 such lines had displaced every other
diagnosis from the ring buffer.

### 35. The EGLImage import failure is the head of the chain
**Resolved 2026-08-21. The chain is closed and the measurement is taken.**
The original reasoning is kept below. Xwayland's
own backtrace shows the failure originating in `libnvidia-eglcore` and
propagating up through glamor. Numbers 22-C, 23, 26, 32 and 33 all pointed
at this without naming it. Fixing the import is expected to resolve the
rest; nothing above it needs its own fix.

---

**BLOCKER FILED 2026-08-21. The next measurement this entry names was taken
and the chain did not reproduce.** Recording what was tried and at what
intensity, because a negative at a stated intensity is worth something and
"we tried" is not.

Number 44 says what it needs: *"Naming it needs a guest kept until it poisons
itself, with the trace already running."* That guest was kept.

**The session.** A GNOME **Wayland** desktop guest, the compositor's own
Xwayland (the instance number 44 says poisons itself, never restarted), up
~40 minutes, `bdf_mediation=1`, `display=1`, `vdisplay=1`, Sunshine capturing
throughout. Deliberately WITHOUT `LEA_DEBUG=2`: the previous attempt at this
ran under it, it sits on a per-frame path, and a state-dependent defect is
exactly the kind that debug output can move.

**What was driven at it:**

  * repeated concurrent `glmark2` + `vkmark` + `vkcube` on the compositor's
    Xwayland — a Vulkan client and an OpenGL client drawing at once under the
    compositor, which is the shape of the game-plus-overlay case, with
    Sunshine capturing as the third leg;
  * **750 GL client lifecycles** (`glxinfo`) against that same never-restarted
    Xwayland, plus repeated `glxgears`;
  * 1, 2, 4, 8 and 32 concurrent CUDA processes, twice, at two very different
    session states.

**Every detector stayed at zero:**

| detector | result |
|---|---|
| `segfault at 8` in `dmesg` | **0** |
| `Failed to acquire the EGL Image` in the journal | **0** |
| `glxgears` on the compositor's Xwayland | ran to its timeout every time |
| `glxinfo` control on the same display | worked every time |
| `NV_ESC_ATTACH_GPUS_TO_FD` answering `-1` | 0 |
| `BDF mediation OFF` in the guest log | 0 |

**And one confounder was removed on the way.** The first version of the
segfault detector read `dmesg` without `sudo`; the guest has
`kernel.dmesg_restrict=1`, so it failed with EPERM and `grep -c` reported 0 —
a counter reading zero for a reason that had nothing to do with segfaults.
Measured: plain `dmesg` 1 line (the error), `sudo dmesg` 709. Every zero above
is from the privileged read.

**Two code-side candidates were examined and neither fired.** The poisoned
object is built for `0xffffffff`, and `bdf_to_guest` in `virtio_nvrm.c` has two
ways to hand out an id the guest should not have: before `bdf_host_id` is
learned it passes host ids through untranslated, and `bdf_disabled` can switch
mediation off permanently mid-session — a path whose own comment records it
happening "in the middle of an X server start" and leaving nvidia-drm with a
half-mediated view, which is the right shape for this defect. On this session
the learning window closed at t=24.9 s, just after NVKMS attached and before
any graphics, and the `BDF mediation OFF` warning never fired. Also checked
and wrong: the idea that the state is per-process. `bdf_host_id` lives in
`struct nvrm_dev`, which is per virtio device, i.e. one per guest.

**A caution for the next reader, because it is load-bearing.** Number 44 reads
`0xffffffff` at `+0x30` as `NVRM_GPU_INVALID_ID`. The healthy values at that
offset are `0x14` and `0x13` — small integers, where a gpu id on this rig is
`0x2d00` natively and `0x6` in this guest. So `+0x30` is more likely an INDEX
whose "not found" is `-1` than a gpu id, and the match with
`NVRM_GPU_INVALID_ID` may be a coincidence of value. That matters because it
is the link the whole "something hands libGLX_nvidia a -1" reading rests on.

**What this run therefore establishes:** the poisoning is not reached by GL
client churn at 750 lifecycles, not by concurrent GL and Vulkan load under the
compositor, not by 40 minutes, and not by driving the backend to 2010 open
descriptors. That is a much stronger negative than the previous one (48
lifecycles, six rounds) and it points the same way.

**What the next attempt needs, and it is now a short list.** The two sessions
that DID poison had one thing this run could not reproduce: **a real game
under Steam, with its overlay** — a Vulkan application and an OpenGL overlay
inside one process tree, plus Moonlight actually connected. Everything else
about those sessions has now been driven harder than they were. So either that
combination is the variable, or the poisoning was removed by number 45's fix
(the signal-restart double-submit that made a one-shot escape fail spuriously),
which landed after both poisoned sessions and has never been tested against
them. **Those two hypotheses are now the whole of this question**, and the
first is one session with Steam away from being decided.

**THE GAME WAS RUN, AND IT DID NOT CRASH.** The variable named above as the one
this run could not reproduce was reproduced after all: the `desktop` rig was
brought up with `--games --with-steam`, and **Shadow of the Tomb Raider — the
exact title of number 44 — was launched under Steam** in the GNOME Wayland
session.

It rendered: **2100 MiB of device memory, 37 % GPU utilisation, 49 threads**,
windows on the compositor's Xwayland, and `steamwebhelper` holding
`/dev/nvidia0` beside it — so the Vulkan application and the OpenGL overlay
were both live in one process tree, which is the configuration number 44
describes. It ran for **about ten minutes** — `ps` read its elapsed time at 3:51 and a
watcher sampled every 20 seconds for 360 s after that — where number 44 records
the crash arriving **15–20 seconds after launch**. So it survived roughly
**30× the interval in which it previously died**.

Every detector stayed at zero throughout: `segfault at 8`, `Failed to acquire
the EGL Image`, `GL_OUT_OF_MEMORY`, `ATTACH_GPUS_TO_FD` answering `-1`,
`BDF mediation OFF`.

**ONE DIFFERENCE REMAINS AND IT IS BLOCKED ON THE IMAGE, NOT ON THE
QUESTION.** Number 44's session was being STREAMED — Sunshine on
`capture=kms` with Moonlight connected. This rig's Sunshine came up
`capture=portal`, and Moonlight is refused by it:

    Launch response: status_code="503"
    "Failed to initialize video capture/encoding. Is a display connected
     and turned on?"

which is the documented portal behaviour the rig warns about at boot
(*"An unpatched Sunshine stops at the portal's permission dialog; bake with
`--desktop-session xorg` for the path the numbers were taken on"*). So the
streaming leg could not be added, and closing it needs a **rebake**, not
another run of this one.

**WHAT THIS LEAVES, and it is now two named things rather than a mystery:**

1. **The stream is the last untested variable.** `--desktop-session xorg`,
   `capture=kms`, Moonlight connected, then the game. That is one bake and one
   run.
2. **Or number 45 already fixed it.** The signal-restart double-submit that
   made a one-shot escape fail spuriously landed AFTER both sessions that
   poisoned, and has never been tested against them. Everything this run drove
   at the defect — 750 GL client lifecycles, concurrent GL and Vulkan under the
   compositor, 2010 open descriptors, and now the game itself — is consistent
   with the defect no longer being there.

**If the next run closes 1 and the game still does not crash, the honest
reading is 2**, and 22-C, 23, 32, 33, 35 and 44 close together on that
evidence. That is six entries on one measurement, which is why it is worth
doing properly rather than quickly.

---

**THE MEASUREMENT THIS ENTRY EXISTED FOR, TAKEN 2026-08-21.** This entry said
the chain was closed and the next measurement was not taken. It is now, and
the chain closes with it: **22-C, 23, 32, 33, 35 and 44 together.**

**What was assembled, and why it took this long.** Number 24 established that
all of these hang on the Wayland path, so an Xorg run proves nothing --
`glxgears` runs there and the display gate is 12/12. Number 44's crashing
session was GNOME Wayland with Sunshine on `capture=kms` and Moonlight
connected. Every earlier attempt at reproduction was missing the streaming
leg, and the reason turned out to be a defect of ours rather than a choice:
the GNOME branch started Sunshine with `env DISPLAY=$D XAUTHORITY=$XA` read
out of gnome-shell's environ, and **under Wayland that environ carries
neither**. Sunshine came up with no session, could not enumerate outputs even
for KMS, and answered Moonlight with 503 *"Is a display connected and turned
on?"* -- under `portal` and under `kms` alike. With the environment passed
(`WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR`, `DBUS_SESSION_BUS_ADDRESS`) its log
reads *"Screencasting with KMS"* and Moonlight decodes HEVC off it.

**The run.** GNOME Wayland, `capture=kms`, Moonlight connected, Shadow of the
Tomb Raider (49 threads) and `steamwebhelper` both holding `/dev/nvidia0`:

    game runtime                      19m 25s   (defect appeared in 15-20s)
    samples                           60, every 20 s
    segfault at 8                      0
    Failed to acquire the EGL Image    0        (crashing session: 744)
    GL_OUT_OF_MEMORY                   0
    Xwayland restarts                  0

**The cause this points at is number 45**, and it is the one number 44 called
"the right shape": the signal-restart double-submit that made a one-shot
escape be issued twice and the second refused, so Xwayland was told an attach
had failed that had succeeded. It landed after both sessions that crashed and
had never been tested against them.

**Honest about what this is.** It is a strong negative in the exact
configuration, not a proof of mechanism -- nobody has shown the poisoned
object being built, because it could not be produced again to watch. The
closure is falsifiable and the test is cheap: a `segfault at 8` in
`libGLX_nvidia`, or `Failed to acquire the EGL Image` on a Wayland session,
reopens it. Both are one `dmesg` and one `journalctl` away.

**Six entries closed on one measurement**, which is what this entry predicted
when it said *"Fixing the import is expected to resolve the rest; nothing
above it needs its own fix."* That prediction held.

**TWO HONEST LIMITS OF THAT RUN, recorded so the closure is not read as more
than it is.**

*The game exited on its own after about twenty minutes, and it exited
CLEANLY.* No `SIGSEGV`, and `coredumpctl` lists four cores for the whole day,
all of them the 32-bit `steam` client faulting in `libX11` at `0x4d0` during
startup, none of them `ShadowOfTheTombRaider`. The crashing session of number
44 left **six** cores including a 1.7 GB one for `WinMain`. So the exit is not
the defect wearing a different hat.

*No mapped window was ever confirmed.* `wmctrl` listed none, and `fbprobe`
read the scanout as **STATIC** -- content present (peak 1014/1024) but
unchanged across 9 polls. The game was demonstrably doing GPU work (2.2 GB of
device memory, 49 threads, 16-17 % utilisation) but nothing here proves it
reached the screen, and by number 30's rule that distinction has to be made
rather than assumed.

That matters less than it might, because number 44's crash arrives **15-20
seconds after launch** -- during startup, at or before window mapping -- and
this run passed that point by roughly sixty times without a fault. But a run
that had reached a drawing game would be stronger evidence than one that may
have sat in a loading screen, and the next attempt should confirm presentation
with a reader that looks at pixels or with a human, exactly as `llm.md` says.

### 36. Two traps of our own making
**Named.** An empty `LEA_DEBUG` switches the firehose *on*, because an
empty value is still a set variable; and a cleanup path deleted the
deadline the frame limiter was waiting on, so the limiter silently did
nothing. Both are in [`llm.md`](llm.md), and every measurement taken
before the fixes had the firehose in it.

### 37. The frame limiter reaches only some clients
**Partly corrected by 41.** Measured at one target rate, the limiter held
`vkcube-wayland` at 56 FPS and did not hold X11 `vkcube` at all. Number 41
swept the whole range and found the conclusion was drawn from too few
points.

### 38. The limiter triggered a double free in the guest
**Resolved; it does not belong in the default.** With the limiter on, the
guest hit `BUG()` in `__slab_free` under `nv_drm_free`, reached through
nvidia-drm's semaphore-surface callback, after which the guest was
unreachable. The limiter is opt-in for this reason.

### 39. Three more candidates for the zero divisor are cleared
**Resolved as a negative result.** The three controls NVIDIA's EGL core
issues immediately before the faulting instruction all answer completely
and plausibly, with sane values. They are not the source of the zero in
number 32.

### 40. The late-unregister race was the cause
**Resolved 2026-08-18.** With the race fixed and the limiter on, `Failed
to acquire the EGL Image` went from 624 occurrences to zero, and CS2 ran
smoothly at about 58 FPS against a 60 Hz target — confirmed by a person
watching the screen, not by an FPS counter.

### 41. Correction to 37: the configured rate holds across the range
**Resolved 2026-08-18.** Swept at 30, 60 and 120 Hz with a fresh client
instance per point: the limiter lands monotonically close to the target at
all three. Number 37 had only the 60 Hz point and drew too much from it.
X11 `vkcube` escapes only sometimes, not always.

**How far it escapes, measured 2026-08-20.** With `LEA_FRL_HZ` unset (the
limiter off, confirmed by the absence of the "frame limiter on" line) and
a guest whose only mode is 1920x1080 at 59.96 Hz, plain `vkcube` on the
compositor's Xwayland ran 900 frames three times in a row at **590.6,
595.6 and 575.4 FPS** -- about ten times the display. So the limiter is
still needed for this client and the display's own rate does not bound it.

Two traps this measurement walked into and out of, both worth the ink. The
FIRST run of the four gave 44.3 FPS and would have supported the opposite
conclusion; it was window-mapping and startup cost inside the 900-frame
window, and repeating it is what exposed that. And the operator watching
the stream reported about 64 FPS at the same time as these 590 -- not a
contradiction but number 30's rule from the other side: 64 is what reached
a screen, 590 is what the client swapped.

### 42. Is `capDescriptor` on 0xc640 really an fd that needs no translation?
**Resolved 2026-08-21: it IS a file descriptor, and it DOES need
translation.** Settled by reading the driver (option 3), which is what the
operator chose. The original reasoning is kept below. `NV0080_CTRL_CMD_FIFO_...` class
0xc640 carries a `capDescriptor` in its alloc parameters, which is an fd in
the guest's numbering, and `alloc_fd_field` in
[`crates/nvrm-abi/src/xlate.rs`](../crates/nvrm-abi/src/xlate.rs) has no
entry for it -- so it is forwarded untranslated. Nothing has been observed
to break, because the field is MIG-only and no measured workload allocates
that class. Two answers are possible and only one is right: either the
field is never a real fd on this path, or the entry is missing and a MIG
guest would hand the host a number from its own table. Deciding it needs a
workload that allocates the class, not more reading.

---

**DECISION MEMO, written 2026-08-21. Nothing below decides it — but this
entry's PREMISE has changed and that part is a measurement.**

**The premise "no measured workload allocates that class" is false.** Class
`0xc640` is `AMPERE_SMC_MONITOR_SESSION`, and `nvidia-smi -q` allocates it —
once, natively, in the `nvml` probe. From the native trace:

    nvos64  hRoot=0xc1d5363e hParent=0xc1d5363e hNew=0xa55a0010
            hClass=0xc640 paramsSize=0x0 flags=0x0 status=0x0

**And it passes no parameters at all.** `paramsSize=0x0`, so
`NVC640_ALLOCATION_PARAMETERS { NvU64 capDescriptor }` (clc640.h:38, 8
bytes, `xlate.rs:812`) is not supplied on this path. The tracer's before and
after dumps of that buffer are byte-identical (`0d 00 00 00 00 00 00 00` on
both), so RM neither read a meaningful value out of it nor wrote one into
it — the bytes are the caller's own leftovers, which is what the
written-ness mask exists to recognise.

So the field this entry asks about is **not exercised by the one workload
that reaches the class**, and it is exercised by nothing in a guest at all:
the class is never allocated there (number 65). That narrows the question
without answering it — it is still "is `capDescriptor` an fd", and there is
still no observation of it carrying one.

**Option 1 — leave it untranslated (status quo).** No entry in
`alloc_fd_field`.
*Cost:* nothing today, now better founded than when this entry was written:
the only observed allocation supplies no `capDescriptor` at all.
*Downstream:* a MIG guest that did supply one would hand the host a number
from its own fd table. This card is Turing and has no MIG, so that guest
cannot be built here.

**Option 2 — add the `alloc_fd_field` entry now.**
*Cost:* one table row, and a real risk in the other direction: if the field
is not an fd on some path, translating it corrupts a valid value. The two
event classes that ARE in that table needed an `alloc_fd_guard` for exactly
this reason, because `NV0005_ALLOC_PARAMETERS` reuses `data` for an fd and
for a callback pointer. There is no observation here to build a guard from.
*Downstream:* a wrong translation of a field nothing supplies is invisible
until the first MIG guest, which is the worst place to find it.

**Option 3 — decide it by reading, not by workload.** `capDescriptor` is an
`NvU64` and the question is whether RM's `0xc640` constructor calls
`osUserHandleToKernelPtr` on it, the way the event path does. That is a read
of open-gpu-kernel-modules, and it is exactly the kind of statement the
`attested` column discussed in the design conversation is FOR — a human
statement with a citation, which is not the same claim as `verified` and
must not share a column with it.
*Cost:* an hour of reading and a citation.
*Downstream:* it answers the question permanently and without MIG hardware,
and it is the only option that produces evidence rather than a bet.

**Recommendation.** Take **3**, and keep **1** until 3 says otherwise. The
measurement above removes the urgency (nothing supplies the field) without
removing the question, and reading the constructor is the only route
available on hardware that has no MIG.

**A person answers with one word:** `1`, `2`, `3`, or `leave-open`.

---

**ANSWERED 2026-08-21 by reading open-gpu-kernel-modules at
`DRIVER_VERSION`. The entry offered two possibilities and the first one is
false.**

*The header says it outright* (`src/common/sdk/nvidia/inc/class/clc640.h:38-46`):

    // capDescriptor is a file descriptor for unix RM clients, but a void
    // pointer for windows RM clients.
    //
    // capDescriptor is transparent to RM clients i.e. RM's user-mode shim
    // populates this field on behalf of clients.

*And the code does exactly that.* `migmonitorsessionConstruct_IMPL`
(`src/nvidia/src/kernel/gpu/mig_mgr/mig_monitor_session.c:52-66`) passes
`pUserParams->capDescriptor` to `osRmCapAcquire`, which on Linux
(`src/nvidia/arch/nvalloc/unix/src/os.c`) begins

    int fd = (int)capDescriptor;

and ends in `os_nv_cap_validate_and_dup_fd(cap, fd)` →
`nv_cap_validate_and_dup_fd` (`kernel-open/nvidia/nv-caps.c:510`), whose first
act is **`fget(fd)`** — a lookup in the CALLING PROCESS's file descriptor
table.

**So the answer to the question as asked is: the entry is missing, and a MIG
guest would hand the host a number from its own table.** That is settled, and
it needed no MIG hardware to settle — only reading, which is why this was the
right option.

**WHAT SAVES IT FROM BEING A PRIVILEGE HOLE, and it is worth knowing:** the
failure is fail-closed three times over. `fget` on an absent fd returns NULL →
`-1` → `NV_ERR_INSUFFICIENT_PERMISSIONS`. Even a number that names a live host
fd is rejected unless `file->f_op == &g_nv_cap_drv_fops`, i.e. it is an fd on
**nv-cap-drv** and nothing else, and then only if its device minor matches the
capability asked for. A stray guest number cannot borrow a host capability; it
can only be refused. And where the capability filesystem is absent entirely,
`osRmCapAcquire` answers `NV_ERR_NOT_SUPPORTED` and the constructor falls back
to `rmclientIsAdmin()`, so the fd is not consulted at all.

**WHY THE ROW IS STILL NOT ADDED, which is now a specific engineering reason
and no longer "nobody has looked".** Adding `0xc640` to `alloc_fd_field`
naively would make things worse on the hardware we have, in two ways the
module's own code names:

1. **`fd_off + 4 > c->size` → `-EINVAL`** (`virtio_nvrm.c`, the fd-field
   step). The one observed allocation of this class passes
   **`paramsSize=0x0`** — measured, `nvml` trace — so there is no params
   block to read a descriptor out of, and the module would refuse an
   allocation that works today.
2. **An unpopulated `capDescriptor` is `0`, and `0` is not the pass-through
   sentinel.** The module passes `n < 0` through unchanged; `n == 0` is
   resolved as a real fd, and fd 0 is stdin. That turns a benign RM refusal
   into `-EBADF` from our own module.

So the row needs a **guard**, in the sense `alloc_fd_guard` already exists for
the two event classes — and unlike those, this struct has no second field to
discriminate on, so the guard has to be built from an observation of a
POPULATED `capDescriptor`. That needs a MIG-capable card; this one is Turing.

**Owed, and blocked on hardware rather than on understanding:** a guarded
`alloc_fd_field` entry for `0xc640`, validated against a real
`AMPERE_SMC_MONITOR_SESSION` allocation. Until then the field is unexercised
(number 65: the class is never allocated in a guest at all) and the failure
mode if it ever were is a refusal, not a leak.
### 43. Which FD does the driver require the mapping ioctl on?
**Resolved 2026-08-21.** The code works, the prose that disagreed with it was
wrong, and the measurement this entry named as the thing that would settle it
has been taken. The original reasoning is kept below.
[`crates/nvrm-client/src/mem.rs`](../crates/nvrm-client/src/mem.rs) issues
the map ioctl on `rm.ctl()`, and that path is measured and works. The
prose in this repository claimed for a while that it must go to the
freshly opened fd instead. The doubt is written down rather than resolved:
the two arrangements have never been compared against a real libcuda
trace, which is what would settle it. Until then the code is the
statement, not the prose.

---

**DECISION MEMO, written 2026-08-21. The FACTUAL question is settled by the
measurement this entry itself named; what is left for a person is what to do
about it.**

This entry says the two arrangements "have never been compared against a
real libcuda trace, which is what would settle it". There are 41 such traces
under `matrix/traces/610.57.04/` now. Read out of them:

**`NV_ESC_RM_MAP_MEMORY` is issued on a `ctl` fd, in every probe, without
exception** — and never on a `gpu` fd:

| probe | 0x4e issued on | calls |
|---|---|---|
| `cuda-core`, `cuda-jit`, `cuda-launch` | `ctl` fd 10 | 29 each |
| `nvdec` | `ctl` fd 11, `ctl` fd 42 | 29, 13 |
| `nvenc` | `ctl` fd 11, `ctl` fd 41 | 29, 87 |
| `opencl` | `ctl` fd 7 | 29 |

**And the prose was wrong because it conflated two different fds.** The
mapping protocol uses two, and the trace shows them plainly, in order:

    open  ctl fd=10
    ...
    MAP_MEMORY  on ctl fd=10   (hMemory 0x5c000006)
    open  ctl fd=16
    mmap  ctl fd=16 len=4096

The **ioctl** goes to the process's primary `ctl` fd every time. The
**mmap** goes to a FRESHLY OPENED fd, one per mapping, opened immediately
before it. "It must go to the freshly opened fd" is true of the `mmap` and
false of the `ioctl`.

**This is what `mem.rs` already does.** The ioctl is
`rm.ctl().ioctl_raw(sys::NV_ESC_RM_MAP_MEMORY_DMA, &mut p)`
([`crates/nvrm-client/src/mem.rs:221`](../crates/nvrm-client/src/mem.rs)) and
the map target is `gpu.open_for_mapping(rm.ctl())` at :314. Its own module
doc has said so since it was written — *"Exactly one mmap context per fd"*
and *"Traces show a fresh `open` before every NV_ESC_RM_MAP_MEMORY"*. The
code was right, the doc beside the code was right, and only the prose
elsewhere was wrong.

**Option 1 — close this as resolved.** The code is confirmed by libcuda's
own behaviour over 41 traces and 6 independent userspaces (CUDA, NVDEC,
NVENC, OpenCL).
*Cost:* none.
*Downstream:* one fewer open question, and the measurement is on record so
the doubt cannot come back without new evidence.

**Option 2 — close it and also record WHY the prose went wrong**, i.e. that
the two-fd protocol has a fresh fd in it and it is the mmap target.
*Cost:* two sentences.
*Downstream:* this is the failure mode that produced the doubt in the first
place, and it will produce it again for the next reader who sees
`open_for_mapping` and remembers "fresh fd".

**Option 3 — leave open** and require a counter-test that issues the ioctl
on the fresh fd to see whether RM also accepts it.
*Cost:* a probe.
*Downstream:* it would answer "what does RM tolerate", which is a different
and less useful question than "what does the driver require" — and this
entry asks the second.

**Recommendation.** Take **2**. The entry's own criterion is met: it asked
for a comparison against a real libcuda trace and that comparison is above.
It is left open here only because this run was asked not to decide the
entries in this group.

**A person answers with one word:** `1`, `2`, `3`, or `leave-open`.

---

**DECIDED 2026-08-21 by the operator: option 2 — close it, and record why the
prose went wrong**, because that is the part that will otherwise be
re-derived.

**The measurement settled it** (the tables are above): `NV_ESC_RM_MAP_MEMORY`
is issued on a `ctl` fd in all 41 libcuda traces, across six independent
userspaces (CUDA, NVDEC, NVENC, OpenCL, GL, Vulkan), and never on a `gpu` fd.
`mem.rs` already does exactly that.

**AND HERE IS WHY THE DOUBT EXISTED, which is worth more than the verdict.**
The mapping protocol uses **two** fds and the prose collapsed them into one:

    open  ctl fd=10
    ...
    MAP_MEMORY  on ctl fd=10   (hMemory 0x5c000006)   <- the IOCTL
    open  ctl fd=16                                   <- a FRESH fd
    mmap  ctl fd=16 len=4096                          <- the MMAP TARGET

"It must go to the freshly opened fd" is **true of the `mmap` and false of the
`ioctl`**. There IS a fresh fd, one per mapping, opened immediately before —
`mem.rs`'s own module doc has said so since it was written ("Exactly one mmap
context per fd", "Traces show a fresh `open` before every
NV_ESC_RM_MAP_MEMORY"). Someone read that, remembered "fresh fd", and attached
it to the wrong call.

So the failure here was never in the code or in the trace. It was a sentence
that named a real thing and pointed it at the neighbouring operation, and the
only reason it survived is that both statements are true of *something*. When
a piece of prose about this codebase disagrees with the code, check whether it
is describing the step next door.
### 44. A game and the compositor die at the same two addresses in NVIDIA's GL core
**Resolved 2026-08-21.** It was the first stack the chain had, and the chain
is closed. The original reasoning is kept below in full -- the core-dump work
is the most detailed evidence this project has about the defect, and it stays
whether or not the defect ever returns. Shadow of
the Tomb Raider (the native Feral port, Vulkan) crashes 15-20 s after
launch in a GNOME **Wayland** session. Two processes dump core at the SAME
frame #0 and #1:

    #0  libnvidia-glcore.so.610.57.04 + 0xd0a989
    #1  libnvidia-glcore.so.610.57.04 + 0xf985b0
    #2  ShadowOfTheTombRaider + 0x1df9535

-- `WinMain`, the game's own process, and `WebViewRenderer`, the Steam
overlay's CEF renderer. The game renders with Vulkan; the overlay with
OpenGL. Both end in `libnvidia-glcore` at one address.

Beside it, the compositor logs **744** EGL failures in the same session,
with backtraces into `libnvidia-eglcore` -- the library number 35 names as
the head of the chain -- and the visible symptom is the one number 10 had:
windows go invisible while gnome-shell keeps running and the scanout
buffer stops changing (`fbprobe`: STATIC, 0/9 polls, peak 1024/1024, so
full rather than black).

Measured 2026-08-20 on 610.57.04, guest module loaded, KMS capture. What
is NEW here and was not available before:

- a reproducer that takes 15-20 seconds rather than hours,
- six core dumps kept under `vm/out-eglcrash/` (1.7 GB for the main one),
  with `coredumpctl info` output beside them,
- both crash sites at once: `glcore` for the clients, `eglcore` for the
  compositor.

**Hypothesis on the table, from the operator:** a missing or wrongly
mediated ioctl in the GL/EGL path. What speaks for it: this is exactly
where a mediated call would surface, in a library that assumes an answer
it did not get. What speaks against it, so far: the backend's failure log
for that session contains no unmediated call and no unknown class -- the
failures it does record are `NV_ERR_NOT_SUPPORTED` on `0x2080012f` /
`0x20800157` and `NV_ERR_OBJECT_NOT_FOUND` on `0x2080014b`, all three of
which probe/README.md section 6 lists as occurring in the DIRECT run as
well. The one group not on that list is the `0x73xxxx` family (NV0073,
display controls), 30 calls answering `NV_ERR_OBJECT_NOT_FOUND` -- which
is plausible for a display NVKMS invents and no physical monitor backs,
and is worth ruling out rather than assuming.

**The native counter-check is in, and it does not crash.** Measured
2026-08-20 on this host: the same game, native, **under Wayland**
(Hyprland), 1440p, about **100 FPS**, stable -- no crash, no EGL failures.
So the two `glcore` addresses are not a place NVIDIA's driver dies on its
own under a Wayland compositor, and whatever brings it there is on the
guest side of the boundary.

Two differences remain between the two runs and have to be closed before
this is called proven: the host compositor is Hyprland (wlroots) and the
guest's is mutter (GNOME), and the resolutions differ (1440p host, 1080p
guest). A GNOME Wayland session on the host would remove the first one.

What is therefore still open is the operator's hypothesis, and it is now
the leading one: a missing or wrongly mediated call in the GL/EGL path.
The remaining measurement is the last call before the crash --
`LEA_DEBUG=2` logs every forwarded one, `LEA_CTRL_DUMP` dumps the ANSWER
of a named control, and that second one matters because the failure class
here may be "an answer that looks valid and is wrong" (number 32), which
a status comparison cannot see and only the bytes can.

Under X11 the same guest runs the same game; the `display` gate is 12/12
green on that path (2026-08-20).

**The crash site is read out of the core, and the bad value is a RETURN
value.** Measured 2026-08-20 from `vm/out-eglcrash/winmain.core` -- which
is the only core still kept in that directory -- against the host's
libraries:

- The two addresses are **not in one function**. `.eh_frame` FDEs put
  `0xd0a989` at +0x139 of `[0xd0a850..0xd0aada)` and `0xf985b0` at +0x1e0
  of `[0xf983d0..0xf98763)`. The second is the return address of
  `call *0x110(%rax)`, so frame #1 calls frame #0 as a virtual method,
  slot `0x110` of the vtable at `glcore+0x26fd1d0` -- confirmed in the
  core, where `vtable[0x110]` is `glcore+0xd0a850` exactly.
- The faulting instruction is `mov 0x8(%rdx),%rdx` with `rdx == 0`, so the
  fault address is exactly **8**: the signature number 33 describes.
- `rsi` is **not an argument**. It is the return value of the indirect
  call at `glcore+0xd0a925`, and glcore checks it (`test %rax,%rax; je`)
  and accepts it. The object is non-NULL and hollow: `+0x00..+0x27` are
  all zero, including the `+8` pointer that faults, while `+0x28 = 9`,
  `+0x30 = 0xffffffff`, `+0x50`, `+0x58 = 0x1d5`, `+0xa0` and
  `+0xa8 = 0x20164010` are populated. A constructed object whose leading
  fields were never filled -- not a fresh allocation, not a wild pointer.
- The producer is **`libGLX_nvidia.so.610.57.04 + 0x83240`**, a function
  entry (FDE `[0x83240..0x83449)`), reached through the dispatch table
  `[[[this+0x5c8]+0x58]+0xba0] + 0x28da0`. It is called with the keys of
  the enabled entries of `this+0x270` (4 of 4, stride 0x58), the count,
  and a selector of **-1** -- and the object it hands back carries `-1`
  at `+0x30`.
- The loop then compares `key->+8` against `returned->+8->+8` and dies on
  the **first** of the four entries.

So the failure class is the one number 32 named and a status comparison
cannot see: **an answer that looks valid and is wrong.** What is new is
that the answer has an author -- `libGLX_nvidia`, the library numbers 23
and 33 also end in -- and that glcore's own NULL check waves it through.

The libraries are the host's bytes by construction: `lea_gl_stage` copies
them out of `lea_nvidia_libdir` (here `/usr/lib`), pinned to
`DRIVER_VERSION`. Recorded so a later run can check it directly rather
than trusting that: `libnvidia-glcore.so.610.57.04` sha256
`3a43bc796820f6ef4c102587db14284d90e9292a2b47d03ad4c1343cc8b3305a`,
`libGLX_nvidia.so.610.57.04` sha256
`7ef1112f99de62db27670075e2dd1318235bbb3fe769850bf714b0c7e657784c`.

**The `0x73xxxx` group is ruled out, not assumed.** All of them -- 32 of
them, not the 30 written above -- come from `nvidia-modeset`, and all fall
in lines 9..66 of a 1410-line log, i.e. session startup. No process on the
crash path (the game, `steamwebhelper`, `Xwayland`, `gnome-shell`) issues
a single one.

**The failure list above is incomplete.** That session records 263
failures in 36 distinct (cmd, status, proc) combinations. Two of them come
from crash-path processes and are NOT in probe/README.md section 6:

- `NV_ESC_ATTACH_GPUS_TO_FD` (`nr 0xd4`, `NV_IOCTL_BASE + 12`) answering
  **`ret -1`** to **Xwayland**, four times, during the game session (log
  lines 321, 436, 463, 962).
- `0x90960101` answering `0x80` to `steamwebhelper`, once.

The first matters because the comment on `bdf_rewrite_attach_gpus()`
([`guest-module/virtio_nvrm/virtio_nvrm.c`](../guest-module/virtio_nvrm/virtio_nvrm.c))
describes exactly this `-1` as a bug found on 2026-08-15 and fixed there.
Its guard needs `dev_tag == NVRM_DEV_CTL` and `bdf_on(dev)`; the log says
`dev 0`, `NVRM_DEV_CTL` is `0`, and `provision.sh` turns `bdf_mediation`
on for the display path this session used. The guard is therefore
satisfied, the rewrite ran, and the call still fails. Unexplained -- and
it is the only failure in the crash path that names the binding between a
GPU and an fd.

**A near miss, written down so it is not walked into twice: the
cross-session fd tokens are NOT the cause.** The log shows 20
`fd_field_token ... CROSS-SESSION` lines and zero `STALE` ones, which
reads exactly like the chain the diagnostic in
[`crates/vhost-user-nvrm/src/nvrm.rs`](../crates/vhost-user-nvrm/src/nvrm.rs)
was added to prove. It is not: that diagnostic resolves against the
CALLER's mirror, while the real translation uses `req.fd_field_proc`
(protocol v6). The whole log contains **no refusal line at all**
(`session N: ...`), so no `EBADF` on `fd_field_token` was ever returned.
The diagnostic fires on the healthy v6 path too, and as written it invites
the wrong conclusion.

**Why this is still not the last call.** The `nvrm.log` kept beside the
cores is not a `LEA_DEBUG=2` trace -- it holds failures, the fd census and
events, 263 failure lines out of 1410. The last forwarded call before the
SIGSEGV is therefore not in the evidence and cannot be recovered from it;
it needs a re-run. What that re-run now has that the last one did not: a
named producer (`libGLX_nvidia+0x83240`), a named suspect call
(`NV_ESC_ATTACH_GPUS_TO_FD` on Xwayland), and two processes to filter to
instead of a whole desktop session.

*Unverified:* that the hollow object and the four failed
`NV_ESC_ATTACH_GPUS_TO_FD` calls are one defect. Both sit on the
Xwayland/GLX path and both concern which GPU an fd is bound to, but
nothing measured so far links the crashing lookup to an fd whose attach
failed. `LEA_DEBUG=2` together with `LEA_OBJLOG=1`, filtered to
`Xwayland` and `steamwebhelper`, is the measurement that would decide it.

Two side findings, not pursued here: the fd census ends the session at 799
of 1021 process fds on `nvidiactl` with `unaccounted -201`, a negative
number that should not be possible; and `Failed to acquire the EGL Image`
stands at 744 occurrences after number 40 measured it down to zero, on a
different session type (GNOME Wayland rather than the CS2/X11 run).

**The game is not needed: `glxgears` is the same crash, in two seconds.**
Measured 2026-08-20 in the guest that was still up from the crash session,
against the compositor's own Xwayland (pid 7453, never restarted):

- `glxgears` on `DISPLAY=:0` dumps core. `glxinfo -B` on the same display
  succeeds and reports NVIDIA `4.6.0`, renderer `Leandro RTX 2070/PCIe/SSE2`,
  8192 MB. The GL stack is up; only the second client dies.
- `dmesg`: `glxgears[11477]: segfault at 8 ... in
  libGLX_nvidia.so.610.57.04`, and beside it **four**
  `FeralLinuxMessa[...]: segfault at 8` in the same library from the game
  session. Number 23's signature, and the game's own launcher shares it.
- The faulting instruction is `cmp %rcx,0x8(%rdx)` at
  `libGLX_nvidia+0x83715`, inside `[0x836e0..0x8376f)`: a linked-list walk
  (head at `container+0x10b0`, next at `+0x50`, hit cached at `+0x10b8`)
  that dereferences `node->+8` for the comparison key. The core gives
  `rdx == 0`, and **both** nodes in that list carry `+8 == NULL`.
- The other branch of that same function calls **`0x83240`** -- the
  function that produced the hollow object in the game's core. The two
  crash sites are the fast and the slow path of one lookup over one
  object type.
- The faulting node reads `+0x28 = 9`, `+0x30 = 0xffffffff`, head zeroed:
  **the same layout and the same values as the game's hollow object**. The
  search keys match as well, `{1, self-pointer, 0x103, 0}` in both cores.

So numbers 23, 33 and 44 are one defect: objects of this type are created
with a NULL pointer at `+8`, and every consumer that walks them dies at
address 8 -- glcore's array search for the game, libGLX_nvidia's list walk
for `glxgears`.

**And a sequence diff would not have found it.** The backend recorded
exactly the same two failures for the client that WORKS and the client that
CRASHES -- `0x2080012f` answering `NV_ERR_NOT_SUPPORTED`, twice each, which
probe/README.md section 6 lists as benign. `NV_ESC_ATTACH_GPUS_TO_FD` did
not fire during the `glxgears` crash at all, which retires the *Unverified*
guess above instead of confirming it. The crash is invisible to a status
comparison, exactly as number 32's failure class predicts; the core dump
found it, the log could not.

What the next measurement gets from this: a reproducer that takes two
seconds, needs no Steam, no game and no Moonlight, runs over
`showcase.sh ssh`, and comes with a WORKING control (`glxinfo`) in the same
session on the same display. `LEA_DEBUG=2` across that pair is a diff of
two clients that differ only in the outcome -- tighter than the native host
run, which differs in compositor, resolution and kernel. Evidence under
`vm/out-glxgears/`: the core (33 MB), the dmesg lines, the backend delta.

**Number 29's discriminator holds on demand, and both arms were taken in
the same minute.** In that same guest: `glxgears` on the compositor's
Xwayland (`:0`) takes SIGSEGV at address 8, and `glxgears` on an Xwayland
this session started itself (`:3`, an ordinary Wayland client of the same
mutter) runs its full twelve seconds and reports 71.7 then 60.0 FPS, with
`glxinfo` naming the same renderer on both. Same binary, same driver, same
guest, same compositor underneath; the only variable is who started
Xwayland. Per number 30 that FPS figure is a swap counter and NOT a claim
that anything reached a screen -- what is measured here is only that the
process does not die.

That pair is the experiment number 44 has been waiting for: two clients
that differ in outcome and in nothing else, both driven over
`showcase.sh ssh`. `LEA_DEBUG=2` across it needs a backend restart, since
the level is read once per process, and a restart costs the live crashed
instance -- which is why the evidence above was written out first.

**The restart was taken, and the crash did not come back.** Measured
2026-08-20 on a guest brought up fresh with `LEA_DEBUG=2`:

- `glxgears` on the compositor's Xwayland runs, 58.8 FPS. Six rounds of
  client churn -- eight `glxinfo` and two `glxgears` each, then a test run
  -- did not change that, and `dmesg` counted zero `segfault at 8`. So the
  state number 33 calls self-poisoning is NOT reached by use: not by time,
  not by client count, not by drawing.
- A second fresh session, traced to 578_690 calls, is equally clean: no
  crash, no refusal, and not one `ret -1` anywhere.

So the reproducer of the section above is a reproducer only on an ALREADY
poisoned instance. What poisons it is still unmeasured, and the two
sessions that had it (the game's and the one `glxgears` was caught on) are
both gone. Naming it needs a guest kept until it poisons itself, with the
trace already running -- which is now cheap to arrange and was not before.

**What the healthy instance did give is the missing half of the
comparison.** Breaking at `libGLX_nvidia+0x836e0` in a healthy guest and
walking the same list shows two nodes, exactly as the poisoned core had,
and they differ from it in precisely two fields:

| field | healthy | poisoned |
|---|---|---|
| `+0x30` | `0x14`, `0x13` | `0xffffffff` on both |
| `+0x08` | a valid pointer | `NULL` on both |

`+0x28` is `9` in both, the node count is 2 in both, and in the healthy
case each node's backing object holds a pointer to ITSELF at `+8`, which
is the identity token the search compares. `0xffffffff` is
`NVRM_GPU_INVALID_ID` (`nvrm_wire.h`). So the object is not corrupted
after the fact: it is BUILT for an id that is already the invalid one, and
gets no backing because there is nothing to back it with. Whatever hands
libGLX_nvidia that `-1` is the defect.

The guest's library is the host's, now measured rather than argued:
`/opt/nvrm-gl/lib/libGLX_nvidia.so.610.57.04` in the guest hashes
`7ef1112f99de62db27670075e2dd1318235bbb3fe769850bf714b0c7e657784c`, the
same sha256 as the host's copy.

**Three of our own diagnostics were lying, and all three are fixed** --
which is the fix this round earned, because it is the part that was
measured:

1. The `CROSS-SESSION` reader in
   [`nvrm.rs`](../crates/vhost-user-nvrm/src/nvrm.rs) asked whether the
   CALLER's mirror holds the token, which stopped being the right question
   at protocol v6: the translation resolves through `fd_field_proc`. It
   reported 20 misses in a session that refused nothing, reading exactly
   like the bug it was added to find. It now fires only on the real
   refusal condition -- field translated at all, device cannot resolve it,
   caller's own mirror cannot either. A fresh traced session reports zero.
2. A hard ioctl failure (`ret != 0`) logged no FD, and for a one-shot
   escape the FD is the whole question. It now prints the host FD and
   token beside the failure.
3. The fd census printed `unaccounted` as `ctl - named - window.len()`,
   subtracting three different units: nvidiactl-only FDs, session-held FDs
   of every node type, and guest memory MAPPINGS. It read negative
   always -- -21 on a bare boot, -203 on a desktop -- for a figure its own
   doc calls "held outside every session". It is now `total - named` and
   named `outside every session`.

None of the three is the crash. They are the reason the crash was hunted
in the wrong place for a session, which is worth the diff on its own.

**And the second of those three immediately found a real defect: see 45,
resolved the same day.** A signal arriving while the guest module waited
for a reply it had ALREADY submitted made the kernel restart the whole
ioctl, so a one-shot escape was issued twice and the second one refused.
Xwayland was told an attach had failed that had succeeded. That is the
right shape for what poisons a GL stack -- a device object built for an id
its owner believes invalid, and `0xffffffff` is the value both the hollow
libGLX nodes and eglcore's live `rax` carry at the fault. It is *not*
proof: the crash of 23/33/44 has never been reproduced on a fresh guest,
so nothing yet shows this fixes it. What it does remove is a real
confound, and it makes the next attempt at reproducing 44 one variable
simpler.

---

**CLOSED 2026-08-21, IN THE CONFIGURATION THIS ENTRY DESCRIBES.** Everything
this entry asked for was finally present at once, which had never happened
before -- each earlier attempt was missing at least one leg:

| leg | earlier attempts | this run |
|---|---|---|
| GNOME Wayland session | yes | yes |
| Sunshine capture | `portal` (refused Moonlight, 503) | **`kms`, "Screencasting with KMS"** |
| Moonlight connected | never | **connected, decoding HEVC** |
| the game | yes, once | yes |
| Steam overlay on the GPU | yes | yes (`steamwebhelper` on `/dev/nvidia0`) |

The reason the streaming leg had never worked is its own small defect, fixed
the same day: the GNOME branch started Sunshine with `env DISPLAY=$D
XAUTHORITY=$XA`, both read from gnome-shell's environ -- and under Wayland
that environ carries NEITHER, so the `env` reduced to a bare `sunshine` with
no session, and it cannot enumerate outputs even for the KMS path.

**The result.** `ShadowOfTheTombRaider`, 49 threads, ran **19 minutes 25
seconds** against the **15-20 seconds** recorded above, with `steamwebhelper`
beside it. Sixty samples at 20-second intervals:

    segfault at 8                     0
    Failed to acquire the EGL Image   0     (this entry records 744)
    GL_OUT_OF_MEMORY                  0
    Xwayland restarts                 0

**Why number 45 is the answer this points at**, and it is the hypothesis this
entry already named as "the right shape": a signal arriving while the guest
module waited for a reply it had already submitted made the kernel restart the
whole ioctl, so `NV_ESC_ATTACH_GPUS_TO_FD` was issued twice and the second was
refused. Xwayland was told an attach had failed that had succeeded -- which is
exactly how a device object comes to be built for an id its owner believes
invalid, and `0xffffffff` is the value both the hollow libGLX nodes and
eglcore's live `rax` carry at the fault. That fix landed AFTER both sessions
that crashed and had never been tested against them until now.

**Two things in this entry that stay true and are worth keeping.** The
`+0x30` caution -- healthy values there are `0x14` and `0x13`, small integers
where a gpu id is `0x2d00`/`0x6`, so reading `0xffffffff` as
`NVRM_GPU_INVALID_ID` may be a coincidence of value rather than an
identification. And the three lying diagnostics this entry fixed on the way
(`CROSS-SESSION`, the FD-less failure log, the negative `unaccounted`) were
never the crash and are still the reason it was hunted in the wrong place for
a session.

**TWO HONEST LIMITS OF THAT RUN, recorded so the closure is not read as more
than it is.**

*The game exited on its own after about twenty minutes, and it exited
CLEANLY.* No `SIGSEGV`, and `coredumpctl` lists four cores for the whole day,
all of them the 32-bit `steam` client faulting in `libX11` at `0x4d0` during
startup, none of them `ShadowOfTheTombRaider`. The crashing session of number
44 left **six** cores including a 1.7 GB one for `WinMain`. So the exit is not
the defect wearing a different hat.

*No mapped window was ever confirmed.* `wmctrl` listed none, and `fbprobe`
read the scanout as **STATIC** -- content present (peak 1014/1024) but
unchanged across 9 polls. The game was demonstrably doing GPU work (2.2 GB of
device memory, 49 threads, 16-17 % utilisation) but nothing here proves it
reached the screen, and by number 30's rule that distinction has to be made
rather than assumed.

That matters less than it might, because number 44's crash arrives **15-20
seconds after launch** -- during startup, at or before window mapping -- and
this run passed that point by roughly sixty times without a fault. But a run
that had reached a drawing game would be stronger evidence than one that may
have sat in a loading screen, and the next attempt should confirm presentation
with a reader that looks at pixels or with a human, exactly as `llm.md` says.

### 45. `NV_ESC_ATTACH_GPUS_TO_FD` answers `-1` to Xwayland
**Resolved 2026-08-20, and the cause was ours rather than the ioctl's.** A
signal interrupted the guest module's wait for the host's reply AFTER the
request had been put on the virtqueue and kicked. The module abandoned the
buffer and answered `-ERESTARTSYS`; the kernel restarted the whole ioctl
under `SA_RESTART`; and the restart issued a SECOND attach on the same fd,
which the host refuses with `EINVAL` once that fd carries GPUs
([`nv.c`](../vendor/open-gpu-kernel-modules/kernel-open/nvidia/nv.c),
`nvlfp->num_attached_gpus != 0`). The caller was told its attach had failed
when it had in fact succeeded.

Caught with `strace -f` on the compositor's Xwayland under 30 concurrent GL
clients -- the same fd, twice, the second one the kernel's own restart:

    ioctl(142, ...0x46, 0xd4...) = ? ERESTARTSYS
    ioctl(142, ...0x46, 0xd4...) = -1 EINVAL

The backend agreed from the other side, once its bookkeeping was keyed
properly: **118 of 118** failures were one token, inside one generation of
one session, attached twice. None was a fresh token, so `nvidia_dev_get()`
never refused an id and BDF mediation was never in it. (Keyed globally
instead of per session the same data looks identical but proves nothing --
`Mirror::insert` numbers tokens per session, so a value legitimately
recurs in another one. The first pass made that mistake.)

Fixed by waiting KILLABLE instead of interruptible once the request is on
the queue: an ordinary signal no longer restarts a call the host has
already carried out, while a fatal one still returns -- and there is no
restart to fear when the task is dying. The queue-full wait above it stays
interruptible on purpose, because nothing has been submitted at that
point.

Measured after, same guest, same load: **2166 attaches over five rounds, 0
failures**, and the compositor's own `strace` shows 90 attaches with 0
`EINVAL` and 0 `ERESTARTSYS`. Before the fix the first round alone gave 30.

Why it hid for so long: it needs signal pressure. Serial probing never
produced one, and "2 in 1_392_006 calls" was a lightly loaded desktop.
Thirty concurrent clients make it fire about twenty times a minute.

*Unverified,* and it is the reason 44 stays open: that this is also what
poisoned the compositor. The shape fits -- Xwayland was told an attach
failed that had succeeded, which is exactly how a GL stack ends up holding
a device object built for an id it believes invalid, and `0xffffffff` is
what both the hollow libGLX nodes and eglcore's live `rax` carry. But the
client crash of 23/33/44 was never reproduced on a fresh guest, so nothing
here has been shown to fix it.

### 47. Two counting rules in our own instruments are wrong
**Resolved 2026-08-21.** Both were measured on 2026-08-20 and found by
building a second consumer of the same traces; both are fixed in both
consumers now. The original reasoning is kept below. Neither is a driver question; both make our
own numbers say something they do not mean.

**A mapping handle is counted as a signature.** The `sig()` key everything
here uses is `(dev, nr, sub)`, and `sub` is documented as the second
*dispatch* level. For `NV_ESC_RM_MAP_MEMORY` (0x4e) it is not: `log.rs`
puts **hMemory** there, deliberately, so that mappings can be matched to
their allocations by handle. A handle is an instance, not a call. In
`lvl3.tsv`, `lvl4blocking.tsv` and `torch5conv.tsv` alike, **29 of the 135
"signatures" are hMemory values** — 21 % — and they are stable only because
RM hands out handles deterministically for a fixed workload. Under an
enumerating Vulkan client the same escape produced **409** of them. The
saturation curve in `trace.sh analyse` therefore starts about 29 rows too
high and, on a workload that maps a lot, would never look saturated at all.
`scripts/ioctl-matrix.sh` collapses 0x4e to one row and says so; `trace.sh`
still counts the old way.

**`grep '_IOC'` counts DRM calls as NVIDIA ones.** The delta column of
`trace.sh` counts strace lines carrying the substring `_IOC`, and
`DRM_IOCTL_VERSION` contains it. For `nvprobe` and `torch` that never
mattered, because a CUDA probe touches no DRM node. For a GL, EGL or Vulkan
client it does: one `eglinfo`-shaped run showed **441 phantom calls** the
tracer had supposedly missed, every one of them a DRM ioctl strace had
named. The token that means "strace has no name for this request" is
`_IOC(` with the parenthesis, and the count has to be restricted to the fds
that are NVIDIA nodes — which needs `strace -y`. Both are the same mistake:
counting what the substring matches instead of what the rule means, and it
is the third time this project has been bitten by exactly that (`grep
'^nvos64'` also matching `nvos64in` is in `probe/README.md`).

**Both were fixed in `scripts/ioctl-matrix.sh` and neither in
`probe/run/trace.sh`, and that is what this entry was still recording.** The
paragraphs above say so in as many words. Re-measured on 2026-08-21 rather
than repeated, because a claim that quotes itself is not evidence:

*The mapping handle.* The raw key against the collapsed one, on the three
traces named above:

| trace | raw key | collapsed |
|---|---|---|
| `lvl3.tsv` | 135 | **107** |
| `lvl4blocking.tsv` | 135 | **107** |
| `torch5conv.tsv` | 134 | **106** |

28 of the 135 were one escape wearing 29 handles, exactly as written. It is
worse where more is mapped: `nvenc` 250 → 135, `vk-enum` 161 → 123.

*The substring.* Over the committed matrix straces, `grep -c '_IOC'` against
the rule:

| probe | `grep _IOC` | correct | phantom |
|---|---|---|---|
| `vk-enum` | 1384 | 961 | 423 |
| `gl-enum` | 593 | 531 | 62 |
| `gles` | 624 | 568 | 56 |
| `egl-xlib` | 604 | 566 | 38 |
| `cuda-core` | 433 | 433 | **0** |
| `nvml` | 179 | 179 | **0** |

The last two rows are why `trace.sh` never saw it: `nvprobe`, `torch` and
`smi` touch no DRM and no NVKMS node, so the wrong rule and the right one
agree on every workload that file traces. Decomposed, `vk-enum`'s 1372
`_IOC(` lines are 961 RM + 405 NVKMS + 6 DRM, and the twelve lines carrying
`_IOC` *without* the parenthesis are `DRM_IOCTL_VERSION`,
`DRM_IOCTL_GEM_CLOSE` and `DRM_IOCTL_AMDGPU_FENCE_TO_HANDLE` — named
requests, which is the mechanism this entry named.

**What closed it.** `trace.sh` asks the one counting rule instead of writing
its own — `lea_matrix_n_tracer`/`_kms`/`_drm` and `lea_matrix_n_strace` — and
its strace invocation gained `-y`, without which the node filter has no fd
paths to filter on. NVKMS and DRM are their own columns. The signature key
is `lea_trace_sig` in `scripts/lib/matrix.sh`, beside the counters, and both
of `trace.sh`'s signature sites use it; the per-device table had the defect
too and was not in this entry.

Measured after, on a fresh `nvprobe` sweep: delta **0** on all six stages,
NVKMS 0/0, NF 9, and the saturation curve flattens where it should — `lvl2`,
`lvl4auto`, `lvl4blocking`, `torch4`, `torch5` and `torch5conv` each add 0
new signatures, where before every extra mapping looked like new surface.
The per-device column sums to 88+5+13 = 106 against a curve of 106.

**The lesson, and it is the one worth keeping:** before the fix both of
`trace.sh`'s numbers were internally consistent at 134 and both were 28 too
high. A wrong rule applied everywhere agrees with itself, so
self-consistency is not a check — which is the argument for the rule living
in one place and every consumer asking it.
### 48. Userspace talks to `/dev/nvidia-modeset`, and the tracer cannot see it
**Resolved 2026-08-21**, for the half this entry is about: the node is
traced. Naming the commands was always the *other* half and is now its own
entry, number 64, so that nothing sits here half answered. The original
reasoning is kept below. The tracer classified `ctl`, `gpu`, `uvm`,
`uvm-tools`, `event`, `drm` and `render`. There was no tag for
`/dev/nvidia-modeset`, so an ioctl on that node was not recorded, not
counted, and not visible in any trace this project had taken.

They exist, and there are more of them than expected. Counted across the
matrix probes: **451 calls**, of which **405 come from `vulkaninfo
--summary` alone** — an enumerating Vulkan client makes more calls to NVKMS
from userspace than the whole NVML path makes to RM. Every GL and EGL probe
makes six. They were found only by counting the tracer against `strace -y`
per node and asking what the remainder was made of.

Two things follow. The cheap one is a device tag in `crates/nvrm-trace`, so
the calls are recorded at all. The expensive one is that **NVKMS command
numbers are their own namespace** — not RM_CONTROL commands, resolving
against no `ctrl*.h` — so naming them needs a reader that does not exist
here. `matrix/catalog-<driver>.md` carries the count and the node and
invents nothing.

This also sharpens what "NVKMS is in-kernel" meant. Its RM traffic is, and
that half is still unobservable from userspace. Its *own* ioctl surface is
not: userspace calls it directly, and that half we could measure today.

**The cheap half is done (2026-08-20).** `NvDev::Modeset` exists, the node
is traced, and it passes the same counter-check against `strace -y` that
`/dev/nvidiactl` and `/dev/nvidiaN` do — 405 against 405 on `vk-enum`, 6
against 6 on every GL and EGL probe, in its own pair of columns so the two
namespaces never share a total. The 451 calls are **14 commands**, and
`nr` is 0 in all of them: NVKMS carries its whole interface under
`_IOWR('m', 0, struct NvKmsIoctlParams)` and puts the real command in a
field of that 16-byte struct. The tracer reads it there, so the catalogue's
`sub` column is the command and its `psize` column is the size of the block
the command points at — the one number a future decoder can be checked
against before it is trusted. Offsets are not written into the reader: the
struct comes through bindgen with its layout tests, like every other.

What is left is the expensive half, unchanged: **naming** them. That is
**number 64** now, with only that in it. Every reference trace taken before
2026-08-20 is incomplete on this node and was re-cut; the older ones stay as
history.

**Confirmed 2026-08-21 against the code and the run**, not against the
status line above. `NvDev::Modeset` is a device tag in `crates/nvrm-trace`,
and the node is counted by `lea_matrix_n_tracer_kms` /
`lea_matrix_n_strace_kms` in `scripts/lib/matrix.sh` — its own pair of
columns, never added into the RM total. Re-measured on the committed
straces: `vk-enum` carries **405** NVKMS `_IOC(` lines against the tracer's
405, and every GL and EGL probe **6** against 6.

That separation is not cosmetic, and this run produced a second
demonstration of it. `probe/run/trace.sh` was folding all nodes into one
total and comparing it against a strace count taken over every fd; on the
workloads that file traces the two errors cancel exactly, and on `vk-enum`
they would not have (number 47). Two namespaces in one total hide each
other in both directions.

The claim this entry existed to make is therefore closed: 451 ioctls that
appeared in no trace are recorded, counted and gated. What they MEAN is a
decoder, and that is 64.
### 49. A deprecated control is forwarded verbatim with a pointer inside it
**Resolved 2026-08-21: RM never reads the field, so nothing happens.** The
entry says the answer "depends on something not measured yet: if RM never
reads the field, nothing happens" -- and that is now read out of the driver.
The original reasoning is kept below. The catalogue flags every
signature that is forwarded without interpretation *and* whose parameter
struct holds an `NvP64` or a file descriptor. Exactly one comes back:
`NV0000_CTRL_CMD_GPU_GET_ID_INFO` (0x202, 40 bytes,
`NV0000_CTRL_GPU_GET_ID_INFO_PARAMS`), seen in every CUDA, GL and Vulkan
probe. Its header says "Deprecated. Please use
`NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2` instead", and V2 (0x205) — which the
same clients also call — has no pointer in it.

It is on no mediation list, so the `szName` pointer travels as a **guest
address handed to the host driver**. Whether that is a defect depends on
something not measured yet: if RM never reads the field, nothing happens.
Unverified either way, which is why it is a question and not a bug report.
Worth settling because it is the exact shape of the failure class number 32
named — a call that succeeds and answers plausibly.

---

**ANSWERED 2026-08-21 by reading open-gpu-kernel-modules at
`DRIVER_VERSION`**, the same way number 42 was settled -- and this one comes
out the other way.

*`szName` is referenced nowhere in the driver's source.* A grep for it across
`src/` returns exactly two hits, and neither is code:

    src/common/sdk/nvidia/inc/ctrl/ctrl0000/ctrl0000gpu.h:88   the declaration
    src/nvidia/generated/g_sdk-structures.h:249                its generated mirror

*And the handler enumerates what it touches.* `0x202` dispatches through
`cliresCtrlCmdGpuGetIdInfo_IMPL` (`rmapi/client_resource.c:1473`) to
`gpumgrGetGpuIdInfo` (`gpu_mgr/gpu_mgr.c`), which reads `gpuId` as input and
writes back exactly seven fields:

    gpuFlags  deviceInstance  subDeviceInstance  sliStatus
    boardId   gpuInstance     numaId

`szName` is not among them. The pointer is neither dereferenced nor
overwritten.

**So the guest address travels to the host driver and is ignored**, which is
the harmless one of the two possibilities this entry set out. The catalogue's
flag is still correct and still worth having -- it found the one forwarded
signature with an `NvP64` in it out of 276, which is exactly its job; what the
flag cannot know is whether the far end reads the field, and only the source
can say.

**It stays worth knowing rather than being deleted**, for the reason the entry
gives: this is the exact shape of number 32's failure class, a call that
succeeds and answers plausibly. The difference is that here the field is
provably dead, so a wrong value in it cannot become a wrong answer three steps
later.

**Consistent with the byte evidence**, which had already hinted at it from the
other side: `verify` reports `GPU_GET_ID_INFO` answering byte-identically in a
guest with the gpuId translated and nothing else moved. A field RM wrote
through would not behave that way.
### 50. Nothing compares the answer bytes, so nothing is verified
**Resolved 2026-08-21.** The harness exists, the mask problem this entry
calls "the hard part" is solved by derivation, and the class it was written
about is no longer empty. The original reasoning is kept below. The gpu and display gates compare status codes,
workload results and a whole PyTorch run bit for bit. Nothing anywhere
compares the **response bytes of a forwarded RM control** against the bytes
the same call returns natively.

The consequence is now a number rather than a worry. Of 290 catalogued
signatures, **75 are governed by the descriptor tables or answered by the
backend, and all 75 are `implemented-unverified`** — the class
`implemented-verified` is empty and stays empty until a differential
harness exists. `matrix/TASKS-<driver>.md` carries the standing task, and
`scripts/ioctl-matrix.sh` reads `matrix/verified-<driver>.json` the moment
something writes it. That file is never written by hand: a hand-written
verification record is not evidence.

Why it matters is already on record twice. Number 32 named the failure
class — an answer that looks valid and is wrong — and number 44 turned out
to be exactly it: an object a NULL check waved through whose leading fields
were never filled. A status comparison cannot see either one.

The hard part is not the comparison, it is the mask: handles, gpuIds and
addresses are translated on purpose, so a harness that flagged them would
cry wolf on every call.

Since 2026-08-20 there is a step between prediction and that harness:
`scripts/ioctl-matrix.sh guest` runs the same probes inside a VM and
compares the signature set and the rm_status fingerprint. A probe that
survives it is `guest-validated`, which is more than `predicted-green` and
strictly less than `implemented-verified` — it says the guest asked the
same questions and got the same KIND of answers, not that the answers
carried the same bytes. Number 51 is what it found on its first run.

And since the same day there is a first slice of the byte comparison
itself, `scripts/ioctl-matrix.sh verify` — see number 55, which is mostly
about what it CANNOT reach.

**What this entry asked for exists and is measured.** It was written when
`implemented-verified` was empty and said it "stays empty until a
differential harness exists". Read off `matrix/verified-610.57.04.json` and
`matrix/catalog-610.57.04.json` as they stand:

| | when this was written | now |
|---|---|---|
| `implemented-verified` | 0 of 75 | **58** |
| `implemented-unverified` | 75 | **17** |
| verified signatures | — | **212**, plus 4 `verified-mediated` |
| answer bytes compared | — | **99.5%** of 249 of 258 comparable signatures |

**And the hard part was the mask, exactly as this entry predicted.** It is
solved the way the entry implies it must be — without a maintained list.
Five masks, every one DERIVED from something the run itself produced:
`gpuId` from the trace's own `cardinfo` line, handles from its own allocation
lines, declared pointers from `tables.txt`, the mediation manifest from
`mediate.rs`, and written-ness from the before-call sample. None of them is
a declaration and each made the comparison sharper rather than looser.

The reach and the criterion are number 55, which is resolved with it. What
the method still cannot judge is stated there and in number 63, and is a
property of byte equality rather than a gap in this harness: 24 signatures
whose answers move between two native runs of the same binary.

The two failure classes this entry cites as the reason it matters are
unchanged and still the point: number 32's *an answer that looks valid and
is wrong*, and number 44, which turned out to be exactly it. Neither is
visible to a status comparison, and 44 is still open.
### 51. A guest answers two nvml controls differently, and both shapes were predicted
**Resolved 2026-08-21.** Both controls are fixed and re-measured, and the
flag-scan undercount this entry also carried is closed. The one thing left
over was never about these two controls and is now number 65. The original
reasoning is kept below. First run of the guest sweep, twenty probes,
seven of them identical to the native trace signature for signature. `nvml`
was not: two controls answer in the guest what they never answer natively,
and both are the failure shape this catalogue already predicted in the
abstract.

| control | native | guest | what its params carry |
|---|---|---|---|
| `NV2080_CTRL_CMD_BIOS_GET_INFO` (0x20800802) | `NV_OK` | `0x1e NV_ERR_INVALID_ADDRESS` | `NvP64 biosInfoList` — a pointer into the caller's address space |
| `NV0000_CTRL_CMD_GPUACCT_GET_ACCOUNTING_STATE` (0xb02) | `NV_OK` | `0x1f NV_ERR_INVALID_ARGUMENT` | `NvU32 gpuId` as its first field |

Both are `passthrough` in the catalogue, i.e. forwarded verbatim. An
INVALID_ADDRESS for a struct whose only interesting field is a guest
pointer, and an INVALID_ARGUMENT for a struct whose first field is a gpuId,
are not mysteries: they are the two mediation classes this project already
has names for. The gpuId one is the same failure as
`GET_P2P_CAPS_MATRIX` in the raytracing work — the host answered
INVALID_ARGUMENT for the guest's id — and the pointer one is task 1 of
`matrix/TASKS-<driver>.md` caught in the act rather than reasoned about.

`nvidia-smi -q` itself succeeded in the guest and printed a plausible
report. That is the point of comparing fingerprints rather than exit codes.

**The flag-scan undercount noted with this entry is closed (2026-08-20).**
`BIOS_GET_INFO` carries a `finn:` comment that evaluates a bare number
instead of naming its params struct, so the catalogue had no struct for the
very control this entry had just fixed. The name is not invented: it is
PROPOSED by the naming convention and accepted only when the typedef is in
the command's own header AND `sizeof` compiles for it. Two conventions turned
out to be real (`_CMD` dropped, and `_CMD` kept), and a command whose header
holds both is left alone. 35 params structs recovered header-wide, and of the
135 named control rows the six without a struct became **four** — all four of
which take no arguments at all: no typedef in the header, and `paramsSize 0`
in every one of the 27 observed calls. That is a closed answer, not a gap.

**The identity mediation is tested for the first time.** The remaining
hypothesis for the `AMPERE_SMC_MONITOR_SESSION` leftover below was the
mediated identity itself. `NV2080_CTRL_CMD_GPU_GET_NAME_STRING` is now
`verified-mediated` over 32 of its 68 bytes: it differs from the native
answer in `gpuNameString` and in NO OTHER BYTE. That does not explain the
missing allocation, but it does remove "the mediation writes somewhere it
should not" from the list of candidates for it.

**Both are fixed and the fix is measured (2026-08-20).** Each was one table
row, because the mechanism for each already existed and only this command
was missing from it:

  - the gpuId joins `NVRM_BDF_SCALARS` (nvrm-genhdr), the same list
    `GET_ID_INFO` and `ASYNC_ATTACH_ID` are in;
  - the pointer joins `nested_ptrs` (xlate.rs) as
    `{ ptr_off: 8, elem: 8 }`, the same shape as `BUS_GET_INFO` and read
    out of its own header — `NV2080_CTRL_BIOS_INFO` is the
    `NVXXXX_CTRL_XXX_INFO { index; data }` pair again.

Re-run in a guest afterwards: both answer `NV_OK`, the status fingerprint
of the whole probe matches the native one, and `verify` puts
`BIOS_GET_INFO`'s answer bytes in the verified class with its pointer field
masked as a declared pointer. 61 of nvml's 72 answers now match; the 11
that do not are the mediated identity (name, PIDs, PCI info) and values
that are not stable between two runs of the same binary — a timer, PEX
counters, the current P-state.

Two things were left over, and both are settled.

**The mediation-flag scan undercount is closed**, and the paragraph above
records how: the naming convention proposes the struct and it is accepted
only when the typedef is in the command's own header AND `sizeof` compiles
for it. 35 params structs recovered header-wide, and the six named control
rows without a struct became **four** — all four of which take no arguments
at all, `paramsSize 0` in every one of the 27 observed calls. That is a
closed answer and not a bounded gap.

**`AMPERE_SMC_MONITOR_SESSION` (class 0xc640) is number 65 now**, with only
that in it. In short, and the detail is there: It is not
downstream of the two controls above: both answer identically since the
fix, and the allocation is still absent. It happens early — right after the
GPU node is opened and `GET_PROBED_IDS` answers, before UVM — so it is part
of `nvidia-smi`'s init and not of its process accounting. Nothing in the
answer comparison explains it, which leaves the mediated identity itself as
the candidate: the guest is told it is a "Leandro RTX 2070", and whether
NVML opens an SMC monitor session is a decision it makes from what the card
says it is.

**Confirmed 2026-08-21 in the code and in the run**, not from the status
line above.

*The two table rows exist.* The gpuId is
[`crates/nvrm-abi/src/mediate.rs:193`](../crates/nvrm-abi/src/mediate.rs),
in `bdf_scalars()` — which is also where `nvrm-genhdr` writes
`NVRM_BDF_SCALARS` from, so the C header the guest module is built from and
the mask `verify` uses come from one table. The pointer is
[`crates/nvrm-abi/src/xlate.rs:1277`](../crates/nvrm-abi/src/xlate.rs),
`{ ptr_off: 8, elem: 8 }`, and `0x20800802` is in `nested_cmds()`.

*And the guest run says they work.* In `matrix/guest-610.57.04.json` the
`nvml` probe records **179 ioctls natively and 179 in the guest**, and
exactly **one** finding — the missing `AMPERE_SMC_MONITOR_SESSION`
allocation, which is number 65. Neither control appears. The status
fingerprint that had two failures has none.

**The lesson this entry was written for, restated because it held.** Each
fix was ONE TABLE ROW. The mechanism for each already existed and only that
command was missing from it. Finding them took a night of building
instruments; deciding them took minutes. Both controls were `passthrough` —
the class the catalogue declares unproblematic — and `nvidia-smi -q` PASSED
in the guest and printed a plausible report while both were failing. Only
the fingerprint diff saw them, which is the argument for the instrument.
### 53. Two libraries are staged into the guest and registered with nobody
**Resolved 2026-08-21.** One half was fixed and measured, the other was
falsified and rehomed, and what is left of the rule belongs to two other
entries. The original reasoning is kept below. `oclprobe` in the guest: *"no OpenCL platform
(loader found no vendor library)"*. `libnvidia-opencl` IS staged — it is in
the `optional` array — but nothing writes `/etc/OpenCL/vendors/nvidia.icd`,
and an ICD loader with no vendor file finds no platform. `eglplat xcb` in
the guest resolves to vendor `Mesa Project` rather than NVIDIA, which is
the same shape one platform further on.

This is a class this project already wrote down for EGL — *without
`10_nvidia.json`, libEGL picks Mesa, silently* — and `lea_gl_stage`
rewrites those manifests for exactly that reason. The rule generalises and
was not generalised: **a staged library that no manifest names is an absent
library that costs disk.** The staging inventory in `DISCOVERY.md` cannot
see it, because it compares file sets and this is a registration.

**The OpenCL half is fixed, and the rule was generalised rather than the
case patched.** The set of manifests is not a list somebody maintains: it is
what a host with the driver installed HAS, derived on 2026-08-20 by

    grep -rl libnvidia /etc/OpenCL /usr/share/glvnd /usr/share/egl \
                       /usr/share/vulkan /usr/share/vulkansc

— **ten files**, of which `lea_gl_stage` already wrote six (the EGL vendor
JSON, four external-platform JSONs, the Vulkan ICD). `lea_guest_setup` now
writes two more, each only when the library it names actually resolves under
`/opt/nvrm/lib` and removed again when it does not: `nvidia.icd` for
`libnvidia-opencl` and `nvidia_icd_vksc.json` for `libnvidia-vksc-core`.
The remaining two are the halves of `nvidia_layers.json` — number 57.

Measured the same day, and it is the whole of the fix: the `opencl` probe
went from FAIL to **guest-validated**, 107 signatures against 107, criterion
*"OpenCL vector add on NVIDIA's platform: 4096/4096 elements verified"*.
Nothing else changed. Note what that says about the class: `libnvidia-opencl`
is a separate userspace on the same driver, and its whole escape surface was
untested for want of a one-line text file.

**The `eglplat xcb` half is FALSIFIED, and it was not the same shape at
all.** Measured 2026-08-20 out of the guest run's own strace: the guest
DOES read `/usr/share/egl/egl_external_platform.d/20_nvidia_xcb.json`, it
DOES read `10_nvidia.json` beside Mesa's `50_mesa.json`, and it DOES
`dlopen` `libnvidia-egl-xcb.so.1` successfully — and `eglQueryString`
still answers `Mesa Project`. The registration chain is complete and the
divergence is one level below it, which is a different question and has its
own entry: number 58.

**Both halves are answered and neither is still this entry's.** Confirmed
2026-08-21 against `matrix/guest-610.57.04.json`:

- *The OpenCL half is fixed.* `opencl` is **guest-validated, 107 signatures
  against 107**, criterion *"OpenCL vector add on NVIDIA's platform:
  4096/4096 elements verified"*. It was FAIL for want of a one-line text
  file, and the rule was generalised rather than the case patched — the set
  of manifests is derived from what a host with the driver installed HAS,
  not from a list somebody keeps.
- *The `eglplat xcb` half is falsified* — the registration chain is
  complete and the vendor library declines anyway — and it has its own
  entry, number **58**, where it belongs.

**What remains of the generalised rule is number 57 and is scoped there.**
Of the ten host files that name an NVIDIA library, eight are written into
the guest. The ninth and tenth are the two halves of `nvidia_layers.json`,
and 57 states why neither can be written without a measurement first:
`VK_LAYER_NV_present` names a library that is not staged, so writing the
file whole would point the loader at something absent — this entry's own
failure mode, in the other direction.

So nothing is left here that is not owned by 57 or 58, which is the
condition for closing it rather than leaving it half open.
### 54. Two probe criteria cannot pass in a guest, because the guest renames the card
**Resolved 2026-08-21**, fixed 2026-08-20 and confirmed against the code
and the artefacts rather than against this entry's own status line. `gl-enum` and `gles` fail in the guest with
*"renderer is 'Leandro RTX 2070/PCIe/SSE2', not NVIDIA"* — and the same
run's version string reads `OpenGL ES 3.2 NVIDIA 610.57.04`. The renderer
IS NVIDIA's; the criterion greps for a product name that the identity
mediation deliberately rewrites. The probes were written against a host and
the criterion inherited that.

**Both criteria now gate on the VERSION string, and it is the sharper test
rather than the looser one.** The renderer string carries the card's product
name, which the mediation rewrites on purpose; the version string is
untouched by it and identical on both sides — measured 2026-08-20:
`4.6.0 NVIDIA 610.57.04` and `OpenGL ES 3.2 NVIDIA 610.57.04`, natively and
in the guest. And it is checked against `DRIVER_VERSION` rather than against
the word `NVIDIA`, so a guest answered by a userspace of the WRONG version
is now a failure where before it was a pass. The renderer string is still
reported beside the criterion: it is the mediated identity, which belongs in
the record and is not a pass criterion.

**The absence half was not where this entry said it was.** It is not a
difference between the two shell probes — those already agreed. All four
platforms in `probe/c/eglplat.c` returned **1** for a missing native
display, and the shell comments claimed 2, so the contract was written down
in one place and implemented in another. Absence is now measured on the
thing itself (`WAYLAND_DISPLAY` unset, `DISPLAY` unset, no
`/dev/dri/renderD128`) and answers **2**; a display that EXISTS and refuses
the connection stays **1**. The exit-code contract sits in the probe's own
header, which is the one place all four platforms read from.

Measured after: `gl-enum` and `gles` meet their criterion in the guest, and
`egl-wayland` reports *"declared-unsupported — a Wayland display
(WAYLAND_DISPLAY) is not available"* instead of failing. Both still appear
as FAIL in the guest column, for the signature-set divergence of number 52
and for nothing to do with this entry — which is exactly the separation the
two columns exist for.

**Confirmed 2026-08-21, in the code and in the run.** This entry said
"Fixed" and that is not evidence; both halves were checked against what is
in the tree.

*The criteria gate on the version string.* Both probes read the version
string, and both check it against `DRIVER_VERSION` through
`lea_want_driver` rather than against the word `NVIDIA` —
[`probe/matrix/gl-enum.sh:47-49`](../probe/matrix/gl-enum.sh) and
[`probe/matrix/gles.sh:51-53`](../probe/matrix/gles.sh). The renderer string
is read and reported beside the criterion and gates nothing, which is the
distinction this entry is about.

*The exit-code contract is on the thing itself.*
[`probe/c/eglplat.c`](../probe/c/eglplat.c) returns **2** for an absence it
measures — `WAYLAND_DISPLAY` unset, `DISPLAY` unset, no
`/dev/dri/renderD128` — and **1** for a display that exists and refuses the
connection, on all four platforms, with the contract in the probe's own
header.

*And the guest run agrees.* In `matrix/guest-610.57.04.json` both probes
carry their criterion as met in the guest: `gl-enum` *"glxinfo resolved to
'4.6.0 NVIDIA 610.57.04' with direct rendering"* and `gles` *"GLES2
enumeration resolved to 'OpenGL ES 3.2 NVIDIA 610.57.04'"*. Both are still
FAIL in the guest column, for number 52's signature-set divergence and for
nothing to do with this entry — which is the separation the two columns
exist for, and the reason a FAIL row had to be read rather than counted.
### 55. The answer bytes are compared now, and the class that can be promoted is not the class that can be reached
**Resolved 2026-08-21.** Both halves are answered and both were confirmed
against `matrix/verified-610.57.04.json` rather than against this entry.
One number below was stale and is corrected at the end. The original
reasoning is kept. What the reach
half asked for was answer evidence for the classes `ctrlout` could not see --
allocations, UVM, and the controls outside the two namespaces it happened to
cover. All of them have it now, and the numbers moved accordingly:
`implemented-verified` went from 5 signatures to 60, `implemented-unverified`
from 70 to 15.

Where the reach ends is measured rather than asserted. Of the 258 comparable
signatures **249 have answer evidence, at 99.5% of their answer bytes**. The
nine without are not gaps in the instrument: five controls whose `paramsSize`
is 0 on every observed call and which therefore carry no answer buffer at
all, two that only `cuda-torch` calls and cuda-torch is blocked in the guest
for want of PyTorch, and `UVM_DEINITIALIZE`, which takes no parameter struct.

Four things got it there, and each is worth naming because each was a
different kind of missing:

  * **the length, four times over.** A control's is `paramsSize`, an
    escape's is `_IOC_SIZE`, and both are the caller's own declared size --
    self-describing, safe at any width, needing no table. An allocation's and
    a UVM command's are not: they come from the CLASS and from the command,
    which is what `xlate::alloc_param_size` and `xlate::uvm_param_size` are
    for. Those are the tables under test, so the tracer uses `size_of` of the
    bindgen struct instead and a test requires the two to agree. Sixty-odd
    hand-computed sizes are checked by the compiler now and all of them were
    right -- which does not make them right by luck any more, it makes them
    checked;
  * **the cap.** 32 bytes, then 256, and 256 was 4.3% of the answer bytes in
    a sweep. 65536 is 99.5% and costs 110 MB of trace;
  * **the before-call sample**, which resolved number 60 and became the fifth
    mask;
  * **one record per allocation** even when the class passes no parameters,
    because then the escape's own struct is the whole answer.

The original entry follows.

**Criterion half answered 2026-08-20.** `scripts/ioctl-matrix.sh verify` compares the
ANSWER of a forwarded control, native run against guest run, call by call
and word by word. It logs nothing new: the tracer has dumped the first 32
bytes of the params buffer after the call since the enumeration work
(`ctrlout` in `log.rs`, written for exactly this diff), so both phases had
been recording the evidence all along.

The mask — the hard half of number 50 — is DERIVED rather than declared. A
differing word is allowed only if it is this side's own `gpu_id`, which
each trace states in its own `cardinfo` line, or a handle this side
allocated, which its own allocation lines name. Anything else that differs
is a mismatch, and one unexplained word anywhere disqualifies the
signature, including when it matched under a different probe.

First run: 67 signatures matched, over the twenty probes that produced
answers on both sides, several of them with `masked {gpuId: n}` — which is
not a difference tolerated but the translation observed working. **0 of them
were `implemented-verified`**, and that was the finding.

Then a third mask was added: a word may differ if it is part of a pointer
field the DESCRIPTOR TABLE declares for that command, read out of
`tables.txt` beside the traces. With it the numbers are **72 matched, 3 of
them `implemented-verified`** — `NV2080_CTRL_CMD_BUS_GET_INFO`,
`NV2080_CTRL_CMD_BIOS_GET_INFO` and `NV2080_CTRL_CMD_GPU_GET_ENGINES`, each
over the whole 16 bytes of its params with only the pointer masked. The
class is not empty any more, and its first entry is the control number 51
had just fixed.

**That it is 3 and not 73 is the finding, and it has two halves that are
worth keeping apart.**

*Reach.* `ctrlout` covers root-client (0x2xx) and subdevice (0x2080xxxx)
controls and nothing else. Allocations and UVM commands have no answer dump
at all — and those are where most of the governed class lives, because they
are the two places a size is not self-describing. This slice cannot see
them.

*Criterion.* Several of the governed controls it does reach are the seven
the backend answers itself, and their answers differ from the native ones
on purpose: that is what mediation is. `NV2080_CTRL_CMD_GPU_GET_NAME` differs at offset
4 by `NVID` against `Lean` — the mediated product name, working exactly as
designed and reported as a mismatch, because byte equality is the wrong
test for a mediated command. The right test is "differs in exactly the
fields the mediation rewrites, and nowhere else", and it needs the
mediation to name its own fields.

So the pipeline is real and its coverage is honest, and the two things it
would take to make `implemented-verified` more than a handful are now
specific rather than a standing wish: an answer dump for allocations and
UVM, and a per-command field mask that the mediation itself declares.

**The second of those exists now (2026-08-20).** The mediation names its own
fields: `crates/nvrm-abi/src/mediate.rs` carries one record per
`(command, field offset, field length, kind)` over six kinds, and
`nvrm-genhdr --mediation-dump` writes it beside `tables.txt`. It is derived
rather than written next to the code — the BDF tables and the PCI address
offsets moved out of the generator into it, so the C header the guest module
is built from and the mask `verify` uses come from ONE table. Proof the move
was safe: the regenerated `nvrm_wire.h` is byte for byte the committed one.

The fourth mask is **inverted** against the other three. They say "this word
may differ, here is the proof"; this one says "this command is mediated, so
it must differ inside these fields AND NOWHERE ELSE". It works at BYTE
granularity, because `GET_PCI_INFO` packs `bus` and `slot` into one word as
two `NvU16` and a mediated field must never shield the neighbour it shares a
word with.

**It found a missing record on its first run, before any deliberate test.**
`GET_PCI_INFO` differed at `bus` — `0x2d` natively, the guest's own `0x05` —
with *"this command IS mediated, and this byte is in none of the fields the
mediation declares"*. The manifest was incomplete and the code was right:
the guest module writes domain, bus and slot beside the id, because gpuId is
DERIVED from the address and the two must not contradict each other. Those
three are records now and `GET_PCI_INFO` is verified over its whole 12 bytes.

The negative test was run twice on a copy and discarded: moving `bus` to a
wrong offset makes the diff report the REAL field as a violation by name,
and removing the identity-string record makes `GPU_GET_NAME_STRING` a
mismatch again.

**`verified-mediated` is its own class and must never share a row with
`implemented-verified`.** Membership is decided on what MOVED in this run,
not on the manifest alone. And it does NOT prove that the mediation is what
moved those bytes — the manifest declares what MAY be rewritten, and a
declared field can also be a value that is simply not stable. That is what
the control test is for, and it took two commands straight back out again
(see below).

**The reach half is untouched and is now the whole of what is left.**
`ctrlout` still covers root-client and subdevice controls only. The two
semaphore-surface controls the backend answers (`0xda0003`, `0xda0005`) are
out of its reach entirely, so no amount of masking can judge them, and
allocations and UVM — where most of the governed class lives — still have no
answer dump at all. That is the tracer work, and it is the designated next
step.

**The control test exists, in its cheap variant.** For every command a native
trace called more than once, the answer words are compared across those
calls; a word that differs native-against-native is not stable and can be
evidence for nothing. `not_verified` splits into `mismatch` / `unstable` /
`stability-unknown`, and only `mismatch` is a potential defect: on this
baseline 11 rows became **1 mismatch, 10 unstable and 2 of unknown
stability**. Nothing lists the unstable rows — TIMER_GET_TIME, the timer
correlation, the PEX counters, BUS_GET_INFO_V2, GR_CTXSW_ZCULL_BIND and the
counter-shaped rows that resolve to no public header classify themselves out
of their own native traces.

It is used in ONE DIRECTION. It can move a difference out of `mismatch`; it
can never move one into a verified class. The first version left `unstable`
out of the disqualification test and the verified count rose from 100 to
102 — a stability classification that promotes has its logic inverted.

Two things it cannot do, both structural and both fixed by the same missing
instrument:

  * **Two calls of one command in one trace are not always the same
    QUESTION.** Index-list commands (`GPU_GET_INFO_V2`, `GR_GET_INFO`,
    `FB_GET_INFO`) ask for a different index each call, so their answers
    differ because the question differed. Of 19 verified signatures with a
    word flagged here, most are of that shape.
  * **A command called once per trace is invisible to it.**
    `PERF_GET_CURRENT_PSTATE` is one, and it is the row that entered
    `verified` on a single lucky call. It stays `verified` with
    `stability: unknown` recorded on the row rather than being silently
    treated as stable.

The variant without either weakness is **a second native trace of the same
probe**, where call i of one run is the same question as call i of the
other. It costs one run per probe. There is already accidental evidence for
how much it would buy: `0x2080a097` answered `0x00000007` in one guest sweep
and `0x00000023` in the next, from the same tree — a row the within-trace
variant calls `stability-unknown` and two runs would settle immediately.

The three masks are worth noting as a pattern, because all three are of one
kind: each is DERIVED from something the run already produced — the card's
own id out of `cardinfo`, the handles out of the allocation lines, the
pointer offsets out of the descriptor stream. None of them is a list
somebody maintains, and each one made the comparison sharper rather than
looser.

One thing the slice settled in passing: `NV0000_CTRL_CMD_GPU_GET_ID_INFO`
(number 49, the deprecated control with an `NvP64` in it) answers
byte-identically in a guest over the first 32 bytes of its 128, with the
gpuId translated and nothing else moved. That is evidence that its pointer
field is not read into, and not yet proof — 32 of 128 bytes is 32 bytes.

**Confirmed against the artefacts, and this entry's own headline was
stale.** The paragraph above reads *"`implemented-verified` went from 5
signatures to 60, `implemented-unverified` from 70 to 15"*. That was true
for about an hour. Thirteen signatures were then found to have been verified
on the QUESTION rather than the answer — `ctrlout` dumps a control's params
buffer, and for a list control that buffer is a count and an `NvP64` — so
they were reclassified out (60 → 47) and earned back by the tracer following
the pointer (→ 58). Read off `matrix/catalog-610.57.04.json` as it stands:

    290 signatures: 182 passthrough, 17 implemented-unverified,
                    58 implemented-verified, 1 implemented-verified-mediated,
                    32 not-governed

So the measured numbers are **58 and 17**, not 60 and 15, and the entry is
corrected rather than left to be quoted. `matrix/verified-610.57.04.json`
agrees: **212 verified + 4 verified-mediated**, 1 mismatch, 1
answer-not-written, 24 unstable, 0 stability-unknown.

**Both halves, checked:**

*Reach.* 249 of 258 comparable signatures carry answer evidence, at 99.5% of
their answer bytes. The nine without are enumerated in the entry and none is
a gap in the instrument. `ctrlout` covers every control, allocations, UVM
and every other escape, before and after each call, at 65536 bytes.

*Criterion.* The mediation names its own fields — 37 commands and 53 fields
in the manifest the evidence file cites, generated from `mediate.rs`, the
same table the guest module's BDF header comes from. `verified-mediated` is
its own class and does not share a row with `verified`.

**What the method still cannot judge is a property of byte equality, not a
gap here.** 24 signatures are `unstable`: their answers move between two
native runs of the same binary, so byte equality is the wrong test for them
and no mask can make it the right one. The control test that establishes
this has both variants now — within-trace and a second native run — unioned,
never substituted, because each finds volatility the other cannot.

The one thing this entry asked for that a later run had to correct is worth
keeping as a caution: *"verified, 16 of 16 bytes"* said the whole answer was
compared and meant the whole question was. Reach and criterion are separate
claims, and a percentage of bytes compared is only as good as the question
of which bytes count.
### 60. One answer survived every mask, and it was memory nobody wrote
**Resolved 2026-08-21, and it is not a defect.** The entry below predicted
its own test: *"if it is zero on entry on both sides and non-zero on return
natively, it is (a) and it is a hole."* The tracer samples the params buffer
BEFORE the call as well as after now, and the answer is neither (a) nor (b).

At offset 24 the eight bytes are **identical in the before and the after
sample, on BOTH sides**. Nothing wrote them. Natively they hold whatever the
caller's own stack held -- which happened to be a canonical user-space
address, because the caller is a program full of pointers -- and in the guest
they hold that caller's leftovers, `out","cm` in ASCII. The two words the
command does answer, at offsets 16 and 20, are `0x1` and `0x64` and they
**match exactly on both sides**.

So `ctrlout` was never dumping an answer there. It was dumping a buffer, and
comparing the part past the end of what the control fills compared two
programs' stack garbage. That generalises into the fifth derived mask -- a
word whose after sample equals its before sample was not written on that
call -- and with it the `mismatch` class is empty for the first time.

The neighbours named at the end of this entry went the same way.
`0x2080a097`, whose two-run difference was obtained by accident, is settled
on purpose now: the second-native-trace control test exists and it is
`unstable`.

What follows is the original entry, unchanged, because the reasoning that
framed the test is the reason the test was built.

**Measured 2026-08-20.** The only `mismatch` left in the byte
comparison after the fourth mask and the control test, and therefore the
only potential defect in `matrix/verified-<driver>.json`. Before this run it
was the twelfth line of a list of forty-five and nobody could have seen it.

`ctl nr=0x2a sub=0x20809064`, 13 calls across `nvml`, `nvdec`, `nvenc` and
`cuda-torch`, `paramsSize 0x208` (520 bytes), **`NV_OK` on both sides**. The
command resolves to no public header, so the catalogue row is
`unknown -- not in public headers` and the field map cannot name its members
— which is why the finding reads as an offset.

Bytes 24..31, little-endian, in the `nvenc` probe:

| | offset 24 |
|---|---|
| native | `30 14 03 80 84 7f 00 00` = **`0x00007f8480031430`** |
| guest | `00 00 00 00 00 00 00 00` = **0** |

In `nvml` and `nvdec` the same eight bytes are zero on BOTH sides, so the
difference appears only where the value is non-zero natively.

`0x00007f84_8003_1430` is a canonical x86-64 user-space address. An
eight-byte field holding one, in a control whose parameter block is 520
bytes, is the shape of an `NvP64` — and this command is in no header, so
`xlate::nested_ptrs` has no entry for it, the descriptor table declares
nothing, and the catalogue calls it `passthrough` because RM_CONTROL is
self-describing. That is precisely the class numbers 32 and 44 are about:
**a forwarded, wrongly-answered call fails quietly three steps later
somewhere else, while a non-carryable one fails loudly and immediately.**

**Not yet a defect, and here is the honest reason.** `ctrlout` dumps the
params buffer AFTER the call, so this cannot presently distinguish:

  a. an OUT pointer field the mediation does not know about, which the
     boundary therefore dropped — a real hole; from
  b. an IN pointer the CALLER supplied, where the guest's
     `libnvidia-encode` simply took a branch that passes NULL — a userspace
     difference and nobody's bug.

Both are testable and the test is cheap: dump the params buffer BEFORE the
call as well and compare. If the field is non-zero on entry natively and
zero on entry in the guest, it is (b); if it is zero on entry on both sides
and non-zero on return natively, it is (a) and it is a hole. That dump is
part of the tracer work the reach half of number 55 needs anyway, so this
entry is one more reason to do it and not a separate project.

Worth stating plainly: **`nvenc` is `guest-validated`** — it met its own
criterion in the guest, its signature set and status fingerprint matched
call for call, and it encoded video. Whatever this field is, it did not
break encoding. That is the whole argument for comparing answer bytes: a
gate that asks "did anything fail" cannot see this, and did not.

Two neighbours in the same class, both `stability-unknown` rather than
mismatches because no native trace called them twice:
`0x2080852f` (offset 0: `0xf7403501` natively, `0x00000001` in the guest)
and `0x2080a097` (offset 8: `0x00000014` natively, `0x00000007` in one guest
sweep and `0x00000023` in the next — which is two-run evidence that it is
volatile, obtained by accident, and exactly what the second-native-trace
variant would establish on purpose).
### 61. Two controls are answered on one side of the boundary and not the other
**Resolved 2026-08-21.** Narrower than it first read, then narrower again,
and then the instrument said the evidence was never sound. The original
reasoning is kept below because the corrections in it are the useful part. The whole of
the potential-defect class in `matrix/verified-<driver>.json` after the fifth
mask, and a class no gate before this one could see: **both sides return
`NV_OK`**, so the status fingerprint matches perfectly, the probe passes its
criterion, and the answer bytes differ anyway.

The instrument is the before-call sample. A word whose after sample equals
its before sample was not written on that call; where RM wrote one natively
and the guest did not, the two sides reached that call in different states.

**`NV0080_CTRL_CMD_GR_GET_CAPS_V2`** (`ctl nr=0x2a sub=0x801109`), in
`nvdec`, `paramsSize 48`, the same target object (`hObject 0x80000000`) and
the same hierarchy on both sides. Natively the buffer goes from the caller's
garbage to a capability table (`b0 62 00 00 ... 04 a0 0f`, `bCapsPopulated`
at offset 40) on **both** of the two calls. In the guest the **first** call
leaves all 48 bytes exactly as the caller had them and the **second** writes
the capability table **byte-identical to the native one**. Reproduced on
three consecutive guest runs.

**THE FIRST READING OF THIS WAS WRONG AND THE CORRECTION IS THE USEFUL
PART.** It read as "the guest was told the call succeeded and got no answer",
which is a boundary defect. `probe/c/rmdirect.c` calls the command twice from
a program with no driver userspace in it at all, with the buffer prefilled
with `0xa5` so the program can answer "did RM write" for itself:

| | call 0 | call 1 |
|---|---|---|
| native | did NOT write | did NOT write |
| guest | did NOT write | did NOT write |

**Identical.** RM itself returns `NV_OK` without populating `capsTbl`, and
whether it populates depends on the caller's state -- plausibly on a
graphics object having been allocated on the device, which `nvdec` does and
this probe does not. The boundary carries the command faithfully, including
its refusal to answer.

So what is open is not "the guest lost an answer". It is narrower, and two
further measurements narrow it again.

**The two calls come from two different CLIENTS.** With `hclient` and
`hobject` on the trace record:

| | client of call 0 | client of call 1 | hObject |
|---|---|---|---|
| native | `0xc1d5034e` — answered | `0xc1d5034f` — answered | `0x80000000` both |
| guest | `0xc1d504c9` — **not** answered | `0xc1d504cb` — answered | `0x80000000` both |

So it is per-client, not per-call-order in any deeper sense, and both sides
target the same device-instance handle.

**And the object state at each call is IDENTICAL on the two sides.** Walking
the traces in order, both sides have allocated exactly 120 objects of exactly
the same classes in the same order before their first `GR_GET_CAPS_V2`, and
exactly the same seven more (`0x41 0x80 0x2080 0x70 0xc361 0x3e 0x40` — a
second client and its device) before the second. Whatever makes RM answer,
the two sides had the same hierarchy in hand when they asked.

That leaves: **the guest's FIRST client does not get the caps answer where
the native first client does, with the same objects allocated and the same
target handle.** It is a per-client state difference that the object graph
does not capture. The next thing to look at is what else distinguishes a
client -- the guest allocates one extra client between the two (the handles
differ by 2 rather than by 1) -- and not another mask over bytes.

**`0x2080a079`** (`ctl nr=0x2a sub=0x2080a079`), in `nvml`, no public header,
one call, written natively and not in the guest at offset 8, `0x3` against
`0x0`. One call is thin evidence and it is stated as one call; the same
caveat applies to it as to the above, and more so, because nothing has
reproduced it from a minimal program.

WHAT THE CLASS MEANS NOW, in the evidence file's own words: a difference the
status fingerprint cannot see, and a statement that the two sides reached the
call in different states. Not by itself a defect. That is weaker than the
class first claimed and it is what the measurement supports.

**PULLED ON THE EXTRA CLIENT, 2026-08-21, AND IT IS NOT ONE.** This entry's
last line points at the handle gap — *"the guest allocates one extra client
between the two (the handles differ by 2 rather than by 1)"*. Read out of the
traces, `nvdec` allocates **four** `hClass=0x41` clients on each side:

| | clients allocated | the two that call `GR_GET_CAPS_V2` |
|---|---|---|
| native | `…d7 …d8 …dc …dd` | `…dc`, `…dd` (differ by 1) |
| guest | `…d8 …d9 …df …e1` | `…df`, `…e1` (differ by 2) |

**The same number of clients, four, on both sides.** The gap is not an extra
client of `nvdec`'s. And the handle the gap implies, `0xc1d53ae0`, **appears
nowhere in the guest trace at all** — not as an allocation, not as an
`hclient`, not as an `hObject`.

**Which means the handle gap is not evidence, and this entry should stop
treating it as a lead.** RM hands these out from one sequence shared by every
client on the machine, so a gap records what ELSE was allocating at that
moment, not what the traced process did. The native side has a gap too, and a
bigger one — `…d8` to `…dc` skips three — which nobody proposed as three extra
clients. On the guest side the extra consumer is most likely the guest module
itself, which allocates on its own behalf and is invisible to an LD_PRELOAD
tracer by construction.

**And the sharper question was asked and came back identical.** The object
graph was compared per CALLING CLIENT rather than globally — what each of the
four clients owned at the moment it issued the call:

    native  call 0  client …dc  owns 5: 0x80 0x2080 0x70 0xc361 0x3e
    native  call 1  client …dd  owns 5: 0x80 0x2080 0x70 0xc361 0x3e
    guest   call 0  client …df  owns 5: 0x80 0x2080 0x70 0xc361 0x3e
    guest   call 1  client …e1  owns 5: 0x80 0x2080 0x70 0xc361 0x3e

Same count, same classes, same order, for the client that is answered and the
client that is not. So the difference is not what the caller owns, and it is
not what the process has allocated globally (already measured: 120 objects,
identical). Two levels of the object graph have now been excluded.

**What is left to look at**, and it is deliberately not another mask: the
difference is per-client and is not in the object graph, so it is in something
the graph does not record — the ORDER in which the four clients were created
relative to each other and to their devices, the fd each client was opened on,
or a property RM keeps per client that no traced call reads back. The first two
are in the traces already. The third is not, and would need the kernel-side
trace point (number 59's third lever) to see at all.

---

**CLOSED 2026-08-21, ON TWO MEASUREMENTS THAT BETWEEN THEM LEAVE NOTHING.**

**1. The two leads this entry named as "already in the traces" are identical.**
Its last paragraph says what to look at next: *"the order the four clients
were created in relative to each other and their devices, and the fd each was
opened on"*. Both, from the `nvdec` traces:

| | native | guest |
|---|---|---|
| clients created | 4 | 4 |
| the two that call `GR_GET_CAPS_V2` | #3 and #4 | #3 and #4 |
| objects each owned at its call | 5: `0x80 0x2080 0x70 0xc361 0x3e` | 5: the same, same order |
| fd the CAPS ioctl was issued on | 42 | 42 |

So creation position, ownership and file descriptor all match. Together with
what was already measured -- 120 objects allocated identically before the
first call, the same `hObject 0x80000000`, the same 25 preceding controls --
**every property the userspace traces can express is the same on both sides.**

**2. And the symptom has been reclassified by the control test.** On the
sweep of 2026-08-21, `NV0080_CTRL_CMD_GR_GET_CAPS_V2` is no longer in
`answer_not_written`. It is in **`unstable`**, with twelve of its words
flagged:

    call 0, capsTbl (NvU8[NV0080_CTRL_GR_CAPS_TBL_SIZE]) at offset 0:
    0x000062b0 natively, 0x7120302e in the guest -- and this word is NOT
    STABLE between two native runs, or between two calls of one native run,
    so it is evidence for nothing

The second native run is what did it, which is the variant number 55 asked
for and number 66 now asks for on the guest side. A buffer whose contents
move native-against-native cannot carry a claim about the boundary, whichever
side wrote it.

**So this closes as a WITHDRAWN finding rather than an explained one**, and
that is the honest shape. The entry already corrected itself twice -- from
"the guest was told a call succeeded and got no answer" to "RM declines to
populate `capsTbl` and does so identically on both sides" (measured with
`rmdirect`, both sides refusing on both calls) to "a per-client state
difference the object graph does not capture". The third reading is now
withdrawn too: the object graph captures everything the traces can see, and
what remains was never stable enough to be a difference.

**What would revive it**, and it is number 59's third lever rather than
anything here: the kernel-side trace point. A property RM keeps per client
that no traced call reads back is invisible from userspace by construction,
and that is the only place left for one to hide. If it is ever built, this
number is worth re-asking -- it keeps its number, and the corrections above
are the map.
### 62. An escape the guest module rewrites is not in the descriptor table
**Resolved 2026-08-21: option 3, the classification follows from a table.**
The original reasoning is kept below. `NV_ESC_CARD_INFO` carries the BDF and the
gpuId in its own inline block, and the guest module rewrites both of them by
hand (`virtio_nvrm.c`, at offsets this tree generates). It has no descriptor-
table row, so `catalog-<driver>.json` calls it **`passthrough`** while
`verified-<driver>.json` calls it **mediated** -- "26 calls, differs only in
bdf-address". Both files are right as each defines its words, and a reader
who takes `passthrough` to mean "carried unchanged" is misled by an artefact
of which table was asked.

It surfaced the moment escape payloads were dumped at all: a `MISMATCH` of
`0x2d` natively against `0x05` in the guest, which is this rig's PCI bus
number against the guest's slot number, and the mediation working exactly as
designed while nothing declared it. The mediation manifest is keyed by the
catalogue's signature now and names the five fields, so the comparison is
correct; what is open is the CLASSIFICATION.

The question is whether an escape the module rewrites by hand should be
`implemented` rather than `passthrough`, and it is not cosmetic: the
catalogue's headline counts are what a reader takes away, and reporting a
mediated call as passthrough understates what has been built -- the same
direction of error that `manifest_answered` exists to prevent for controls.
Moving it changes what the catalogue counts, which is why it is a question
here and not a commit.

---

**DECISION MEMO, written 2026-08-21. Nothing below decides it: this changes
what the catalogue counts, which is a person's call.**

The exact state, read out of the two artefacts:

| | says |
|---|---|
| `catalog-610.57.04.json` | `ctl 0xc8 -`, kind `escape`, status **`passthrough`**, flags `none`, 44 calls, seen in 20 probes |
| `verified-610.57.04.json` | 26 calls compared, **2304 of 2304 bytes**, masked `mediated:bdf-address` ×26 and `gpuId` ×26, every compared word stable |

and the mediation it names five fields for: `pci_info.domain @4`,
`pci_info.bus @8`, `pci_info.slot @9`, `pci_info.function @10`,
`gpu_id @16`. So one file masks it as mediated over its whole answer while
the other calls it carried-unchanged.

**Option 1 — reclassify it as mediated.**
*Cost:* the catalogue's headline counts move: `passthrough` 182 → **181**,
and it lands in `implemented-verified-mediated`, 1 → **2**. Every artefact
quoting "182 passthrough" becomes stale, including three handoffs and this
document.
*Downstream:* the counts start meaning what a reader takes them to mean. It
also sets the rule for the next case: an escape the module rewrites by hand
counts as implemented, whether or not a descriptor-table row exists.

**Option 2 — leave `passthrough` and add a note to the row.**
*Cost:* nothing moves; the note carries the caveat.
*Downstream:* the headline count keeps understating what is built, which is
the error direction this entry objects to, and `manifest_answered` exists
precisely to prevent that direction for controls. A note is a footnote on a
number people quote without the footnote.

**Option 3 — give it a descriptor-table row, so the classification follows
from the table rather than from a judgement.**
*Cost:* real work, and it changes the guest module: the rewrite currently
happens by hand in `virtio_nvrm.c`, and the table would have to be able to
express an inline block, which is not the shape `nested_ptrs` has.
*Downstream:* this is the only option that makes the two files agree by
construction instead of by decision, which is the property the rest of this
pipeline has and the reason the disagreement was visible at all. It is also
the largest.

**Recommendation.** Take **1** now and put **3** on the list. The
disagreement is a classification error today and 1 fixes it in one place;
3 is right and is a different size of job. Reporting a mediated call as
passthrough understates what has been built, and this project's rule
everywhere else is that the honest direction of error is the conservative
one — which here means calling it mediated, not calling it carried.

**Note the counts are load-bearing:** whichever option is taken, the
catalogue must be regenerated and the handoffs' "182 passthrough" lines
become historical. That is the whole reason this is a question and not a
commit.

**A person answers with one word:** `1`, `2`, `3`, or `leave-open`.

---

**DECIDED 2026-08-21 by the operator: option 3** -- make the classification
follow from a table rather than from a judgement. Built, and the counts moved
exactly as the memo predicted:

    before   182 passthrough, 1 implemented-verified-mediated
    after    181 passthrough, 2 implemented-verified-mediated

**But not the table the memo assumed, and that is the useful part.** Option 3
was written as "give it a descriptor-table row", and that would have been
dishonest: an `ioctl` row in the descriptor table carries TRANSLATION offsets
-- an fd field, an embedded pointer, an XFER wrapper -- and `NV_ESC_CARD_INFO`
needs none of them. It carries its answer in the inline block and the guest
module rewrites it in place. A row with no translation fields would have said
"this escape needs translation of kind X" where X does not exist, to make a
count come out right.

**The table that already knows this fact is the mediation manifest**, and it
is generated from `crates/nvrm-abi/src/mediate.rs` -- the SAME table the guest
module's BDF header is generated from, so a field the module rewrites and the
manifest does not know about cannot exist. `ioctlmatrix` classifies an escape
from the descriptor table OR the manifest now, and the row says which:

    no descriptor row and mediated anyway: the guest module rewrites
    pci_info.domain @4 (bdf-address); pci_info.bus @8 (bdf-address);
    pci_info.slot @9 (bdf-address); pci_info.function @10 (bdf-address);
    gpu_id @16 (bdf-scalar) in the inline block (mediation.txt, generated
    from mediate.rs)

So the requirement is met -- derived from a generated table, no judgement in
the loop -- and nothing had to be invented to meet it.

**The catalogue had ALREADY WRITTEN DOWN this contradiction as a fact**, in
`manifest_answered`'s docstring: *"an escape carries its mediation under `sub`
of '-' and is classified from the descriptor table like any other escape --
NV_ESC_CARD_INFO is passthrough and mediated at once, which is exactly what
the evidence file says about it."* A comment that explains why two artefacts
disagree is a bug report with nobody assigned; that one is now wrong and
removed.

**Descriptor-table kinds are deliberately not counted** by the new reader --
a pointer or an fd field is the descriptor table's business and `gi` already
classifies from it. Counting them in both places would say nothing new and
would let the two readers disagree.

**It landed in `implemented-verified-mediated`, not `implemented-unverified`**,
because `verify` had already judged its bytes: 26 calls, 2304 of 2304, differing
in exactly the five declared fields and no other byte. The classification was
the only thing missing.

**Still true and still worth doing, from option 3's original wording:** if a
future escape needs real translation, that IS a descriptor-table row and this
change does not remove the need for one. What it removes is the assumption
that the descriptor table is the only table a classification may come from.
### 63. Two answers behind an NvP64 differ, and both look like the hardware saying so
**Resolved 2026-08-21: option 3, a `host_assigned` class.** One half answered
itself before the decision was taken. The original reasoning is kept below.
They became visible the moment the tracer
started following a control's `NvP64` — before that, `ctrlout` dumped the
params buffer, which for a list control is the QUESTION, and thirteen
signatures were reported `verified` on it (number 55, and the class
`answer_behind_a_pointer` that briefly existed). With the pointer followed,
two of the thirteen do not match.

**`NV2080_CTRL_CMD_BUS_GET_INFO`** (`ctl nr=0x2a sub=0x20801802`), in `nvml`.
The `busInfoList` entry at index `0x2d` —
`NV2080_CTRL_BUS_INFO_INDEX_PCIE_GEN_INFO` (ctrl2080bus.h:329) — answers

| | data |
|---|---|
| native | `0x00222000` |
| guest | `0x00212000` |

The other entry of the same list, index 0, answers `0x3` on both sides. The
difference is one bit position in a PCIe generation field, and the guest's
view of the link is genuinely not the host's: the card is passed through and
what the guest sees of the PCIe topology is the virtual one. Plausibly
correct on both sides and declared by nothing.

**`NV0080_CTRL_CMD_FIFO_GET_CHANNELLIST`** (`ctl nr=0x2a sub=0x80170d`), in
eight probes — every CUDA one, `nvdec`, `nvenc`, `opencl`. The command has
two `NvP64`s, `pChannelHandleList` and `pChannelList`, so the dump is a
handle followed by a channel id. The handle matches (the handle mask covers
it); the **channel id** is `0x35` natively and `0x37` in the guest,
consistently, in all eight.

A hardware channel id is assigned by RM when the channel is allocated, and
the host has channels of its own that the guest does not. It is the same
shape as `workSubmitToken` on `0xc36f0108`, which differs for the same
reason and is likewise unmasked.

**WHAT IS OPEN IS WHETHER THESE NEED A MASK, AND OF WHAT KIND.** Both are
values the hardware or the host assigns, which a guest cannot be expected to
match — the same category as a gpuId or an RM handle, and those have derived
masks. A channel-id mask could be derived the way the handle mask is: a value
this side's OWN trace shows being assigned to a channel. The PCIe field has
no such derivation; declaring it would be a declared mask, which this
project has avoided on purpose, and the alternative is to leave it reported.

Neither should be masked by declaration on the strength of looking
plausible. Both are reported as mismatches today, which is the safe place for
them to sit while the question is open.

---

---

**DECISION MEMO, written 2026-08-21. Nothing below decides it: option 2
would add the first declared mask in this tree, which is a person's call.**

**HALF OF THIS ENTRY HAS ALREADY ANSWERED ITSELF, and by the method this
project prefers.** `BUS_GET_INFO`'s PCIe generation field is **no longer a
mismatch**. In `verified-610.57.04.json` as it stands it is classified
`unstable`, with the reason *"call 1, +4 in the buffer behind the NvP64:
0x00222000 natively, 0x00202000 in the guest -- and this word is NOT STABLE
between two native runs"*. Note the guest value: this entry recorded
`0x00212000` and the artefact now says `0x00202000`. **The field moved
between runs, which is what "unstable" means**, and the control test
classified it out without anybody deciding anything.

So the PCIe field needs no mask at all. It needed a second native run, and
it got one. That is worth keeping as the general lesson: the answer to *"is
this difference real?"* was not a mask, it was a control.

**What is left is one row.** `NV0080_CTRL_CMD_FIFO_GET_CHANNELLIST`
(`ctl 0x2a 0x80170d`), the sole surviving `mismatch` in the evidence file,
in eight probes, `stability: every word that differs was constant across the
calls one native run made`.

**Re-measured 2026-08-21, and it is sharper than the entry states.** The
dump behind the two `NvP64`s is a handle followed by a channel id, 8 bytes
per channel. In `nvenc`, the first three channels:

| | handle | channel id |
|---|---|---|
| native | `0x5c000019`, `0x5c00001f`, `0x5c000023` | `0x35`, `0x36`, `0x37` |
| guest | `0x5c000019`, `0x5c00001f`, `0x5c000023` | `0x37`, `0x38`, `0x39` |

**The handles are identical and the ids are the native ones + 2**, and the
first channel is `0x35` natively against `0x37` in the guest in all five
probes checked individually (`cuda-core`, `cuda-jit`, `nvdec`, `nvenc`,
`opencl`). A constant offset is what "the host has channels of its own that
the guest does not" predicts.

**AND A DERIVED MASK IS NOT AVAILABLE, which is the decisive new fact.**
The handle mask works because a handle is observed being ASSIGNED in the
trace's own allocation lines. A channel id is not: the channel allocation
(`hClass 0xc46f`, 376 bytes of answer) does not carry it — checked in both
the native and the guest `nvenc` traces, zero occurrences in the guest's
allocation answers. The only place the id appears is inside the answer of
the very command under test, so deriving the mask from it would make the
instrument agree with what it is measuring — the same circularity that
stopped allocation dump lengths being taken from the table under test.

**Option 1 — leave it reported as a mismatch (status quo).**
*Cost:* the `mismatch` class is never empty, so "mismatch is empty" stops
being a usable one-line health statement for the sweep.
*Downstream:* the safe direction. A real defect appearing later in this
class is still visible, just alongside a known row.

**Option 2 — declare a channel-id mask.**
*Cost:* **the first declared mask in the tree.** All five existing masks are
derived from what a run produced, and the handoffs record that property as
deliberate and worth keeping. Declaring one spends it.
*Downstream:* `mismatch` becomes empty and stays a meaningful alarm. But the
next plausible-looking difference has a precedent to point at, and that
precedent is the thing this project has refused on purpose.

**Option 3 — a new class, beside `unstable` and `mismatch`:
`host-assigned`.** Not masked, not a defect, listed separately: values the
HOST or the hardware assigns that a guest cannot be expected to match.
`workSubmitToken` on `0xc36f0108` is already the same shape and is likewise
unmasked, so this class would have two members on the day it is created.
*Cost:* a class and its criterion — and the criterion must be stated so it
cannot become a place to put anything inconvenient.
*Downstream:* `mismatch` becomes empty and stays an alarm, WITHOUT declaring
a mask. The claim moves from "these bytes are equal" to "these bytes differ
and here is the category", which is what the evidence actually supports.

**Recommendation.** Take **3**. It is the only option that gets `mismatch`
back to being a usable alarm without spending the derived-mask property, and
the honest statement about a channel id is not "ignore this word" but "this
word is assigned by the host". Option 2 buys the same alarm for a principle
this project has held on purpose; option 1 keeps the principle and loses the
alarm.

**Whichever is taken, the constant +2 deserves one more measurement first:**
it should be checked on a rig where the host has a DIFFERENT number of
channels open, because if the offset tracks that count the category is
proven rather than inferred. That is a cheap run and it is not blocking.

**A person answers with one word:** `1`, `2`, `3`, or `leave-open`.

---

**DECIDED 2026-08-21 by the operator: option 3 -- a `host_assigned` class
beside `unstable` and `mismatch`.** Built, and the result is measured:

    before   1 MISMATCH, 212 verified + 4 mediated
    after    0 MISMATCH, 212 verified + 4 mediated, 1 host_assigned

**`mismatch` is empty and `verified` did not move.** That second half is the
point: the class LABELS, it does not promote. A row in it joins neither
verified class, exactly as `unstable` does not.

**Only ONE of the two rows is in it, and the other never needed it.**
`BUS_GET_INFO`'s PCIe generation field had already classified itself out -- the
control test calls it `unstable`, and its guest value moved between runs
(`0x00212000` when this entry was written, `0x00202000` in the artefact
since). A second native run answered it, which is the method this project
prefers to a mask, and no declaration was spent on it.

**A correction to the memo above:** it said `workSubmitToken` (`0xc36f0108`)
was "already the same shape" and would make this class two members on day one.
It would not. That signature is `unstable` today and correctly so -- a submit
token changes per channel allocation. The class has exactly one member,
`NV0080_CTRL_CMD_FIFO_GET_CHANNELLIST`.

**What keeps it honest**, written into `answerdiff.py` beside the table:

  * it is a CLASS, not a MASK, and the difference is the whole design. A mask
    says "these bytes may differ" and the signature can still be verified -- a
    claim about correctness. This says "these bytes DO differ, and here is the
    category". The cost of a wrong entry is bounded to losing an alarm; it can
    never make something count as verified that is not.
  * it is checked **last**, after the derived masks, the control test, the
    mediation manifest and the written-ness label. Only a word both sides
    wrote, that is stable, and that nothing else explains, can reach it.
  * the criterion is four numbered conditions, so it cannot become a place to
    put anything inconvenient, and every entry names its evidence.

**Why a DERIVED mask was not available**, which is what forced a declaration
and is the fact that decides this entry: the channel id appears nowhere in the
trace except inside the answer of the command under test. The channel
allocation (`hClass 0xc46f`, 376 bytes of answer) does not carry it -- checked
in both the native and the guest `nvenc` traces. Deriving a mask from the
command's own answer would make the instrument agree with what it is
measuring, the same circularity that stopped allocation dump lengths being
taken from the table under test.

**Still worth one more measurement, and it is not blocking:** the offset is a
constant +2 on this rig. On a rig where the host has a different number of
channels open it should differ by that count instead, and if it does the
category is proven rather than inferred.
### 66. The control test proves stability on one side of the boundary only
**Resolved 2026-08-21: the third variant exists.** The original reasoning is
kept below. A signature moved from `verified` to
`mismatch` on a re-run that changed nothing but the guest traces, and the
reason is a gap in the instrument rather than a change in the boundary.

`ctl 0x2a 0x2080a0a8`, `unknown -- not in public headers`, in `nvml`, 32908
bytes of answer, at offset 1064:

| trace | word @1064 |
|---|---|
| native run | `0x001cd6d0` |
| native CONTROL run (the second native trace) | `0x001cd6d0` |
| guest run, 2026-08-21 sweep | `0x001d1168` |
| guest run, previous sweep | matched the native value -- the signature was `verified` in the committed artefact |

**So the word is STABLE across two native runs and VARIES across two guest
runs**, and the control test cannot see that, because both of its variants
compare native against native:

  * the within-trace variant compares calls of one native run;
  * the second-native-run variant compares call *i* of native run A against
    call *i* of native run B.

There is no guest-side variant, so a value that moves between guest runs is
indistinguishable from a value the boundary got wrong. It lands in
`mismatch`, which is the safe place for it, and it is why the mismatch class
is not empty today after being empty yesterday.

**What this is NOT.** It is not the `unstable` class doing its job -- that
class means "native-against-native moved", and this did not. It is not a
regression either: nothing in `xlate.rs`, the backend or the guest module
changed between the two sweeps that produced the two guest values.

**What would settle it**, and it is the exact mirror of what number 55 asked
for on the native side: **a second GUEST run per probe**, unioned with the
native control the same way. `trace` already takes a second native run for
this reason (`<traces>/control/`); the guest phase takes one. A word that
moves between two guest runs of the same probe is evidence for nothing, in
either direction, and should be classified out rather than reported as a
difference.

The cost is one extra guest sweep per run -- about two minutes, measured --
and the guest phase already loops over probes, so it is the same change
`trace` took.

**Until then, read a lone `mismatch` in this namespace with suspicion.** The
`0x2080axxx` unknowns are where the counter-shaped values live: `0x2080a079`
and `0x2080a097` are already recorded as moving between runs, and this is a
third of the same shape. That is a hint about what the value is and it is not
evidence -- which is the whole reason the class exists.

---

**BUILT THE SAME DAY.** `ioctl-matrix.sh guest` now takes a SECOND guest run
of every probe that passes, into `<traces>/guest/control/`, exactly as `trace`
has taken a second native run since number 55 asked for one. Same rules: not
gated, not counted, never able to fail a probe or enter the catalogue.

The reader needed almost nothing, and that is the pleasant part:
`answerdiff.read_control` already took a DIRECTORY and compared
`<dir>/control/` against `<dir>/`, so pointing it at the guest side works
without it knowing which side it is looking at. Three variants are unioned
now, and none replaces another.

**WHAT IT ACTUALLY ADDS, measured rather than assumed:**

    native control : 251 signatures with a second run, 24 carrying unstable words
    guest  control : 244 signatures with a second run, 20 carrying unstable words
    found ONLY by the guest pair: 2

        ctl 0x2a 0x2080018d   word at offset 16
        ctl 0x2a 0x2080a081   word at offset 84

Two signatures carry a word that moves between two GUEST runs and never
between two native ones. That is precisely the class this entry describes,
and before today nothing could see it. `0x2080018d` is one of the seven
commands the BACKEND answers itself, which makes it the least surprising
possible member: a value this project computes is a value this project can
compute differently twice.

**AND AN HONEST LIMIT, because the signature that motivated this entry was
NOT re-caught.** `ctl 0x2a 0x2080a0a8` came back `verified` on this sweep:
all four traces -- native, native control, guest, guest control -- read
`0x001cd6d0`. Its previous guest value was `0x001d1168`. So that word varies
**between sweeps** and was steady **within** one, and two guest runs minutes
apart cannot see that. `mismatch` is 0 again, but it is 0 because the value
happened to agree, not because the new test classified it out.

So this closes the gap it was written about -- a word that moves only on the
guest side is now visible -- and it does not close the harder case underneath:
a value that is steady within a session and moves across sessions. Whether
`0x2080a0a8` is that, or something about a fresh guest boot, is unanswered and
deliberately left so: it is one signature, in the `0x2080axxx` unknowns where
the counter-shaped values live, and nothing rests on it. If it reappears as a
`mismatch` the same reasoning applies and the sweep-to-sweep variant is the
next instrument.
