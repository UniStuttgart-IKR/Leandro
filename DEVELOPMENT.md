<!-- SPDX-License-Identifier: MIT -->
# Development

- Host command blocks start from the core checkout unless stated otherwise.

## Requirements

- Core software work: Linux x86-64, pinned Rust toolchain, C compiler, `make`, `pkg-config`, Clang/libclang, Python 3, Git and curl.
- Checks: `edid-decode`, ShellCheck, clang-format 22.1.8, and ASan/UBSan for C tests. `nix develop` supplies most tools; check the formatter version separately.
- Hardware runs: NVIDIA GPU/driver, matching host libraries, `/dev/kvm`, `qemu-img`, `mkfs.vfat`, `mcopy`, OpenSSH, `ip` and `iptables`.
- Guest NVIDIA userspace must match the host driver exactly; the backend does not verify guest library versions over the wire.
- Keep [Leandro-Test](../Leandro-Test/README.md) beside this checkout. It owns acceptance images, probes, workloads and VM state.
- Allow several GiB for sources/images and about 2.5 GiB per PyTorch environment; desktops/games need more.
- Supported ABIs: [driver versions](docs/abi-versions.md). Operating limits: [SECURITY.md](docs/SECURITY.md).

## Configure paths

- Set `LEANDRO` to select a non-sibling core checkout; Test scripts also accept `LEA_ROOT` explicitly.
- `LEA_TEST_ROOT` selects Test assets and libraries. Core vendor trees and release binaries stay under `LEA_ROOT`.
- Copy [Test local.env.example](../Leandro-Test/local.env.example) to Leandro-Test `local.env` and set VM defaults there. Core software checks do not load VM configuration.
- `LEA_VM_DIR` holds disks, SSH state, logs and measurements; default: Test `vm/`. Preserve its explicit old location when reusing a rig.
- `LEA_BIN_DIR` defaults to core `target/release`; `LEA_CH` selects the patched hypervisor.
- `LEA_BASE_IMAGE` selects the Ubuntu disk; `LEA_NIXOS_DIR` selects a NixOS image.
- `LEA_HOSTVENV` defaults to Test `vendor/hostvenv`; override it to reuse an existing native reference environment.
- Environment settings override defaults when `local.env` uses the documented `: "${VAR:=value}"` form. Relative state paths resolve in Test; binary paths resolve in core.
- Remaining defaults: [Test configuration](../Leandro-Test/scripts/lib/config.sh).

## Build

```sh
# From Leandro; all = vendor + ch + cargo.
./scripts/build.sh --dry-run
./scripts/build.sh all
./scripts/build.sh check-driver
```

- Core commands: `vendor`, `vendor-abi`, `ch`, `cargo`, `check-driver` and `all`.
- The software build needs no loaded NVIDIA driver; `check-driver` and `--driver auto` inspect the running driver.
- Core `--driver VERSION|auto` overrides one invocation without rewriting `DRIVER_VERSION`. It does not regenerate bindings; follow [the ABI workflow](docs/abi-versions.md).
- The patched hypervisor is required for shared mappings: [CH_VERSION](CH_VERSION), [patch series](patches/).
- Core `build.sh full` delegates to Test's complete build; run the Test commands directly when working on the rig:

```sh
cd ../Leandro-Test
./scripts/build.sh --dry-run
./scripts/build.sh preflight
./scripts/build.sh all
```

- Test's `all` prepares core binaries, probes, an image, the native PyTorch reference and a baked guest, then runs software checks.
- `--minimal` skips baking; `--full` also installs guest PyTorch.
- Individual Test steps: `probes`, `image`, `hostvenv`, `bake --set-default`, and `package`.
- `make -C probe cpu-probes` builds without CUDA; `cuda-probes` needs the toolkit, and `all-probes` adds graphics probes and PTX. Building a probe does not run it.
- `edid-verify` and `vdisp-frame` compile from core `tests/tools/`; Test carries no duplicate sources.
- `bake --set-default` writes Test `local.env`. Preserve the generated package manifest: Ubuntu packages depend on the archive at bake time.
- Rebuild guest images and display modules after changing driver versions.

## Start, inspect, stop

```sh
cd ../Leandro-Test
./scripts/showcase.sh net up
./scripts/showcase.sh up --name vm0 --index 0
./scripts/showcase.sh status
./scripts/showcase.sh ssh --name vm0 nvidia-smi
./scripts/showcase.sh ssh --name vm0 'cd ~/gpu && ./nvprobe 3'
./scripts/showcase.sh down --name vm0
```

- `net up` uses sudo to configure the bridge, taps, forwarding, and NAT; repeat after reboot unless managed by NixOS.
- `up` starts one backend for the VM, provisions userspace and probes, and installs the guest modules.
- Expected compute result: `stage 3 ok (kernel, result correct)`.
- Stop the VM before its backend. `down` applies this order.
- Give simultaneous VMs distinct names and indices. Default indices use `tap0` through `tap7`.
- Add `--vram-limit 2048` for an accounting cap or `--vram-profile 3072` for the reservation policy; these are not hardware partitions.
- Display/streaming requirements: [DISPLAY.md](docs/DISPLAY.md). Complete examples: [SHOWCASE.md](docs/SHOWCASE.md).

## Run tests

```sh
# From Leandro:
./scripts/build.sh vendor
cargo fmt --all -- --check
./scripts/ci/check-c-format.sh
./scripts/check.sh
./scripts/ci/check-c.sh all --sanitize
```

