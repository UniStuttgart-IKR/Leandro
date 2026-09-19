<!-- SPDX-License-Identifier: MIT -->
# Demonstration

Use the [manual quickstart](QUICKSTART.md) to start two Ubuntu desktops on one GPU.
It covers host setup, guest installation, Cloud Hypervisor and two Moonlight windows.
The same guide includes two NixOS compute guests.

## Check the result

- In each desktop, `glxinfo -B` must name NVIDIA; `llvmpipe` means software rendering.
- Run `glxgears` in both desktops and check that both streamed windows change.
- `nvidia-smi` checks device access, not rendering or CUDA execution.
- Run a compute workload with a checked result before claiming compute works.
- Stop guest applications, shut down the VMs, then stop remaining backends.

## Scope

- The manual recipe has not yet been validated from fresh guest images.
- Automated provisioning, games disks, benchmarks and acceptance commands belong
  to the optional private Leandro-Test repository.
- Recorded functional results: [Testing](TESTING.md).
- Display configuration and known failures: [Display](DISPLAY.md).
- Per-VM limits do not establish hostile-tenant isolation: [Security](SECURITY.md).
