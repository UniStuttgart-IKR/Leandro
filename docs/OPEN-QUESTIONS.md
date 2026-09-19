<!-- SPDX-License-Identifier: MIT -->
# Questions

- Numbers are permanent. Keep existing headings so code comments and links remain useful.
- This is the working index. The [2026-09-18 snapshot](history/OPEN-QUESTIONS-2026-09-18.md) preserves every original entry, measurement and correction.
- Historical closure describes the recorded investigation, not a claim that all related behavior or driver versions are verified.
- Current review work is tracked in 76–83. Entries distinguish implemented fixes from remaining runtime evidence. See [security limits](SECURITY.md) and [testing](TESTING.md).

## Open

### 14. Fence waits sometimes fall back to the polling timer

- **Open.** Occasional fence waits still reach the 10.07 ms fallback. Earlier tests narrowed the cause; no causal fix is established. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#14-fence-waits-sometimes-fall-back-to-the-polling-timer).

### 15. Concurrent CUDA processes failed on a long-running guest

- **Open.** One long-running guest failed with concurrent CUDA processes. Later attempts did not reproduce it; retain the original rig state and counters. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#15-concurrent-cuda-processes-failed-on-a-long-running-guest).

### 16. Connector detect breaks after a session that really drew

- **Open.** Connector detection failed after an abnormal compositor exit. The failing virtual-display control is known; the trigger is not reproduced. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#16-connector-detect-breaks-after-a-session-that-really-drew).

### 27. `nvidia_drm vblank=0` throttles better than `vblank=1`

- **Open.** The vblank configuration changes throttling. Decide which behavior the virtual display should promise before changing it. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#27-nvidia_drm-vblank0-throttles-better-than-vblank1).

### 31. The backend holds thousands of `nvidiactl` file descriptors

- **Open.** Backend control FDs can accumulate. Separate live event registrations, pooled slots and leaked ownership before choosing a fix. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#31-the-backend-holds-thousands-of-nvidiactl-file-descriptors).
- Waiter retirement now owns FDs through cancellation acknowledgement. Measure the new FD census; this change alone does not explain the historical total.

### 46. Sound continues while the picture hangs -- the CPU half

- **Open.** CPU starvation can freeze the picture while audio continues. Keep this separate from the VRAM-pressure failure in 67. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#46-sound-continues-while-the-picture-hangs----the-cpu-half).

### 56. The surface is tracked per run, and the question is per ioctl

- **Open.** Per-run surface coverage does not establish each ioctl shape across driver versions and GPU architectures. Preserve per-call provenance. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#56-the-surface-is-tracked-per-run-and-the-question-is-per-ioctl).

### 59. The probes are entry paths, and the class that can be missing is the one they do not reach

- **Open.** Probe entry paths leave classes unexercised. A compiled descriptor or reached library entry point does not prove a class works. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#59-the-probes-are-entry-paths-and-the-class-that-can-be-missing-is-the-one-they-do-not-reach).

### 67. A transient VRAM squeeze wedges the compositor permanently

- **Open.** A transient VRAM squeeze left the compositor frozen after memory recovered. Reproduction and recovery guarantees remain open. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#67-a-transient-vram-squeeze-wedges-the-compositor-permanently).

### 68. The VRAM cap is accounting, not a reservation

- **Open.** VRAM accounting is not a physical reservation. Driver-owned context memory and concurrent tenants can exhaust the card before a VM reaches its cap. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#68-the-vram-cap-is-accounting-not-a-reservation).

### 69. A vGPU-shaped VRAM policy: the card names the numbers

- **Open.** Profile naming, host reserve and guest-visible capacity need one documented policy. Existing measurements do not establish isolation or a universal reserve. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#69-a-vgpu-shaped-vram-policy-the-card-names-the-numbers).

### 70. What a VRAM limit costs, and what happens at the edge

