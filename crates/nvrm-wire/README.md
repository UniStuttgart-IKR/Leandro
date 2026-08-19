<!-- SPDX-License-Identifier: MIT -->
# `nvrm-wire` — the protocol between guest module and host daemon

The schema both ends agree on: a fixed 160-byte request header (`Req`)
and a 32-byte reply header (`Rsp`) plus payload, little-endian,
`MAX_PAYLOAD = 16384` (= `NV_ABSOLUTE_MAX_IOCTL_SIZE`). `no_std`-capable,
because the guest side of the same definitions is compiled into a kernel
module. Routing is by host-issued token, never by guest fd number; the
crate docs (`src/lib.rs`) carry that argument and the one-carrier history
(virtio-nvrm only, since 2026-08-04).

| File | What it is |
|---|---|
| `src/lib.rs` | `Req`/`Rsp`, `Kind`, `PROTO_VERSION`, the payload rules |
| `src/tables.rs` | the descriptor-table format the guest module interprets |

## `PROTO_VERSION`

Currently **6**. The rule that decides a bump is not "did the layout
change" but **"did the meaning of a request the host already accepts
change"** — a moved offset is caught by a size check, a changed meaning is
not. Both bumps so far (4 → 5, 5 → 6) added a word into an existing
padding hole and left `Req` at 160 bytes; they were bumped anyway, for
that reason. Purely additive `Kind`s do not bump.

The C side of these structs is **generated**, never written:
`cargo run --bin nvrm-genhdr -- guest-module/virtio_nvrm/nvrm_wire.h`.
Every offset there carries a `_Static_assert`, so the module cannot be
built against a stale layout.

## The descriptor-table stream

`tables.rs` describes what the guest module receives: all `u32`,
little-endian, naturally aligned, with lengths and counts in the header so
the interpreter can bounds-check **before** every access rather than after.
