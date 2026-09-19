<!-- SPDX-License-Identifier: MIT -->
# Questions

- Numbers and explicit anchors are permanent; titles may be shortened.
- This is the working index. The [2026-09-18 snapshot](history/OPEN-QUESTIONS-2026-09-18.md) preserves every original entry, measurement and correction.
- Historical closure describes the recorded investigation, not a claim that all related behavior or driver versions are verified.
- Current review work is tracked in 76–83. Entries distinguish implemented fixes from remaining runtime evidence. See [security limits](SECURITY.md) and [testing](TESTING.md).

## Open

### 14. Fence-wait polling fallback <a id="14-fence-waits-sometimes-fall-back-to-the-polling-timer"></a>

- **Open.** Occasional fence waits still reach the 10.07 ms fallback. Earlier tests narrowed the cause; no causal fix is established. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#14-fence-waits-sometimes-fall-back-to-the-polling-timer).

### 15. Concurrent CUDA failure <a id="15-concurrent-cuda-processes-failed-on-a-long-running-guest"></a>

- **Open.** One long-running guest failed with concurrent CUDA processes. Later attempts did not reproduce it; retain the original rig state and counters. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#15-concurrent-cuda-processes-failed-on-a-long-running-guest).

### 16. Connector detection after compositor exit <a id="16-connector-detect-breaks-after-a-session-that-really-drew"></a>

- **Open.** Connector detection failed after an abnormal compositor exit. The failing virtual-display control is known; the trigger is not reproduced. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#16-connector-detect-breaks-after-a-session-that-really-drew).

### 27. Vblank throttling behavior <a id="27-nvidia_drm-vblank0-throttles-better-than-vblank1"></a>

- **Open.** The vblank configuration changes throttling. Decide which behavior the virtual display should promise before changing it. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#27-nvidia_drm-vblank0-throttles-better-than-vblank1).

### 31. Backend control-FD growth <a id="31-the-backend-holds-thousands-of-nvidiactl-file-descriptors"></a>

- **Open.** Backend control FDs can accumulate. Separate live event registrations, pooled slots and leaked ownership before choosing a fix. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#31-the-backend-holds-thousands-of-nvidiactl-file-descriptors).
- Waiter retirement now owns FDs through cancellation acknowledgement. Measure the new FD census; this change alone does not explain the historical total.

### 46. Display stalls under CPU starvation <a id="46-sound-continues-while-the-picture-hangs----the-cpu-half"></a>

- **Open.** CPU starvation can freeze the picture while audio continues. Keep this separate from the VRAM-pressure failure in 67. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#46-sound-continues-while-the-picture-hangs----the-cpu-half).

### 56. Per-ioctl coverage provenance <a id="56-the-surface-is-tracked-per-run-and-the-question-is-per-ioctl"></a>

- **Open.** Per-run surface coverage does not establish each ioctl shape across driver versions and GPU architectures. Preserve per-call provenance. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#56-the-surface-is-tracked-per-run-and-the-question-is-per-ioctl).

### 59. Unexercised RM classes <a id="59-the-probes-are-entry-paths-and-the-class-that-can-be-missing-is-the-one-they-do-not-reach"></a>

- **Open.** Probe entry paths leave classes unexercised. A compiled descriptor or reached library entry point does not prove a class works. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#59-the-probes-are-entry-paths-and-the-class-that-can-be-missing-is-the-one-they-do-not-reach).

### 67. Compositor freeze under VRAM pressure <a id="67-a-transient-vram-squeeze-wedges-the-compositor-permanently"></a>

- **Open.** A transient VRAM squeeze left the compositor frozen after memory recovered. Reproduction and recovery guarantees remain open. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#67-a-transient-vram-squeeze-wedges-the-compositor-permanently).

### 68. VRAM accounting limits <a id="68-the-vram-cap-is-accounting-not-a-reservation"></a>

- **Open.** VRAM accounting is not a physical reservation. Driver-owned context memory and concurrent tenants can exhaust the card before a VM reaches its cap. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#68-the-vram-cap-is-accounting-not-a-reservation).