- **Open.** Measure limit overhead and actual refusal/recovery behavior. A workload adapting to the reported capacity does not exercise the failure path. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#70-what-a-vram-limit-costs-and-what-happens-at-the-edge).

### 71. The catalogue moves: a type is not one size twice

- **Open.** Profile capacity can depend on current host use, and hidden per-VM memory raises its real cost. A stable type needs a stable capacity contract. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#71-the-catalogue-moves-a-type-is-not-one-size-twice).

### 72. A branch is not a layout, and R610 already carries two

- **Open.** The ABI configuration now states that a branch can contain different layouts. Review naming/support when two incompatible minor releases of one branch must coexist. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#72-a-branch-is-not-a-layout-and-r610-already-carries-two).

### 74. The virtual display's class does not exist before R595

- **Open.** The displayless class is absent before R595. Header generation works for older versions; the older guest display path remains unverified. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#74-the-virtual-displays-class-does-not-exist-before-r595).

### 76. RM source handles need a VM ownership policy

- **Open, source review 2026-09-18.** RM DUP checks the destination client/OFD, then authorizes access to the globally named source object through its share policy.
- The automatic grant now uses backend PID rather than host UID. This closes that automatic same-user grant; it does not validate every guest-supplied source root or override a wider explicit sharing policy.
- `client_policy.rs` now excludes private pool roots from reviewed guest operations. That exclusion is not a registry of all guest-owned native roots or their aliases.
- Before a hostile-guest claim, enforce VM-owned source roots across RM DUP, UVM imports and related sharing paths, and test two separate backends. See [security limits](SECURITY.md).

### 77. Waiter cancellation needs an ownership handshake

- **Implemented 2026-09-19; runtime stress pending.** Poll snapshots retain owned FDs; each arm has a generation. Installation/cancellation are acknowledged by the worker, and cancelled slots remain unavailable until the old poll and queued completion are retired.
- Normal completion is published after snapshot retirement. Unregister matches the client, surface, index, wait value and callback identity; a failed native unregister keeps its arm live.
- Deterministic regressions cover delayed snapshots, rearm, stale completion and worker failure. Hardware event latency, FD census and sustained cancellation/rearm still need measurement.
- This handshake establishes poller quiescence, not final RM/GPU backing release or lifetime of callbacks already queued in the guest.

### 78. Request preparation needs rollback

- **Implemented for the reviewed paths, 2026-09-19.** `request_shape.rs` validates host-known envelopes, pointer spans and FD metadata before event acquisition. Event/waiter acquisition follows buffer translation and validation.
- Fake-driver regressions reject missing, short, overlapping and misplaced descriptors before driver execution while preserving NULL query and sparse nested-buffer forms.
- Private UVM/RM pool owners now record acquisitions and roll back in reverse dependency order; failed cleanup retains the complete owner and charge for retry.
- Unannotated fields and native aliases remain separate work in 80–81; this is not proof of every driver operation's lifetime.

### 79. OS descriptor GPU FD is cached across client OFDs

- **Open, source review 2026-09-18.** A session caches one GPU FD registered against the first control FD, although one guest process can own multiple RM clients/control OFDs.
- Confirm the supported association with vendor source and a two-client hardware test. If association is per control OFD, key the cache by that owned identity and release it with the client.
- This is an ownership concern from source review, not a demonstrated GPU failure.

### 80. Source teardown does not release every backing reference

- **Open, source review 2026-09-19.** RM DUP shares a memory descriptor; exported/imported objects can survive source-client destruction. FD close can defer native client cleanup.
- The host retains forwarded OS-descriptor arenas after source release. The guest quarantines uncertain submitted allocations and failed teardown, but ordinary successful frees still lack a complete alias-release protocol.
- Track source, duplicate, import, export-FD and implicit UVM ownership under one backing identity. A software count reaching zero needs a verified native release fence before guest pages become reusable.
- Test source/export/destination teardown in different orders, delayed replies and forced guest page reuse. A successful FREE/CLOSE/PROC_GONE or poller acknowledgement is insufficient evidence.

