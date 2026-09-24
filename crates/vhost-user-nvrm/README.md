<!-- SPDX-License-Identifier: MIT -->
# vhost-user-nvrm

- Host backend for `virtio_nvrm.ko`, serving one VM per backend process.
- Validates and translates guest RM/UVM requests before invoking the host driver.
- Guest-process sessions own mirrored device FDs, mappings and resource tracking.
  An ioctl must retain the appropriate open file description because RM associates
  client state with it.
- Pool backing creates private host RM clients. The backend owns these resources
  and rewrites selected controls.

## Source map

| File | Responsibility |
|---|---|
| `src/main.rs` | Arguments and startup |
| `src/nvrm.rs` | Virtqueues, shared-memory window, sessions and event delivery |
| `src/session.rs` | Request validation, translation, execution and lifecycle bookkeeping |
| `src/request_shape.rs` | Pure host-ABI envelope, pointer-span and FD-metadata validation |
| `src/client_policy.rs` | Private pool-client guards for reviewed RM/UVM fields |
| `src/syscalls.rs` | Injectable RM operations for tests |
| `src/mirror.rs` | Host FD ownership behind guest tokens |
| `src/host_pool.rs` | Arena/pool owners, aggregate pin budget, cleanup retries and private clients |
| `src/vram.rs` | Shared VM ledger, memory policies and control-result rewriting |
| `src/waiters.rs` | Owned poll snapshots, acknowledged cancellation and generation-checked completions |
| `src/guest_words.rs` | Checked guest address/length helpers |
| `src/grid.rs` | Cached host card properties |
| `fuzz/` | Message-handler fuzz target and replay corpus |

## Resource and reporting limits

- `LEA_MAX_PIN_MIB` limits one host arena (default 256 MiB).
- `LEA_MAX_PIN_TOTAL_MIB` limits page-rounded registrations across all VM sessions
  (provisional default 1024 MiB). Pending, active, retained and quarantined arenas count.
- The aggregate budget counts registrations, not unique physical pages. Forwarded
  OS descriptors stay charged after source free because native aliases may survive;
  repeated allocation/free workloads can exhaust it.
- Private pool cleanup releases UVM, then RM, then the arena/charge. Failures retain
  the complete owner for retry; exceptional final failure retains it until backend exit.
- RM memory handles are scoped to a client. Session labels and FD tokens identify
  different resources and must not substitute for that namespace.
- Guest process queries are rewritten from this VM's ledger. Configured VRAM
  policies also rewrite reported capacity and selected card-name fields.
- The ledger counts explicit guest allocations, excluding RM's internal GPU
  overhead and managed-memory guest RAM. A reserved profile is an allowance,
  not a guarantee of total physical occupancy.
- `LEA_GPU_NAME_RAW=1` retains the driver's card name.
- The card is named `Leandro VirtIO <board>-<size>` by default (`nvrm_abi::naming`, since
  2026-09-24); `LEA_GPU_NAME_FORMAT=legacy` gives the earlier `Leandro <board>-<size>`. The
  v1 gates in `../Leandro-Test` accept either.
- Known request layouts are validated before resource acquisition. The host refuses
  34 untranslated/attribution controls, seven capability-FD classes, serialized RM
  layouts, unknown frontend envelopes and UVM tools. See [the policy](../../docs/SECURITY.md#refused-operations).
- Private-client guards and PID-scoped grants do not establish ownership of every
  guest-supplied native source handle. [Security limits](../../docs/SECURITY.md) also
  cover backing aliases, guest-page reuse and unsupported live device unbind.

## Run and test

```sh
vhost-user-nvrm --nvrm /path/to/socket
cargo test -p vhost-user-nvrm --lib
```

- Stop the guest before its backend; an active guest depends on backend replies.
- Software tests use fake RM calls and mapped test RAM. GPU behavior still needs
  the hardware gates.
- Suggested reading order: `guest_words`, `mirror`, `request_shape`, `client_policy`,
  `vram`, `host_pool`, `waiters`, then `session` and `nvrm`.
