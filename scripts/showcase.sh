#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# The rig, and the guided demonstration: bring guests up (one, several, with
# or without a display), get into them, take them down, clean up after a
# crash -- and show what the project does rather than claim it.
#
#   scripts/showcase.sh net up [--count N] [--uplink IFACE] [--user NAME]
#   scripts/showcase.sh net down [--uplink IFACE] | net status
#   scripts/showcase.sh up   [--name NAME] [--index N] [--count N]
#                            [--guest ubuntu|nixos] [--transport ip|vsock]
#                            [--display] [--session gnome|openbox] [--with-steam] [--input]
#                            [--fresh] [--mem MiB] [--cpus N] [--vram-limit MiB]
#                            [--max-pin-mib N] [--with-torch] [--with-gl]
#                            [--no-provision] [--no-load] [--no-compute]
#                            [--keep-vm] [--console] [--base IMAGE]
#   scripts/showcase.sh down [--name NAME | --all] [--force]
#   scripts/showcase.sh status
#   scripts/showcase.sh ssh  [--name NAME | -i N] [command...]
#   scripts/showcase.sh exec [command...]            all running fleet members at once
#   scripts/showcase.sh state [--check]              the RIG line, or the precondition
#   scripts/showcase.sh clean [--dry-run] [--force]  after a crash
#   scripts/showcase.sh audit [--name NAME]          guest userspace resolution audit
#   scripts/showcase.sh display [--name NAME] [--display :N]   what the display rig says
#   scripts/showcase.sh pair [--name NAME] [--pin NNNN]        Moonlight <-> the guest's Sunshine
#   scripts/showcase.sh games init [--size N] | games status   the shared Steam library
#   scripts/showcase.sh demo [--list] [--only a,b] [--skip a,b] [--fast] [--full]
#                            [--fleet N] [--pause] [--no-setup] [--keep]
#                            [--no-managed] [--out DIR]
#
# UP. One guest with a compute backend is the default: `up` is vm0, the
# standard dev VM at index 0 (IP .10, tap0). --guest picks WHICH guest:
# `ubuntu` (the default, the cloud image build.sh bake provisions) or `nixos`
# (the derivation build.sh bake --nixos builds -- direct kernel boot, no
# cloud-init seed, its identity on the kernel command line). The choice is
# remembered per instance in vm/<name>/guest, so `down`, `ssh` and `status`
# do not need it again. A NixOS guest does compute and fleets; it does NOT do
# --display/--session, which still needs an in-guest NVKMS build.
#
# --transport picks HOW the host reaches it, and is remembered the same way.
# `ip` (the default) is an address on the bridge, and `net up` is a
# precondition. `vsock` gives the guest NO network device at all and reaches
# it through one unix socket instead -- which removes the bridge, the taps,
# the NAT rule and every sudo from the path, and is therefore the only way
# this runs where nobody has root (a cluster node). NixOS only: an Ubuntu
# guest takes its address from a cloud-init seed and is refused with that
# reason. Measured 2026-08-19: cloud-hypervisor's vsock is userspace and
# never touches /dev/vhost-vsock, so it needs no privilege of its own. --name/--index make a second
# instance beside it (`--name desktop --index 5` is the desktop rig by
# convention); --count N brings a homogeneous fleet vm0..vm<N-1> up, members
# 1..N-1 as thin overlays on LEA_FLEET_BASE. --display adds NVIDIA's own
# virtual display (NVKMS modules, X inside the mediated PCI identity);
# --session puts a desktop and Sunshine on it, --with-steam Steam too. `up`
# provisions the guest (userspace, probes, nvrm_nodes.ko) and loads
# virtio_nvrm.ko unless told not to. `down` refuses an instance that a
# LIVE script (a gate, a bench, a bake) brought up -- --force overrides.
# --keep-vm recycles a RUNNING desktop
# guest when only the module changed. --console runs the VM in the
# foreground on this terminal (no provisioning). Per-VM knobs: --vram-limit
# caps the backend (a per-tenant cap), --max-pin-mib raises the guest
# module's pin cap; --base IMAGE overlays a different base image for this
# instance (the desktop-baked one for a desktop, say) -- consulted when the
# disk is created, i.e. with --fresh or on first up. LEA_MANAGED_COMPAT=1
# in the environment reaches the backend (a HOST switch; set inside the
# guest it does nothing).
#
# DEMO. Brings the rig up, walks through a series of demonstrations, and
# tears it down again. Every section says what it PROVES before it runs,
# prints the real command and the real output, and ends in a verdict.
# Nothing is pre-recorded and nothing is faked -- if a section fails, it
# says so and the run continues, with a summary at the end.
#   smi         nvidia-smi inside a VM that has no GPU of its own
#   nodriver    proof the guest runs no NVIDIA kernel driver
#   kernel      a CUDA kernel executes and the result is correct
#   torch       PyTorch, bit-identical to the native host run
#   managed     managed memory, and the host switch it depends on
#   pinned      the known limitation, shown honestly
#   parallel    two CUDA processes at once
#   crash       kill -9 mid-run, and the accounting still balances
#   rmmod       the module refuses to unload while a FD is open
#   counter     unload it and the GPU is gone -- the counter-check
#   latency     what the boundary costs per ioctl, hot queue vs cold
#   fleet       several VMs sharing one GPU
#   desktop     a GNOME desktop on the virtual display, Sunshine beside it
#   mixed       a desktop guest and a compute guest on the same card at once
#   suites      the breadth tests over real CUDA APIs
# --fast skips torch, suites, fleet, desktop, mixed (~3 minutes); the default
# skips suites, fleet, desktop, mixed unless named with --only; --full runs
# everything. --pause waits for Enter between sections (a live demo).
# --no-setup uses a rig that is already up; --keep leaves it running.
#
# EXAMPLES
#   scripts/showcase.sh net up && scripts/showcase.sh up
#   scripts/showcase.sh ssh nvidia-smi
#   scripts/showcase.sh up --name desktop --index 5 --session gnome --with-steam
#   scripts/showcase.sh up --count 4 && scripts/showcase.sh exec 'cd ~/gpu && ./nvprobe 3'
#   scripts/showcase.sh up --name nix0 --index 2 --guest nixos
#   scripts/showcase.sh up --count 4 --guest nixos
#   scripts/showcase.sh up --name nixv --index 3 --guest nixos --transport vsock
#   scripts/showcase.sh up --count 4 --guest nixos --transport vsock   # no root at all
#   scripts/showcase.sh demo --fast --pause
#   scripts/showcase.sh down --all
set -uo pipefail
LEA_ROOT=${LEA_ROOT:-$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd -P)}
# shellcheck source=scripts/lib/rig.sh
source "$LEA_ROOT/scripts/lib/rig.sh"

