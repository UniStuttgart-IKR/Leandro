<!-- SPDX-License-Identifier: MIT -->
# Security boundaries

## Supported use

- Research workloads with trusted guests and operators.
- Hostile-guest containment has not been established; passing functional tests is not an isolation proof.
- One backend process serves one VM. NVIDIA's host driver, GPU firmware, and physical GPU remain shared.
- A guest may control every request field, process ID, token, handle, address, and length.
- Guest process separation is enforced by the guest kernel. The host policy boundary is the VM.

## Existing checks

- Request sizes, translated pointer spans, descriptor metadata, and memory ranges are validated before their use.
- Mirrored tokens resolve to backend-owned FDs; explicit cross-process owners must resolve in the named session.
- Tokens record the opened device type. A request cannot reinterpret a control FD as UVM or a GPU FD.
- Host ABI descriptors require known envelopes and translation metadata; UVM tools requests are unsupported.
- Pure request validation runs before event acquisition. It checks exact known pointer/FD offsets, nested spans, negative FD sentinels and OS-descriptor GPA forms; unused zero-length pointers are cleared.
- OS-descriptor arenas and allocation records include the RM client root in their keys.
- Failed RM frees retain bookkeeping; failed VMM unmaps retain window slots and FDs.
- A shared VM pin budget charges page-rounded pending, active, retained and quarantined registrations. The provisional default is 1 GiB (`LEA_MAX_PIN_TOTAL_MIB`); the per-arena limit remains 256 MiB (`LEA_MAX_PIN_MIB`). This is not a count of unique physical pages.
- Pool owners release UVM mappings before RM allocations. Failed cleanup retains the owner and its budget charge for retry.
- Forwarded OS-descriptor arenas remain charged after source-object release because duplicate/exported references may survive.
- Private pool clients are excluded from supported guest copy/import and client operations.
- Poll snapshots own their FDs. Cancellation waits for the worker to retire snapshots before event slots can be reused.
- Guest process IDs do not repeat during one module lifetime. Uncertain submitted requests retain their backing pages and module references.
- Open guest contexts, requests, windows, and pools hold device references. Removal stops submissions and drains transport requests.
- Automatic object duplication grants use `RS_SHARE_TYPE_PID`, restricting those grants to the backend process rather than its host UID.
- `CAP_SYS_ADMIN` is dropped from effective, permitted, and inheritable sets unless `LEA_ADMIN_PRIV=1`. Inspection or drop failure aborts startup.
- These checks cover specific boundaries. They do not establish complete validation of the NVIDIA ioctl surface.

## Refused operations

- The host blocks 34 RM controls: two existing attribution controls, four additional untranslated Unix FD-input controls, and 28 additional controls with unhandled embedded pointers.
- The authoritative list is `blocked_ctrls()` in [xlate.rs](../crates/nvrm-abi/src/xlate.rs). Generated guest tables advertise the same control policy; the host enforces it independently.
- Capability-FD classes `c637`, `c638`, `c639`, `c640`, `b0cd`, `b0ce` and `cdcd` are refused, including NULL-parameter allocations. Guest FD virtualization for their capability descriptors is absent.
- Serialized RM_CONTROL/RM_ALLOC payloads, non-NULL rights pointers, unsupported VID_HEAP pointer/callback forms, unknown frontend envelopes and UVM tools requests are refused.
- Debugging, P2P, MIG, profiling and uncommon display/encode paths may encounter these refusals. Restore an operation only after translation, bounds tests and relevant workload validation.
- This inventory comes from the vendored driver source. Header checks across four ABIs do not prove that all four driver implementations have identical pointer behavior.

## Remaining risks

- **Foreign RM handles:** driver DUP looks up source clients globally. The backend still lacks a VM-wide source-object ownership policy across DUP, sharing, and UVM imports. PID-scoped automatic grants reduce exposure but do not revoke wider policies set through other paths.
- **Unannotated fields:** blocked operations cover known hazards; other nested RM control/class pointers, FDs or client handles remain unaudited. A broader audit or strict operation allowlist is still needed. Private-client guards cover reviewed layouts only.
- **Normal backing-page release:** successful source FREE/CLOSE or pool unmap does not prove that native duplicate/exported references are gone. Guest-page reuse needs a complete reference ledger and release protocol.
- **Device removal:** device references do not preserve a removed shared-memory BAR. In-flight NVKMS mapping operations still need a teardown barrier; live unbind/rebind remains unsupported.
- **Identity reset:** reloading the guest module restarts process IDs. Start a fresh backend with it; reconnecting to retained backend state is unsupported.
- **Quarantine availability:** retained arenas consume the VM budget until teardown. Failed private-owner cleanup can retain resources until backend exit. This favors retention over unsafe reuse, but can exhaust the budget.
- **Guest quarantine:** uncertain submitted allocations retain guest backing and its quota/module references until guest restart. Ordinary successful frees are not covered by a complete native-reference ledger.
- **Shared GPU availability:** per-VM VRAM accounting does not bound every internal allocation, guarantee residency, schedule engines, or prevent device-wide faults.
- **Mapping acknowledgements:** SHMEM lifetime tracking requires `REPLY_ACK`; the supported hypervisor negotiates it, but the backend does not enforce that feature selection.
- **Backend privileges:** the current capability drop is not a complete sandbox. Other capabilities, host UID permissions, devices, and process limits depend on deployment.
- These are code-review findings. Lifetime races and cross-VM exploitability require targeted runtime tests; no live exploit is claimed here.

## Operating constraints

- Use dedicated test hardware or workloads where a GPU reset/driver failure is acceptable.
- Do not expose this backend to mutually untrusted tenants.
- Keep `LEA_ADMIN_PRIV` unset for ordinary runs.
- Stop guest applications, then the VM, then its backend. Do not live-unbind an in-use device.
- Guest quarantine deliberately prevents module unload. Uncertain cleanup requires guest restart; leftover fileless kernel backing may require device unbind after all users stop.
- Keep sockets and instance directories accessible only to their intended operator/VMM.
- Separate host identities and OS resource limits are additional defenses; they are not substitutes for source-object ownership validation.
- Run the exact driver/library pair validated for the workload.

## Validation needed before stronger claims

- Cross-backend object-import attempts using two processes under one UID, plus a same-backend positive control.
- Delayed host completion while killing a guest caller and forcing guest page reuse.
- Open/mmap/unbind/close tests under a guest kernel with KASAN and lock debugging.
- Hardware validation of FD close/reuse/rearm and failed allocation cleanup; deterministic worker and fake-driver regressions cover these paths without a GPU.
- Repeated allocation/free workloads to measure retained-budget growth and confirm acceptable limits.
- Workloads using refused controls; event/frame latency and FD counts after waiter installation/cancellation.
- Sustained multi-VM stress with driver logs, accounting, and host/guest memory diagnostics.
- Software checks stay in core. Leandro-Test provides optional automated hardware gates.
- Recorded compute, virtual-display and desktop passes establish functionality for those runs, not isolation.
- Test commands and coverage: [TESTING.md](TESTING.md).