### 69. Profile capacity policy <a id="69-a-vgpu-shaped-vram-policy-the-card-names-the-numbers"></a>

- **Open.** Profile naming, host reserve and guest-visible capacity need one documented policy. Existing measurements do not establish isolation or a universal reserve. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#69-a-vgpu-shaped-vram-policy-the-card-names-the-numbers).

### 70. VRAM-limit overhead and recovery <a id="70-what-a-vram-limit-costs-and-what-happens-at-the-edge"></a>

- **Open.** Measure limit overhead and actual refusal/recovery behavior. A workload adapting to the reported capacity does not exercise the failure path. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#70-what-a-vram-limit-costs-and-what-happens-at-the-edge).

### 71. Stable profile capacity <a id="71-the-catalogue-moves-a-type-is-not-one-size-twice"></a>

- **Open.** Profile capacity can depend on current host use, and hidden per-VM memory raises its real cost. A stable type needs a stable capacity contract. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#71-the-catalogue-moves-a-type-is-not-one-size-twice).

### 72. ABI differences within a driver branch <a id="72-a-branch-is-not-a-layout-and-r610-already-carries-two"></a>

- **Open.** The ABI configuration now states that a branch can contain different layouts. Review naming/support when two incompatible minor releases of one branch must coexist. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#72-a-branch-is-not-a-layout-and-r610-already-carries-two).

### 74. Pre-R595 virtual-display compatibility <a id="74-the-virtual-displays-class-does-not-exist-before-r595"></a>

- **Open.** The displayless class is absent before R595. Header generation works for older versions; the older guest display path remains unverified. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#74-the-virtual-displays-class-does-not-exist-before-r595).

### 76. VM ownership of source handles <a id="76-rm-source-handles-need-a-vm-ownership-policy"></a>

- **Open, source review 2026-09-18.** RM DUP checks the destination client/OFD, then authorizes access to the globally named source object through its share policy.
- The automatic grant now uses backend PID rather than host UID. This closes that automatic same-user grant; it does not validate every guest-supplied source root or override a wider explicit sharing policy.
- `client_policy.rs` now excludes private pool roots from reviewed guest operations. That exclusion is not a registry of all guest-owned native roots or their aliases.
- Before a hostile-guest claim, enforce VM-owned source roots across RM DUP, UVM imports and related sharing paths, and test two separate backends. See [security limits](SECURITY.md).

### 77. Waiter cancellation ownership <a id="77-waiter-cancellation-needs-an-ownership-handshake"></a>

- **Implemented 2026-09-19; runtime stress pending.** Poll snapshots retain owned FDs; each arm has a generation. Installation/cancellation are acknowledged by the worker, and cancelled slots remain unavailable until the old poll and queued completion are retired.
- Normal completion is published after snapshot retirement. Unregister matches the client, surface, index, wait value and callback identity; a failed native unregister keeps its arm live.
- Deterministic regressions cover delayed snapshots, rearm, stale completion and worker failure. Hardware event latency, FD census and sustained cancellation/rearm still need measurement.
- This handshake establishes poller quiescence, not final RM/GPU backing release or lifetime of callbacks already queued in the guest.

### 78. Request rollback <a id="78-request-preparation-needs-rollback"></a>

- **Implemented for the reviewed paths, 2026-09-19.** `request_shape.rs` validates host-known envelopes, pointer spans and FD metadata before event acquisition. Event/waiter acquisition follows buffer translation and validation.
- Fake-driver regressions reject missing, short, overlapping and misplaced descriptors before driver execution while preserving NULL query and sparse nested-buffer forms.
- Private UVM/RM pool owners now record acquisitions and roll back in reverse dependency order; failed cleanup retains the complete owner and charge for retry.
- Unannotated fields and native aliases remain separate work in 80–81; this is not proof of every driver operation's lifetime.

### 79. GPU-FD association across clients <a id="79-os-descriptor-gpu-fd-is-cached-across-client-ofds"></a>

