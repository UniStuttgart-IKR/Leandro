# SPDX-License-Identifier: GPL-2.0-only
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# nvrm_nodes guest module: build inside the GUEST (needs linux-headers-$(uname -r)).
#
#   make
#   sudo insmod nvrm_nodes.ko
#   sudo ./nvrm-nodes-tool provision params params.txt
#   ./nvrm-nodes-tool gpa 4          # as an ordinary user -- that is the point
obj-m := nvrm_nodes.o
nvrm_nodes-y := nvrm_nodes_main.o

KDIR ?= /lib/modules/$(shell uname -r)/build

all: modules nvrm-nodes-tool

modules:
	$(MAKE) -C $(KDIR) M=$(CURDIR) modules

nvrm-nodes-tool: nvrm-nodes-tool.c nvrm_nodes_uapi.h
	$(CC) -O1 -g -Wall -Wextra -o $@ $<

clean:
	$(MAKE) -C $(KDIR) M=$(CURDIR) clean
	rm -f nvrm-nodes-tool

.PHONY: all modules clean
