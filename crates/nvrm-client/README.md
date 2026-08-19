<!-- SPDX-License-Identifier: MIT -->
# `nvrm-client` — the RM level

Client, object tree, handle allocation: the layer above
[`nvrm-abi`](../nvrm-abi) that knows RM has *lifetimes*, not just calls.
Nothing on the data path uses it — `vhost-user-nvrm` holds no RM client
of its own — so its users are the diagnostic binaries below. The two
facts the design rests on (handles pass through verbatim; free is
transitive along edges only the driver source states) are anchored in
the crate docs (`src/lib.rs`), where they cannot be lost.

| File | What it is |
|---|---|
| `src/lib.rs` | `RmClient` — one client = one `NV01_ROOT_CLIENT` = one fd on `/dev/nvidiactl` |
| `src/handle.rs` | handle allocation from a reserved range that cannot collide with `libcuda`'s |
| `src/object.rs` | object tracking with transitive free; modelled on gVisor nvproxy's `object.go` |
| `src/mem.rs` | allocate `NV01_MEMORY_SYSTEM`, map into the VASpace (the GPU's virtual address space object), map into our own address space |

## The binaries

Each diagnostic asks the driver a single question directly, so an answer
can be checked against what a real program believes:

| Binary | Question it answers |
|---|---|
| `classlist` | what the GPU says it can do (`NV0080_CTRL_CMD_GPU_GET_CLASSLIST`) |
| `smipids` | what `nvidia-smi` asks for its process list (`NV2080_CTRL_CMD_GPU_GET_PIDS`) |
| `fbclients` | who is holding VRAM right now, and under which guest-process identity |
| `mmapping` | round-trip latency of one window mapping, with no CUDA around it |
| `e1-extmap` | does the OS-descriptor → UVM external-mapping chain carry? (host-local, no VM) |
| `vsockconnect` | not a diagnostic: the `ssh -o ProxyCommand` bridge onto cloud-hypervisor's hybrid vsock, used by the NixOS-guest transport |

The diagnostics need a real card and the matching driver version;
`vsockconnect` needs neither.