- `check` includes debug/release Rust tests, doctests, rustdoc, Clippy, generated ABI checks, C fixtures, and repository checks.
- It needs no GPU. It may need network access to fetch build inputs.
- C formatting excludes vendored and generated headers.
- Run hardware gates after building current release binaries:

```sh
# From Leandro:
./scripts/build.sh cargo
cd ../Leandro-Test
./scripts/showcase.sh state --check
./lea acceptance gpu vdisplay display
```

- Fix failed prerequisites before testing; the scripts report skipped gates with exit code `2`.
- `lea acceptance` runs the full relocated suite; `lea gate` remains a narrower MeisterStack smoke check.
- `gpu`: compute and native-reference comparisons.
- `vdisplay`: virtual-display device, EDID, pixels, and teardown.
- `display`: desktop rendering, presentation, events, capture, and streaming.
- Run sequentially on an otherwise idle GPU. Gates own their VM instances; do not run them against an active demonstration.
- `state --check` requires persistence mode. Enable it for the run with `sudo nvidia-smi -pm 1`; restore the previous setting afterward if it was temporary.
- Coverage, JSON results, fuzzing, and measurement rules: [TESTING.md](docs/TESTING.md).

## Nix

```sh
nix develop
nix build .#vhost-user-nvrm .#cloud-hypervisor
nix build .#guest-modules .#test-tools
nix flake check

cd ../Leandro-Test
./scripts/build.sh bake --nixos --set-default
./scripts/showcase.sh up --guest nixos
./scripts/test.sh gpu --guest nixos
```

- Core `nix develop .#prebuilt` selects store-built binaries through `LEA_BIN_DIR` and `LEA_CH`.
- Core exposes production packages/modules and `test-tools`; Test exposes `acceptance-scripts` and the showcase/test/bench apps. Core no longer exports `leandro-scripts` or those apps.
- `services.leandro.dev.enable = true` requires an explicit `scriptsPackage` from Test; production deployments can leave `dev.enable` off. With both flake inputs available:

```nix
services.leandro = {
  dev.enable = true;
  scriptsPackage = inputs.leandro-test.packages.${pkgs.stdenv.hostPlatform.system}.acceptance-scripts;
};
```

- The flake exposes host and guest modules: [host example](nix/example-host.nix), [guest example](nix/example-guest.nix).
- Host options: [nix/module.nix](nix/module.nix). Guest options: [nix/module-guest.nix](nix/module-guest.nix).
- The guest image contains `kernel`, `initrd`, `rootfs.qcow2`, and `image.env`; keep them together.
- `.#guest-image-uefi` builds the firmware-boot image.
- NVIDIA userspace is staged from the host; it is not included in the guest image.
- NixOS guest library lookup uses its configured environment, not Ubuntu's `ldconfig` mechanism.
- The NixOS guest display path is not implemented; use Ubuntu for display gates.
- New source files must be visible to the flake's Git source filter before a normal `nix build` includes them.

## Vsock and cluster runs

- `--transport vsock` avoids bridge/tap networking for guest access. It uses the built `vsockconnect` helper and the hypervisor's socket.
- Cluster requirements: KVM access, GPU allocation, Apptainer, writable scratch, and matching host driver/userspace.
- Prepare the package on a machine with network access:

```sh
cd ../Leandro-Test
./scripts/build.sh bake --nixos --with-torch
./scripts/build.sh package --out /shared/leandro-pkg
./scripts/bench.sh slurm --package /shared/leandro-pkg --out ./jobs --counts '1 2 4'
```

- Submit the scripts listed in `jobs/submit.txt` using your site's scheduler policy.
- Test a job locally before submitting a batch:

```sh
cd ../Leandro-Test
./scripts/bench.sh slurm --run gate-1 --package /shared/leandro-pkg --out ./results
./scripts/bench.sh slurm --collect ./jobs/results
```

- MIG, additional GPU architectures, and different driver pairs need separate validation; a package build does not establish support.

## Trace calls

```sh
# From Leandro:
cargo build --locked --release -p nvrm-trace
mkdir -p traces
LEA_TRACE_FILE=traces/run.tsv LD_PRELOAD="$PWD/target/release/libnvrm_trace.so" ./your-program
```

- The tracer is a diagnostic tool, not the transport.
- Output fields and format selection: [nvrm-trace](crates/nvrm-trace/README.md).

## Troubleshooting

- **Driver/library mismatch:** compare `nvidia-smi`, `/proc/driver/nvidia/version`, the configured ABI, and guest libraries. Installing a new driver package does not replace an already loaded kernel module.
- **Stale binary:** rebuild with `scripts/build.sh cargo`; do not use `LEA_ALLOW_STALE=1` for validation.
- **Guest unreachable:** inspect `showcase.sh status`, network setup, and instance logs under `LEA_VM_DIR`. Docker's forwarding rules can affect bridge traffic.
- **Guest cannot load libraries:** follow [GUEST-USERSPACE.md](docs/GUEST-USERSPACE.md); `ldd` does not list every library loaded through `dlopen`.
- **No CUDA device after VGX override:** remove `mode` from `LEA_VGPU_MEDIATE`; the normal default is `uuid,enc`.
- **Allocation refusal:** inspect backend logs, configured VRAM policy, and the guest, per-arena and aggregate host pin limits. Available host VRAM and a VM's accounting balance are different quantities.
- **Display freezes or black frames:** use the independent pixel/presentation checks in [DISPLAY.md](docs/DISPLAY.md); FPS alone does not prove presentation.
- **Hung teardown:** preserve logs before restarting. Known cancellation, unbind, and event-lifetime risks are listed in [SECURITY.md](docs/SECURITY.md).
