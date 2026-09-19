<!-- SPDX-License-Identifier: MIT -->
# nvrm-abi

- Describes RM/UVM ioctl payloads: sizes, embedded pointers, FD fields and encoding.
- Provides device wrappers and curated types checked against generated bindings.
- Direct-call tools use [nvrm-client](../nvrm-client/README.md) for RM ownership.
  The forwarding backend maintains its own ownership records.

## Source map

| File | Responsibility |
|---|---|
| `src/lib.rs` | Ioctl encoding, wrappers and device FDs |
| `src/xlate.rs` | Per-call translation rules |
| `src/nvgpu.rs` | Curated ABI types and layout assertions |
| `src/table.rs` | Descriptor stream built from translation rules |
| `src/xfer.rs` | `NV_ESC_IOCTL_XFER_CMD` wrapping |
| `src/share.rs` | RM object-sharing grants |
| `src/mediate.rs` | Shared offsets/constants for rewritten controls |
| `src/vgpu.rs` | Card-derived profile calculations |
| `src/bin/nvrm-genhdr.rs` | Guest C header and table generator |

## Editing rules

- Change translation knowledge in `xlate.rs`; `table.rs` queries it instead of
  maintaining a second list. Startup verification checks agreement.
- The guest interprets descriptor tables, but also has NVIDIA-specific UVM,
  display and DRM paths. Their generated constants still need version review.
- Keep ABI size, alignment and field-offset assertions when changing curated types.
- Regenerate the guest header after changing wire definitions or exported constants.

```sh
cargo run --bin nvrm-genhdr -- guest-module/virtio_nvrm/nvrm_wire.h
cargo run --bin nvrm-genhdr -- --check
tools/check.sh
```

- The software check exercises the C table interpreter against Rust-generated data.
- Version policy and generator inputs: [driver versions](../../docs/abi-versions.md).