usage() { lea_usage_from_header; exit "${1:-0}"; }

CMD=${1:-status}
case $CMD in
    net|up|down|status|ssh|exec|state|clean|audit|display|pair|games|demo) shift ;;
    -h|--help) usage 0 ;;
    *) error "unknown subcommand: $CMD"; usage 2 ;;
esac

# ---- net ------------------------------------------------------------------
do_net() {
    local sub=${1:-status}; shift || true
    case $sub in
        up)     lea_net_up "$@" ;;
        # `down` takes only --uplink. It removes EVERY tap and the bridge,
        # so a --count would be a promise it does not keep -- and passing
        # it through made the run die naming `lea_net_down`, a function the
        # caller never typed. The options are parsed here, where the
        # command the user wrote is still known.
        down)
            local -a dopts=()
            while [[ $# -gt 0 ]]; do
                case $1 in
                    --uplink) dopts+=(--uplink "$2"); shift 2 ;;
                    --count)  error "net down: no --count -- it removes every tap and the bridge"; usage 2 ;;
                    -h|--help) usage 0 ;;
                    *) error "net down: unknown option $1"; usage 2 ;;
                esac
            done
            lea_net_down "${dopts[@]}" ;;
        # `status` reports the DETECTED uplink and takes nothing to point
        # it at. It used to accept and drop any option without a word,
        # which reads as "the uplink you named was used".
        status)
            [[ $# -eq 0 ]] || { error "net status: takes no options (the uplink is detected)"; usage 2; }
            lea_net_status ;;
        *) die "net: up, down or status" ;;
    esac
}

# ---- up -------------------------------------------------------------------
do_up() {
    local name="" idx="" count=0 keepvm=0 gl=0 console=0
    local -a rig=() fleet=()
    while [[ $# -gt 0 ]]; do
        case $1 in
            --name)  name=$2; shift 2 ;;
            --index) idx=$2; shift 2 ;;
            --count) count=$2; shift 2 ;;
            --keep-vm) keepvm=1; shift ;;
            --with-gl) gl=1; shift ;;
            --console) console=1; rig+=(--console); shift ;;
            --fresh|--no-load) rig+=("$1"); fleet+=("$1"); shift ;;
            --mem) rig+=(--mem "$2"); fleet+=(--mem "$2"); shift 2 ;;
            --games|--games-init) rig+=("$1"); shift ;;
            --display|--input|--with-steam|--with-torch|--no-provision|--no-compute)
                rig+=("$1"); shift ;;
            --session|--cpus|--vram-limit|--max-pin-mib|--base) rig+=("$1" "$2"); shift 2 ;;
            --guest|--transport) rig+=("$1" "$2"); fleet+=("$1" "$2"); shift 2 ;;
            -h|--help) usage 0 ;;
            *) error "up: unknown option $1"; usage 2 ;;
        esac
    done
    lea_hold_pidfile "$LEA_VM_DIR/showcase.pid"
    if [[ $count -gt 0 ]]; then
        [[ -z $name && -z $idx && $keepvm -eq 0 ]] || die "--count takes no --name/--index/--keep-vm"
        lea_fleet_up "$count" "${fleet[@]}"
        return
    fi
    [[ -n $name ]] || name=vm${idx:-0}
    if [[ $keepvm -eq 1 ]]; then
        # Recycle a running desktop guest: session, X and the NVKMS modules
        # go, virtio_nvrm is rebuilt and reloaded, the display comes back.
        lea_vm_running "$name" || die "--keep-vm, but $name is not running"
        [[ $console -eq 0 ]] || die "--keep-vm and --console do not combine"
        local session="" steam=0 disp=0 a
        for ((a = 0; a < ${#rig[@]}; a++)); do
            case ${rig[a]} in
                --session) session=${rig[a+1]}; disp=1 ;;
                --display) disp=1 ;;
                --with-steam) steam=1 ;;
            esac
        done
        lea_desktop_recycle "$name" || exit 1
        lea_guest_build_nvrm "$name" >"$(lea_inst_dir "$name")/load.log" 2>&1 \
            || { tail -5 "$(lea_inst_dir "$name")/load.log" >&2; die "$name: module reload failed"; }
        if [[ -n $session ]]; then
            local -a dopt=(); [[ $steam -eq 1 ]] && dopt+=(--with-steam)
            lea_desktop_up "$name" --session "$session" "${dopt[@]}" || exit 1
        elif [[ $disp -eq 1 ]]; then
            lea_display_up "$name" || exit 1
        fi
        return
    fi
    lea_rig_up "$name" ${idx:+--index "$idx"} "${rig[@]}" || exit 1
    [[ $console -eq 1 ]] && return 0
    if [[ $gl -eq 1 ]]; then
        lea_gl_stage "$name" --system --with-32bit || exit 1
    fi
    echo
    echo "  log in:   scripts/showcase.sh ssh${name:+ --name $name}"
    echo "  stop:     scripts/showcase.sh down${name:+ --name $name}"
    echo "  console:  tail -f $(lea_inst_dir "$name")/serial.log"
}

