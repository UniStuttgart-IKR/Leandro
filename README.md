<!-- SPDX-License-Identifier: MIT -->
# Leandro

- GPU paravirtualization for Linux VMs using an NVIDIA GPU shared with the host.
- Guest applications use NVIDIA userspace. `virtio_nvrm.ko` forwards driver calls to `vhost-user-nvrm` on the host.
- One backend process per VM; requires the patched cloud-hypervisor in this repository.
- Research software developed alongside [MeisterStack](https://github.com/UniStuttgart-IKR/MeisterStack), University of Stuttgart, IKR.
- Most original code was AI-assisted. Human review is in progress.
- **Use trusted guests. Hostile-guest isolation is not established.** See [security boundaries and unresolved risks](docs/SECURITY.md).

## Start here

- [Build, run, and troubleshoot](DEVELOPMENT.md)
- [Architecture and code map](docs/ARCHITECTURE.md)
- [First code review and maintenance guide](docs/REVIEW.md)
- [Tests and what they verify](docs/TESTING.md)
- [Display and streaming](docs/DISPLAY.md)
- [Complete demonstration](docs/SHOWCASE.md)
- [Driver ABI versions](docs/abi-versions.md)
- [Known issues](docs/OPEN-QUESTIONS.md)
- [Changes proposed before the thesis freeze](docs/THESIS-FREEZE.md)

## Requirements

- Hardware runs: Linux x86-64 host, KVM access, NVIDIA kernel driver, and matching NVIDIA userspace. Core software checks need no GPU.
- Guest NVIDIA userspace must match the host driver exactly.
- Default driver: [DRIVER_VERSION](DRIVER_VERSION). Compiled ABI versions: [abi.toml](crates/nvrm-sys/abi.toml).
- Rust: [rust-toolchain.toml](rust-toolchain.toml). C kernel modules are build-checked on Linux 6.8 and 6.12.
- Host tools, storage, network setup, and optional Nix environment: [Development](DEVELOPMENT.md#requirements).

## Build and run

- Core builds fetch vendor sources and build the patched hypervisor and Rust binaries. They do not provision a VM.
- Keep [Leandro-Test](../Leandro-Test/README.md) beside this checkout for images, probes, VM provisioning and hardware gates. Its scripts default to `../Leandro`; set `LEANDRO` for another core checkout.
- Use [the ABI workflow](docs/abi-versions.md) before selecting a different driver. A build option does not regenerate committed bindings.

```sh
# From the Leandro checkout:
./scripts/build.sh --dry-run
./scripts/build.sh

cd ../Leandro-Test
./scripts/build.sh --dry-run
./scripts/build.sh all
./scripts/showcase.sh net up
./scripts/showcase.sh up
./scripts/showcase.sh ssh nvidia-smi
./scripts/showcase.sh ssh 'cd ~/gpu && ./nvprobe 3'
./scripts/showcase.sh down --name vm0
```

- `net up` configures a host bridge, taps, and NAT using sudo.
- `up` starts the backend, boots the VM, stages userspace, and builds/loads the guest modules.
- `nvprobe 3` should report `stage 3 ok (kernel, result correct)`.
- Stop the VM before its backend; `down` handles this order.
- State and images live under `LEA_VM_DIR`, defaulting to Leandro-Test `vm/`. Set it in Leandro-Test `local.env`; preserve an explicit path when reusing an existing rig.

## Test it yourself

```sh
# From Leandro:
./scripts/build.sh vendor
cargo fmt --all -- --check
./scripts/ci/check-c-format.sh
./scripts/check.sh
./scripts/ci/check-c.sh all --sanitize
```

- C formatting requires clang-format 22.1.8.
- Sanitizers require a C compiler with ASan/UBSan and `edid-decode`.
- Hardware gates require a prepared rig and current release binaries:

```sh
cd ../Leandro-Test
./scripts/showcase.sh state --check
./lea acceptance gpu vdisplay display
```

- `lea acceptance` runs the relocated full gates. `lea gate` is a separate, narrower MeisterStack smoke check.
- Gate exit codes: `0` passed, `1` failed, `2` skipped. A skip is not a pass.
- Run gates sequentially with no other GPU workload. The display gate needs a desktop image, Sunshine, and Moonlight.
- CI runs Rust tests, Clippy, Rust/C formatting, C sanitizers, kernel builds, and Nix checks. Hardware gates run separately.
- CodeRabbit configuration is included; automatic PR reviews require the GitHub App.

## Multiple VMs

```sh
cd ../Leandro-Test
./scripts/showcase.sh up --name vm0 --index 0
./scripts/showcase.sh up --name vm1 --index 1 --vram-limit 2048
./scripts/showcase.sh ssh --name vm1 nvidia-smi
./scripts/showcase.sh down --name vm1
./scripts/showcase.sh down --name vm0
```

- Each VM gets a separate backend, socket, and memory ledger.
- `--vram-limit` bounds accounted allocations; it is not a physical GPU partition.
- `--vram-profile` adds the project's reservation policy. Neither mode guarantees availability against another tenant exhausting the GPU.
- Encoder capacity reporting does not enforce encoder scheduling.

## Recorded hardware coverage

- On 2026-09-19, compute, virtual-display and desktop gates passed from Leandro-Test after removal of the old core harness. These are functional results, not an isolation proof.
- Older results below describe their recorded hardware, not every revision or workload.
- RTX 2070 / Turing, driver 610.57.04: compute and virtual-display gates passed; full display gate recorded on 2026-08-20.
- RTX 5060 Ti / Blackwell, driver 610.57.04: compute and virtual-display gates recorded as passed; full display gate not run.
- Other GPU/driver pairs require their own validation. Generated bindings compiling is not hardware validation.
- Measurements and corrections: [Testing](docs/TESTING.md), [Display](docs/DISPLAY.md), [known issues](docs/OPEN-QUESTIONS.md).

## Components

- `guest-module/virtio_nvrm`: guest device nodes, forwarding, mappings, and virtual display.
- `guest-module/nvrm_nodes`: guest parameters and address-translation helper.
- `crates/vhost-user-nvrm`: request validation, host driver calls, mappings, events, and quotas.
- `crates/nvrm-wire`: wire layout; `crates/nvrm-abi`: translation metadata and header generator.
- `crates/nvrm-sys`: generated driver bindings; `crates/nvrm-client`: diagnostic RM client.
- `crates/nvrm-trace`: ioctl tracer; `crates/vhost-user-input`: input backend.
- `scripts/build.sh`, `scripts/check.sh`, `scripts/ci/`: core builds and software checks.
- `tests/tools/`: canonical EDID/frame tools and deterministic frame fixture.
- Leandro-Test is currently private; core builds and software checks do not require access.
- [Leandro-Test](../Leandro-Test/README.md): probes, guest workloads, provisioning, hardware acceptance and measurements.

## Contributing and attribution

- Keep changes focused; include a reproducer for bugs and the commands used to validate the fix.
- State GPU model, exact driver version, guest kernel, and commit for hardware results.
- Core code: MIT. File-specific licences and vendor attribution: [LICENSES.md](LICENSES.md).
- Driver layouts derive from [NVIDIA open-gpu-kernel-modules](https://github.com/NVIDIA/open-gpu-kernel-modules).
- RM object-model and ABI work draws on [gVisor nvproxy](https://github.com/google/gvisor).
