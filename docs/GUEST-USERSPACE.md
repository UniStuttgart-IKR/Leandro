<!-- SPDX-License-Identifier: MIT -->
# What NVIDIA userspace the guest needs, and how to check

The guest gets no NVIDIA kernel driver — that is what this project
replaces. It does get NVIDIA's complete **userspace** half, staged from
the host at exactly the version in `DRIVER_VERSION`.

This document is about how you find out what is missing, because the
obvious method does not work.

## `ldd` answers the wrong question

NVIDIA's libraries name their siblings mostly **at runtime**, not in
`DT_NEEDED`:

    $ objdump -p /usr/lib/libcuda.so.610.43.03 | grep NEEDED
    (not one NVIDIA library)

`libcuda` depends on nothing NVIDIA-shaped according to the linker, and
`dlopen`s half the JIT chain anyway. The consequence has already cost this
project days:

**A missing `dlopen` is not a link error and not a loud failure.** It is a
capability that is simply absent. That is exactly how `libnvidia-rtcore`
went missing: CS2 reported "Failed to initialize Vulkan", and it was found
with `strace` — **no RM trace can show an `ENOENT`, because it is not an
ioctl.**

The right question is therefore: *which names does the library mention,
and do they resolve in the guest?* Both sources together, `DT_NEEDED` and
the `dlopen` names inside the binary:

    strings libcuda.so.$WANT | grep -oE 'libnvidia-[a-z0-9-]+\.so[.0-9]*'

## The reader

`scripts/showcase.sh audit` does that against a running guest: it
collects both name sources from every staged library and checks whether
each name resolves on the loader path **of its own bit width**.

    scripts/showcase.sh audit [--name NAME]

Two things about it are not cosmetic:

**Bit widths stay separate.** A 64-bit hit in `/usr/lib/x86_64-linux-gnu`
says nothing about a 32-bit client, and that is exactly where the gap was.

**It converges, it does not terminate in one pass.** Every newly staged
library brings its own `dlopen` names — `libcudadebugger` is what made
`libnvidia-opencl` and `libnvidia-vksc-core` visible at all. Run it again
after every stage until it comes back clean.

An earlier version of the reader forgot `/opt/nvrm/lib`, where `libcuda`
lives, and reported `libcuda.so.1` as missing while in truth the 64-bit
compute side had not been checked at all. A reader that cannot answer the
question still answers it.

## The state, measured 2026-08-18

Driver 610.43.03.

**64-bit: complete.** Seven names were missing and all are staged now —
`libnvidia-tileiras`, `libnvidia-nvvm70`, `libnvidia-pkcs11`,
`libnvidia-pkcs11-openssl3`, `libcudadebugger`, `libnvidia-opencl`,
`libnvidia-vksc-core`. They are in `lea_payload_stage`'s optional list
(`scripts/lib/provision.sh`): present gets staged, absent gets named and
does not abort the payload. They are capabilities (JIT fallback, tiled
raster, PKCS#11, debugger, OpenCL), not the CUDA core.

The price is real: `libnvidia-tileiras` is 101 MB and `libnvidia-nvvm70`
24 MB, which takes the payload to 369 MB.

**32-bit: the JIT chain was missing entirely.** We shipped 32-bit
`libcuda` and none of its JIT partners. `libnvidia-nvvm`,
`libnvidia-ptxjitcompiler` and `libnvidia-tileiras` are staged now, plus
an optional set that was nowhere before (`libnvidia-fbc`,
`libnvidia-encode`, `libnvcuvid`, `libnvidia-ml`, `libnvidia-opticalflow`,
`libGLESv2_nvidia`, `libGLESv1_CM_nvidia`).

**Three names cannot be fixed, and that is a driver fact.** Checked on the
host at 610.43.03: `libnvidia-nvvm70`, `libnvidia-pkcs11`(`-openssl3`),
`libcudadebugger` and `libnvidia-rtcore` have **no 32-bit build at all**.
So the reader's final output —

    == 64-bit (compute + GL payload) ==
       all referenced NVIDIA names resolve
    == 32-bit (GL payload) ==
       MISSING: libnvidia-nvvm70.so.4
       MISSING: libnvidia-pkcs11-openssl3.so.610.43.03
       MISSING: libnvidia-pkcs11.so.610.43.03

— is not an open item but a documented boundary.

`libnvidia-rtcore` is the one that matters: **a 32-bit client that enables
`VK_KHR_acceleration_structure` cannot be served.** That is a statement
about the driver, not a gap we can close, and it belongs in any later
packaging as a known limit.

## GBM selects by name, not by PCI ID

Measured separately, but it belongs here because it is the same class of
problem: something looked up by name and silently not found.

`libgbm.so.1` imports exactly two symbols from libdrm — `drmGetVersion`
and `drmFreeVersion` — and then `dlopen`s `<drivername>_gbm.so`. Our node
answers correctly:

    /dev/dri/card0      drmGetVersion name = 'nvidia-drm'
    /dev/dri/renderD128 drmGetVersion name = 'nvidia-drm'

The 64-bit backend was installed by the display staging (`lea_display_stage`); the 32-bit backend
never was. A 32-bit client therefore finds only Mesa's `dri_gbm.so`, falls
into Mesa's loader, and **that** one asks the PCI ID — which is the line
in Steam's log:

    pci id for fd 136: 1af4:107c, driver (null)

the display staging installs the 32-bit backend now, when the host has
the 32-bit userspace to stage (`lib32-nvidia-utils`; without it the
staging warns and goes on). Note this is a testable
prediction rather than an established cause: Steam has 32-bit parts and
pressure-vessel copies the gap into its container overrides, so if Steam's
window stays invisible afterwards, this was not it.

Xwayland is 64-bit and loads the backend that already existed, so this gap
does **not** explain defect 22-C. B and C stay separate.
