<!-- SPDX-License-Identifier: MIT -->
# Development

Run commands from the repository root. For VM installation and launch, use the
[quickstart](docs/QUICKSTART.md).

## Requirements

- Linux x86-64, pinned Rust toolchain, C compiler, make, pkg-config, Python 3,
  Git and curl. ABI regeneration also needs Clang/libclang and vendor headers.
- Checks: clang-format 22.1.8, ShellCheck, `edid-decode`, ASan/UBSan.
  `nix develop` supplies most tools; check the formatter version separately.
- Hardware: NVIDIA GPU/driver, matching userspace, KVM and guest networking.
  [Host preparation](docs/VM-PREPARATION.md) lists packages and setup commands.
- Core has no dependency on Leandro-Test. That optional private repository owns
  automated provisioning, workloads, benchmarks and hardware gates.

## Build

```sh
./tools/build.sh --dry-run
./tools/build.sh all
./tools/build.sh check-driver
```

- `all` runs `vendor`, `ch` and `cargo`. Run individual steps after a focused change.
- `vendor-abi <version>` fetches versioned headers for ABI generation.
- Builds need no loaded NVIDIA driver. `check-driver` and `--driver auto` inspect it.
- `--driver VERSION` overrides one invocation; it does not change `DRIVER_VERSION`
  or regenerate bindings. Follow the [ABI workflow](docs/abi-versions.md).
- Cloud Hypervisor needs the complete [patch series](patches/README.md).
- Build guest modules against the guest kernel's headers, not the host kernel.
  See [virtio_nvrm](guest-module/virtio_nvrm/README.md).

## Check

```sh
./tools/build.sh vendor
cargo fmt --all -- --check
./tools/ci/check-c-format.sh
./tools/check.sh
./tools/ci/check-c.sh all --sanitize
```

- `check.sh` covers debug/release Rust tests, Clippy, generated ABI consistency,
  C fixtures, documentation and repository checks. It needs no GPU.
- C formatting excludes generated and vendored files.
- Run focused tests while editing; run the full software check before review.
- After runtime changes, rebuild release binaries and validate the affected GPU
  workload. [Testing](docs/TESTING.md) explains coverage and result requirements.

## Nix

```sh
nix develop
nix build .#vhost-user-nvrm .#cloud-hypervisor
nix build .#guest-modules .#test-tools
nix flake check
```

- `nix develop .#prebuilt` selects store binaries through `LEA_BIN_DIR` and `LEA_CH`.
- Host/guest configuration: [host example](nix/example-host.nix),
  [guest example](nix/example-guest.nix), [host options](nix/module.nix),
  [guest options](nix/module-guest.nix).
- Optional evdev backend: `nix build .#vhost-device-input`; see [Input](docs/INPUT.md).
- `.#guest-deb` builds the Ubuntu DKMS package; `.#host-deb` builds host packages.
- `.#guest-image` produces `kernel`, `initrd`, `rootfs.qcow2` and `image.env`;
  keep them together. `.#guest-image-uefi` builds the firmware-boot variant.
- Stage matching NVIDIA userspace separately. NixOS uses its configured loader
  environment; Ubuntu uses `ldconfig`.
- NixOS compute is supported by the image recipe. Its display path is unimplemented.
- New files must be visible to the flake's Git source filter before `nix build` includes them.

## Trace calls

```sh
cargo build --locked --release -p nvrm-trace
mkdir -p traces
LEA_TRACE_FILE=traces/run.tsv LD_PRELOAD="$PWD/target/release/libnvrm_trace.so" ./your-program
```

- Tracing adds overhead. Disable it for timing runs.
- Formats and interception limits: [nvrm-trace](crates/nvrm-trace/README.md).

## Troubleshooting

| Symptom | Check |
|---|---|
| Driver mismatch | Loaded `/proc/driver/nvidia/version`, compiled ABI and guest libraries; a package update does not replace a loaded module |
| Old behavior after rebuild | Binary path and build timestamp; rebuild release artifacts |
| Guest unreachable | Serial/hypervisor logs, tap/bridge setup and firewall forwarding |
| Missing capability/library | [Userspace dependencies](docs/GUEST-USERSPACE.md), including `dlopen` lookups |
| CUDA disappears after mediation change | Remove `mode` from `LEA_VGPU_MEDIATE`; default is `uuid,enc` |
| Allocation refused | Backend logs; guest pin limit, host arena/aggregate limits and VRAM policy |
| Black or frozen stream | [Display checks](docs/DISPLAY.md); verify changing pixels, not only FPS |
| Teardown hangs | Preserve logs; stop clients before the VM and backend. Live unbind is unsupported |

See [security limits](docs/SECURITY.md) before changing ownership or teardown.