- **Open, source review 2026-09-18.** A session caches one GPU FD registered against the first control FD, although one guest process can own multiple RM clients/control OFDs.
- Confirm the supported association with vendor source and a two-client hardware test. If association is per control OFD, key the cache by that owned identity and release it with the client.
- This is an ownership concern from source review, not a demonstrated GPU failure.

### 80. Native aliases and backing release <a id="80-source-teardown-does-not-release-every-backing-reference"></a>

- **Open, source review 2026-09-19.** RM DUP shares a memory descriptor; exported/imported objects can survive source-client destruction. FD close can defer native client cleanup.
- The host retains forwarded OS-descriptor arenas after source release. The guest quarantines uncertain submitted allocations and failed teardown, but ordinary successful frees still lack a complete alias-release protocol.
- Track source, duplicate, import, export-FD and implicit UVM ownership under one backing identity. A software count reaching zero needs a verified native release fence before guest pages become reusable.
- Test source/export/destination teardown in different orders, delayed replies and forced guest page reuse. A successful FREE/CLOSE/PROC_GONE or poller acknowledgement is insufficient evidence.

### 81. Unchecked pointer and FD layouts <a id="81-known-request-shapes-do-not-cover-every-driver-pointer-or-fd"></a>

- **Partly implemented, 2026-09-19.** Host-derived validation rejects missing translation metadata. The host additionally refuses 28 known embedded-pointer controls, four untranslated Unix FD-input controls, seven capability-FD classes and serialized RM layouts.
- The two existing attribution controls remain blocked. The [current policy](SECURITY.md#refused-operations) and `xlate::blocked_ctrls` identify the supported boundary.
- Unknown control/class internals still need a broader audit or strict operation allowlist. The vendor-source review is not four complete driver audits; numeric header agreement is weaker evidence.
- Restore refused operations only with translation/bounds tests and workloads that use them. Debugging, P2P/MIG, profiling and uncommon display/encode compatibility remain to be measured on this batch.

### 82. Live device removal <a id="82-device-references-do-not-make-live-unbind-safe"></a>

- **Open, partly implemented 2026-09-19.** Contexts, requests, windows and pools retain device storage. Queue stop rejects new submissions, synchronizes callbacks and drains detached requests/work.
- Monotonic process IDs survive rebind within one module lifetime. Reload resets them and requires a fresh backend.
- Device references do not revoke mapped SHMEM BARs or drain whole NVKMS operations. An in-progress mapping can outlive removal's mapping-list drain; live unbind/rebind remains unsupported.
- Uncertain backing retains module references and quota until guest restart. Add delayed-completion/reset/failed-close tests with KASAN and lock debugging before strengthening teardown claims.

### 83. Retained pin-budget growth <a id="83-conservative-pin-retention-can-exhaust-the-vm-budget"></a>

- **Implemented accounting; capacity validation open, 2026-09-19.** The shared `PinBudget` charges page-rounded pending, active, retained and quarantined registrations. Defaults are 1024 MiB per VM and 256 MiB per arena.
- Source free does not refund forwarded registrations while aliases are unknown. Private cleanup failures retain charges and retryable owners; failed final cleanup can retain them until backend exit.
- This bounds registration admission, not unique physical pages. The aggregate default is provisional; repeated successful allocation/free can reach it.
- Measure retained/quarantined bytes under repeated CUDA, display and PRIME workloads. If normal use exhausts the cap, implement the reference ledger rather than treating a larger limit as reclamation.

## Resolved and decided

### 1. Descriptor-table compatibility <a id="1-does-the-descriptor-table-warrant-a-protocol-change"></a>

- **Decided.** Additive descriptor kinds do not alone require a protocol bump; retain compatibility checks. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#1-does-the-descriptor-table-warrant-a-protocol-change).

### 2. NVIDIA proc-interface ownership <a id="2-who-owns-procdrivernvidia-when-both-modules-are-loaded"></a>

- **Decided.** Keep one owner for the NVIDIA proc interface when both modules are loaded. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#2-who-owns-procdrivernvidia-when-both-modules-are-loaded).

### 3. Project and crate names <a id="3-project-name-and-the-nvshim--crate-prefix"></a>

- **Decided.** Use Leandro and the `nvrm` crate names. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#3-project-name-and-the-nvshim--crate-prefix).

### 4. Session scope <a id="4-one-host-session-per-vm-or-per-guest-process"></a>

- **Decided.** Sessions follow guest processes; pool virtual addresses can repeat across processes. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#4-one-host-session-per-vm-or-per-guest-process).

### 5. Request preparation and execution <a id="5-where-does-the-seam-run-in-sessionrs"></a>

- **Decided.** Keep request preparation and syscall execution distinct; 78 records the new validation-before-acquisition boundary. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#5-where-does-the-seam-run-in-sessionrs).

### 6. Guest FD translation <a id="6-how-does-the-kernel-path-resolve-a-guest-file-descriptor"></a>

- **Resolved.** Translate guest file descriptors through tokens owned by the appropriate guest session. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#6-how-does-the-kernel-path-resolve-a-guest-file-descriptor).

### 6a. Original FD question <a id="6a-the-same-question-as-first-written"></a>

- **Superseded by 6.** Preserved original wording; this number remains permanent. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#6a-the-same-question-as-first-written).

### 7. Vblank callback translation <a id="7-a-vblank-callback-is-a-guest-kernel-function-pointer"></a>

- **Resolved.** Replace guest kernel callbacks with host event IDs and return notifications through the event path. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#7-a-vblank-callback-is-a-guest-kernel-function-pointer).

### 8. GLX FBConfig ID zero <a id="8-glx-clients-get-fbconfig-id-0"></a>

- **Superseded by 9.** The observed GL failure followed a dead shared-memory channel. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#8-glx-clients-get-fbconfig-id-0).

### 9. NVKMS failure after X-server exit and reload <a id="9-nvkms-wedges-after-a-killed-x-server-plus-a-module-reload"></a>

- **Resolved in archived runs.** The killed-X-server/reload investigation includes the evidence and recovery sequence. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#9-nvkms-wedges-after-a-killed-x-server-plus-a-module-reload).

### 10. Black compositor windows <a id="10-black-windows-under-the-compositor-then-an-assert"></a>

- **Resolved.** The archived compositor failure links to its downstream crash investigation. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#10-black-windows-under-the-compositor-then-an-assert).

### 11. Game-specific Vulkan failure <a id="11-cs2-gets-no-vulkan-while-vkcube-runs-beside-it"></a>

- **Resolved.** The game-specific Vulkan path and its counter-tests are recorded. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#11-cs2-gets-no-vulkan-while-vkcube-runs-beside-it).

### 12. VRAM allocation accounting <a id="12-the-vram-cap-counted-one-door-and-the-card-has-several"></a>

- **Resolved.** Account for all supported allocation paths. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#12-the-vram-cap-counted-one-door-and-the-card-has-several).

### 13. Probe licenses <a id="13-two-directories-of-probes-two-licences"></a>

- **Decided.** Keep the probe/source license boundary explicit. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#13-two-directories-of-probes-two-licences).

### 17. Black Sunshine capture under Wayland <a id="17-sunshine-capture--kms-under-wayland-shows-a-black-stream"></a>

- **Resolved in archived runs.** Framebuffer content survived; the black stream investigation identified compositor behavior. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#17-sunshine-capture--kms-under-wayland-shows-a-black-stream).

### 18. Excess flip completions <a id="18-flip-completions-arrive-in-excess"></a>

- **Resolved in archived runs.** The two NVIDIA completion counters measure different things. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#18-flip-completions-arrive-in-excess).

### 19. Physical-page query permissions <a id="19-get_surface_phys_pages-is-refused-with-insufficient_permissions"></a>

- **Resolved.** RM privileges explain the physical-page query refusal in the userspace backend. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#19-get_surface_phys_pages-is-refused-with-insufficient_permissions).

### 20. Empty game window <a id="20-cs2-renders-but-its-window-stays-empty"></a>

- **Superseded by 22.** The EGLImage import investigation carries the remaining diagnosis. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#20-cs2-renders-but-its-window-stays-empty).

### 21. Control-FD and RM-client leaks <a id="21-the-backend-leaked-devnvidiactl-descriptors-and-rm-clients"></a>

- **Fixed in archived runs.** Control-FD and RM-client cleanup was measured; later growth is tracked separately in 31. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#21-the-backend-leaked-devnvidiactl-descriptors-and-rm-clients).

### 22. Xwayland EGLImage import failure <a id="22-gl_out_of_memory-on-eglimage-import-under-xwayland"></a>

- **Resolved in archived runs.** EGLImage import required cross-process FD ownership translation. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#22-gl_out_of_memory-on-eglimage-import-under-xwayland).

### 23. Guest GLX crashes <a id="23-glx-clients-segfault-in-the-guest"></a>

- **Resolved in archived runs.** The GLX crash depended on shared graphics-stack state. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#23-glx-clients-segfault-in-the-guest).

### 24. X11/Wayland comparison <a id="24-the-x11-counter-test-all-three-defects-hang-on-the-wayland-path"></a>

- **Counter-test recorded.** The same symptoms were compared through X11 and Wayland paths. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#24-the-x11-counter-test-all-three-defects-hang-on-the-wayland-path).

### 25. Displayless HAL and EVO privileges <a id="25-the-displayless-hal-is-forced-and-the-evo-path-is-a-privilege-question"></a>

- **Decided.** The virtual display uses the displayless HAL; the hardware EVO path has additional privilege constraints. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#25-the-displayless-hal-is-forced-and-the-evo-path-is-a-privilege-question).

### 26. EGLImage import counter-test <a id="26-the-eglimage-import-itself-is-clean"></a>

- **Negative result.** The EGLImage import itself was clean in the recorded comparison. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#26-the-eglimage-import-itself-is-clean).

### 28. Missing 32-bit GLX registration <a id="28-32-bit-clients-could-not-find-nvidias-glx"></a>

- **Fixed.** Register the staged 32-bit NVIDIA GLX libraries as well as the 64-bit ones. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#28-32-bit-clients-could-not-find-nvidias-glx).

### 29. Xwayland ownership and crash state <a id="29-the-crash-depends-on-whose-xwayland-instance-it-is"></a>

- **Resolved in archived runs.** The Xwayland owner and instance state changed the observed crash behavior. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#29-the-crash-depends-on-whose-xwayland-instance-it-is).

### 30. FPS versus presentation <a id="30-a-clients-fps-counter-is-not-evidence-of-presentation"></a>

- **Measurement rule.** A client swap counter does not establish visible presentation. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#30-a-clients-fps-counter-is-not-evidence-of-presentation).

### 32. NVIDIA EGL-core SIGFPE <a id="32-xwayland-dies-on-sigfpe-inside-nvidias-egl-core"></a>

- **Resolved in archived runs.** The EGL-core SIGFPE investigation and its eliminated causes are preserved. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#32-xwayland-dies-on-sigfpe-inside-nvidias-egl-core).

### 33. State-dependent GLX null dereference <a id="33-the-crash-in-23-is-a-null-pointer-and-depends-on-state"></a>

- **Resolved in archived runs.** The GLX fault was a null pointer and depended on process state. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#33-the-crash-in-23-is-a-null-pointer-and-depends-on-state).

### 34. Per-ioctl driver logging <a id="34-the-host-writes-one-dmesg-line-per-ioctl"></a>

- **Known trap.** Driver logging can occur on every ioctl and distort both timing and diagnostic history. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#34-the-host-writes-one-dmesg-line-per-ioctl).

### 35. EGLImage failure chain <a id="35-the-eglimage-import-failure-is-the-head-of-the-chain"></a>

- **Resolved in archived runs.** The EGLImage failure was upstream of the later crash chain. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#35-the-eglimage-import-failure-is-the-head-of-the-chain).

### 36. Environment and process-matching errors <a id="36-two-traps-of-our-own-making"></a>

- **Known traps.** Empty environment variables and process-name matching caused misleading results. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#36-two-traps-of-our-own-making).

### 37. Frame-limiter coverage <a id="37-the-frame-limiter-reaches-only-some-clients"></a>

- **Corrected by 41.** The later rate sweep corrected the original limiter conclusion. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#37-the-frame-limiter-reaches-only-some-clients).

### 38. Frame-limiter double free <a id="38-the-limiter-triggered-a-double-free-in-the-guest"></a>

- **Resolved in archived runs.** The frame limiter exposed a guest lifetime bug; its fix and scope are recorded. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#38-the-limiter-triggered-a-double-free-in-the-guest).

### 39. Rejected zero-divisor hypotheses <a id="39-three-more-candidates-for-the-zero-divisor-are-cleared"></a>

- **Negative result.** Three control answers were ruled out as the zero-divisor source. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#39-three-more-candidates-for-the-zero-divisor-are-cleared).

### 40. Late-unregister race <a id="40-the-late-unregister-race-was-the-cause"></a>

- **Resolved in archived runs.** The measured late-unregister defect was fixed. The host poller lifetime issue in 77 is a separate finding. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#40-the-late-unregister-race-was-the-cause).

### 41. Corrected frame-limiter measurements <a id="41-correction-to-37-the-configured-rate-holds-across-the-range"></a>

- **Correction recorded.** The limiter held across the measured rate range; client paths still differed. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#41-correction-to-37-the-configured-rate-holds-across-the-range).

### 42. Capability-descriptor FD translation <a id="42-is-capdescriptor-on-0xc640-really-an-fd-that-needs-no-translation"></a>

- **Resolved.** Treat capDescriptor as an FD and apply the required ownership translation. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#42-is-capdescriptor-on-0xc640-really-an-fd-that-needs-no-translation).

### 43. Mapping ioctl FD ownership <a id="43-which-fd-does-the-driver-require-the-mapping-ioctl-on"></a>

- **Resolved.** The mapping code used the right FD; the original explanation was wrong. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#43-which-fd-does-the-driver-require-the-mapping-ioctl-on).

### 44. Shared NVIDIA GL-core crash sites <a id="44-a-game-and-the-compositor-die-at-the-same-two-addresses-in-nvidias-gl-core"></a>

- **Resolved in archived runs.** The archive records the shared GL-core crash addresses and failure chain. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#44-a-game-and-the-compositor-die-at-the-same-two-addresses-in-nvidias-gl-core).

### 45. Interrupted GPU attach ioctl <a id="45-nv_esc_attach_gpus_to_fd-answers--1-to-xwayland"></a>

- **Resolved in archived runs.** An interrupted attach ioctl required the recorded retry/cleanup behavior. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#45-nv_esc_attach_gpus_to_fd-answers--1-to-xwayland).

### 47. Probe counting errors <a id="47-two-counting-rules-in-our-own-instruments-are-wrong"></a>

- **Fixed.** Correct the instrument counting rules before interpreting coverage numbers. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#47-two-counting-rules-in-our-own-instruments-are-wrong).

### 48. NVKMS trace decoding <a id="48-userspace-talks-to-devnvidia-modeset-and-the-tracer-cannot-see-it"></a>

- **Resolved.** Decode the NVIDIA modeset interface separately from DRM ioctls. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#48-userspace-talks-to-devnvidia-modeset-and-the-tracer-cannot-see-it).

### 49. Deprecated embedded pointer <a id="49-a-deprecated-control-is-forwarded-verbatim-with-a-pointer-inside-it"></a>

- **Resolved.** Vendor source showed the deprecated embedded pointer was not read by RM. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#49-a-deprecated-control-is-forwarded-verbatim-with-a-pointer-inside-it).

### 50. Response-byte verification <a id="50-nothing-compares-the-answer-bytes-so-nothing-is-verified"></a>

- **Resolved.** Compare returned bytes; reaching an ioctl alone is not verification. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#50-nothing-compares-the-answer-bytes-so-nothing-is-verified).

### 51. NVML response layouts <a id="51-a-guest-answers-two-nvml-controls-differently-and-both-shapes-were-predicted"></a>

- **Fixed in archived runs.** Two NVML response shapes were corrected; the remaining SMC case is 65. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#51-a-guest-answers-two-nvml-controls-differently-and-both-shapes-were-predicted).

### 52. Native/guest graphics queries <a id="52-the-guests-graphics-stack-asks-a-different-set-of-questions"></a>

- **Corrected and resolved.** Native/guest graphics queries were compared with controlled X connections. The later DRM correction is part of the evidence. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#52-the-guests-graphics-stack-asks-a-different-set-of-questions).

### 53. OpenCL and EGL registration <a id="53-two-libraries-are-staged-into-the-guest-and-registered-with-nobody"></a>

- **Fixed with counter-tests.** Register the OpenCL ICD. The proposed EGL explanation did not survive the comparison. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#53-two-libraries-are-staged-into-the-guest-and-registered-with-nobody).

### 54. Mediated GPU names in probes <a id="54-two-probe-criteria-cannot-pass-in-a-guest-because-the-guest-renames-the-card"></a>

- **Fixed.** Probe criteria must account for the mediated GPU name. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#54-two-probe-criteria-cannot-pass-in-a-guest-because-the-guest-renames-the-card).

### 55. Verified results versus reached classes <a id="55-the-answer-bytes-are-compared-now-and-the-class-that-can-be-promoted-is-not-the-class-that-can-be-reached"></a>

- **Resolved.** Distinguish compared answer bytes from merely reached classes. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#55-the-answer-bytes-are-compared-now-and-the-class-that-can-be-promoted-is-not-the-class-that-can-be-reached).

### 57. Unused Vulkan implicit layer <a id="57-a-vendor-manifest-is-registered-nowhere-and-a-knob-depends-on-it"></a>

- **Resolved.** The Vulkan implicit layer did not affect the tested path; the inert knobs were removed. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#57-a-vendor-manifest-is-registered-nowhere-and-a-knob-depends-on-it).

### 58. xcb/xlib EGL comparison <a id="58-nvidias-xcb-egl-platform-declines-in-a-guest-and-its-xlib-platform-does-not"></a>

- **Resolved in archived runs.** The xcb/xlib EGL difference was compared under matching X configuration. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#58-nvidias-xcb-egl-platform-declines-in-a-guest-and-its-xlib-platform-does-not).

### 60. Unwritten output memory <a id="60-one-answer-survived-every-mask-and-it-was-memory-nobody-wrote"></a>

- **Resolved.** Unwritten output memory explained the apparent stable answer. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#60-one-answer-survived-every-mask-and-it-was-memory-nobody-wrote).

### 61. Native/guest control responses <a id="61-two-controls-are-answered-on-one-side-of-the-boundary-and-not-the-other"></a>

- **Corrected and resolved.** The initial source-based conclusion was corrected by runtime comparisons. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#61-two-controls-are-answered-on-one-side-of-the-boundary-and-not-the-other).

### 62. CARDINFO mediation metadata <a id="62-an-escape-the-guest-module-rewrites-is-not-in-the-descriptor-table"></a>

- **Resolved.** Classify CARDINFO mediation explicitly rather than implying the descriptor table covers it. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#62-an-escape-the-guest-module-rewrites-is-not-in-the-descriptor-table).

### 63. Host-assigned response values <a id="63-two-answers-behind-an-nvp64-differ-and-both-look-like-the-hardware-saying-so"></a>

- **Resolved.** Separate host-assigned answers from deterministic values in response comparisons. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#63-two-answers-behind-an-nvp64-differ-and-both-look-like-the-hardware-saying-so).

### 64. NVKMS command names <a id="64-the-nvkms-commands-are-recorded-and-none-of-them-has-a-name"></a>

- **Resolved.** The NVKMS decoder supplied command names and matched the recorded traces. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#64-the-nvkms-commands-are-recorded-and-none-of-them-has-a-name).

### 65. NVML SMC-monitor comparison <a id="65-nvml-allocates-an-smc-monitor-session-natively-and-never-in-a-guest"></a>

- **Resolved in archived runs.** The native/guest SMC-monitor comparison falsified the remaining hypothesis. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#65-nvml-allocates-an-smc-monitor-session-natively-and-never-in-a-guest).

### 66. Native/native/guest stability controls <a id="66-the-control-test-proves-stability-on-one-side-of-the-boundary-only"></a>

- **Resolved.** Use native/native/guest comparisons to separate instability from boundary differences. [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#66-the-control-test-proves-stability-on-one-side-of-the-boundary-only).

### 73. ABI type renames <a id="73-a-renamed-type-with-an-identical-body-is-indistinguishable-from-a-removal"></a>

- **Implemented in current source.** The ABI configuration records renames and the generator normalizes and checks them. The historical entry predates that implementation. See [abi.toml](../crates/nvrm-sys/abi.toml) and the [generator](../crates/xtask/src/abi/mod.rs). [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#73-a-renamed-type-with-an-identical-body-is-indistinguishable-from-a-removal).

### 75. Mediated ABI footprint <a id="75-every-version-pair-is-breaking-and-the-word-has-stopped-carrying-information"></a>

- **Implemented in current source.** The ABI configuration now separates the mediated core, with a workspace usage check for volatile names. The historical entry predates that implementation. See [abi.toml](../crates/nvrm-sys/abi.toml) and the [generator](../crates/xtask/src/abi/mod.rs). [Evidence](history/OPEN-QUESTIONS-2026-09-18.md#75-every-version-pair-is-breaking-and-the-word-has-stopped-carrying-information).

<!-- Compatibility anchors for historical subsections. Evidence remains in the snapshot. -->
<a id="2026-08-21-the-same-symptom-a-different-cause-and-a-discriminator-that-works"></a>
- [Freeze discriminator (2026-08-21)](history/OPEN-QUESTIONS-2026-09-18.md#2026-08-21-the-same-symptom-a-different-cause-and-a-discriminator-that-works).
<a id="2026-08-21-second-run-the-freeze-latches-it-does-not-recover-when-the-memory-does"></a>
- [Freeze persists after memory recovery](history/OPEN-QUESTIONS-2026-09-18.md#2026-08-21-second-run-the-freeze-latches-it-does-not-recover-when-the-memory-does).
<a id="resolved-2026-08-21-two-counters-that-disagree-both-of-them-nvidias"></a>
- [Flip-completion counters](history/OPEN-QUESTIONS-2026-09-18.md#resolved-2026-08-21-two-counters-that-disagree-both-of-them-nvidias).
<a id="resolved-2026-08-21-it-is-rms-privilege-model-and-this-backend-is-in-userspace"></a>
- [RM physical-page permissions](history/OPEN-QUESTIONS-2026-09-18.md#resolved-2026-08-21-it-is-rms-privilege-model-and-this-backend-is-in-userspace).
<a id="resolved-2026-08-21-one-rig-one-variable-and-all-six-come-back"></a>
- [Controlled rig comparison](history/OPEN-QUESTIONS-2026-09-18.md#resolved-2026-08-21-one-rig-one-variable-and-all-six-come-back).
<a id="correction-2026-08-21-same-day-the-drm-claim-in-this-entry-is-wrong"></a>
- [Corrected DRM claim](history/OPEN-QUESTIONS-2026-09-18.md#correction-2026-08-21-same-day-the-drm-claim-in-this-entry-is-wrong).
<a id="resolved-2026-08-21-probepythonnvkmsdecodepy-and-14-of-14-agree"></a>
- [NVKMS trace decoding](history/OPEN-QUESTIONS-2026-09-18.md#resolved-2026-08-21-probepythonnvkmsdecodepy-and-14-of-14-agree).
<a id="resolved-2026-08-21-both-halves-measured-and-the-surviving-hypothesis-is-falsified"></a>
- [SMC-monitor hypothesis rejected](history/OPEN-QUESTIONS-2026-09-18.md#resolved-2026-08-21-both-halves-measured-and-the-surviving-hypothesis-is-falsified).
