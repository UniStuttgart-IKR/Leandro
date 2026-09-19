<!-- SPDX-License-Identifier: MIT -->
# Review and maintenance guide

- Review one boundary per session. Start with the request path below; follow display and performance work later.
- Before changing code, write down its input, resource owner, success condition and failure cleanup.
- Prepare the pinned toolchain and vendor headers using [DEVELOPMENT.md](../DEVELOPMENT.md). Commands below run from core unless stated otherwise.
- For a first session, follow `prepare_builds_a_forward_plan_with_the_ioc_request` in the host tests, then find the matching guest request construction. Explain that round trip before editing it.

## 1. Guest transport

- Read [virtio_nvrm.c](../guest-module/virtio_nvrm/virtio_nvrm.c): `nvrm_node_ioctl`, request construction, `nvrm_xfer_run`, completion handling, then `nvrm_remove`.
- Track the `nvrm_xfer` owner across enqueue, completion and timeout. The pointer-to-pointer submission API can transfer ownership away from the caller.
- Separate userspace buffers, pinned guest pages and virtqueue buffers; completion of one operation does not automatically release every backing reference.
- Read [nvrm_tables.c](../guest-module/virtio_nvrm/nvrm_tables.c) beside [tabreject.c](../guest-module/virtio_nvrm/test/tabreject.c). Check malformed lengths and missing descriptors before the happy path.
- Run `./scripts/ci/check-c.sh tables --sanitize`. These helper tests do not exercise kernel scheduling or device removal.

## 2. Host routing and request validation

- Read [nvrm.rs](../crates/vhost-user-nvrm/src/nvrm.rs) for VM/session routing, then [session.rs](../crates/vhost-user-nvrm/src/session.rs): `handle_msg_with`, `prepare`, `Plan`, `execute`.
- Read [request_shape.rs](../crates/vhost-user-nvrm/src/request_shape.rs) for host-derived pointer/FD checks and [client_policy.rs](../crates/vhost-user-nvrm/src/client_policy.rs) for private-client exclusions.
- Follow a token through [mirror.rs](../crates/vhost-user-nvrm/src/mirror.rs). Its device tag and owning session matter; the token is not a native FD or an authenticated process identity.
- Distinguish syscall failure from RM's embedded status. Successful transport or `ioctl` return alone cannot justify removing resource bookkeeping.
- Read the rejection tests and [syscalls.rs](../crates/vhost-user-nvrm/src/syscalls.rs) before adding another mocked operation.

```sh
cargo test --locked -p vhost-user-nvrm --lib --all-features request_shape::tests
cargo test --locked -p vhost-user-nvrm --lib --all-features session::tests
```

## 3. Wire format and NVIDIA ABI

- Read [wire types](../crates/nvrm-wire/src/lib.rs) and [table format](../crates/nvrm-wire/src/tables.rs), then host [xlate.rs](../crates/nvrm-abi/src/xlate.rs) and [table.rs](../crates/nvrm-abi/src/table.rs).
- Keep three layers distinct: transport bytes, translation descriptors, and the selected NVIDIA driver's layouts.
- Follow one pointer descriptor from Rust generation to the guest C interpreter. Check offset, width, length source and NULL behavior on both sides.
- `Session<A: RmAbi>` uses a compiled ABI implementation selected at startup. A generic type parameter does not negotiate guest library compatibility.
- Change [abi.toml](../crates/nvrm-sys/abi.toml) through the [ABI workflow](abi-versions.md); regenerate bindings and `nvrm_wire.h` rather than editing generated output.
- Run `cargo test --locked -p nvrm-wire -p nvrm-abi --all-features` and `cargo xtask abi --check`.

## 4. Pool and waiter ownership

- Read [host_pool.rs](../crates/vhost-user-nvrm/src/host_pool.rs): `PinBudget`, `PinLease`, `Arena`, `Backing`, `PoolMap::cleanup`, then `PoolState`.
- Write the cleanup order down: UVM range, RM source object, then arena/charge. At each failed step, identify the owner retained for retry.
- Read [waiters.rs](../crates/vhost-user-nvrm/src/waiters.rs): `Watch`, `RegistrationId`, worker commands and acknowledgements; return to Session's pending/active/retired waiter states.
- Explain why an old poll snapshot or queued completion must not close or reuse a newly armed slot.
- Review [vram.rs](../crates/vhost-user-nvrm/src/vram.rs) separately: VRAM allocation accounting and guest-page registration accounting are different policies.

```sh
cargo test --locked -p vhost-user-nvrm --lib --all-features host_pool::tests
cargo test --locked -p vhost-user-nvrm --lib --all-features waiters::tests
```

## 5. Diagnostic client

- Read [RmClient](../crates/nvrm-client/src/lib.rs), [ObjectTree](../crates/nvrm-client/src/object.rs) and [memory owners](../crates/nvrm-client/src/mem.rs).
- Trace allocation, mapping and explicit free. Check how failed native cleanup leaves the object available for retry.
- This client supports diagnostics; it is not the VM's forwarding session. Do not infer host isolation from its local object tree.
- Run `cargo test --locked -p nvrm-client --all-features`.

## 6. Tooling and the review loop

- Core [check.sh](../scripts/check.sh) runs software checks; [CI](../.github/workflows/check.yml) adds formatting, sanitizer, kernel-build and Nix coverage.
- Core [tests/tools](../tests/tools/) owns the shared EDID/frame sources. [Leandro-Test](../../Leandro-Test/README.md) owns workloads, provisioning and hardware acceptance.
- After a focused change: run its tests, inspect the diff, then run `./scripts/check.sh`, both formatters and the relevant sanitizer checks in [TESTING.md](TESTING.md).
- For hardware validation, use the full Test runner below on a prepared idle rig. `lea gate` is a separate, narrower smoke check. Record both repository revisions and retain failures/skips.

```sh
cd ../Leandro-Test
./scripts/showcase.sh state --check
./lea acceptance gpu vdisplay display
```

## Rust ownership to notice here

- `OwnedFd` closes its descriptor when dropped; a copied `RawFd` is only a number and can become stale. See `Mirror` and waiter `Watch`.
- `Arc<T>` shares one owner's lifetime; cloning it does not clone the underlying FD. `Mutex`/atomics supply synchronization where needed; `Arc` alone does not.
- `Drop` provides scope-based cleanup, including early returns. It cannot return a cleanup error, so these owners also need explicit fallible cleanup and retained state.
- `Result` and the question-mark operator propagate failure; they do not undo driver side effects. Review acquired resources and rollback after every fallible step.
- `Option::take` moves a resource out and leaves `None`. Check that the receiving owner now covers it, especially in plans, retirement queues and quarantine.

## Implemented boundaries and remaining work

- Implemented with targeted regressions: request validation before acquisition, client-qualified OS-descriptor keys, failed-free bookkeeping retention and private-client guards.
- Implemented with targeted regressions: pool rollback/quarantine, shared registration admission, owned waiter snapshots, generation checks and cancellation acknowledgement.
- Deferred: a native backing-reference ledger, complete VM ownership of source handles, broader pointer/FD auditing, and safe live unbind. See [SECURITY.md](SECURITY.md#remaining-risks) and [questions 76–83](OPEN-QUESTIONS.md#76-rm-source-handles-need-a-vm-ownership-policy).
- Retained registrations can exhaust the provisional budget; raising it does not implement reclamation. Sustained event/FD and repeated-allocation behavior still needs measured evidence.
- Software regressions and functional gates cover exercised behavior. Neither establishes hostile-guest isolation or general driver compatibility.
