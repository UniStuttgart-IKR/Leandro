# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# Probes: small, single-purpose instruments. Sources in c/, generated
# binaries in bin/ (gitignored -- see below), kernels in kernels/.
#
#   make            nvprobe + the PTX the gate needs
#   make probes     the CUDA-free measuring tools
#   make host-tools ONLY the two the gates build from source, no CUDA at all
#   make all-probes everything that builds on this host
#   make clean
#
# WARNING: bin/ is NOT versioned, and that is the point. Prebuilt probe
# binaries used to be checked in, and a stale one made the gate grep output
# strings that no longer existed in any source -- it went green against text
# nothing produced any more. Every consumer builds them now
# (lea_guest_setup in scripts/lib/provision.sh does so before copying).
CUDA_HOME ?= /opt/cuda
SM        ?= 75

CFLAGS  := -O1 -g -Wall -Wextra -I$(CUDA_HOME)/include
LDFLAGS := -L$(CUDA_HOME)/lib64 -lcuda
NVCC    := $(CUDA_HOME)/bin/nvcc

SRC := c
BIN := bin
KDIR := kernels

all: $(BIN)/nvprobe $(KDIR)/kernels.ptx

$(BIN):
	@mkdir -p $(BIN)

# --- CUDA probes (need the CUDA headers and libcuda) ---
$(BIN)/nvprobe $(BIN)/hostregprobe $(BIN)/oomprobe $(BIN)/managedprobe: $(BIN)/%: $(SRC)/%.c | $(BIN)
	$(CC) $(CFLAGS) $< -o $@ $(LDFLAGS)

# --- CUDA-free probes (only need /dev/nvidiactl, or nothing at all) ---
# ioctlping: round-trip latency per ioctl.
# ctrlping:  the same with a GROWING payload -- the follow-up question to
#            ioctlping (does the module's setup surcharge depend on size?).
$(BIN)/ioctlping $(BIN)/ctrlping $(BIN)/ofdtest $(BIN)/granttest: $(BIN)/%: $(SRC)/%.c | $(BIN)
	$(CC) -O1 -g -Wall -Wextra -o $@ $<

# primeimport: the display device allocates, the render device imports -- the
# direction a PRIME desktop uses, on its own. Plain DRM ioctls, no libdrm and
# no GL, so it runs on a guest whose glamor is unavailable.
$(BIN)/primeimport: $(SRC)/primeimport.c | $(BIN)
	$(CC) -O1 -g -Wall -Wextra -o $@ $<

# glinterop needs EGL and GL on top of libcuda: it asks whether CUDA accepts
# a texture from an NVIDIA GL context that was built without any DRM node.
# libEGL/libGLESv2 are GLVND, so the NVIDIA implementation is picked at
# runtime -- nothing NVIDIA-specific is linked here.
$(BIN)/glinterop: $(SRC)/glinterop.c | $(BIN)
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS) -lEGL -lGLESv2

# The two the gates build from source rather than run from bin/.
#
# -Werror, unlike everything above it: these are new files with no history of
# tolerated warnings, they are compiled by CI on every push (the `check` job
# builds this target), and the vdisplay gate compiles edid-verify.c itself
# before it reads an EDID. A warning that is allowed to accumulate here would
# turn into a gate that cannot build its own instrument.
#
# edid-verify: reads an EDID with a parser that shares no code with the
#              builder that wrote it, which is what makes the display gates'
#              byte comparison against the module's OWN builder mean
#              something. No CUDA, no DRM, no hardware -- it reads a file.
# vdisp-frame: writes one deterministic frame onto the virtual display's
#              CRTC and reads it back through GETFB2 + PRIME export.
#              Needs DRM and root, i.e. it runs IN THE GUEST -- except for
#              `--reference WxH`, which only hashes the pattern and is how
#              data/vdisp-frame.ref is generated on a machine with no GPU.
$(BIN)/edid-verify $(BIN)/vdisp-frame: $(BIN)/%: $(SRC)/%.c | $(BIN)
	$(CC) -O2 -g -Wall -Wextra -Werror -o $@ $<

# vkalloc needs only the Vulkan loader: it asks which memory types the guest
# can actually allocate from, after `vulkaninfo` started listing the card but
# nothing could put anything in memory.
$(BIN)/vkalloc: $(SRC)/vkalloc.c | $(BIN)
	$(CC) -O1 -g -Wall -Wextra -o $@ $< -lvulkan

# forkprobe links against the libcuda STUB: it measures what a fork does to
# the driver state, and must not drag the real library in.
$(BIN)/forkprobe: $(SRC)/forkprobe.c | $(BIN)
	$(CC) -O1 -g -Wall -I$(CUDA_HOME)/include -o $@ $< -L$(CUDA_HOME)/lib64/stubs -lcuda

# Sets only the sharing flag in UVM_INITIALIZE. It is preloaded into a CUDA
# probe but needs no CUDA itself (dlsym and one ioctl), so it belongs in the
# CUDA-free set -- and `all-probes` picks it up through `probes`.
$(BIN)/uvminit_shim.so: $(SRC)/uvminit_shim.c | $(BIN)
	$(CC) -O1 -g -Wall -Wextra -shared -fPIC -o $@ $< -ldl

# PTX rather than a cubin, deliberately: the JIT is part of what has to run
# unchanged in the guest later. Pre-compiling here would stop measuring it.
#
# The .ptx IS versioned, unlike the binaries: it is portable text, and
# keeping it means the gate does not need nvcc on the machine it runs on.
#
# The SPDX line is put back AFTER nvcc, because nvcc does not emit one and
# the file is checked in: without this the licence step of `test.sh check`
# goes red the first time anybody regenerates the .ptx, naming a file
# nobody edited (that happened once: a regenerated .ptx lost its header
# while a stale one kept it). PTX takes // comments, so this is a comment
# in the language, not a prelude the JIT has to tolerate.
$(KDIR)/kernels.ptx: $(KDIR)/kernels.cu
	$(NVCC) -ptx -arch=sm_$(SM) $< -o $@.tmp
	printf '// SPDX-License-Identifier: MIT\n' > $@
	cat $@.tmp >> $@
	rm -f $@.tmp

probes: $(BIN)/ofdtest $(BIN)/granttest $(BIN)/forkprobe $(BIN)/ioctlping $(BIN)/ctrlping $(BIN)/primeimport $(BIN)/uvminit_shim.so host-tools

# What CI can build: no CUDA, no NVIDIA headers, no hardware. Kept separate
# from `probes` so the GPU-free workflow has exactly one target to name.
host-tools: $(BIN)/edid-verify $(BIN)/vdisp-frame

all-probes: all probes $(BIN)/hostregprobe $(BIN)/oomprobe $(BIN)/managedprobe $(BIN)/glinterop $(BIN)/vkalloc

clean:
	rm -rf $(BIN)

distclean: clean
	rm -rf traces

.PHONY: all probes host-tools all-probes clean distclean
