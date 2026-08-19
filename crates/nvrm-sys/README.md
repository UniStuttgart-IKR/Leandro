<!-- SPDX-License-Identifier: MIT -->
# `nvrm-sys` — raw bindings to the NVIDIA driver ABI

Generated `bindgen` output for the NVIDIA SDK headers, for **exactly one
driver version** (the one in `DRIVER_VERSION` at the repo root). Every
other crate reaches the driver ABI through here; nobody transcribes a
struct by hand. The version-lockstep argument — why a mismatch panics
instead of failing loudly on its own — lives in the crate docs
(`src/lib.rs`), next to `assert_driver_version()`.

| File | What it is |
|---|---|
| `src/lib.rs` | `include!` of the generated bindings, plus the version check |
| `build.rs` | drives `bindgen` over the vendor headers; the binding policy and its reasons |
| `wrapper.h` | the list of vendor headers to bind, and why each is in it |

## Building

Needs `libclang` (Debian/Ubuntu: `libclang-dev`) and the vendored NVIDIA
kernel-module tree — `scripts/build.sh vendor` fetches it at the pinned
version.

## Limits, honestly

- The output is bindgen's, ugly on purpose; `src/lib.rs` says why tidying
  it would be wasted work.
- The crate is excluded from doctests and from `cargo doc` in the check
  band (`--exclude nvrm-sys`): it is machine output, and `build.rs`
  records the binding flags that make that the right call.
- `assert_driver_version()` runs at the start of every **host** binary
  that talks to RM; guest-side diagnostics deliberately skip it.
