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

### 14. Fence waits sometimes fall back to the polling timer
**Open, rarer since an unrelated fix.** A woken guest answers a fence in
tenths of a millisecond. A polling guest answers in exactly 10.07 ms,
because that is the fallback timer. The `events` stage of the display gate
counts how many waits took the fallback, so a regression is visible rather
than merely slow. It has not been traced to a cause.

### 15. Concurrent CUDA processes failed on a long-running guest
**Open, seen once on 2026-08-16, not reproduced since.** On a desktop
guest after a long probing session, two simultaneous `nvprobe 3` runs
became unreliable and four were hopeless (`cuCtxCreate: out of memory`,
`cuInit: no CUDA-capable device`); one alone still worked, and thirty-two
at once got one through. A fresh guest does not show it, which points at
accumulated state rather than at concurrency itself. Possibly the same
root as number 31.

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

### 22. `GL_OUT_OF_MEMORY` on EGLImage import under Xwayland
**Open; three separate defects under one number.** Steam's and CS2's
windows exist and are mapped but are never drawn, and glamor reports
`GL_OUT_OF_MEMORY — Failed to acquire the EGL Image memory`. Defect A is
named (see 25); B is a 32-bit gap in GBM packaging; C is the import
failure itself, which is now understood as the head of the chain in 35.

### 23. GLX clients segfault in the guest
**Open, reproduced, root cause unknown.** `glxgears` and Steam's
`gldriverquery` both segfault at address 8 inside
`libGLX_nvidia.so`. Number 33 established that the faulting pointer is
exactly NULL rather than a wrongly mapped address, and that the crash
depends on process state rather than on which client runs. Number 29
narrowed it further: the same binary in a self-started Xwayland instance
does not crash. Number 44 names the object (2026-08-20): the NULL is the
`+8` field of a config object that libGLX_nvidia's list walk and glcore's
array search both dereference, both dying at address 8, and the game's
crash is the same defect rather than a neighbouring one.

### 25. The displayless HAL is forced, and the EVO path is a privilege question
**Open; cause measured, the choice between three routes is not made.** With
`display=2` the EVO path prints exactly one line:
`NV0073_CTRL_CMD_SPECIFIC_GET_ALL_HEAD_MASK` returns
`NV_ERR_INSUFFICIENT_PERMISSIONS`, and nvidia-modeset gives up with
`Failed to get head configuration`. So the EVO path is not missing, it is
refused — a privilege question, not a capability one. Three routes out
exist and the choice is a design decision, not a measurement.

### 31. The backend holds thousands of `nvidiactl` file descriptors
**Open.** Measured on a running rig: after 4 h 20 min the backend held
**2003** open `/dev/nvidiactl` descriptors while exactly one guest process
(`gnome-shell`) had the GPU open. It is the first hard, monotonically
growing resource leak on our own side. It is not the leak number 26 was
looking for, and it is a strong candidate for the root of number 15.

### 32. Xwayland dies on SIGFPE inside NVIDIA's EGL core
**Open; the faulting instruction is named, the field is not.** Twice, both
times at the same instruction inside `libnvidia-eglcore`, Xwayland took a
floating point exception and aborted. A division by zero means some value
we supply is zero where the driver assumes it cannot be. Number 39 cleared
three candidate controls, which answer completely and plausibly.

### 33. The crash in 23 is a NULL pointer, and depends on state
**Open.** All `segfault at 8` addresses of one boot were resolved back to
two instructions in `libGLX_nvidia`, both dereferencing offset 8 of a base
pointer that is exactly zero. That rules out the "wrongly mapped address"
hypothesis. The crash follows process state rather than the client, and
the instance appears to poison itself over time.

### 35. The EGLImage import failure is the head of the chain
**Open; the chain is closed, the next measurement is not taken.** Xwayland's
own backtrace shows the failure originating in `libnvidia-eglcore` and
propagating up through glamor. Numbers 22-C, 23, 26, 32 and 33 all pointed
at this without naming it. Fixing the import is expected to resolve the
rest; nothing above it needs its own fix.

### 42. Is `capDescriptor` on 0xc640 really an fd that needs no translation?
**Open, and deliberately left as it is.** `NV0080_CTRL_CMD_FIFO_...` class
0xc640 carries a `capDescriptor` in its alloc parameters, which is an fd in
the guest's numbering, and `alloc_fd_field` in
[`crates/nvrm-abi/src/xlate.rs`](../crates/nvrm-abi/src/xlate.rs) has no
entry for it -- so it is forwarded untranslated. Nothing has been observed
to break, because the field is MIG-only and no measured workload allocates
that class. Two answers are possible and only one is right: either the
field is never a real fd on this path, or the entry is missing and a MIG
guest would hand the host a number from its own table. Deciding it needs a
workload that allocates the class, not more reading.

### 43. Which FD does the driver require the mapping ioctl on?
**Open; the code works and the documentation used to disagree with it.**
[`crates/nvrm-client/src/mem.rs`](../crates/nvrm-client/src/mem.rs) issues
the map ioctl on `rm.ctl()`, and that path is measured and works. The
prose in this repository claimed for a while that it must go to the
freshly opened fd instead. The doubt is written down rather than resolved:
the two arrangements have never been compared against a real libcuda
trace, which is what would settle it. Until then the code is the
statement, not the prose.

### 44. A game and the compositor die at the same two addresses in NVIDIA's GL core
**Open, and this is the first stack the chain in 32/35 has.** Shadow of
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

---

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

### 24. The X11 counter-test: all three defects hang on the Wayland path
**Resolved 2026-08-18.** One session switch answered three open questions.
On real Xorg the GLX vendor is NVIDIA rather than SGI/glamor, `glxgears`
runs instead of segfaulting, and `vkcube` under FIFO sits at 58.1 FPS
instead of 1520. That resolved a contradiction that had stood since
2026-08-17.

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

### 34. The host writes one `dmesg` line per ioctl
**Named as a trap.** At `ResmanDebugLevel: 0` the driver still prints
`NV_DBG_INFO`, which is one line per ioctl. The line looks like a
rejection and is not. The damage is real twice over: a kernel log write on
a per-frame path costs time, and 2734 such lines had displaced every other
diagnosis from the ring buffer.

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
