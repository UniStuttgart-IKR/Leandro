<!-- SPDX-License-Identifier: MIT -->
# `nvrm-abi` — the ioctl level

What has to be known per `(device, ioctl_nr)` to carry an RM call across
a process boundary: payload size, which fields hold file descriptors,
which hold embedded pointers — plus the `_IOC` encoding, call wrappers
and curated struct definitions underneath. The crate docs (`src/lib.rs`)
carry the full statement of scope; session and object lifetime live in
[`nvrm-client`](../nvrm-client).

| File | What it is |
|---|---|
| `src/lib.rs` | `_IOC` encoding (hand-written: bindgen emits nothing for function-like macros), call wrappers, device FDs |
| `src/xlate.rs` | the knowledge table: per-call payload size, fd fields, embedded pointers. **The source of truth.** |
| `src/nvgpu.rs` | ABI definitions, cross-checked against gVisor's `pkg/abi/nvgpu` |
| `src/table.rs` | serialises `xlate`'s knowledge into the descriptor stream the guest module interprets |
| `src/xfer.rs` | `NV_ESC_IOCTL_XFER_CMD` — the case that breaks "the size is in the ioctl number" |
| `src/share.rs` | cross-process `DUP_OBJECT` grants — why they must exist is that file's header |
| `src/bin/nvrm-genhdr.rs` | generates the guest module's C header from the Rust structs |

## The design rule worth knowing before you edit

**The guest module contains no NVIDIA constant.** Not an escape number, not
a struct size, not a field offset. The host serialises a descriptor table
out of `xlate.rs` at startup and the module is its *interpreter*. That is
why `xlate.rs` is the source of truth and why `table.rs` **queries** it
across its whole key space instead of keeping a second list that could go
stale. What cannot be enumerated is recomputed against
`xlate::embedded_ptr` on every host start (`verify_against_xlate`).

## Gate

`cargo run --bin nvrm-genhdr -- --check` says whether the checked-in C
header still matches. `scripts/test.sh check` asks that first, then runs the
module's own C interpreter over the real stream and diffs reading against
writing field by field.

`assert_layout!` (in `nvgpu.rs`) asserts size, alignment **and every field
offset** against the bindgen output, so a field that moves between driver
versions is a compile error naming the field, not a silent misread.
