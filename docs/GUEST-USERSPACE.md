<!-- SPDX-License-Identifier: MIT -->
# Guest NVIDIA userspace

- The guest uses NVIDIA userspace libraries with Leandro's guest modules.
  It does not load NVIDIA's GPU kernel driver.
- Stage userspace matching the host driver. The normal rig uses `DRIVER_VERSION`;
  version experiments can select `LEA_DRIVER` through the launcher.
- Staging and auditing live in [Leandro-Test provision.sh](../../Leandro-Test/scripts/lib/provision.sh); the driver pin and guest modules stay in core.

## Check missing libraries

- `ldd` covers linked dependencies, but NVIDIA libraries also load dependencies
  with `dlopen`. Missing optional libraries can disable a capability without a
  linker error.
- Inspect both `DT_NEEDED` and library names embedded in the binaries:

```sh
objdump -p /path/to/libcuda.so.$WANT | grep NEEDED
strings /path/to/libcuda.so.$WANT | grep -oE 'libnvidia-[a-z0-9-]+\.so[.0-9]*'
```

- Run the staged-guest audit from Test:

```sh
cd ../Leandro-Test
./scripts/showcase.sh audit --name vm0
```

- The audit checks the loader paths for each bit width separately:
  `/opt/nvrm/lib` and `/opt/nvrm-gl/lib` for 64-bit, `/opt/nvrm-gl/lib32` for 32-bit.
- Repeat after staging missing libraries: each added library can introduce more
  runtime dependencies.
- Use `strace` for failed file lookups. An RM ioctl trace cannot show an `ENOENT`
  from a library load.

## Recorded audit: 610.43.03, 2026-08-18

- This is historical evidence, not certification of the currently staged driver.
- The 64-bit audit resolved all referenced NVIDIA names after adding optional
  tile raster, NVVM, PKCS#11, debugger, OpenCL and Vulkan SC libraries.
- The 32-bit JIT chain gained `libnvidia-nvvm`, `libnvidia-ptxjitcompiler` and
  `libnvidia-tileiras`, plus available video and GLES libraries.
- The recorded host lacked 32-bit builds of `libnvidia-nvvm70`,
  `libnvidia-pkcs11`, `libnvidia-pkcs11-openssl3`, `libcudadebugger` and
  `libnvidia-rtcore`. Recheck availability when changing drivers.
- The remaining audit output was:

```text
== 64-bit (compute + GL payload) ==
   all referenced NVIDIA names resolve
== 32-bit (GL payload) ==
   MISSING: libnvidia-nvvm70.so.4
   MISSING: libnvidia-pkcs11-openssl3.so.610.43.03
   MISSING: libnvidia-pkcs11.so.610.43.03
```

- Without 32-bit `libnvidia-rtcore`, the recorded setup cannot provide that
  library to a 32-bit Vulkan client requesting acceleration structures.

## GBM backend selection

- GBM loads `<drivername>_gbm.so` from the name returned by `drmGetVersion`.
  Leandro's DRM nodes report `nvidia-drm`.
- Stage the backend for both bit widths when the host provides both.
  `lea_display_stage` warns when 32-bit userspace is unavailable.
- A missing 32-bit NVIDIA backend can send a client through Mesa's fallback
  loader. The recorded Steam output included:

```text
pci id for fd 136: 1af4:107c, driver (null)
```

- This was a suspected cause of the Steam issue, not a confirmed explanation.
  It did not explain the separate 64-bit Xwayland issue; see
  [OPEN-QUESTIONS](OPEN-QUESTIONS.md), issue 22.