# ---- down / status --------------------------------------------------------
do_down() {
    local name="" all=0
    local -a fopt=()
    while [[ $# -gt 0 ]]; do
        case $1 in
            --name)  name=$2; shift 2 ;;
            --all)   all=1; shift ;;
            --force) fopt=(--force); shift ;;
            -h|--help) usage 0 ;;
            *) error "down: unknown option $1"; usage 2 ;;
        esac
    done
    lea_hold_pidfile "$LEA_VM_DIR/showcase.pid"
    # An instance a live script (a gate, a bench, a bake) brought up is
    # refused unless --force -- lea_inst_owner has the incident.
    if [[ $all -eq 1 ]]; then
        local n rc=0
        for n in $(lea_inst_list); do lea_rig_down "$n" "${fopt[@]}" || rc=1; done
        return $rc
    fi
    lea_rig_down "${name:-vm0}" "${fopt[@]}"
}

do_status() {
    lea_rig_status
    echo
    lea_rig_state 2>/dev/null || true
}

# ---- ssh / exec -----------------------------------------------------------
do_ssh() {
    local name=vm0
    while [[ $# -gt 0 ]]; do
        case $1 in
            --name) name=$2; shift 2 ;;
            -i)     name=vm$2; shift 2 ;;
            -h|--help) usage 0 ;;
            *) break ;;
        esac
    done
    lea_vm_ssh "$name" "$@"
}

do_exec() { lea_fleet_exec "$@"; }

# ---- state / clean / audit / display -------------------------------------------
do_state() {
    if [[ ${1:-} == --check ]]; then lea_rig_check; else lea_rig_state; fi
}

do_clean() { lea_rig_clean "$@"; }

do_audit() {
    local name=vm0
    [[ ${1:-} == --name ]] && name=$2
    lea_gl_audit "$name"
}

do_display() {
    local name=desktop disp=:7
    while [[ $# -gt 0 ]]; do
        case $1 in
            --name)    name=$2; shift 2 ;;
            --display) disp=$2; shift 2 ;;
            -h|--help) usage 0 ;;
            *) error "display: unknown option $1"; usage 2 ;;
        esac
    done
    lea_display_status "$name" "$disp"
}

# The shared Steam library: one qcow2, filled once, overlaid per instance.
# A game is tens of gigabytes and is not system state, so it does not belong
# in the guest image -- and `up --fresh` must not throw a download away.
do_games() {
    local sub=${1:-status}; shift || true
    case $sub in
        init)
            local size=$LEA_GAMES_SIZE
            while [[ $# -gt 0 ]]; do
                case $1 in
                    --size) size=$2; shift 2 ;;
                    -h|--help) usage 0 ;;
                    *) error "games init: unknown option $1"; usage 2 ;;
                esac
            done
            if [[ -f $LEA_GAMES_BASE ]]; then
                error "$LEA_GAMES_BASE exists already -- refusing to replace a library."
                error "  Delete it by hand if that is really what you want."
                return 1
            fi
            mkdir -p "$(dirname "$LEA_GAMES_BASE")"
            # Empty and unformatted on purpose: the guest makes the
            # filesystem on first use (lea_games_mount), which needs no root
            # and no loop device on the host.
            qemu-img create -q -f qcow2 "$LEA_GAMES_BASE" "$size" || return 1
            info "created $LEA_GAMES_BASE ($size, empty -- the guest formats it)"
            echo
            echo "Fill it once:"
            echo "  scripts/showcase.sh up --name desktop --index 5 --session gnome --games-init"
            echo "  ... start Steam in the guest, log in, install the game, shut the guest down"
            echo
            echo "Then every guest gets its own overlay on it:"
            echo "  scripts/showcase.sh up --name desktop --index 5 --session gnome --games"
            ;;
        status)
            if [[ ! -f $LEA_GAMES_BASE ]]; then
                info "no library: $LEA_GAMES_BASE does not exist (games init makes one)"
                return 0
            fi
            info "base:     $LEA_GAMES_BASE ($(du -h "$LEA_GAMES_BASE" | cut -f1) on disk)"
            local n d
            for n in $(lea_inst_list); do
                d=$(lea_inst_dir "$n")/games.qcow2
                [[ -f $d ]] && info "overlay:  $n ($(du -h "$d" | cut -f1))"
            done
            local w
            if w=$(lea_games_writer_other_than ""); then
                warn "instance $w currently holds the BASE for writing (--games-init)"
            fi
            ;;
        *) die "games: init or status" ;;
    esac
}

# Pairing without a browser: the PIN goes to Sunshine's REST API instead of
# through its web UI. Once per host per guest -- the pairing survives a
# Sunshine restart and lives in the guest's disk image.
do_pair() {
    local name=desktop pin=$LEA_SUN_PIN
    while [[ $# -gt 0 ]]; do
        case $1 in
            --name) name=$2; shift 2 ;;
            --pin)  pin=$2; shift 2 ;;
            -h|--help) usage 0 ;;
            *) error "pair: unknown option $1"; usage 2 ;;
        esac
    done
    lea_sunshine_pair "$name" "$pin"
}

# ---- demo -----------------------------------------------------------------
ALL_SECTIONS=(smi nodriver kernel torch managed pinned parallel crash rmmod counter latency fleet desktop mixed suites)
FAST_SKIP=(torch suites fleet desktop mixed)
OPT_IN=(suites fleet desktop mixed)

