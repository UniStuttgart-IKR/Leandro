<!-- SPDX-License-Identifier: MIT -->
> [!NOTE]
> This code is part of a Master's Thesis. Resolving issues
> and feature requests is not the top priority of the
> maintainer.
# Leandro

GPU paravirtualization for Linux VMs sharing an NVIDIA GPU with the host.
Guest NVIDIA libraries call `virtio_nvrm.ko`; a per-VM `vhost-user-nvrm`
backend forwards supported operations to the host driver. Shared mappings
carry GPU submission without forwarding each doorbell write.

**Use trusted guests. Hostile-guest isolation is not established.**
See [security limits](docs/SECURITY.md).

> [!WARNING]
> Be aware that basically all of the code in this repository is generated
> by LLMs. The code and documentation is under active review at the moment
> when the review is done this warning will be removed!
> This is experimental software!

## Start here

- [Documentation index](docs/README.md): guides, design notes and component READMEs.
- [Quickstart](docs/QUICKSTART.md): two Ubuntu desktops or two NixOS compute VMs.
- [Development](DEVELOPMENT.md): build, check, debug and package.
- [Architecture](docs/ARCHITECTURE.md): request path, mappings and ownership.
- [Testing](docs/TESTING.md): coverage, hardware evidence and measurement rules.
- [Display](docs/DISPLAY.md), [driver versions](docs/abi-versions.md),
  [known issues](docs/OPEN-QUESTIONS.md), [future work](docs/FUTURE.md).

## Requirements

- Linux x86-64, KVM, an NVIDIA host driver and matching guest NVIDIA userspace.
- Patched Cloud Hypervisor: [version](CH_VERSION), [patches](patches/README.md).
- Pinned [Rust toolchain](rust-toolchain.toml) and [driver](DRIVER_VERSION).
  Exact compiled driver versions are listed in [abi.toml](crates/nvrm-sys/abi.toml).
- Guest modules are compile-checked on Linux 6.8 and 6.12. Other kernels need validation.
- Core builds, software checks and manual VM launches need no Leandro-Test checkout.

## Build and check

Run from the repository root; dependencies are in [Development](DEVELOPMENT.md#requirements).

```sh
./tools/build.sh all
cargo fmt --all -- --check
./tools/ci/check-c-format.sh
./tools/check.sh
./tools/ci/check-c.sh all --sanitize
```

- `all` fetches sources, patches/builds Cloud Hypervisor and builds the Rust workspace.
- Software checks need no GPU. C formatting uses clang-format 22.1.8.
- CI also builds guest modules and checks Nix. Hardware tests run separately.
- CodeRabbit PR reviews require its GitHub App; configuration is included.
- Start VMs with the commands in the [quickstart](docs/QUICKSTART.md).
  Give each VM its own backend, sockets, writable disk and network identity.

## Hardware evidence

| GPU / driver | Recorded coverage |
|---|---|
| RTX 2070 / 610.57.04 | Compute, virtual display and desktop gates; latest recorded run 2026-09-19 |
| RTX 5060 Ti / 610.57.04 | Earlier compute and virtual-display gates; no full desktop gate |

- The build targets 615.71.09 (`DRIVER_VERSION`) since 2026-09-24. No hardware
  run on 615.71.09 is recorded yet; the rows above are 610.57.04 runs.

- Results cover the recorded revisions and workloads, not every configuration.
- VRAM limits account for mediated allocations; they do not partition hardware,
  guarantee residency or schedule GPU engines.
- [Testing](docs/TESTING.md) records coverage and limits. The optional private
  Leandro-Test repository owns automated provisioning and hardware acceptance.

## Components

| Path | Role |
|---|---|
| `guest-module/virtio_nvrm` | Guest nodes, forwarding, mappings, events and display |
| `guest-module/nvrm_nodes` | Guest driver parameters and diagnostic pinning |
| `crates/vhost-user-nvrm` | Host validation, driver calls, mappings and quotas |
| `crates/nvrm-wire`, `crates/nvrm-abi` | Wire schema, translation tables and header generator |
| `crates/nvrm-sys` | Generated NVIDIA driver bindings |
| `crates/nvrm-client`, `crates/nvrm-trace` | RM diagnostics and ioctl tracing |
| [Upstream input backend](docs/INPUT.md) | Optional host keyboard/mouse forwarding |
| `tools/`, `tests/tools/` | Builds, software checks and EDID/frame fixtures |

## Contributing and attribution

- Keep changes focused. Include a reproducer and validation commands.
- For hardware results, record the commit, GPU, driver, guest kernel and workload.
- Most original code was AI-assisted; human review is ongoing.
- Developed at University of Stuttgart, IKR, alongside
  [MeisterStack](https://github.com/UniStuttgart-IKR/MeisterStack).
- Driver definitions derive from [NVIDIA open-gpu-kernel-modules](https://github.com/NVIDIA/open-gpu-kernel-modules);
  RM ABI and ownership work also draws on [gVisor nvproxy](https://github.com/google/gvisor).
- Rust/tools/docs: MIT. Guest modules: GPL-2.0-only. See [LICENSES.md](LICENSES.md).

> [!IMPORTANT]
> A vfio-user variant of that software that emulates the GSP of the card,
> instead of multiplexing the RM, is actively being worked on. For CUDA, with
> the exception of really short kernels, nearly native performance could be achieved.
> This differs from the vhost-user approach because the unmodified NVIDIA driver
> can be loaded, and Windows guests can be supported!
