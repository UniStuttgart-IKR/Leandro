<!-- SPDX-License-Identifier: MIT -->
# nvrm-wire

- Wire schema shared by the host backend and generated guest C header.
- Little-endian encoding; `Req` is 160 bytes, `Rsp` is 32 bytes. Byte views use native
  representation and require little-endian hosts and guests.
- `MAX_PAYLOAD` is 16384 bytes. The crate supports `no_std`.
- Host-issued tokens identify open devices; guest FD numbers are local to the guest.
- Tokens are scoped to guest-process sessions. Explicit FD-owner fields support
  cross-process imports within one VM; guest process IDs are untrusted metadata.

## Source map

| File | Responsibility |
|---|---|
| `src/lib.rs` | Requests, replies, message kinds and protocol version |
| `src/tables.rs` | Descriptor-table records and bounds |

## Compatibility

- `PROTO_VERSION` is **6**.
- Bump it when the meaning of an accepted request changes, even when struct sizes
  stay unchanged. Versions 5 and 6 used existing padding for new routing fields.
- Additive message kinds have historically kept the version; review unknown-kind
  handling and feature negotiation before applying that rule to a new message.
- Descriptor headers carry lengths, counts and a checksum. Validate them before
  interpreting records.
- Generate the C schema; do not edit its declarations by hand:

```sh
cargo run --bin nvrm-genhdr -- guest-module/virtio_nvrm/nvrm_wire.h
cargo run --bin nvrm-genhdr -- --check
```

- C static assertions check compiled struct layouts. The generator check detects
  a checked-in header that no longer matches the Rust definitions.
