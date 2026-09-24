<!-- SPDX-License-Identifier: MIT -->
# Documentation

Start with [Quickstart](QUICKSTART.md) to run Leandro, or
[Architecture](ARCHITECTURE.md) to read the code. Read [Security](SECURITY.md)
before interpreting isolation claims.

## Setup and use

| File | Contents |
|---|---|
| [Project README](../README.md) | Overview, requirements and build commands |
| [QUICKSTART.md](QUICKSTART.md) | Two Ubuntu desktops or two NixOS compute guests |
| [VM-PREPARATION.md](VM-PREPARATION.md) | Host installation, Cloud Hypervisor patches, networking and images |
| [UBUNTU-DESKTOP.md](UBUNTU-DESKTOP.md) | Guest modules, NVIDIA userspace, GNOME/X11 and Sunshine |
| [WAYLAND.md](WAYLAND.md) | GNOME/Wayland, KMS capture and verification |
| [INPUT.md](INPUT.md) | Moonlight input and optional upstream evdev forwarding |
| [GUEST-USERSPACE.md](GUEST-USERSPACE.md) | NVIDIA library installation and version requirements |
| [SHOWCASE.md](SHOWCASE.md) | Demonstration checks and what they establish |

## Design and development

| File | Contents |
|---|---|
| [ARCHITECTURE.md](ARCHITECTURE.md) | Request path, ownership, mappings, events and source map |
| [SECURITY.md](SECURITY.md) | Trust boundary, validation, remaining risks and required tests |
| [DISPLAY.md](DISPLAY.md) | Virtual display, capture, module parameters and limits |
| [abi-versions.md](abi-versions.md) | Supported driver ABIs, generation and version changes |
| [Development](../DEVELOPMENT.md) | Build, checks, Nix, tracing and troubleshooting |
| [TESTING.md](TESTING.md) | Automated checks, hardware evidence and measurement rules |
| [OPEN-QUESTIONS.md](OPEN-QUESTIONS.md) | Numbered investigations, findings and unresolved issues |
| [FUTURE.md](FUTURE.md) | Planned correctness, compatibility and research work |
| [VIRTIO-UAPI.md](VIRTIO-UAPI.md) | Proposal for generalizing table-described driver forwarding |
| [NAMING.md](NAMING.md) | Component names and terminology |
| [llm.md](llm.md) | Maintenance notes; filename retained for existing links |
| [Licenses](../LICENSES.md) | Licensing and third-party attribution |

## Crate and module READMEs

Each README describes the component and its source files.

| README | Component |
|---|---|
| [guest-module/virtio_nvrm](../guest-module/virtio_nvrm/README.md) | Guest RM/UVM forwarding, mappings, events and display |
| [guest-module/nvrm_nodes](../guest-module/nvrm_nodes/README.md) | Guest NVIDIA proc data, optional placeholder nodes and pinning diagnostics |
| [crates/vhost-user-nvrm](../crates/vhost-user-nvrm/README.md) | Per-VM host backend, sessions and resource ownership |
| [crates/nvrm-wire](../crates/nvrm-wire/README.md) | Shared request/reply schema and generated C header |
| [crates/nvrm-sys](../crates/nvrm-sys/README.md) | Generated NVIDIA bindings and layout metadata |
| [crates/nvrm-abi](../crates/nvrm-abi/README.md) | Ioctl descriptors, embedded pointers, FDs and mediation layouts |
| [crates/nvrm-client](../crates/nvrm-client/README.md) | Direct RM ownership wrappers and diagnostic tools |
| [crates/nvrm-trace](../crates/nvrm-trace/README.md) | `LD_PRELOAD` call tracing |
| [patches](../patches/README.md) | Cloud Hypervisor changes, validation and upstreaming |
| [matrix](../matrix/README.md) | NVIDIA userspace API coverage and recorded probes |

[crates/xtask](../crates/xtask/src/main.rs) has no separate README.
Its ABI generation commands are documented in [abi-versions.md](abi-versions.md).

## Historical evidence

These files describe recorded runs, not guarantees for the current revision.

| File or directory | Contents |
|---|---|
| [history](history/README.md) | Archived investigation, testing and maintenance documents |
| [measurements/fence-14](measurements/fence-14/README.md) | Vulkan fence-wait polling fallback |
| [measurements/vram-68](measurements/vram-68/README.md) | Two-guest VRAM policy comparison |
| [measurements/vram-69](measurements/vram-69/README.md) | Four-guest VRAM policy tests |
| [measurements/vram-69b](measurements/vram-69b/README.md) | Admission limits and vGPU-mode behavior |
| [vram-69b/libcuda/TRACES.md](measurements/vram-69b/libcuda/TRACES.md) | Trace file inventory |
| [measurements/vram-70](measurements/vram-70/README.md) | Workloads under different VRAM limits |
| [measurements/sottr-4q](measurements/sottr-4q/README.md) | Two concurrent Shadow of the Tomb Raider benchmarks |
| [measurements/vhost-user-check-2026-09-23](measurements/vhost-user-check-2026-09-23/commands.txt) | Manual VM check of the vhost-user patch series 0001-0003 |
