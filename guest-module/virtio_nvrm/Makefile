# SPDX-License-Identifier: GPL-2.0-only
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# Build inside the GUEST (that is where the kernel headers are):
#   make -C ~/guest-module/virtio_nvrm
#
# nvrm_wire.h is generated but checked in -- that is why the guest needs no
# Rust toolchain. Anyone changing the wire structs regenerates it on the host:
#   cargo run --release --bin nvrm-genhdr -- guest-module/virtio_nvrm/nvrm_wire.h
obj-m += virtio_nvrm.o

KDIR ?= /lib/modules/$(shell uname -r)/build
PWD  := $(shell pwd)

all:
	$(MAKE) -C $(KDIR) M=$(PWD) modules

clean:
	$(MAKE) -C $(KDIR) M=$(PWD) clean

.PHONY: all clean
