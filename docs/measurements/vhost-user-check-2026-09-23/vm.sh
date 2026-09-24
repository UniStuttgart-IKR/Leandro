#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Launcher for the manual vhost-user VM check (patches/README.md, "Manual VM
# check"). The command lines are the ones of docs/QUICKSTART.md sections 1
# and 2 with the v1check paths, `-v` added to the hypervisor, and the size of
# the Caraxes VMs (8 vCPUs, 4 GiB shared memory).
#
#   vm.sh backend TAG    start vhost-user-nvrm; log $V/backend-TAG.log
#                        (LEA_DEBUG and LEA_TEST_SHMEM_MAP_OOB pass through)
#   vm.sh vm TAG         start the VM;  log $V/hypervisor-TAG.log, serial $V/serial-TAG.log
#   vm.sh ssh CMD...     run CMD in the guest
#   vm.sh stop           power the guest off, wait for the hypervisor, stop the backend
#
# Environment: V (VM directory), CH (hypervisor binary), IP, TAP, MAC, HOST.
set -euo pipefail
W=/home/silas/git/Caraxes/.claude/worktrees/agent-a45f20dcd340b4776
V=${V:-/mnt/vmstore/caraxes/v1check}
CH=${CH:-$W/target/ch-scratch/release/cloud-hypervisor}
NVRM=$W/target/release/vhost-user-nvrm
IMG=/mnt/vmstore/leandro/vm/guest-image
KEY=/mnt/vmstore/leandro/vm/id_leandro
IP=${IP:-192.168.105.25}
TAP=${TAP:-caraxes5}
MAC=${MAC:-02:00:00:00:00:25}

gssh() {
    ssh -i "$KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -o LogLevel=ERROR -o ConnectTimeout=5 -o BatchMode=yes "leandro@$IP" "$@"
}

case ${1:?} in
backend)
    tag=${2:?}
    rm -f "$V/nvrm.sock"
    ( ulimit -n 65536
      LEA_DEBUG="${LEA_DEBUG:-}" LEA_TEST_SHMEM_MAP_OOB="${LEA_TEST_SHMEM_MAP_OOB:-}" \
      setsid nohup "$NVRM" --nvrm "$V/nvrm.sock" >"$V/backend-$tag.log" 2>&1 </dev/null &
      echo $! >"$V/backend.pid" )
    for _ in $(seq 50); do [[ -S $V/nvrm.sock ]] && break; sleep 0.2; done
    [[ -S $V/nvrm.sock ]] || { echo "backend did not come up"; cat "$V/backend-$tag.log"; exit 1; }
    echo "backend pid $(cat "$V/backend.pid") on $V/nvrm.sock"
    ;;
vm)
    tag=${2:?}
    setsid nohup "$CH" -v --cpus boot=8 --memory size=4G,shared=on \
      --kernel "$IMG/vmlinuz" --initramfs "$IMG/initrd" \
      --cmdline 'root=/dev/vda1 rw console=ttyS0 net.ifnames=0' \
      --disk path="$V/rootfs.qcow2",image_type=qcow2,backing_files=on \
             path="$V/seed.img",readonly=on \
      --net tap="$TAP",mac="$MAC" \
      --generic-vhost-user "device_type=60,socket=$V/nvrm.sock,queue_sizes=[256,256]" \
      --serial file="$V/serial-$tag.log" --console off \
      >"$V/hypervisor-$tag.log" 2>&1 </dev/null &
    echo $! >"$V/hypervisor.pid"
    for _ in $(seq 90); do gssh true 2>/dev/null && break; sleep 2; done
    gssh true || { echo "no ssh"; tail -20 "$V/hypervisor-$tag.log"; exit 1; }
    echo "hypervisor pid $(cat "$V/hypervisor.pid"), ssh up at $IP"
    ;;
ssh)
    shift; gssh "$@"
    ;;
stop)
    gssh sudo poweroff || true
    hp=$(cat "$V/hypervisor.pid")
    for _ in $(seq 60); do kill -0 "$hp" 2>/dev/null || break; sleep 1; done
    kill -0 "$hp" 2>/dev/null && { echo "hypervisor $hp still running"; exit 1; }
    bp=$(cat "$V/backend.pid")
    if kill -0 "$bp" 2>/dev/null && [[ $(readlink "/proc/$bp/exe") == "$NVRM" ]]; then
        kill "$bp"; for _ in $(seq 20); do kill -0 "$bp" 2>/dev/null || break; sleep 0.5; done
    fi
    echo "stopped (hypervisor $hp, backend $bp)"
    ;;
*) echo "unknown command $1"; exit 2 ;;
esac
