# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
#
# The four guest modules, in the one order that links:
#
#   nvrm_nodes      nothing
#   virtio_nvrm     nothing; exports nvidia_get_rm_ops
#   nvidia-modeset  NVIDIA's, links against virtio_nvrm instead of nvidia.ko
#   nvidia-drm      NVIDIA's, links against nvidia-modeset
#
# nvidia-modeset has an OS-agnostic half in C++ (src/nvidia-modeset) that
# kernel-open expects as a prebuilt .o_binary; NVIDIA's top-level Makefile
# would build nvidia.ko alongside it, which a guest must not have.
KDIR ?= /lib/modules/$(shell uname -r)/build
KERNEL_UNAME ?= $(shell uname -r)
ARCH_DIR := Linux_$(shell uname -m)

all:
	$(MAKE) -C nvrm_nodes KDIR=$(KDIR) modules
	$(MAKE) -C virtio_nvrm KDIR=$(KDIR)
	$(MAKE) -C nvkms/src/nvidia-modeset
	ln -sf ../../src/nvidia-modeset/_out/$(ARCH_DIR)/nv-modeset-kernel.o \
	  nvkms/kernel-open/nvidia-modeset/nv-modeset-kernel.o_binary
	$(MAKE) -C nvkms/kernel-open modules KERNEL_UNAME=$(KERNEL_UNAME) \
	  NV_KERNEL_MODULES="nvidia-modeset nvidia-drm" \
	  KBUILD_EXTRA_SYMBOLS=$(CURDIR)/virtio_nvrm/Module.symvers

clean:
	-$(MAKE) -C nvrm_nodes KDIR=$(KDIR) clean
	-$(MAKE) -C virtio_nvrm KDIR=$(KDIR) clean
	-$(MAKE) -C nvkms/src/nvidia-modeset clean
	-$(MAKE) -C nvkms/kernel-open clean KERNEL_UNAME=$(KERNEL_UNAME) \
	  NV_KERNEL_MODULES="nvidia-modeset nvidia-drm"

.PHONY: all clean