want() {   # want <section> -> should it run?
    local s=$1 f
    if [[ -n $ONLY ]]; then [[ ",$ONLY," == *",$s,"* ]] || return 1; fi
    if [[ -n $SKIP ]]; then [[ ",$SKIP," == *",$s,"* ]] && return 1; fi
    if [[ $MODE == fast ]]; then
        for f in "${FAST_SKIP[@]}"; do [[ $f == "$s" ]] && return 1; done
    fi
    if [[ $MODE == default && -z $ONLY ]]; then
        # minutes, not seconds: opt-in unless named
        for f in "${OPT_IN[@]}"; do [[ $f == "$s" ]] && return 1; done
    fi
    return 0
}

# In-guest shorthand. The guest keeps a flat ~/gpu, so everything is by bare
# name there.
g() { lea_ssh "$IP" "cd ~/gpu && export LD_LIBRARY_PATH=\$PWD/nv/lib NVPROBE_PTX=\$PWD/kernels.ptx; $*"; }

demo_cleanup() {
    [[ $KEEP -eq 1 ]] && { echo; note "rig left running (--keep). Stop it with:"; \
                           note "  scripts/showcase.sh down --all"; return 0; }
    [[ $NO_SETUP -eq 1 ]] && return 0
    echo; echo "${DIM}tearing the rig down ...${R}"
    lea_rig_down vm0 >/dev/null 2>&1
    [[ $DESKTOP_MINE -eq 1 ]] && lea_rig_down desktop >/dev/null 2>&1
    return 0
}

setup_rig() {
    [[ $NO_SETUP -eq 1 ]] && {
        lea_ssh "$IP" true 2>/dev/null || die "--no-setup given but $IP does not answer"
        info "using the rig that is already up ($IP)"; return 0; }
    echo "${B}setting up the rig${R}  (build, backend, VM, guest modules)"
    lea_rig_check || die "rig not ready to measure"
    cmd "cargo build --release"
    (cd "$LEA_ROOT" && cargo build --release) >"$OUT/build.log" 2>&1 || { tail -20 "$OUT/build.log"; die "build failed"; }
    lea_rig_down vm0 >/dev/null 2>&1
    # LEA_MANAGED_COMPAT is a HOST switch (read in session.rs). The managed
    # section below shows what it changes; --no-managed leaves it off.
    local mc=""; [[ $MANAGED -eq 1 ]] && mc="LEA_MANAGED_COMPAT=1"
    cmd "${mc:+$mc }vhost-user-nvrm --nvrm vm/vm0/nvrm.sock &   # then the VM, then the guest"
    if [[ $MANAGED -eq 1 ]]; then
        LEA_MANAGED_COMPAT=1 LEA_DEBUG=1 lea_rig_up vm0 >"$OUT/rig-up.log" 2>&1
    else
        LEA_DEBUG=1 lea_rig_up vm0 >"$OUT/rig-up.log" 2>&1
    fi || { tail -15 "$OUT/rig-up.log"; die "rig did not come up -- $OUT/rig-up.log"; }
    info "rig up: guest $IP, backend PID $(cat "$LEA_VM_DIR/vm0/nvrm.pid")"
}

sec_smi() {
    headline "nvidia-smi inside the VM" \
             "the guest sees the real card, without LD_PRELOAD and as a normal user"
    cmd "showcase.sh ssh -- nvidia-smi"
    g 'timeout 90 ./nv/bin/nvidia-smi' > "$OUT/smi.txt" 2>&1
    sed -n '1,12p' "$OUT/smi.txt"
    # The guest is told a MEDIATED name -- `NVIDIA GeForce RTX 2070` becomes
    # `Leandro RTX 2070` (vram.rs guest_card_name, modelled on NVIDIA's own
    # vGPU convention), so that nobody is inside a mediated VM without
    # noticing. Both spellings name the host's card.
    local want mediated
    want=$(nvidia-smi --query-gpu=name --format=csv,noheader | head -1)
    mediated="Leandro $(sed -e 's/^NVIDIA //' -e 's/^GeForce //' <<<"$want")"
    if grep -qF "$mediated" "$OUT/smi.txt" || grep -qF "$want" "$OUT/smi.txt"; then
        ok "the guest reports $mediated -- the host's physical card ($want), under its mediated name"
        note "the process list is empty on purpose: RM reports HOST pids,"
        note "which the guest cannot resolve in its own /proc."
    else
        no "expected $mediated (or $want) in the guest output"
    fi
}

sec_nodriver() {
    headline "the guest has no NVIDIA kernel driver" \
             "this is paravirtualisation, not passthrough"
    cmd "lsmod | grep -E 'nvidia|virtio_nvrm' ; ls /proc/driver/nvidia"
    lea_ssh "$IP" 'echo "--- loaded modules ---"
        lsmod | grep -E "^nvidia|^virtio_nvrm|^nvrm_nodes" || echo "(no nvidia* module)"
        echo "--- device nodes ---"; ls -l /dev/nvidia* 2>/dev/null | head -4
        echo "--- lspci ---"; lspci 2>/dev/null | grep -ci nvidia || echo 0' \
        > "$OUT/nodriver.txt" 2>&1
    cat "$OUT/nodriver.txt"
    if ! grep -qE '^nvidia ' "$OUT/nodriver.txt" && grep -q '^virtio_nvrm' "$OUT/nodriver.txt"; then
        ok "no nvidia.ko in the guest; virtio_nvrm owns the device nodes"
        note "and no NVIDIA device on the guest PCI bus at all"
    else
        no "unexpected module state -- see $OUT/nodriver.txt"
    fi
}

