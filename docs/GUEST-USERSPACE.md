<!-- SPDX-License-Identifier: MIT -->
# Guest NVIDIA userspace

- Use NVIDIA userspace matching the host driver exactly.
- Leandro supplies the guest RM/UVM interfaces; do not load NVIDIA's `nvidia.ko`
  or `nvidia_uvm.ko` in the guest.
- Display uses NVIDIA's `nvidia-modeset`/`nvidia-drm` built against Leandro.
- Installation: [Ubuntu](UBUNTU-DESKTOP.md),
  [NixOS compute](VM-PREPARATION.md#nixos-userspace-after-boot).

## Missing libraries

`ldd` shows linked dependencies, but NVIDIA also loads libraries with `dlopen`.
Inspect both linked and runtime names; verify failures with `strace -e trace=file`.

```sh
objdump -p /path/to/libcuda.so.VERSION | grep NEEDED
strings /path/to/libcuda.so.VERSION | grep -oE 'libnvidia-[a-z0-9-]+\.so[.0-9]*'
```

- A library name in `strings` is a candidate dependency, not proof it is used.
- Check 32-bit and 64-bit loader paths separately.
- Repeat after adding libraries: each can introduce further dependencies.
- An ioctl trace cannot reveal a failed library-file lookup.
- NixOS uses its configured loader environment; Ubuntu uses `ldconfig`.

## GBM

- GBM resolves `<drivername>_gbm.so` using the DRM driver name, `nvidia-drm` here.
- Install the matching NVIDIA GBM backend for each required bit width.
- Missing 32-bit GBM support was suspected in a historical Steam failure; it
  did not explain the separate 64-bit Xwayland failure. See issue 22 in
  [Known issues](OPEN-QUESTIONS.md).

## Historical audit: 610.43.03, 2026-08-18

- The staged 64-bit set resolved all referenced NVIDIA names after optional
  tile raster, NVVM, PKCS#11, debugger, OpenCL and Vulkan SC libraries were added.
- The 32-bit set gained NVVM, PTX JIT, tile raster and available video/GLES libraries.
- The host lacked 32-bit NVVM70, PKCS#11, PKCS#11 OpenSSL3, CUDA debugger and rtcore.
  Unresolved names remained `libnvidia-nvvm70.so.4`,
  `libnvidia-pkcs11-openssl3.so.610.43.03` and `libnvidia-pkcs11.so.610.43.03`.
- Without a 32-bit rtcore library, that setup could not provide it to 32-bit
  Vulkan acceleration-structure clients. Recheck availability on each driver.
- This audit describes that staged payload, not the current installation.
