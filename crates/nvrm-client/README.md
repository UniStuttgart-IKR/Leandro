<!-- SPDX-License-Identifier: MIT -->
# nvrm-client

- Owns RM clients, object trees and handle allocation above
  [nvrm-abi](../nvrm-abi/README.md).
- Used by the tools below; the host backend owns forwarded and pool clients separately.
- Child objects must be released in dependency order; bookkeeping must follow
  successful RM operations.

## Source map

| File | Responsibility |
|---|---|
| `src/lib.rs` | `RmClient`, root client FD and ioctl operations |
| `src/handle.rs` | Handle allocation within a client namespace |
| `src/object.rs` | Object dependencies and free order |
| `src/mem.rs` | System-memory allocation and CPU/GPU mappings |

## Binaries

| Binary | Purpose |
|---|---|
| `classlist` | Query supported RM classes |
| `smipids` | Query the driver's process list |
| `fbclients` | Inspect VRAM owners and guest-process identities |
| `mmapping` | Measure mapping round-trip latency |
| `e1-extmap` | Exercise OS-descriptor/UVM external mapping without a VM |
| `vgpuprofile` | Read card properties and calculate memory profiles |
| `vsockconnect` | Bridge SSH to cloud-hypervisor's hybrid vsock |

- GPU diagnostics need a real card and matching driver/userspace setup.
- `vsockconnect` needs neither the GPU nor NVIDIA libraries.
- GPU-free ownership tests: `cargo test -p nvrm-client --lib`.