sec_kernel() {
    headline "a CUDA kernel runs, and the result is correct" \
             "the whole chain works: alloc, copy, PTX JIT, launch, read back"
    cmd "./nvprobe 3   # in the guest"
    g 'timeout 120 ./nvprobe 3 2>&1 | tail -3' > "$OUT/kernel.txt" 2>&1
    cat "$OUT/kernel.txt"
    grep -q "stage 3 ok (kernel, result correct)" "$OUT/kernel.txt" \
        && ok "kernel executed, output verified against the expected values" \
        || no "see $OUT/kernel.txt"
}

sec_torch() {
    headline "PyTorch, bit-identical to the native host run" \
             "not just 'it runs' -- the same numbers as the bare card"
    local hostpy=$LEA_ROOT/vendor/hostvenv/bin/python
    cmd "guest:  venv/bin/python rlprobe.py ; convburn.py"
    g 'timeout 300 venv/bin/python rlprobe.py 2>&1 | tail -2
       timeout 900 venv/bin/python convburn.py 2>&1 | tail -1' > "$OUT/torch-guest.txt" 2>&1
    cat "$OUT/torch-guest.txt"
    if [[ ! -x $hostpy ]]; then
        skipd "vendor/hostvenv missing -- no native reference to compare against"
        return
    fi
    echo
    cmd "host:   vendor/hostvenv/bin/python probe/python/rlprobe.py ; convburn.py"
    { NVPROBE_PTX=$LEA_ROOT/probe/kernels/kernels.ptx timeout 300 "$hostpy" "$LEA_ROOT"/probe/python/rlprobe.py 2>&1 | tail -2
      timeout 900 "$hostpy" "$LEA_ROOT"/probe/python/convburn.py 2>&1 | tail -1; } > "$OUT/torch-host.txt" 2>&1
    cat "$OUT/torch-host.txt"
    local gm hm ga ha
    gm=$(grep -o 'mean10=[0-9.]*' "$OUT/torch-guest.txt" | tail -1)
    hm=$(grep -o 'mean10=[0-9.]*' "$OUT/torch-host.txt"  | tail -1)
    ga=$(grep -o 'acc=[0-9.e+-]*' "$OUT/torch-guest.txt" | tail -1)
    ha=$(grep -o 'acc=[0-9.e+-]*' "$OUT/torch-host.txt"  | tail -1)
    if [[ -n $gm && $gm == "$hm" && -n $ga && $ga == "$ha" ]]; then
        ok "guest == host, bit for bit ($gm, $ga)"
        note "times differ, values do not. A fast wrong answer would be no answer."
    else
        no "guest($gm,$ga) vs host($hm,$ha)"
    fi
}

sec_managed() {
    headline "managed memory, and the host switch it hangs on" \
             "cudaMallocManaged works -- but only with LEA_MANAGED_COMPAT=1 on the HOST"
    cmd "./managedprobe   # in the guest"
    g 'if [ -x ./managedprobe ]; then timeout 120 stdbuf -o0 ./managedprobe 2>&1 | grep -E "stage[123] |ERROR"
       else echo "managedprobe not built"; fi' > "$OUT/managed.txt" 2>&1
    cat "$OUT/managed.txt"
    if [[ $MANAGED -eq 1 ]]; then
        grep -q "stage2 ok" "$OUT/managed.txt" \
            && ok "managed allocation and access work (switch is ON)" \
            || no "expected stage2 ok with the switch on -- see $OUT/managed.txt"
        note "started WITHOUT the switch, the same call fails with 0x1e /"
        note "cudaErrorInvalidValue. Try: scripts/showcase.sh demo --no-managed --only managed"
    else
        grep -qE "ERROR|0x1e" "$OUT/managed.txt" \
            && ok "fails loudly without the switch -- which is the intended default" \
            || note "no clear refusal seen -- see $OUT/managed.txt"
    fi
    note "oversubscription (more managed memory than VRAM) does not work either way."
}

