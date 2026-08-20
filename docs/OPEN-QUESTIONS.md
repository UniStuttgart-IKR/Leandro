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
does not crash.

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