### 81. Known request shapes do not cover every driver pointer or FD

- **Partly implemented, 2026-09-19.** Host-derived validation rejects missing translation metadata. The host additionally refuses 28 known embedded-pointer controls, four untranslated Unix FD-input controls, seven capability-FD classes and serialized RM layouts.
- The two existing attribution controls remain blocked. The [current policy](SECURITY.md#refused-operations) and `xlate::blocked_ctrls` identify the supported boundary.
- Unknown control/class internals still need a broader audit or strict operation allowlist. The vendor-source review is not four complete driver audits; numeric header agreement is weaker evidence.
- Restore refused operations only with translation/bounds tests and workloads that use them. Debugging, P2P/MIG, profiling and uncommon display/encode compatibility remain to be measured on this batch.

### 82. Device references do not make live unbind safe

- **Open, partly implemented 2026-09-19.** Contexts, requests, windows and pools retain device storage. Queue stop rejects new submissions, synchronizes callbacks and drains detached requests/work.
- Monotonic process IDs survive rebind within one module lifetime. Reload resets them and requires a fresh backend.
- Device references do not revoke mapped SHMEM BARs or drain whole NVKMS operations. An in-progress mapping can outlive removal's mapping-list drain; live unbind/rebind remains unsupported.
- Uncertain backing retains module references and quota until guest restart. Add delayed-completion/reset/failed-close tests with KASAN and lock debugging before strengthening teardown claims.

### 83. Conservative pin retention can exhaust the VM budget

- **Implemented accounting; capacity validation open, 2026-09-19.** The shared `PinBudget` charges page-rounded pending, active, retained and quarantined registrations. Defaults are 1024 MiB per VM and 256 MiB per arena.
- Source free does not refund forwarded registrations while aliases are unknown. Private cleanup failures retain charges and retryable owners; failed final cleanup can retain them until backend exit.
- This bounds registration admission, not unique physical pages. The aggregate default is provisional; repeated successful allocation/free can reach it.
- Measure retained/quarantined bytes under repeated CUDA, display and PRIME workloads. If normal use exhausts the cap, implement the reference ledger rather than treating a larger limit as reclamation.

## Resolved and decided

### 1. Does the descriptor table warrant a protocol change?

- **Decided.** Additive descriptor kinds do not alone require a protocol bump; retain compatibility checks. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#1-does-the-descriptor-table-warrant-a-protocol-change).

### 2. Who owns `/proc/driver/nvidia` when both modules are loaded?

- **Decided.** Keep one owner for the NVIDIA proc interface when both modules are loaded. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#2-who-owns-procdrivernvidia-when-both-modules-are-loaded).

### 3. Project name, and the `nvshim-*` crate prefix

- **Decided.** Use Leandro and the nvrm crate names; preserve the naming rationale. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#3-project-name-and-the-nvshim--crate-prefix).

### 4. One host session per VM, or per guest process?

- **Decided.** Sessions follow guest processes; pool virtual addresses can repeat across processes. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#4-one-host-session-per-vm-or-per-guest-process).

### 5. Where does the seam run in `session.rs`?

- **Decided.** Keep request preparation and syscall execution distinct; 78 records the new validation-before-acquisition boundary. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#5-where-does-the-seam-run-in-sessionrs).

### 6. How does the kernel path resolve a guest file descriptor?

- **Resolved.** Translate guest file descriptors through tokens owned by the appropriate guest session. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#6-how-does-the-kernel-path-resolve-a-guest-file-descriptor).

### 6a. The same question as first written

- **Superseded by 6.** Preserved original wording; this number remains permanent. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#6a-the-same-question-as-first-written).

### 7. A vblank callback is a guest kernel function pointer

- **Resolved.** Replace guest kernel callbacks with host event IDs and return notifications through the event path. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#7-a-vblank-callback-is-a-guest-kernel-function-pointer).

### 8. GLX clients get FBConfig id 0

- **Superseded by 9.** The observed GL failure followed a dead shared-memory channel. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#8-glx-clients-get-fbconfig-id-0).

### 9. NVKMS wedges after a killed X server plus a module reload

- **Resolved in archived runs.** The killed-X-server/reload investigation includes the evidence and recovery sequence. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#9-nvkms-wedges-after-a-killed-x-server-plus-a-module-reload).

### 10. Black windows under the compositor, then an assert

- **Resolved.** The archived compositor failure links to its downstream crash investigation. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#10-black-windows-under-the-compositor-then-an-assert).

### 11. CS2 gets no Vulkan while `vkcube` runs beside it

- **Resolved.** The game-specific Vulkan path and its counter-tests are recorded. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#11-cs2-gets-no-vulkan-while-vkcube-runs-beside-it).

### 12. The VRAM cap counted one door and the card has several

- **Resolved.** Account for multiple allocation paths rather than only one RM door. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#12-the-vram-cap-counted-one-door-and-the-card-has-several).

### 13. Two directories of probes, two licences

- **Decided.** Keep the probe/source license boundary explicit. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#13-two-directories-of-probes-two-licences).

### 17. Sunshine `capture = kms` under Wayland shows a black stream

- **Resolved in archived runs.** Framebuffer content survived; the black stream investigation identified compositor behavior. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#17-sunshine-capture--kms-under-wayland-shows-a-black-stream).

### 18. Flip completions arrive in excess

- **Resolved in archived runs.** The two NVIDIA completion counters measure different things. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#18-flip-completions-arrive-in-excess).

### 19. `GET_SURFACE_PHYS_PAGES` is refused with `INSUFFICIENT_PERMISSIONS`

- **Resolved.** RM privileges explain the physical-page query refusal in the userspace backend. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#19-get_surface_phys_pages-is-refused-with-insufficient_permissions).

### 20. CS2 renders but its window stays empty

- **Superseded by 22.** The EGLImage import investigation carries the remaining diagnosis. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#20-cs2-renders-but-its-window-stays-empty).

### 21. The backend leaked `/dev/nvidiactl` descriptors and RM clients

- **Fixed in archived runs.** Control-FD and RM-client cleanup was measured; later growth is tracked separately in 31. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#21-the-backend-leaked-devnvidiactl-descriptors-and-rm-clients).

### 22. `GL_OUT_OF_MEMORY` on EGLImage import under Xwayland

- **Resolved in archived runs.** Cross-process FD ownership mattered to EGLImage import; keep the regression context. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#22-gl_out_of_memory-on-eglimage-import-under-xwayland).

### 23. GLX clients segfault in the guest

- **Resolved in archived runs.** The GLX crash depended on shared graphics-stack state; preserve the counter-tests. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#23-glx-clients-segfault-in-the-guest).

### 24. The X11 counter-test: all three defects hang on the Wayland path

- **Counter-test recorded.** The same symptoms were compared through X11 and Wayland paths. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#24-the-x11-counter-test-all-three-defects-hang-on-the-wayland-path).

### 25. The displayless HAL is forced, and the EVO path is a privilege question

- **Decided.** The virtual display uses the displayless HAL; the hardware EVO path has additional privilege constraints. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#25-the-displayless-hal-is-forced-and-the-evo-path-is-a-privilege-question).

### 26. The EGLImage import itself is clean

- **Negative result.** The EGLImage import itself was clean in the recorded comparison. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#26-the-eglimage-import-itself-is-clean).

### 28. 32-bit clients could not find NVIDIA's GLX

- **Fixed.** Register the staged 32-bit NVIDIA GLX libraries as well as the 64-bit ones. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#28-32-bit-clients-could-not-find-nvidias-glx).

### 29. The crash depends on *whose* Xwayland instance it is

- **Resolved in archived runs.** The Xwayland owner and instance state changed the observed crash behavior. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#29-the-crash-depends-on-whose-xwayland-instance-it-is).

### 30. A client's FPS counter is not evidence of presentation

- **Measurement rule.** A client swap counter does not establish visible presentation. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#30-a-clients-fps-counter-is-not-evidence-of-presentation).

### 32. Xwayland dies on SIGFPE inside NVIDIA's EGL core

- **Resolved in archived runs.** The EGL-core SIGFPE investigation and its eliminated causes are preserved. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#32-xwayland-dies-on-sigfpe-inside-nvidias-egl-core).

### 33. The crash in 23 is a NULL pointer, and depends on state

- **Resolved in archived runs.** The GLX fault was a null pointer and depended on process state. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#33-the-crash-in-23-is-a-null-pointer-and-depends-on-state).

### 34. The host writes one `dmesg` line per ioctl

- **Known trap.** Driver logging can occur on every ioctl and distort both timing and diagnostic history. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#34-the-host-writes-one-dmesg-line-per-ioctl).

### 35. The EGLImage import failure is the head of the chain

- **Resolved in archived runs.** The EGLImage failure was upstream of the later crash chain. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#35-the-eglimage-import-failure-is-the-head-of-the-chain).

### 36. Two traps of our own making

- **Known traps.** Empty environment variables and process-name matching caused misleading results. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#36-two-traps-of-our-own-making).

### 37. The frame limiter reaches only some clients

- **Corrected by 41.** The original limiter conclusion was too broad; read the later sweep. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#37-the-frame-limiter-reaches-only-some-clients).

### 38. The limiter triggered a double free in the guest

- **Resolved in archived runs.** The frame limiter exposed a guest lifetime bug; its fix and scope are recorded. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#38-the-limiter-triggered-a-double-free-in-the-guest).

### 39. Three more candidates for the zero divisor are cleared

- **Negative result.** Three control answers were ruled out as the zero-divisor source. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#39-three-more-candidates-for-the-zero-divisor-are-cleared).

### 40. The late-unregister race was the cause

- **Resolved in archived runs.** The measured late-unregister defect was fixed. The host poller lifetime issue in 77 is a separate finding. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#40-the-late-unregister-race-was-the-cause).

### 41. Correction to 37: the configured rate holds across the range

- **Correction recorded.** The limiter held across the measured rate range; client paths still differed. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#41-correction-to-37-the-configured-rate-holds-across-the-range).

### 42. Is `capDescriptor` on 0xc640 really an fd that needs no translation?

- **Resolved.** Treat capDescriptor as an FD and apply the required ownership translation. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#42-is-capdescriptor-on-0xc640-really-an-fd-that-needs-no-translation).

### 43. Which FD does the driver require the mapping ioctl on?

- **Resolved.** The mapping code used the right FD; the original explanation was wrong. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#43-which-fd-does-the-driver-require-the-mapping-ioctl-on).

### 44. A game and the compositor die at the same two addresses in NVIDIA's GL core

- **Resolved in archived runs.** The shared GL-core crash addresses and full chain are preserved. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#44-a-game-and-the-compositor-die-at-the-same-two-addresses-in-nvidias-gl-core).

### 45. `NV_ESC_ATTACH_GPUS_TO_FD` answers `-1` to Xwayland

- **Resolved in archived runs.** An interrupted attach ioctl required the recorded retry/cleanup behavior. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#45-nv_esc_attach_gpus_to_fd-answers--1-to-xwayland).

### 47. Two counting rules in our own instruments are wrong

- **Fixed.** Correct the instrument counting rules before interpreting coverage numbers. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#47-two-counting-rules-in-our-own-instruments-are-wrong).

### 48. Userspace talks to `/dev/nvidia-modeset`, and the tracer cannot see it

- **Resolved.** Decode the NVIDIA modeset interface separately from DRM ioctls. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#48-userspace-talks-to-devnvidia-modeset-and-the-tracer-cannot-see-it).

### 49. A deprecated control is forwarded verbatim with a pointer inside it

- **Resolved.** Vendor source showed the deprecated embedded pointer was not read by RM. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#49-a-deprecated-control-is-forwarded-verbatim-with-a-pointer-inside-it).

### 50. Nothing compares the answer bytes, so nothing is verified

- **Resolved.** Compare returned bytes; reaching an ioctl alone is not verification. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#50-nothing-compares-the-answer-bytes-so-nothing-is-verified).

### 51. A guest answers two nvml controls differently, and both shapes were predicted

- **Fixed in archived runs.** Two NVML response shapes were corrected; the remaining SMC case is 65. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#51-a-guest-answers-two-nvml-controls-differently-and-both-shapes-were-predicted).

### 52. The guest's graphics stack asks a different set of questions

- **Corrected and resolved.** Native/guest graphics queries were compared with controlled X connections. The later DRM correction is part of the evidence. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#52-the-guests-graphics-stack-asks-a-different-set-of-questions).

### 53. Two libraries are staged into the guest and registered with nobody

- **Fixed with counter-tests.** Register the OpenCL ICD. The proposed EGL explanation did not survive the comparison. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#53-two-libraries-are-staged-into-the-guest-and-registered-with-nobody).

### 54. Two probe criteria cannot pass in a guest, because the guest renames the card

- **Fixed.** Probe criteria must account for the mediated GPU name. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#54-two-probe-criteria-cannot-pass-in-a-guest-because-the-guest-renames-the-card).

### 55. The answer bytes are compared now, and the class that can be promoted is not the class that can be reached

- **Resolved.** Distinguish compared answer bytes from merely reached classes. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#55-the-answer-bytes-are-compared-now-and-the-class-that-can-be-promoted-is-not-the-class-that-can-be-reached).

### 57. A vendor manifest is registered nowhere, and a knob depends on it

- **Resolved.** The Vulkan implicit layer did not affect the tested path; the inert knobs were removed. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#57-a-vendor-manifest-is-registered-nowhere-and-a-knob-depends-on-it).

### 58. NVIDIA's xcb EGL platform declines in a guest, and its xlib platform does not

- **Resolved in archived runs.** The xcb/xlib EGL difference was compared under matching X configuration. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#58-nvidias-xcb-egl-platform-declines-in-a-guest-and-its-xlib-platform-does-not).

### 60. One answer survived every mask, and it was memory nobody wrote

- **Resolved.** Unwritten output memory explained the apparent stable answer. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#60-one-answer-survived-every-mask-and-it-was-memory-nobody-wrote).

### 61. Two controls are answered on one side of the boundary and not the other

- **Corrected and resolved.** The original source-evidence argument was insufficient; retain the corrected comparisons. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#61-two-controls-are-answered-on-one-side-of-the-boundary-and-not-the-other).

### 62. An escape the guest module rewrites is not in the descriptor table

- **Resolved.** Classify CARDINFO mediation explicitly rather than implying the descriptor table covers it. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#62-an-escape-the-guest-module-rewrites-is-not-in-the-descriptor-table).

### 63. Two answers behind an NvP64 differ, and both look like the hardware saying so

- **Resolved.** Separate host-assigned answers from deterministic values in response comparisons. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#63-two-answers-behind-an-nvp64-differ-and-both-look-like-the-hardware-saying-so).

### 64. The NVKMS commands are recorded and none of them has a name

- **Resolved.** The NVKMS decoder supplied command names and matched the recorded traces. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#64-the-nvkms-commands-are-recorded-and-none-of-them-has-a-name).

### 65. NVML allocates an SMC monitor session natively and never in a guest

- **Resolved in archived runs.** The native/guest SMC-monitor comparison falsified the remaining hypothesis. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#65-nvml-allocates-an-smc-monitor-session-natively-and-never-in-a-guest).

### 66. The control test proves stability on one side of the boundary only

- **Resolved.** Use native/native/guest comparisons to separate instability from boundary differences. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#66-the-control-test-proves-stability-on-one-side-of-the-boundary-only).

### 73. A renamed type with an identical body is indistinguishable from a removal

- **Implemented in current source.** The ABI configuration records renames and the generator normalizes and checks them. The historical entry predates that implementation. See [abi.toml](../crates/nvrm-sys/abi.toml) and the [generator](../crates/xtask/src/abi/mod.rs). [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#73-a-renamed-type-with-an-identical-body-is-indistinguishable-from-a-removal).

### 75. Every version pair is breaking, and the word has stopped carrying information

- **Implemented in current source.** The ABI configuration now separates the mediated core, with a workspace usage check for volatile names. The historical entry predates that implementation. See [abi.toml](../crates/nvrm-sys/abi.toml) and the [generator](../crates/xtask/src/abi/mod.rs). [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#75-every-version-pair-is-breaking-and-the-word-has-stopped-carrying-information).

<!-- Compatibility anchors for historical subsections. Evidence remains in the snapshot. -->
<a id="2026-08-21-the-same-symptom-a-different-cause-and-a-discriminator-that-works"></a>
- [Archived: 2026-08-21: the same symptom, a different cause, and a discriminator that works](history/OPEN-QUESTIONS-2026-09-18.md#2026-08-21-the-same-symptom-a-different-cause-and-a-discriminator-that-works).
<a id="2026-08-21-second-run-the-freeze-latches-it-does-not-recover-when-the-memory-does"></a>
- [Archived: 2026-08-21, second run: THE FREEZE LATCHES. It does not recover when the memory does.](history/OPEN-QUESTIONS-2026-09-18.md#2026-08-21-second-run-the-freeze-latches-it-does-not-recover-when-the-memory-does).
<a id="resolved-2026-08-21-two-counters-that-disagree-both-of-them-nvidias"></a>
- [Archived: Resolved 2026-08-21. Two counters that disagree, both of them NVIDIA's.](history/OPEN-QUESTIONS-2026-09-18.md#resolved-2026-08-21-two-counters-that-disagree-both-of-them-nvidias).
<a id="resolved-2026-08-21-it-is-rms-privilege-model-and-this-backend-is-in-userspace"></a>
- [Archived: Resolved 2026-08-21. It is RM's privilege model, and this backend is in userspace.](history/OPEN-QUESTIONS-2026-09-18.md#resolved-2026-08-21-it-is-rms-privilege-model-and-this-backend-is-in-userspace).
<a id="resolved-2026-08-21-one-rig-one-variable-and-all-six-come-back"></a>
- [Archived: Resolved 2026-08-21. One rig, one variable, and all six come back.](history/OPEN-QUESTIONS-2026-09-18.md#resolved-2026-08-21-one-rig-one-variable-and-all-six-come-back).
<a id="correction-2026-08-21-same-day-the-drm-claim-in-this-entry-is-wrong"></a>
- [Archived: CORRECTION, 2026-08-21 (same day): the DRM claim in this entry is WRONG.](history/OPEN-QUESTIONS-2026-09-18.md#correction-2026-08-21-same-day-the-drm-claim-in-this-entry-is-wrong).
<a id="resolved-2026-08-21-probepythonnvkmsdecodepy-and-14-of-14-agree"></a>
- [Archived: Resolved 2026-08-21. `probe/python/nvkmsdecode.py`, and 14 of 14 agree.](history/OPEN-QUESTIONS-2026-09-18.md#resolved-2026-08-21-probepythonnvkmsdecodepy-and-14-of-14-agree).
<a id="resolved-2026-08-21-both-halves-measured-and-the-surviving-hypothesis-is-falsified"></a>
- [Archived: Resolved 2026-08-21. Both halves measured, and the surviving hypothesis is falsified.](history/OPEN-QUESTIONS-2026-09-18.md#resolved-2026-08-21-both-halves-measured-and-the-surviving-hypothesis-is-falsified).