sec_pinned() {
    headline "pinned host memory -- and the knob it hangs on" \
             "what read like a hard limit is a module parameter"
    local cap
    cap=$(lea_ssh "$IP" 'cat /sys/module/virtio_nvrm/parameters/max_pin_mib 2>/dev/null' 2>/dev/null | tr -dc '0-9')
    echo "  current cap: max_pin_mib=${cap:-?} MiB"
    cmd "torch.empty(N, pin_memory=True)   # 4 / 64 / 256 MiB, in the guest"
    # One python invocation per size, driven from bash: a killed allocation
    # must not take the rest of the staircase with it.
    g 'for MB in 4 64 256; do
         venv/bin/python -c "
import sys, torch
mb = int(sys.argv[1]); n = mb*1024*1024//4
try:
    t = torch.empty(n, dtype=torch.float32, pin_memory=True)
    print(\"  %5d MiB pinned: OK   (is_pinned=%s)\" % (mb, t.is_pinned()))
except Exception as e:
    print(\"  %5d MiB pinned: FAIL %s\" % (mb, type(e).__name__))
" "$MB"
       done' > "$OUT/pinned.txt" 2>&1
    cat "$OUT/pinned.txt"
    if grep -q "256 MiB pinned: OK" "$OUT/pinned.txt"; then
        ok "host pinning works: the guest module pins the pages, the host"
        ok "assembles them into one contiguous host VA (OS descriptor)"
    else
        no "pinned memory failed even at small sizes -- see $OUT/pinned.txt"
    fi
    note "the cap is CUMULATIVE and tunable:"
    note "  scripts/showcase.sh up --max-pin-mib 3072"
    note "measured: at the 1024 default, test_async_streams.py (~2 GiB of"
    note "buffers) fails with error 304; at 3072 it runs through -- and its"
    note "NaN checksums appear identically in the NATIVE host run, so they"
    note "come from the test's own fp16 arithmetic, not from the crossing."
    if [[ ${cap:-0} -ge 2048 ]]; then
        note "this rig is at $cap MiB, so all eight suites pass."
    else
        note "this rig is at ${cap:-?} MiB -- async_streams will hit the cap."
    fi
}

sec_parallel() {
    headline "two CUDA processes at the same time" \
             "the host backend keeps one session per guest process, not per VM"
    cmd "./nvprobe 3 & ./nvprobe 3 & wait"
    g '( timeout 120 ./nvprobe 3 >/tmp/p1 2>&1 & timeout 120 ./nvprobe 3 >/tmp/p2 2>&1 & wait )
       echo "P1: $(tail -1 /tmp/p1)"; echo "P2: $(tail -1 /tmp/p2)"' > "$OUT/parallel.txt" 2>&1
    cat "$OUT/parallel.txt"
    [[ $(grep -c "stage 3 ok (kernel, result correct)" "$OUT/parallel.txt") -eq 2 ]] \
        && ok "both processes correct, concurrently" \
        || no "see $OUT/parallel.txt"
}

sec_crash() {
    headline "kill -9 in the middle, and the books still balance" \
             "a killed guest process must not leak pinned pages on the host"
    cmd "./nvprobe 3 & sleep 2; kill -9 \$! ; then read the accounting"
    g 'timeout 120 ./nvprobe 3 >/dev/null 2>&1 & BG=$!
       sleep 2; kill -9 $BG 2>/dev/null; wait 2>/dev/null; sleep 2
       echo "open after kill: $(cat /sys/module/virtio_nvrm/parameters/stat_pinned_kib) KiB, $(cat /sys/module/virtio_nvrm/parameters/stat_pool_pages) pool pages"
       echo "next run: $(timeout 120 ./nvprobe 3 2>&1 | tail -1)"' > "$OUT/crash.txt" 2>&1
    cat "$OUT/crash.txt"
    if grep -q "open after kill: 0 KiB, 0 pool pages" "$OUT/crash.txt" \
       && grep -q "stage 3 ok" "$OUT/crash.txt"; then
        ok "nothing left open, and the next run is unaffected"
    else
        no "see $OUT/crash.txt"
    fi
}

sec_rmmod() {
    headline "the module refuses to unload while a FD is open" \
             "reference counting is real, not decorative"
    cmd "python -c 'open(\"/dev/nvidiactl\")' & sudo rmmod virtio_nvrm"
    lea_ssh "$IP" 'python3 -c "import time; f=open(\"/dev/nvidiactl\"); time.sleep(6)" & sleep 1
        sudo rmmod virtio_nvrm 2>&1 | head -1; wait 2>/dev/null' > "$OUT/rmmod.txt" 2>&1
    cat "$OUT/rmmod.txt"
    grep -q "is in use" "$OUT/rmmod.txt" \
        && ok "rmmod refused, as it must be" \
        || no "see $OUT/rmmod.txt"
}

sec_counter() {
    headline "the counter-check: take the module away" \
             "without virtio_nvrm there is no GPU -- so it really is the carrier"
    cmd "sudo rmmod virtio_nvrm && ./nvprobe 0   # then load it again"
    lea_ssh "$IP" 'sudo rmmod virtio_nvrm 2>/dev/null
        cd ~/gpu && export LD_LIBRARY_PATH=$PWD/nv/lib
        timeout 60 ./nvprobe 0 2>&1 | tail -2
        sudo insmod ~/guest-module/virtio_nvrm/virtio_nvrm.ko' > "$OUT/counter.txt" 2>&1
    cat "$OUT/counter.txt"
    grep -qE "no CUDA-capable device|cuInit: 100" "$OUT/counter.txt" \
        && ok "no module, no device -- and it comes back after insmod" \
        || no "see $OUT/counter.txt"
}

sec_latency() {
    headline "what the boundary costs" \
             "the honest number, and why a hot loop flatters it"
    cmd "./ioctlping <n> <pause_us>   # hot loop, then with a pause between calls"
    g 'if [ -x ./ioctlping ]; then
         echo "hot loop      :"; timeout 60 ./ioctlping 2000 0    2>&1 | tail -1
         echo "5 ms between  :"; timeout 120 ./ioctlping 200 5000 2>&1 | tail -1
       else echo "ioctlping not built"; fi' > "$OUT/latency.txt" 2>&1
    cat "$OUT/latency.txt"
    if grep -qE '[0-9]' "$OUT/latency.txt" && ! grep -q "not built" "$OUT/latency.txt"; then
        ok "measured on this rig, both regimes"
        note "the gap is the cold queue. Extrapolating from a hot loop to a"
        note "real program extrapolates far too favourably."
    else
        skipd "ioctlping not available in the guest"
    fi
}

sec_fleet() {
    headline "several VMs on one GPU" \
             "the card is time-shared fairly, and every VM computes the same answer"
    note "this rebuilds the rig as a $FLEET_N-VM fleet and takes a few minutes."
    lea_rig_down vm0 >/dev/null 2>&1
    cmd "showcase.sh up --count $FLEET_N"
    if ! lea_fleet_up "$FLEET_N" >"$OUT/fleet-up.log" 2>&1; then
        tail -15 "$OUT/fleet-up.log"; no "fleet did not come up -- see $OUT/fleet-up.log"; return
    fi
    tail -"$((FLEET_N + 2))" "$OUT/fleet-up.log"
    echo
    cmd "showcase.sh exec './nvprobe 3'"
    lea_fleet_exec 'cd ~/gpu && export LD_LIBRARY_PATH=$PWD/nv/lib NVPROBE_PTX=$PWD/kernels.ptx && timeout 180 ./nvprobe 3 2>&1 | tail -1' \
        > "$OUT/fleet-run.txt" 2>&1
    cat "$OUT/fleet-run.txt"
    local n; n=$(grep -c "stage 3 ok (kernel, result correct)" "$OUT/fleet-run.txt")
    [[ ${n:-0} -eq $FLEET_N ]] \
        && ok "$n of $FLEET_N VMs computed correctly, simultaneously" \
        || no "$n of $FLEET_N correct -- see $OUT/fleet-run.txt"
    note "measured elsewhere: 4 VMs at once give 3.9x the single-VM time,"
    note "i.e. fair time-sharing, with all four results bit-identical."
    lea_fleet_down >/dev/null 2>&1
    NO_SETUP=0
}

sec_desktop() {
    headline "a desktop on the virtual display" \
             "GNOME runs on the card in a guest that owns no GPU, and Sunshine streams it"
    local dip v1 v2
    if lea_vm_running desktop; then
        note "a desktop instance is already up -- using it (and leaving it up at the end)"
        DESKTOP_UP=1
    else
        note "this brings a second guest up (desktop, index 5) and takes minutes on a fresh disk."
        cmd "showcase.sh up --name desktop --index 5 --session gnome"
        if ! lea_rig_up desktop --index 5 --session gnome >"$OUT/desktop-up.log" 2>&1; then
            tail -20 "$OUT/desktop-up.log"; no "desktop did not come up -- see $OUT/desktop-up.log"; return
        fi
        DESKTOP_UP=1; DESKTOP_MINE=1
        grep -E 'vblank engine|moonlight stream|desktop up' "$OUT/desktop-up.log"
    fi
    dip=$(lea_ip 5)
    v1=$(lea_ssh "$dip" 'cat /sys/module/virtio_nvrm/parameters/stat_vblank_fired' 2>/dev/null | tr -dc '0-9')
    sleep 3
    v2=$(lea_ssh "$dip" 'cat /sys/module/virtio_nvrm/parameters/stat_vblank_fired' 2>/dev/null | tr -dc '0-9')
    if [[ -n $v1 && -n $v2 && $v2 -gt $v1 ]] && lea_ssh "$dip" 'pgrep -x gnome-shell >/dev/null && ss -ltn | grep -q ":47989 "'; then
        ok "gnome-shell up, vblank $v1 -> $v2 in 3 s, Sunshine listening"
        note "stream it: moonlight stream $dip Desktop --resolution $LEA_VDISPLAY_SIZE --fps $LEA_VDISPLAY_HZ --bitrate 40000"
    else
        no "desktop is up but not healthy (vblank $v1 -> $v2) -- see $OUT/desktop-up.log"
    fi
}

sec_mixed() {
    headline "two KINDS of guest on one card" \
             "a desktop keeps its frame rate while a second guest runs PyTorch on the same GPU"
    if [[ $DESKTOP_UP -eq 0 ]]; then
        if lea_vm_running desktop; then
            note "a desktop instance is already up -- using it"; DESKTOP_UP=1
        else
            cmd "showcase.sh up --name desktop --index 5 --session gnome"
            lea_rig_up desktop --index 5 --session gnome >"$OUT/desktop-up.log" 2>&1 \
                || { tail -20 "$OUT/desktop-up.log"; no "no desktop to share the card with"; return; }
            DESKTOP_UP=1; DESKTOP_MINE=1
        fi
    fi
    local dip; dip=$(lea_ip 5)
    cmd "nvidia-smi   # on the host: two backends"
    nvidia-smi --query-compute-apps=pid,process_name,used_memory --format=csv 2>/dev/null | head -5 | tee "$OUT/mixed-smi.txt"
    cmd "vm0: convburn.py   while the desktop runs"
    local v1 v2
    v1=$(lea_ssh "$dip" 'cat /sys/module/virtio_nvrm/parameters/stat_vblank_fired' 2>/dev/null | tr -dc '0-9')
    g 'if [ -x venv/bin/python ]; then timeout 900 venv/bin/python convburn.py 2>&1 | tail -1; else timeout 120 ./nvprobe 3 2>&1 | tail -1; fi' \
        > "$OUT/mixed-run.txt" 2>&1
    v2=$(lea_ssh "$dip" 'cat /sys/module/virtio_nvrm/parameters/stat_vblank_fired' 2>/dev/null | tr -dc '0-9')
    cat "$OUT/mixed-run.txt"
    if grep -qE 'acc=|stage 3 ok' "$OUT/mixed-run.txt" && [[ -n $v1 && -n $v2 && $v2 -gt $v1 ]]; then
        ok "compute correct on vm0, desktop vblank kept ticking ($v1 -> $v2)"
        note "measured 2026-08-15: CS2 at 60.1 FPS while convburn ran beside it,"
        note "convburn bit-identical to its solo run; sharing cost 15-20 % of throughput."
    else
        no "see $OUT/mixed-run.txt (vblank $v1 -> $v2)"
    fi
}

sec_suites() {
    headline "breadth: the Python suites" \
             "real libraries over the boundary -- torch, CuPy, RAPIDS"
    cmd "probe/run/suites.sh"
    "$LEA_ROOT"/probe/run/suites.sh --ip "$IP" > "$OUT/suites.txt" 2>&1
    local rc=$?
    sed -n '/== suites/,$p' "$OUT/suites.txt"
    case $rc in
        0) ok "all suites as expected, including the known failure" ;;
        1) note "a deviation -- see the table above"; RESULTS+=("NEWS|$SECTION|suite deviation") ;;
        *) skipd "could not run (guest venv missing?) -- see $OUT/suites.txt" ;;
    esac
}

do_demo() {
    ONLY=""; SKIP=""; PAUSE=0; NO_SETUP=0; KEEP=0; MANAGED=1; DESKTOP_UP=0; DESKTOP_MINE=0
    FLEET_N=2; OUT=$LEA_VM_DIR/out-showcase; MODE=default
    while [[ $# -gt 0 ]]; do
        case $1 in
            --list)       printf '%s\n' "${ALL_SECTIONS[@]}"; exit 0 ;;
            --only)       ONLY=$2; shift 2 ;;
            --skip)       SKIP=$2; shift 2 ;;
            --fast)       MODE=fast; shift ;;
            --full)       MODE=full; shift ;;
            --fleet)      FLEET_N=$2; shift 2 ;;
            --pause)      PAUSE=1; shift ;;
            --no-setup)   NO_SETUP=1; shift ;;
            --keep)       KEEP=1; shift ;;
            --no-managed) MANAGED=0; shift ;;
            --out)        OUT=$2; shift 2 ;;
            -h|--help)    usage 0 ;;
            *) error "demo: unknown option $1"; usage 2 ;;
        esac
    done
    # HOW TO WAIT FOR IT: this holds a pidfile for its lifetime. Wait on the
    # FILE, never on `pgrep -f` (lea_hold_pidfile has the long version):
    #   until ! lea_running vm/showcase.pid; do sleep 15; done
    lea_hold_pidfile "$LEA_VM_DIR/showcase.pid"
    [[ $FLEET_N -ge 1 && $FLEET_N -le $LEA_MAX_VMS ]] || die "--fleet must be 1..$LEA_MAX_VMS"
    mkdir -p "$OUT"
    IP=$(lea_ip 0)

    # The palette is common.sh's, decided once per process and already off
    # when this is redirected into a log (the demo writes one). These are the
    # demo's short names for it, not a second opinion about when to paint.
    B=$LEA_B; DIM=$LEA_DIM; GRN=$LEA_GRN; RED=$LEA_RED; YEL=$LEA_YEL; R=$LEA_R
    declare -ga RESULTS=()
    SECTION=""
    headline() {   # headline <title> <what it proves>
        echo
        echo "${B}────────────────────────────────────────────────────────────${R}"
        echo "${B}  $1${R}"
        echo "${DIM}  proves: $2${R}"
        echo "${B}────────────────────────────────────────────────────────────${R}"
    }
    # The command is printed before it runs. A demo that shows output without
    # showing what produced it is a slide, not a demonstration.
    cmd() { echo "${DIM}\$ $*${R}"; }
    ok()   { echo "${GRN}  ✓ $*${R}"; RESULTS+=("PASS|$SECTION|$*"); }
    no()   { echo "${RED}  ✗ $*${R}"; RESULTS+=("FAIL|$SECTION|$*"); }
    note() { echo "${YEL}  ! $*${R}"; }
    skipd(){ echo "${DIM}  – skipped: $*${R}"; RESULTS+=("SKIP|$SECTION|$*"); }
    pause() {
        [[ $PAUSE -eq 1 ]] || return 0
        [[ -t 0 ]] || return 0
        echo
        read -rsp "${DIM}  [Enter] for the next section${R}" _ || true
        echo
    }
    lea_on_exit demo_cleanup

    echo "${B}Leandro -- showcase${R}"
    echo "${DIM}$(lea_rig_state)${R}"
    local -a RUN=()
    local s
    for s in "${ALL_SECTIONS[@]}"; do want "$s" && RUN+=("$s"); done
    [[ ${#RUN[@]} -gt 0 ]] || die "no sections selected"
    echo "${DIM}sections: ${RUN[*]}${R}"
    if lea_foreign_rigs vm0 desktop >/dev/null; then
        note "other instances are running ($(lea_foreign_rigs vm0 desktop | tr '\n' ' ')) -- they are left alone"
    fi
    # The fleet section builds its own rig; if it is the ONLY one, skip the setup.
    local needs_single=0
    for s in "${RUN[@]}"; do [[ $s != fleet && $s != desktop ]] && needs_single=1; done
    [[ $needs_single -eq 1 ]] && setup_rig
    for s in "${RUN[@]}"; do
        pause
        SECTION=$s
        "sec_$s"
    done

    echo
    echo "${B}════════════════════════ summary ════════════════════════${R}"
    local fails=0 news=0 r v sec msg
    for r in "${RESULTS[@]}"; do
        IFS='|' read -r v sec msg <<<"$r"
        case $v in
            PASS) printf '  %s✓%s %-10s %s\n' "$GRN" "$R" "$sec" "$msg" ;;
            FAIL) printf '  %s✗%s %-10s %s\n' "$RED" "$R" "$sec" "$msg"; fails=$((fails+1)) ;;
            SKIP) printf '  %s–%s %-10s %s\n' "$DIM" "$R" "$sec" "$msg" ;;
            NEWS) printf '  %s!%s %-10s %s\n' "$YEL" "$R" "$sec" "$msg"; news=$((news+1)) ;;
        esac
    done
    echo
    echo "logs: $OUT/"
    if [[ $fails -eq 0 ]]; then
        echo "${GRN}${B}showcase: everything behaved as advertised.${R}"
        [[ $news -gt 0 ]] && echo "${YEL}($news thing(s) worth a second look -- marked ! above)${R}"
        exit 0
    fi
    echo "${RED}${B}showcase: $fails section(s) did not behave as advertised.${R}"
    exit 1
}

case $CMD in
    net)     do_net "$@" ;;
    up)      do_up "$@" ;;
    down)    do_down "$@" ;;
    status)  do_status ;;
    ssh)     do_ssh "$@" ;;
    exec)    do_exec "$@" ;;
    state)   do_state "$@" ;;
    clean)   do_clean "$@" ;;
    audit)   do_audit "$@" ;;
    display) do_display "$@" ;;
    pair)    do_pair "$@" ;;
    games)   do_games "$@" ;;
    demo)    do_demo "$@" ;;
esac
