# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# The rig: everything host-side that runs or holds a guest. Not a script --
# source it:
#   source "$LEA_ROOT/scripts/lib/rig.sh"
#
# ONE implementation of each of these, used by every entry point:
#   network       lea_net_up / lea_net_down / lea_net_status
#   instances     lea_inst / lea_inst_list / lea_vm_running
#   backends      lea_backend_start / lea_backend_stop / lea_vhu_arg
#   one VM        lea_vm_start / lea_vm_stop / lea_vm_ssh
#   one rig       lea_rig_up / lea_rig_down / lea_rig_status / lea_foreign_rigs
#   N rigs        lea_fleet_up / lea_fleet_down / lea_fleet_status / lea_fleet_exec
#   the rig state lea_rig_state / lea_rig_check
#   after a crash lea_rig_clean
#
# THE INSTANCE MODEL. One guest = one directory, vm/<name>/, holding all of
# its state:
#   index          which slot it occupies: IP .<LEA_IP_FIRST+i>, tap<i>, MAC
#   rootfs.qcow2   the disk (an overlay on LEA_BASE_IMAGE or LEA_FLEET_BASE)
#   seed.img       its cloud-init seed -- disk and seed describe ONE instance
#                  and are created and dropped together
#   ch.pid ch.log serial.log        cloud-hypervisor
#   nvrm.sock nvrm.pid nvrm.log     the vhost-user-nvrm backend
#   input.sock input.pid input.log input.fifo   the vhost-user-input backend
#   up.log setup.log load.log exec.log exec.rc  what the scripts logged
# The default name for slot i is vm<i>; vm0 is the standard dev VM. Named
# instances (--name desktop --index 5) live beside it. Everything that ran
# earlier (three naming schemes, backends without pidfiles, a glob that
# missed one of them) collapses into "look under vm/*/".
#
# ORDER, in both directions, and it is not cosmetic: backends first, VM
# after; on the way down VM first, backends after. A vhost-user backend that
# dies under a running cloud-hypervisor makes it exit instantly, and that
# looks exactly like a guest crash. lea_rig_up and lea_rig_down are the two
# places that know this; nothing else starts a backend.

[[ -n ${_LEA_RIG_LOADED:-} ]] && return 0
_LEA_RIG_LOADED=1

# shellcheck source=scripts/lib/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
# shellcheck source=scripts/lib/provision.sh
source "$(dirname "${BASH_SOURCE[0]}")/provision.sh"

# ---- instances ------------------------------------------------------------
# lea_inst NAME [INDEX] -- resolve one instance into the INST_* variables:
#   INST_NAME INST_IDX INST_DIR INST_IP INST_TAP INST_MAC INST_GUEST
#   INST_TRANSPORT INST_VSOCK_CID INST_VSOCK_SOCK
# The index is remembered in vm/<name>/index the first time it is given, so
# later calls need only the name. vm<i> needs no index at all.
#
# INST_GUEST is which OS the instance carries -- `ubuntu` (the cloud image
# build.sh bake provisions) or `nixos` (the derivation build.sh bake --nixos
# builds). It is remembered in vm/<name>/guest for the same reason the index
# is: `down`, `status`, `ssh` and every provisioning step have to know, and
# asking the caller to repeat --guest on each of them is how the two answers
# drift apart. Absent file = ubuntu, which is what every instance created
# before this existed is.
#
# INST_TRANSPORT is how the host reaches it -- `ip` (an address on the
# bridge) or `vsock` (one unix socket, no network device in the guest at
# all). Remembered in vm/<name>/transport for the same reason the guest OS is
# remembered beside it: every later command would otherwise have to be told
# again, and the two answers would drift.
#
# WARNING: these are globals. A function that calls another instance-taking
# function after lea_inst must copy what it needs into locals first.
lea_inst() {
    local name=$1 idx=${2:-}
    [[ $name =~ ^[A-Za-z0-9_-]+$ ]] || die "instance name '$name' -- letters, digits, - and _ only"
    INST_NAME=$name
    INST_DIR=$(lea_inst_dir "$name")
    INST_GUEST=ubuntu
    [[ -f $INST_DIR/guest ]] && INST_GUEST=$(tr -d '[:space:]' < "$INST_DIR/guest")
    INST_TRANSPORT=ip
    [[ -f $INST_DIR/transport ]] && INST_TRANSPORT=$(tr -d '[:space:]' < "$INST_DIR/transport")
    if [[ -z $idx ]]; then
        if [[ -f $INST_DIR/index ]]; then
            idx=$(tr -d '[:space:]' < "$INST_DIR/index")
        elif [[ $name =~ ^vm([0-9]+)$ ]]; then
            idx=${BASH_REMATCH[1]}
        else
            die "instance '$name' has no index yet -- pass --index N the first time it comes up"
        fi
    fi
    [[ $idx =~ ^[0-9]+$ && $idx -lt $LEA_MAX_VMS ]] \
        || die "index '$idx' for instance '$name' -- must be 0..$((LEA_MAX_VMS - 1)) (LEA_MAX_VMS)"
    INST_IDX=$idx
    INST_IP=$(lea_ip "$idx")
    INST_TAP=$(lea_tap "$idx")
    INST_MAC=$(lea_mac "$INST_IP")
    INST_VSOCK_CID=$(lea_vsock_cid "$idx")
    INST_VSOCK_SOCK=$(lea_vsock_sock "$idx")
    return 0
}

# _lea_inst_save -- remember the index, the guest OS and the transport of
# the current instance on disk.
_lea_inst_save() {
    mkdir -p "$INST_DIR"
    echo "$INST_IDX" > "$INST_DIR/index"
    echo "${INST_GUEST:-ubuntu}" > "$INST_DIR/guest"
    echo "${INST_TRANSPORT:-ip}" > "$INST_DIR/transport"
}

# lea_guest_os NAME -- which OS that instance carries, without disturbing the
# INST_* globals of a caller that is in the middle of something.
lea_guest_os() {
    local f; f=$(lea_inst_dir "$1")/guest
    [[ -f $f ]] && tr -d '[:space:]' < "$f" || echo ubuntu
}

# lea_transport_of NAME -- the same, for the transport.
lea_transport_of() {
    local f; f=$(lea_inst_dir "$1")/transport
    [[ -f $f ]] && tr -d '[:space:]' < "$f" || echo ip
}

# lea_guest_idx NAME -- and the same for the index, so that a teardown can
# find this instance's slot-keyed endpoints without disturbing the INST_*
# globals of whoever is halfway through something.
lea_guest_idx() {
    local f; f=$(lea_inst_dir "$1")/index
    [[ -f $f ]] && tr -d '[:space:]' < "$f" && return 0
    [[ $1 =~ ^vm([0-9]+)$ ]] && echo "${BASH_REMATCH[1]}"
}

# lea_nixos_image -- read the NixOS image's own facts into LEA_NIXOS_*.
#
# Direct kernel boot needs three files and one fact -- kernel, initrd, rootfs,
# and WHICH `init=` this image's system generation is. Guessing the last one
# boots the wrong generation, silently and with a working shell, so the image
# states it: nix/guest-image.nix writes image.env beside the three files and
# this is the only thing that reads it.
lea_nixos_image() {
    local env=$LEA_NIXOS_DIR/image.env f
    [[ -f $env ]] || {
        error "$env missing -- no NixOS guest image here.
Fix: scripts/build.sh bake --nixos   (or point LEA_NIXOS_DIR at one)"
        return 1
    }
    # shellcheck source=/dev/null
    source "$env"
    for f in kernel initrd rootfs.qcow2; do
        [[ -f $LEA_NIXOS_DIR/$f ]] || {
            error "$LEA_NIXOS_DIR/$f missing beside image.env -- rebuild: scripts/build.sh bake --nixos"
            return 1
        }
    done
    [[ -n ${LEA_NIXOS_INIT:-} ]] || { error "$env names no LEA_NIXOS_INIT"; return 1; }
    # The image was built with ONE guest user baked into its /etc/passwd; the
    # scripts address the guest as LEA_GUEST_USER. Two names that disagree
    # produce a VM that boots perfectly and refuses every login -- the same
    # failure the Ubuntu seed has, and worth the same hard check.
    [[ ${LEA_NIXOS_USER:-} == "$LEA_GUEST_USER" ]] || {
        error "the NixOS image was built for user '${LEA_NIXOS_USER:-?}', this rig uses '$LEA_GUEST_USER'.
Fix: set LEA_GUEST_USER=${LEA_NIXOS_USER:-?}, or rebuild the image with
     guestUser = \"$LEA_GUEST_USER\" in flake.nix and: scripts/build.sh bake --nixos"
        return 1
    }
    return 0
}

# lea_inst_list -- every instance directory under vm/, one name per line.
lea_inst_list() {
    local d
    for d in "$LEA_VM_DIR"/*/; do
        [[ -f $d/index ]] || continue
        basename "$d"
    done
}

# lea_vgpu_admit NAME TYPE MAX -- may another VM of this type start?
#
# THIS IS THE PIECE THE BACKEND CANNOT DO, and the reason is the whole
# architecture: one backend serves one VM, holds no RM client, and has no
# path to a sibling. NVIDIA's vGPU refuses at CREATION
# (kvgpumgrValidateVgpuTypeCreatable, kernel_vgpu_mgr.c:322 --
# NV_ERR_INSUFFICIENT_RESOURCES when existingVgpus >= maxInstance) because
# the HOST driver owns every profile on the card. Here the equivalent owner
# is the script that starts VMs: it can see every instance directory, and
# that is the only place on this side of the boundary where the question
# can be asked at all.
#
# Two rules, both vGPU's:
#   * at most MAX instances of a type (its maxInstance);
#   * HOMOGENEOUS -- every live VM on the card runs the same type. vGPU's
#     default mode is homogeneous placement and its arithmetic assumes it;
#     copying the arithmetic without the constraint would hand out
#     framebuffer twice.
# An instance whose backend is not running does not occupy a placement,
# which is why this reads the pidfile rather than the type file alone.
lea_vgpu_admit() {
    local name=$1 want=$2 max=$3 n=0 other dir live
    for other in $(lea_inst_list); do
        [[ $other == "$name" ]] && continue
        dir=$(lea_inst_dir "$other")
        [[ -f $dir/vgpu-type ]] || continue
        lea_running "$dir/nvrm.pid" || continue
        live=$(tr -d '[:space:]' < "$dir/vgpu-type")
        if [[ $live != "$want" ]]; then
            error "$name: this card already runs $live (instance $other) and vGPU
       placement is homogeneous -- every VM on a card is the same type.
       Take it down or start $name as $live."
            return 1
        fi
        n=$((n + 1))
    done
    if (( n >= max )); then
        error "$name: $n instances of $want are running and its maxInstance is $max.
       That is NV_ERR_INSUFFICIENT_RESOURCES, refused at creation the way
       kernel_vgpu_mgr.c:322 refuses it -- before the VM exists, rather than
       by failing an allocation at minute two."
        return 1
    fi
    info "  $name: placement $((n + 1)) of $max for $want"
    return 0
}

# lea_vm_running NAME -- is that instance's cloud-hypervisor alive?
lea_vm_running() {
    lea_running "$(lea_inst_dir "$1")/ch.pid"
}

# lea_inst_owner NAME -- the pid of the LIVE script that brought this
# instance up, if it is not us. Every `up` records its own pid in
# vm/<name>/owner (lea_vm_start), and a gate, a bench or a bake holds its
# instance for its whole run. Measured 2026-08-18: a `down --all` typed in
# another terminal took the bake instance down in the middle of its apt
# run -- the file is what lets `down` refuse that. Prints nothing when the
# owner is gone or is this very process.
lea_inst_owner() {
    local f pid
    f=$(lea_inst_dir "$1")/owner
    [[ -f $f ]] || return 1
    pid=$(tr -d '[:space:]' < "$f")
    [[ -n $pid && $pid != "$$" ]] || return 1
    kill -0 "$pid" 2>/dev/null || return 1
    echo "$pid"
}

# _lea_index_taken IDX [NAME] -- the RUNNING instance (other than NAME) that
# already occupies that slot, if any. Two instances on one index share IP
# and tap; the second one boots and never answers.
_lea_index_taken() {
    local idx=$1 me=${2:-} n
    for n in $(lea_inst_list); do
        [[ $n == "$me" ]] && continue
        [[ $(tr -d '[:space:]' < "$LEA_VM_DIR/$n/index") == "$idx" ]] || continue
        lea_vm_running "$n" && { echo "$n"; return 0; }
    done
    return 1
}

# ---- network --------------------------------------------------------------
# A bridge, a set of taps, and NAT out to the world. Needs root for the link
# and firewall changes; it calls sudo itself rather than demanding to be run
# as root, because everything else here is usable as a normal user.
#
# WARNING: none of this survives a reboot. The links, the iptables rules and
# the ip_forward sysctl are all runtime state. Re-run `net up` after
# booting the host. (On NixOS the module in nix/module.nix declares the
# same bridge, taps and NAT, and then this is never needed.)
_LEA_SUBNET="$LEA_NET_PREFIX.0/$LEA_NETMASK"

# _lea_guest_dns -- the resolvers to hand the guest, as a comma-separated list.
#
# NOT hard-coded 8.8.8.8/1.1.1.1, which is what this was until 2026-08-21.
# Plenty of networks -- university and corporate ones especially -- block or
# hijack outbound DNS to public resolvers, and then a guest with a PERFECT
# NAT still cannot resolve anything: `net status` reports uplink, NAT and both
# FORWARD rules present, and apt says "Temporary failure resolving". Reported
# from exactly such a host.
#
# The host's own upstream resolvers are the answer that is right by
# construction, for the same reason _lea_uplink asks the routing table rather
# than guessing an interface name. Loopback addresses are dropped: 127.0.0.53
# is systemd-resolved's stub, which resolves beautifully on the host and is
# nothing at all from inside a guest. IPv6 is dropped too -- the guest's
# netplan block is IPv4-only, so a v6 resolver there is an address it has no
# route to.
#
# LEA_GUEST_DNS overrides, for a host whose resolver the guest cannot reach
# (a VPN-only resolver, say). The public pair remains the last resort, which
# is what every previous run used.
_lea_guest_dns() {
    [[ -n ${LEA_GUEST_DNS:-} ]] && { echo "$LEA_GUEST_DNS"; return 0; }
    local -a dns=()
    local a
    while read -r a; do
        [[ $a =~ ^127\. ]] && continue
        [[ $a == *:* ]] && continue
        dns+=("$a")
    done < <( { resolvectl dns 2>/dev/null | tr ' ' '\n'
                grep -E '^nameserver' /etc/resolv.conf 2>/dev/null | awk '{print $2}'
              } | grep -E '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$' | awk '!seen[$0]++' )
    [[ ${#dns[@]} -gt 0 ]] || dns=(8.8.8.8 1.1.1.1)
    local IFS=,
    echo "${dns[*]}"
}

# _lea_uplink -- the interface carrying the default route.
#
# NOT hard-coded: the interface name differs per machine (enp39s0 here,
# eth0/wlan0 elsewhere), and a wrong name produces a NAT rule that silently
# matches nothing -- guests boot, get an address, and simply have no route
# out. Asking the routing table is the one answer that is right by
# construction.
_lea_uplink() {
    local dev
    dev=$(ip route get 1.1.1.1 2>/dev/null | awk '{for(i=1;i<NF;i++) if($i=="dev") print $(i+1); exit}')
    [[ -n $dev ]] || return 1
    echo "$dev"
}

# Every rule is added only if an identical one is absent (-C), and removed
# with the same specification on teardown. Without the -C check a second
# `up` stacks duplicates; without the -D on `down` they accumulate across
# long uptimes until the FORWARD chain is a wall of copies.
#
# The FORWARD rules go in at position 1 (-I), not appended (-A): Docker
# installs its own chains plus a DROP policy, and an appended ACCEPT would
# be evaluated after Docker has already dropped the packet.
_lea_fw_add() {
    local uplink=$1 dir
    if sudo iptables -t nat -C POSTROUTING -s "$_LEA_SUBNET" -o "$uplink" -j MASQUERADE 2>/dev/null; then
        info "  nat: already present"
    else
        sudo iptables -t nat -A POSTROUTING -s "$_LEA_SUBNET" -o "$uplink" -j MASQUERADE
        info "  nat: MASQUERADE $_LEA_SUBNET out of $uplink"
    fi
    # Inserted source-rule first, then destination-rule, so the resulting
    # order reads -d, -s from the top.
    for dir in -s -d; do
        if sudo iptables -C FORWARD "$dir" "$_LEA_SUBNET" -j ACCEPT 2>/dev/null; then
            info "  forward ($dir): already present"
        else
            sudo iptables -I FORWARD 1 "$dir" "$_LEA_SUBNET" -j ACCEPT
            info "  forward ($dir): ACCEPT $_LEA_SUBNET (inserted at position 1)"
        fi
    done
}

_lea_fw_del() {
    local uplink=$1 dir
    if [[ -n $uplink ]]; then
        while sudo iptables -t nat -C POSTROUTING -s "$_LEA_SUBNET" -o "$uplink" -j MASQUERADE 2>/dev/null; do
            sudo iptables -t nat -D POSTROUTING -s "$_LEA_SUBNET" -o "$uplink" -j MASQUERADE
            info "  nat: removed MASQUERADE $_LEA_SUBNET out of $uplink"
        done
    else
        warn "no uplink determined -- NAT rule left in place. Remove it with:"
        warn "  sudo iptables -t nat -D POSTROUTING -s $_LEA_SUBNET -o <IFACE> -j MASQUERADE"
    fi
    for dir in -s -d; do
        while sudo iptables -C FORWARD "$dir" "$_LEA_SUBNET" -j ACCEPT 2>/dev/null; do
            sudo iptables -D FORWARD "$dir" "$_LEA_SUBNET" -j ACCEPT
            info "  forward ($dir): removed ACCEPT $_LEA_SUBNET"
        done
    done
}

# lea_net_up [--count N] [--uplink IFACE] [--user NAME] -- idempotent.
lea_net_up() {
    local count=$LEA_MAX_VMS uplink="" tapuser=${SUDO_USER:-$USER} i tap
    while [[ $# -gt 0 ]]; do
        case $1 in
            --count)  count=$2; shift 2 ;;
            --uplink) uplink=$2; shift 2 ;;
            --user)   tapuser=$2; shift 2 ;;
            *) die "lea_net_up: unknown option $1" ;;
        esac
    done
    [[ $count -ge 1 && $count -le $LEA_MAX_VMS ]] \
        || die "--count must be between 1 and $LEA_MAX_VMS (LEA_MAX_VMS)"
    id -u "$tapuser" >/dev/null 2>&1 || die "no such user: $tapuser"
    [[ -n $uplink ]] || uplink=$(_lea_uplink) || die "cannot determine the uplink interface.
       'ip route get 1.1.1.1' returned no device -- the host has no default
       route, or it is down. Pass one explicitly: --uplink <IFACE>"

    info "uplink: $uplink    bridge: $LEA_BRIDGE    subnet: $_LEA_SUBNET"

    # -- bridge --
    if ip link show "$LEA_BRIDGE" >/dev/null 2>&1; then
        info "  bridge $LEA_BRIDGE: exists"
    else
        sudo ip link add name "$LEA_BRIDGE" type bridge
        info "  bridge $LEA_BRIDGE: created"
    fi
    # STP off and forward_delay 0: with the default 15 s delay a freshly
    # attached tap does not forward while the guest is booting, so cloud-init
    # comes up before the link does and DHCP/first contact silently misses.
    sudo ip link set "$LEA_BRIDGE" type bridge stp_state 0 forward_delay 0
    if ip -4 addr show dev "$LEA_BRIDGE" | grep -q "inet $LEA_GATEWAY/$LEA_NETMASK"; then
        info "  bridge address $LEA_GATEWAY/$LEA_NETMASK: present"
    else
        sudo ip addr add "$LEA_GATEWAY/$LEA_NETMASK" dev "$LEA_BRIDGE"
        info "  bridge address $LEA_GATEWAY/$LEA_NETMASK: added"
    fi
    sudo ip link set "$LEA_BRIDGE" up

    # -- taps --
    # vnet_hdr is required: cloud-hypervisor passes virtio-net headers
    # through, and without it every packet is malformed. persist keeps the
    # tap alive between VM runs, user makes it usable without root.
    for i in $(seq 0 $((count - 1))); do
        tap=$(lea_tap "$i")
        if ip link show "$tap" >/dev/null 2>&1; then
            info "  $tap: exists"
        else
            sudo ip tuntap add dev "$tap" mode tap user "$tapuser" vnet_hdr
            info "  $tap: created (owner $tapuser, vnet_hdr)"
        fi
        sudo ip link set "$tap" master "$LEA_BRIDGE"
        sudo ip link set "$tap" up
    done

    # -- forwarding --
    if [[ $(sysctl -n net.ipv4.ip_forward) == 1 ]]; then
        info "  ip_forward: already on"
    else
        sudo sysctl -q -w net.ipv4.ip_forward=1
        info "  ip_forward: enabled"
    fi
    _lea_fw_add "$uplink"

    info ""
    info "network up. WARNING: links, iptables rules and ip_forward are"
    info "runtime state -- none of it survives a reboot. Re-run 'net up' then."
}

# lea_net_down [--uplink IFACE] -- firewall rules first: if a later step
# fails, the rules are the part that would otherwise pile up invisibly.
# Links are cheap to spot with `ip link`; a duplicated iptables rule is not.
lea_net_down() {
    local uplink="" i tap
    while [[ $# -gt 0 ]]; do
        case $1 in
            --uplink) uplink=$2; shift 2 ;;
            *) die "lea_net_down: unknown option $1" ;;
        esac
    done
    [[ -n $uplink ]] || uplink=$(_lea_uplink) || true
    _lea_fw_del "$uplink"
    for i in $(seq 0 $((LEA_MAX_VMS - 1))); do
        tap=$(lea_tap "$i")
        if ip link show "$tap" >/dev/null 2>&1; then
            sudo ip link del "$tap"
            info "  $tap: removed"
        fi
    done
    if ip link show "$LEA_BRIDGE" >/dev/null 2>&1; then
        sudo ip link del "$LEA_BRIDGE"
        info "  bridge $LEA_BRIDGE: removed"
    fi
    # ip_forward is left ON on purpose: it is a host-wide setting that other
    # things (Docker, libvirt, a VPN) may also depend on. Turning it off
    # here would break them silently.
    info "network down (ip_forward left as it is -- it is host-wide)."
}

lea_net_status() {
    local up i tap n=0
    up=$(_lea_uplink || echo "?")
    info "uplink (detected): $up"
    if ip link show "$LEA_BRIDGE" >/dev/null 2>&1; then
        info "bridge $LEA_BRIDGE: up, $(ip -4 -o addr show dev "$LEA_BRIDGE" | awk '{print $4}' | tr '\n' ' ')"
    else
        info "bridge $LEA_BRIDGE: absent"
    fi
    for i in $(seq 0 $((LEA_MAX_VMS - 1))); do
        tap=$(lea_tap "$i")
        ip link show "$tap" >/dev/null 2>&1 && n=$((n + 1))
    done
    info "taps present: $n/$LEA_MAX_VMS"
    info "ip_forward: $(sysctl -n net.ipv4.ip_forward)"
    if [[ $up != "?" ]] && sudo -n iptables -t nat -C POSTROUTING -s "$_LEA_SUBNET" -o "$up" -j MASQUERADE 2>/dev/null; then
        info "nat: present ($_LEA_SUBNET out of $up)"
    else
        info "nat: absent or not checkable without sudo"
    fi
    # THE FORWARD RULES, which this used to leave out -- and a MASQUERADE with
    # no FORWARD accept is precisely the state that looks configured and
    # carries nothing. It is the normal state on a host running Docker, whose
    # FORWARD policy is DROP. Both directions are reported because they are
    # added as two rules and one of them can go missing on its own.
    local d ok=0 miss=""
    for d in "-s" "-d"; do
        if sudo -n iptables -C FORWARD "$d" "$_LEA_SUBNET" -j ACCEPT 2>/dev/null; then
            ok=$((ok + 1))
        else
            miss="$miss $d"
        fi
    done
    if [[ $ok -eq 2 ]]; then
        info "forward: both ACCEPT rules present"
    else
        info "forward: ${ok}/2 ACCEPT rules ($_LEA_SUBNET) --${miss:- } missing or not checkable without sudo"
        info "         policy: $(sudo -n iptables -S FORWARD 2>/dev/null | head -1 || echo '(needs sudo)')"
    fi
}

# lea_net_ready COUNT -- bring the network up quietly, as every rig start
# does first (idempotent, so cheap when it already is).
lea_net_ready() {
    lea_net_up --count "$1" >/dev/null || die "network setup failed -- run 'showcase.sh net up' to see why"
}

# ---- backends -------------------------------------------------------------
# lea_vhu_arg NAME nvrm|input -- the cloud-hypervisor word for that device.
#
# Two queues for nvrm: request/response on 0, host->guest EVENTS on 1
# (OPEN-QUESTIONS nr 10). With one size the guest module finds a single
# queue and disables event delivery ("two queues refused" in dmesg).
# virtio-input: eventq and statusq, both 256.
lea_vhu_arg() {
    local dir; dir=$(lea_inst_dir "$1")
    case $2 in
        nvrm)  echo "--generic-vhost-user device_type=60,socket=$dir/nvrm.sock,queue_sizes=[256,256]" ;;
        input) echo "--generic-vhost-user device_type=input,socket=$dir/input.sock,queue_sizes=[256,256]" ;;
        *) die "lea_vhu_arg: unknown backend $2" ;;
    esac
}

# _lea_stop_stale PIDFILE -- kill a backend recorded there, if still alive.
# Called before starting a new one on the same socket.
#
# Without this, `up` on a rig whose VM is already gone starts a SECOND
# backend on the same socket and overwrites the pidfile. The first keeps
# running, owned by nobody. Measured 2026-08-07: seven orphaned
# vhost-user-input, the oldest four hours old. Only `input` piles up, and
# that asymmetry is the tell: the nvrm backend ends itself when the VMM
# hangs up, vhost-user-input does not.
_lea_stop_stale() {
    local f=$1 pid
    [[ -f $f ]] || return 0
    pid=$(cat "$f" 2>/dev/null) || return 0
    if [[ -n $pid ]] && kill -0 "$pid" 2>/dev/null; then
        info "  stale backend $pid from $(basename "$(dirname "$f")")/$(basename "$f") -- stopping it first"
        kill "$pid" 2>/dev/null || true
    fi
    rm -f "$f"
}

# lea_backend_start NAME nvrm|input [--vram-limit MiB] [--vram-profile MiB]
# -- start one backend for that instance, detached, and wait for its socket.
#
# WARNING: THE PARENTHESES, and `</dev/null`. A backend is long-lived and
# must outlive the shell that started it. As a bare background job it
# stays in that shell's process group and dies with it -- silently: a
# SIGHUP or SIGTERM leaves no log line and no core, so the log simply
# stops mid-sentence and the guest later hangs on a host that is not
# there. Measured on 2026-08-07 with the desktop guest, and it cost an
# evening.
#
# The env knobs are passed on EXPLICITLY rather than left to inheritance --
# an env var that only works because nobody cleared it is not a documented
# one. LEA_MANAGED_COMPAT and LEA_VRAM_LIMIT_MIB are HOST switches (read in
# session.rs), not guest ones: set inside the guest they do nothing.
# LEA_VRAM_LIMIT_MIB is per BACKEND, i.e. per VM, which is what makes a
# per-tenant cap mean something. LEA_VRAM_PROFILE_MIB is the OTHER policy
# for that same VM (OPEN-QUESTIONS 68): the cap is what the guest may
# allocate, the profile is what the VM may cost the CARD, and the backend
# refuses to start with both set rather than ranking them. The two are
# passed on separately so a run can A/B them without editing anything. LEA_OBJLOG / LEA_FD_CENSUS are the host
# halves of two ledgers whose guest halves are worthless alone; LEA_DEBUG
# likewise -- and a measuring run keeps all of them off, because every log
# line in the hot path is a measurement error.
lea_backend_start() {
    local name=$1 kind=$2; shift 2
    local cap="${LEA_VRAM_LIMIT_MIB:-}" prof="${LEA_VRAM_PROFILE_MIB:-}"
    local vtype="${LEA_VGPU_TYPE:-}" vprof="" vfb=""
    while [[ $# -gt 0 ]]; do
        case $1 in
            --vram-limit)   cap=$2; shift 2 ;;
            --vram-profile) prof=$2; shift 2 ;;
            --vgpu-type)    vtype=$2; shift 2 ;;
            *) die "lea_backend_start: unknown option $1" ;;
        esac
    done
    lea_inst "$name"
    local dir=$INST_DIR sock pidf log bin
    mkdir -p "$dir"
    sock=$dir/$kind.sock; pidf=$dir/$kind.pid; log=$dir/$kind.log
    _lea_stop_stale "$pidf"
    rm -f "$sock"
    case $kind in
        nvrm)
            bin=$LEA_BIN_DIR/vhost-user-nvrm
            [[ -x $bin ]] || die "$bin missing -- run: scripts/build.sh cargo"
            if [[ -n $vtype ]]; then
                # RESOLVE THE TYPE AGAINST THE CARD, here and not in the
                # backend: the backend holds no RM client, so it cannot
                # read the catalogue its own policy is named after. This is
                # the same split vGPU makes -- the host RM owns the
                # catalogue, the per-VM plugin gets a slice.
                local _vg
                _vg=$("$LEA_BIN_DIR/vgpuprofile" --select "$vtype" 2>/dev/null) || {
                    error "$name: no vGPU type '$vtype' on this card."
                    "$LEA_BIN_DIR/vgpuprofile" >&2 || true
                    return 1; }
                local vgpu_type="" vgpu_profile_mib="" vgpu_fb_mib="" vgpu_max_instance=""
                local vgpu_segments="" vgpu_segment_mib=""
                eval "$_vg"
                vtype=$vgpu_type; vprof=$vgpu_profile_mib; vfb=$vgpu_fb_mib
                info "  $name: vGPU type $vtype -- profile ${vprof} MiB, guest FB ${vfb} MiB ($vgpu_segments x ${vgpu_segment_mib} MiB VMMU segments)"
                lea_vgpu_admit "$name" "$vtype" "$vgpu_max_instance" || return 1
                echo "$vtype" > "$dir/vgpu-type"
            fi
            [[ -n $cap ]] && info "  $name: VRAM cap ${cap} MiB (LEA_VRAM_LIMIT_MIB)"
            if [[ -n $prof ]]; then
                local _res="${LEA_VRAM_RESERVE_MIB:-256}"
                info "  $name: VRAM profile ${prof} MiB = $((prof - _res)) MiB guest FB + ${_res} MiB reserved"
                # ADMISSION, as far as this side of the boundary can do it,
                # and it is deliberately a WARNING. NVIDIA's vGPU refuses a
                # VM at creation when the card is full
                # (kernel_vgpu_mgr.c:322, NV_ERR_INSUFFICIENT_RESOURCES) --
                # it can, because the host driver owns every profile on the
                # card. Here nothing owns them: one backend serves one VM
                # and cannot see a sibling. What this script CAN see is the
                # card's free memory at this instant, which is a fact about
                # the past the moment it is printed. Overprovisioning is
                # allowed on purpose (OPEN-QUESTIONS 68); this only makes
                # sure nobody does it without having been told.
                local _free
                _free=$(nvidia-smi --query-gpu=memory.free --format=csv,noheader,nounits 2>/dev/null | head -1)
                if [[ -n ${_free:-} ]] && (( prof > _free )); then
                    warn "  $name: profile ${prof} MiB is more than the card has free right now (${_free} MiB) -- overprovisioned. That is allowed and is not checked anywhere else; OPEN-QUESTIONS 67 is what it looks like when the sum does not fit."
                fi
            fi
            # ONE FD PER GUEST CLIENT, and a desktop has hundreds. The backend
            # opens a real /dev/nvidiactl or /dev/nvidia0 for every RM client
            # the guest creates, which is the design -- the mirror hands the
            # guest tokens and keeps the FDs. GNOME, Steam with its dozen
            # helpers and Sunshine together reached 913 open FDs against the
            # 1024 soft limit (measured 2026-08-20), and past that every new
            # client got EMFILE: "open /dev/nvidia0: Too many open files".
            # In the guest that arrives as a Vulkan swapchain that cannot be
            # created, with nothing anywhere naming a file descriptor.
            #
            # The hard limit is 524288 here, so raising the soft limit needs
            # no privilege. `|| true`: a system with a lower hard limit gets
            # what it can and the daemon still starts.
            ( ulimit -n "${LEA_NOFILE:-65536}" 2>/dev/null || true
              LEA_MANAGED_COMPAT="${LEA_MANAGED_COMPAT:-}" \
              LEA_VRAM_LIMIT_MIB="$cap" \
              LEA_VRAM_PROFILE_MIB="$prof" \
              LEA_VRAM_RESERVE_MIB="${LEA_VRAM_RESERVE_MIB:-}" \
              LEA_VGPU_TYPE="$vtype" \
              LEA_VGPU_PROFILE_MIB="$vprof" \
              LEA_VGPU_FB_MIB="$vfb" \
              LEA_MAX_PIN_MIB="${LEA_MAX_PIN_MIB:-}" \
              LEA_OBJLOG="${LEA_OBJLOG:-}" \
              LEA_FD_CENSUS="${LEA_FD_CENSUS:-}" \
              LEA_TEST_SHMEM_MAP_OOB="${LEA_TEST_SHMEM_MAP_OOB:-}" \
              LEA_FRL_HZ="${LEA_FRL_HZ:-}" \
              LEA_CTRL_DUMP="${LEA_CTRL_DUMP:-}" \
              LEA_DEBUG="${LEA_DEBUG:-}" \
              LEA_ADMIN_PRIV="${LEA_ADMIN_PRIV:-}" \
              setsid nohup "$bin" --nvrm "$sock" >"$log" 2>&1 </dev/null &
              echo "$!" > "$pidf" )
            ;;
        input)
            bin=$LEA_BIN_DIR/vhost-user-input
            [[ -x $bin ]] || die "$bin missing -- run: scripts/build.sh cargo"
            # The fifo source rather than an evdev node: a real host device
            # would mean this VM eats the keyboard of whoever is sitting at
            # the machine. The fifo is what makes the input path testable:
            #   printf '1 30 1\n0 0 0\n' > vm/<name>/input.fifo   presses KEY_A
            ( setsid nohup "$bin" --socket "$sock" --fifo "$dir/input.fifo" \
                >"$log" 2>&1 </dev/null &
              echo "$!" > "$pidf" )
            ;;
        *) die "lea_backend_start: unknown backend $kind" ;;
    esac
    lea_wait_socket "$sock" || {
        error "$name: $kind backend did not come up -- $log"
        tail -5 "$log" >&2
        return 1
    }
    info "  $name: $kind backend up"
}

# lea_backend_stop NAME [nvrm|input ...] -- kill by pidfile, remove socket
# (and the fifo: a named pipe whose reader is gone is worse than no fifo --
# a `printf > input.fifo` then blocks forever instead of failing).
lea_backend_stop() {
    local name=$1 dir kind; shift
    dir=$(lea_inst_dir "$name")
    [[ $# -gt 0 ]] || set -- nvrm input
    for kind in "$@"; do
        if [[ -f $dir/$kind.pid ]]; then
            kill "$(cat "$dir/$kind.pid")" 2>/dev/null || true
            rm -f "$dir/$kind.pid"
        fi
        rm -f "$dir/$kind.sock"
        [[ $kind == input ]] && rm -f "$dir/input.fifo"
    done
    return 0
}

# _lea_backend_alive NAME kind -- up / down
_lea_backend_alive() {
    lea_running "$(lea_inst_dir "$1")/$2.pid" && echo up || echo down
}

# ---- one VM ---------------------------------------------------------------
# _lea_seed_write NAME -- the cloud-init seed: static IP + SSH key, no
# payload runner (the VM stays up). The seed carries the guest IDENTITY
# (user, key, hostname). It is only rebuilt on --fresh or when missing --
# otherwise cloud-init would think this is a new instance. A seed written
# before the guest identity changed produces a VM that boots perfectly and
# refuses every login, which is why disk and seed are dropped together.
_lea_seed_write() {
    local seed=$INST_DIR/seed.img stage=$INST_DIR/.seed host pubkey
    host=$LEA_HOSTNAME_PREFIX-$INST_NAME
    pubkey=$(cat "$LEA_SSH_KEY.pub")
    rm -rf "$stage"; mkdir -p "$stage"
    cat >"$stage/meta-data" <<EOS
instance-id: $LEA_GUEST_USER-$INST_NAME-$(date +%s)
local-hostname: $host
EOS
    # Network NOT via cloud-init (that only runs on first boot and leaves
    # eth0 'False' on a restart). A persistent netplan file instead, applied
    # on EVERY boot -- it survives in the overlay.
    # The resolvers, derived from this host, in the two spellings the seed
    # needs: a YAML list for netplan and printf lines for resolv.conf. Both
    # come from one call so they can never disagree.
    local _LEA_DNS _LEA_DNS_LIST _LEA_DNS_LINES
    _LEA_DNS=$(_lea_guest_dns)
    _LEA_DNS_LIST=${_LEA_DNS//,/, }
    _LEA_DNS_LINES=$(printf 'nameserver %s\\n' ${_LEA_DNS//,/ })
    info "  $name: guest resolvers $_LEA_DNS"
    cat >"$stage/network-config" <<EOS
version: 2
ethernets: {}
EOS
    cat >"$stage/user-data" <<EOS
#cloud-config
hostname: $host
users:
  - name: $LEA_GUEST_USER
    sudo: "ALL=(ALL) NOPASSWD:ALL"
    shell: /bin/bash
    lock_passwd: false
    plain_text_passwd: $LEA_GUEST_USER
    ssh_authorized_keys:
      - $pubkey
ssh_pwauth: true
write_files:
  - path: /etc/netplan/60-leandro.yaml
    permissions: "0600"
    content: |
      network:
        version: 2
        ethernets:
          eth0:
            match: { name: eth0 }
            addresses: [$INST_IP/$LEA_NETMASK]
            routes:
              - to: default
                via: $LEA_GATEWAY
            nameservers:
              addresses: [$_LEA_DNS_LIST]
  - path: /etc/hosts
    append: true
    content: "127.0.1.1 $host\n"
runcmd:
  # Bring eth0 up now (netplan does it itself from the next boot on) and
  # replace the resolv.conf symlink with the resolved stub (else DNS is dead).
  - [ netplan, apply ]
  - [ bash, -c, "rm -f /etc/resolv.conf; printf '$_LEA_DNS_LINES' > /etc/resolv.conf" ]
  # Ubuntu 24.04 ships ssh socket-activated (ssh.socket). That reports
  # "Listening" but refuses connections after a restart. Switch to the
  # classic always-running ssh.service -- persists in the overlay and
  # listens on port 22 from EVERY boot.
  - [ systemctl, disable, --now, ssh.socket ]
  - [ systemctl, enable, ssh.service ]
  # Repair empty host keys (left behind by an earlier hard stop), THEN start
  # sshd exactly once, with retries. It used to be started twice within a
  # second (enable --now, then a restart from the repair loop), and on the
  # pristine cloud image's first boot the second start raced the first and
  # sshd never came back -- measured 2026-08-18, one boot in three.
  - [ bash, -c, "for f in /etc/ssh/ssh_host_*_key; do [ -s \"\$f\" ] || { rm -f /etc/ssh/ssh_host_*; ssh-keygen -A; break; }; done; for i in 1 2 3 4 5; do systemctl restart ssh.service && break; sleep 3; done; sync" ]
EOS
    rm -f "$seed"; truncate -s 2M "$seed"; mkfs.vfat -n CIDATA "$seed" >/dev/null
    local f
    for f in user-data meta-data network-config; do mcopy -i "$seed" "$stage/$f" "::$f"; done
    rm -rf "$stage"
    info "  $INST_NAME: seed built (IP $INST_IP, key installed, user $LEA_GUEST_USER)"
}

# lea_games_writer_other_than NAME -- is another RUNNING instance holding the
# games base for writing? Prints its name if so.
#
# Two qcow2 writers on one file corrupt it, and the corruption is silent
# until something reads the part that was overwritten. The overlay path
# cannot hit this (nothing writes to the base there); --games-init can, and
# it is the one path a person reaches for twice by accident.
lea_games_writer_other_than() {
    local me=$1 n d
    for n in $(lea_inst_list); do
        [[ $n == "$me" ]] && continue
        lea_vm_running "$n" || continue
        d=$(lea_inst_dir "$n")
        # A running instance whose argv names the base itself, not an overlay.
        if [[ -r $d/ch.pid ]] && pgrep -a -F "$d/ch.pid" 2>/dev/null \
                | grep -qF "path=$LEA_GAMES_BASE,"; then
            echo "$n"; return 0
        fi
    done
    return 1
}

# lea_vm_start NAME [--index N] [--fresh] [--cpus N] [--mem MiB] [--console]
#              [--base IMAGE] [--guest ubuntu|nixos] [--transport ip|vsock]
#              [--vhu WORD]...
# Boots one guest on its tap. Generates the SSH keypair and the seed on
# first use (or on --fresh), creates the qcow2 overlay, builds the CH argv,
# retries the launch while the tap is still busy, waits for SSH.
# --vhu adds one --generic-vhost-user word (from lea_vhu_arg); the backend
# behind it must already be listening. --console runs cloud-hypervisor in
# the foreground on this terminal (Ctrl-C stops it).
#
# --guest picks WHICH GUEST, and it changes three of the argv words and
# nothing else (see the case below): kernel, initramfs, cmdline, and whether
# there is a second --disk. `ubuntu` is the default and its argv is byte for
# byte what it was before --guest existed -- diffed against a real run,
# 2026-08-18. `nixos` boots the derivation from nix/guest-image.nix and has
# NO SEED DISK: its identity rides on the kernel command line instead,
# because a stock NixOS does not read a cloud-init NoCloud seed and teaching
# it to would be a second configuration system inside a system whose whole
# point is not needing one.
lea_vm_start() {
    local name=$1; shift
    local idx="" fresh=0 cpus=$LEA_CPUS mem=$LEA_MEM console=0 base="" guest="" transport=""
    local games=0 games_init=0
    local -a extra=()
    while [[ $# -gt 0 ]]; do
        case $1 in
            --index)     idx=$2; shift 2 ;;
            --fresh)     fresh=1; shift ;;
            --cpus)      cpus=$2; shift 2 ;;
            --mem)       mem=$2; shift 2 ;;
            --console)   console=1; shift ;;
            --base)      base=$2; shift 2 ;;
            --guest)     guest=$2; shift 2 ;;
            --transport) transport=$2; shift 2 ;;
            --games)      games=1; shift ;;
            --games-init) games=1; games_init=1; shift ;;
            --vhu)       extra+=("$2"); shift 2 ;;
            *) die "lea_vm_start: unknown option $1" ;;
        esac
    done
    lea_inst "$name" "$idx"
    local was=$INST_GUEST
    [[ -n $guest ]] && INST_GUEST=$guest
    [[ -n $transport ]] && INST_TRANSPORT=$transport
    # The games disk belongs in this same early block, and for the very
    # reason the paragraph above gives: lea_vm_start refuses a conflicting
    # writer with `die`, and by then the backends are running -- which is
    # how a refused `--games` left an nvrm backend up with no VM under it
    # (measured 2026-08-20, and it is what `showcase.sh status` then shows
    # as "NVRM up, VM down").
    if [[ ${#gameopt[@]} -gt 0 ]]; then
        local gbusy
        if gbusy=$(lea_games_writer_other_than "$name"); then
            die "$gbusy is filling the games base right now (--games-init).
       Only one guest may write the base, and an overlay taken while it
       moves underneath reads garbage later. Let that one finish first."
        fi
    fi
    case $INST_GUEST in ubuntu|nixos) ;; *) die "--guest wants ubuntu or nixos, not '$INST_GUEST'" ;; esac
    case $INST_TRANSPORT in ip|vsock) ;; *) die "--transport wants ip or vsock, not '$INST_TRANSPORT'" ;; esac
    # VSOCK IS FOR THE NIXOS GUEST ONLY, and the refusal names why rather than
    # letting an Ubuntu guest boot to a login it can never be reached at. An
    # Ubuntu guest takes its identity from a cloud-init NoCloud seed that
    # configures a STATIC ADDRESS on eth0 (_lea_seed_write), and its sshd is
    # the distribution's, listening on TCP. Both halves would have to be
    # replaced to reach it over vsock, and the result would be a second
    # provisioning mechanism for an image whose reason to exist is that it is
    # convenient rather than reproducible.
    [[ $INST_TRANSPORT == vsock && $INST_GUEST != nixos ]] && die \
        "--transport vsock is for a NixOS guest; '$name' is $INST_GUEST.
       The Ubuntu image is reached at a static address its cloud-init seed
       configures, and its sshd listens on TCP only. Use --guest nixos (its
       sshd is put on AF_VSOCK by systemd's own ssh generator), or leave this
       instance on --transport ip."
    # A DISK BELONGS TO AN OS. An overlay whose backing file is the Ubuntu
    # image cannot be booted as NixOS, and the failure is not a good one: the
    # kernel and initrd come from the new guest, the root filesystem from the
    # old, and what the operator sees is a boot that hangs looking for
    # /nix/store. Recreated instead, and said out loud -- the alternative is
    # to refuse, and refusing means the operator deletes the disk by hand,
    # which is this with an extra step.
    if [[ $INST_GUEST != "$was" && -f $INST_DIR/rootfs.qcow2 ]]; then
        info "  $name: was a $was instance, is now $INST_GUEST -- its disk is recreated"
        fresh=1
    fi
    # The base image follows the guest unless the caller named one. Resolved
    # HERE rather than as an option default, because the default depends on a
    # flag that is parsed in the same loop.
    if [[ -z $base ]]; then
        case $INST_GUEST in
            nixos)  lea_nixos_image || return 1; base=$LEA_NIXOS_DIR/rootfs.qcow2 ;;
            ubuntu) base=$LEA_BASE_IMAGE ;;
        esac
    elif [[ $INST_GUEST == nixos ]]; then
        lea_nixos_image || return 1
    fi
    _lea_inst_save
    echo $$ > "$INST_DIR/owner"
    local dir=$INST_DIR ip=$INST_IP tap=$INST_TAP mac=$INST_MAC
    local rootfs=$dir/rootfs.qcow2 seed=$dir/seed.img pidf=$dir/ch.pid
    local serial=$dir/serial.log chlog=$dir/ch.log taken

    [[ -x $LEA_CH ]]  || die "$LEA_CH missing. Fix: scripts/build.sh ch"
    [[ -f $base ]]    || die "$base missing. Fix: scripts/build.sh image"
    # A baked image carries the driver version its userspace was staged
    # for (its .manifest). One that disagrees with DRIVER_VERSION is the
    # mismatch this whole project warns about, on a disk nobody re-reads.
    local mf bd
    mf=${base%.qcow2}.manifest
    if [[ -f $mf ]]; then
        bd=$(sed -n 's/^driver userspace: *//p' "$mf" | tr -d '[:space:]')
        [[ -z $bd || $bd == "$(lea_want_driver)" ]] \
            || warn "$(basename "$base") was baked for driver $bd, this tree targets $(lea_want_driver) -- the guest's libcuda will not match nvidia.ko. Bake again (build.sh bake)."
    fi

    if lea_running "$pidf"; then
        info "$name already running (PID $(cat "$pidf")). SSH: showcase.sh ssh --name $name"
        return 0
    fi
    taken=$(_lea_index_taken "$INST_IDX" "$name") && \
        die "index $INST_IDX (IP $ip) is in use by running instance '$taken'"

    # The tap must exist and be owned by this user -- it is NOT created
    # here, that is host setup (lea_net_up).
    #
    # ON VSOCK THERE IS NO TAP, and this block is the ONLY sudo on the path
    # from `up` to a running guest. Skipping it is what makes an unprivileged
    # run possible at all: no bridge, no tap, no NAT rule, no ip_forward, and
    # therefore nothing that needs CAP_NET_ADMIN. See DEVELOPMENT.md section 7.
    if [[ $INST_TRANSPORT == ip ]]; then
        ip link show "$tap" >/dev/null 2>&1 || die "tap $tap missing. Fix: showcase.sh net up"
        local br
        br=$(ip -o link show "$tap" | grep -o 'master [^ ]*' | awk '{print $2}' || true)
        sudo ip link set "$tap" up
        [[ -n $br ]] && sudo ip link set "$br" up || true
    fi

    if [[ ! -f $LEA_SSH_KEY ]]; then
        mkdir -p "$(dirname "$LEA_SSH_KEY")"
        ssh-keygen -q -t ed25519 -N "" -f "$LEA_SSH_KEY" -C "$LEA_GUEST_USER-vm"
        info "SSH key generated: $LEA_SSH_KEY(.pub)"
    fi

    # Disk and seed are ONE instance: whenever the disk is (re)created the
    # seed goes with it, so cloud-init sees a new instance-id and re-runs.
    if [[ $fresh -eq 1 || ! -f $rootfs ]]; then
        # Persistent qcow2 overlay on the untouched base image. The type is
        # passed EXPLICITLY as image_type=qcow2 below -- otherwise the newer
        # CH guesses 'raw', discards the backing file and blocks sector-0
        # writes (I/O error dev vda), and first-boot state does not survive.
        rm -f "$rootfs" "$seed"
        qemu-img create -q -f qcow2 -F qcow2 -b "$base" "$rootfs" "$LEA_DISK_SIZE"
        info "  $name: fresh instance disk $rootfs ($LEA_DISK_SIZE, sparse overlay on $(basename "$base"))"
    fi
    # WHERE THE GUEST IDENTITY COMES FROM, and it is the one place the two
    # guests genuinely differ. Ubuntu: a FAT "CIDATA" seed disk that
    # cloud-init reads on first boot -- hence the second --disk and
    # `ds=nocloud`. NixOS: six words on the kernel command line, read by
    # leandro-identity.service before the network comes up (nix/guest-image.nix
    # documents the vocabulary). base64 for the key because the kernel splits
    # the command line on spaces and an authorized_keys line has two; no dots
    # in the names because `foo.bar=` is a MODULE parameter to the kernel and
    # it complains about every unclaimed one on every boot.
    local kernel initrd cmdline
    local -a disks=("path=$rootfs,image_type=qcow2,backing_files=on")
    case $INST_GUEST in
        ubuntu)
            kernel=$LEA_KERNEL; initrd=$LEA_INITRD
            cmdline="root=/dev/vda1 rw console=ttyS0 ds=nocloud net.ifnames=0 loglevel=4 systemd.mask=systemd-networkd-wait-online.service systemd.mask=snapd.service systemd.mask=snapd.seeded.service"
            [[ -f $seed ]] || _lea_seed_write
            disks+=("path=$seed,image_type=raw")
            ;;
        nixos)
            kernel=$LEA_NIXOS_DIR/kernel; initrd=$LEA_NIXOS_DIR/initrd
            local key64
            key64=$(base64 -w0 < "$LEA_SSH_KEY.pub") || return 1
            cmdline="init=$LEA_NIXOS_INIT $LEA_NIXOS_CMDLINE_BASE"
            cmdline+=" lea_user=$LEA_GUEST_USER"
            cmdline+=" lea_host=$LEA_HOSTNAME_PREFIX-$INST_NAME"
            # The ADDRESS half of the identity only on the transport that has
            # one. A vsock guest has no network device, so telling it an
            # address would have it write a .network file matching an
            # interface that does not exist -- harmless, and a lie in the
            # guest's own configuration. leandro-identity handles the absence
            # (it says so on the console and leaves eth0 to networkd).
            [[ $INST_TRANSPORT == ip ]] && \
                cmdline+=" lea_ip=$ip lea_prefix=$LEA_NETMASK lea_gw=$LEA_GATEWAY"
            cmdline+=" lea_sshkey=$key64"
            # A seed left over from a previous life of this instance name
            # would be handed to nothing; drop it rather than leave a file
            # that reads as if it were in force.
            rm -f "$seed"
            info "  $name: NixOS, identity on the command line (no seed disk)"
            ;;
    esac

    # LEA_GUEST_CMDLINE_EXTRA -- appended verbatim, for the runs that want a
    # guest kernel in a different mood than the default one. It exists for
    # debug knobs that have to be set at boot and cannot be set later:
    # `slub_debug=FZPU page_poison=1` turns a use-after-free in the guest
    # module from a hang three seconds later into a splat with the
    # allocating and freeing stacks in it (the ioctl-matrix guest sweep sets
    # exactly that). Kept out of the default because those knobs cost real
    # time on every allocation, and every measurement taken with them on is
    # a measurement of them too.
    #
    # NOT a place for identity or paths: it is appended after everything the
    # cases above decided, so a value here wins by position and would
    # silently override them.
    if [[ -n ${LEA_GUEST_CMDLINE_EXTRA:-} ]]; then
        cmdline+=" $LEA_GUEST_CMDLINE_EXTRA"
        info "  $name: extra kernel cmdline: $LEA_GUEST_CMDLINE_EXTRA"
    fi

    # The Steam library, when asked for. --games-init hands the guest the
    # BASE itself, to fill it once; --games gives it a thin overlay, which
    # is what lets two guests run from the same library at the same time.
    # A base that is not there is not an error: the flag is a request, and
    # the guest simply has no library disk (the mount is `nofail`).
    local games_disk=""
    if [[ $games -eq 1 ]]; then
        if [[ $games_init -eq 1 ]]; then
            [[ -f $LEA_GAMES_BASE ]] \
                || die "--games-init: no $LEA_GAMES_BASE -- run: showcase.sh games init"
            local other
            if other=$(lea_games_writer_other_than "$name"); then
                die "--games-init: $other already holds the games BASE for writing.
       Only one guest may fill it at a time, or the image is corrupted.
       Stop it first, or start this one with --games (a read-only overlay)."
            fi
            games_disk=$LEA_GAMES_BASE
            warn "$name: writing DIRECTLY to $LEA_GAMES_BASE (--games-init)"
        elif [[ -f $LEA_GAMES_BASE ]]; then
            # The other direction of the same rule: an overlay on a base that
            # someone is WRITING is an overlay onto a moving target, and what
            # it reads afterwards is undefined. The writer is the one that
            # has to finish first.
            local busy
            if busy=$(lea_games_writer_other_than "$name"); then
                die "$busy is filling the games base right now (--games-init).
       An overlay taken while the base changes underneath it reads garbage.
       Let it finish and shut it down first."
            fi
            games_disk=$(lea_inst_dir "$name")/games.qcow2
            if [[ ! -f $games_disk ]]; then
                qemu-img create -q -f qcow2 -F qcow2 -b "$LEA_GAMES_BASE" "$games_disk" \
                    || die "$name: could not create the games overlay"
                info "  $name: games overlay on $(basename "$LEA_GAMES_BASE")"
            fi
        else
            warn "$name: --games, but $LEA_GAMES_BASE does not exist -- no library disk"
        fi
        if [[ -n $games_disk ]]; then
            disks+=("path=$games_disk,image_type=qcow2,backing_files=on")
            # Part of the instance's state, like `guest` and `transport`:
            # the provisioning step must not go looking for an unformatted
            # disk on a guest that was never given one.
            echo "$games_disk" > "$(lea_inst_dir "$name")/games"
        fi
    else
        rm -f "$(lea_inst_dir "$name")/games"
    fi

    # HOW THE HOST REACHES IT, and it is exactly one argv word either way.
    # ip:    a virtio-net device on this slot's tap, and the guest gets the
    #        address the identity carries.
    # vsock: a virtio-vsock device and NO network device at all -- which is
    #        the point, not a side effect. Nothing in the guest waits for a
    #        link that will never come up: measured 2026-08-19, a NixOS guest
    #        booted this way reaches multi-user with only `lo`, and systemd's
    #        ssh generator has already put sshd on AF_VSOCK port 22.
    #
    # The Ubuntu path cannot reach the vsock branch (refused above), so its
    # argv is what it always was -- diffed against a real run and byte-identical.
    #
    # BOTH branches drop a stale socket at this slot first. `lea_rig_down`
    # removes it, but a VM killed hard never gets there, and the leftover is
    # an inode that still passes `[[ -S ... ]]` -- so on the vsock branch
    # cloud-hypervisor would refuse to bind over it, and on the IP branch
    # lea_ssh_opts would read this slot as a vsock instance and route ssh to
    # a socket nobody is listening on. One `rm -f` covers both.
    local -a net_args=()
    rm -f "$INST_VSOCK_SOCK"
    case $INST_TRANSPORT in
        ip)    net_args=(--net "tap=$tap,mac=$mac") ;;
        vsock) net_args=(--vsock "cid=$INST_VSOCK_CID,socket=$INST_VSOCK_SOCK") ;;
    esac

    local -a ch_args=(
        --cpus "boot=$cpus"
        --memory "size=${mem}M,shared=on"
        --kernel "$kernel"
        --initramfs "$initrd"
        --cmdline "$cmdline"
        --disk "${disks[@]}"
        "${net_args[@]}"
    )
    local w
    for w in "${extra[@]}"; do
        # Each word is "--generic-vhost-user device_type=...": two argv
        # entries, split on the one space it contains.
        # shellcheck disable=SC2206
        ch_args+=($w)
    done

    if [[ $console -eq 1 ]]; then
        ch_args+=(--serial tty --console off)
        info "starting $name interactively (Ctrl-C stops). Login: $LEA_GUEST_USER / $LEA_GUEST_USER"
        "$LEA_CH" "${ch_args[@]}"
        return $?
    fi

    # Background, serial to a file, then wait for SSH. The start is retried
    # tolerantly: after a previous run the kernel needs a moment to release
    # the tap.
    ch_args+=(--serial "file=$serial" --console off)
    : >"$serial"
    local attempt chpid
    for attempt in 1 2 3 4 5; do
        "$LEA_CH" "${ch_args[@]}" >"$chlog" 2>&1 &
        chpid=$!
        sleep 0.5
        if kill -0 "$chpid" 2>/dev/null; then break; fi
        if grep -q "resource busy" "$chlog" 2>/dev/null; then
            info "  tap still busy, retrying ($attempt) ..."; sleep 1; continue
        fi
        error "$name did not start:"; tail -8 "$chlog" >&2; return 1
    done
    echo "$chpid" >"$pidf"
    info "  $name: VM starting (PID $chpid, IP $ip), serial: $serial"

    local rc=0
    lea_wait_ssh "$ip" 60 "$pidf" || rc=$?
    if [[ $rc -eq 0 ]]; then
        info "  $name: SSH up ($ip)"
        return 0
    fi
    if [[ $rc -eq 2 ]]; then
        error "$name exited. Console:"; tail -20 "$serial" >&2
    else
        error "$name: SSH did not come up within 120s. First boot messages:"
        tail -30 "$serial" >&2
        error "A frequent cause is a stale cloud-init seed carrying an older guest identity -- retry with --fresh."
    fi
    return 1
}

# lea_vm_stop NAME -- GRACEFUL FIRST: the guest has to shut down cleanly,
# otherwise unsynced writes are lost -- among them the CONTENT of the SSH
# host keys, which are then left as 0-byte files and stop sshd from
# starting on the next boot (connection refused). poweroff syncs and shuts
# down; CH exits as soon as the guest is gone. Only if the guest did not
# react: hard, but after the attempt.
lea_vm_stop() {
    local name=$1 pidf p
    lea_inst "$name"
    pidf=$INST_DIR/ch.pid
    [[ -f $pidf ]] || return 0
    p=$(cat "$pidf")
    if kill -0 "$p" 2>/dev/null; then
        lea_ssh "$INST_IP" 'sudo systemctl poweroff' >/dev/null 2>&1 || true
        for _ in $(seq 100); do kill -0 "$p" 2>/dev/null || break; sleep 0.2; done
    fi
    if kill -0 "$p" 2>/dev/null; then
        kill "$p" 2>/dev/null || true
        for _ in $(seq 50); do kill -0 "$p" 2>/dev/null || break; sleep 0.1; done
        kill -9 "$p" 2>/dev/null || true
    fi
    rm -f "$pidf"
    return 0
}

# lea_vm_ssh NAME [command...] -- into that instance (or a shell).
lea_vm_ssh() {
    local name=$1; shift
    lea_inst "$name"
    [[ -f $LEA_SSH_KEY ]] || die "$LEA_SSH_KEY missing -- no VM has been started yet"
    lea_ssh "$INST_IP" "$@"
}

# ---- one rig --------------------------------------------------------------
# lea_rig_up NAME [--index N] [--fresh] [--mem MiB] [--cpus N]
#            [--no-compute] [--input] [--display] [--session gnome|openbox]
#            [--with-steam] [--with-torch] [--no-provision] [--no-load]
#            [--vram-limit MiB] [--vram-profile MiB] [--vgpu-type TYPE]
#            [--max-pin-mib N]
#            [--console] [--base IMAGE]
#            [--guest ubuntu|nixos] [--transport ip|vsock]
# The whole path for one guest: network, backends, VM, provisioning, guest
# module -- and with --display the virtual-display rig on top (NVKMS
# modules, X), with --session a desktop and Sunshine beside it.
#
# THE ORDER IS THE POINT and every earlier caller had its own copy of it:
#   1. lea_guest_setup       userspace, probes, nvrm_nodes.ko   (unless --no-provision)
#   2. lea_guest_build_nvrm  builds and loads virtio_nvrm.ko    (unless --no-load)
#   3. lea_display_up        NVKMS modules + X on the virtual display (--display)
#      -- AFTER 2, because nvidia-modeset.ko links against virtio_nvrm's
#      symbols, and because step 2 reloads virtio_nvrm and thereby resets
#      every module parameter the display setup writes.
#   4. lea_desktop_up        gdm3/GNOME or openbox, Sunshine, read-back (--session)
lea_rig_up() {
    local name=$1; shift
    local idx="" fresh=0 mem="" cpus="" compute=1 input=0 display=0 session=""
    local steam=0 torch=0 provision=1 load=1 cap="" prof="" vtype="" pin="" console=0 base="" guest="" transport="" wayland=0
    local -a gameopt=()
    while [[ $# -gt 0 ]]; do
        case $1 in
            --index)        idx=$2; shift 2 ;;
            --base)         base=$2; shift 2 ;;
            --guest)        guest=$2; shift 2 ;;
            --transport)    transport=$2; shift 2 ;;
            --fresh)        fresh=1; shift ;;
            --mem)          mem=$2; shift 2 ;;
            --cpus)         cpus=$2; shift 2 ;;
            --no-compute)   compute=0; shift ;;
            --input)        input=1; shift ;;
            --display)      display=1; input=1; shift ;;
            --session)      session=$2; display=1; input=1; shift 2 ;;
            --with-steam)   steam=1; shift ;;
            --with-torch)   torch=1; shift ;;
            --no-provision) provision=0; shift ;;
            --no-load)      load=0; shift ;;
            --vram-limit)   cap=$2; shift 2 ;;
            --vram-profile) prof=$2; shift 2 ;;
            --vgpu-type)    vtype=$2; shift 2 ;;
            --max-pin-mib)  pin=$2; shift 2 ;;
            --wayland)      wayland=1; shift ;;
            --games)        gameopt=(--games); shift ;;
            --games-init)   gameopt=(--games-init); shift ;;
            --console)      console=1; shift ;;
            *) die "lea_rig_up: unknown option $1" ;;
        esac
    done
    lea_inst "$name" "$idx"
    # WHAT THIS INSTANCE WAS, read before anything overwrites it. _lea_inst_save
    # below has to run early -- lea_backend_start resolves the instance by NAME
    # alone and needs vm/<name>/index on disk -- but writing the new guest and
    # transport first would erase the difference lea_vm_start's "a disk belongs
    # to an OS" rule is looking for, leaving that rule as dead code on this
    # path: an Ubuntu overlay would then be booted with a NixOS kernel.
    local was_os=$INST_GUEST
    [[ -n $guest ]] && INST_GUEST=$guest
    [[ -n $transport ]] && INST_TRANSPORT=$transport
    # VALIDATED BEFORE IT IS WRITTEN DOWN. lea_vm_start checks these too, but
    # it is called after the backends are up and it reports a bad value with
    # `die` -- which exits the process past lea_rig_up's own cleanup, leaving a
    # backend running and a typo like `--transport vsokc` recorded on disk,
    # where every later flagless `up` reads it back and fails the same way.
    case $INST_GUEST in ubuntu|nixos) ;; *) die "--guest wants ubuntu or nixos, not '$INST_GUEST'" ;; esac
    case $INST_TRANSPORT in ip|vsock) ;; *) die "--transport wants ip or vsock, not '$INST_TRANSPORT'" ;; esac
    [[ $INST_TRANSPORT == vsock && $INST_GUEST != nixos ]] && die \
        "--transport vsock is for a NixOS guest; '$name' is $INST_GUEST.
       The Ubuntu image is reached at a static address its cloud-init seed
       configures, and its sshd listens on TCP only. Use --guest nixos, or
       leave this instance on --transport ip."
    if [[ $INST_GUEST != "$was_os" && -f $INST_DIR/rootfs.qcow2 ]]; then
        info "  $name: was a $was_os instance, is now $INST_GUEST -- its disk is recreated"
        fresh=1
    fi
    _lea_inst_save
    local dir=$INST_DIR ip=$INST_IP index=$INST_IDX os=$INST_GUEST tr=$INST_TRANSPORT
    # The desktop rig wants more of everything (measured 2026-08-15).
    if [[ $display -eq 1 ]]; then
        : "${mem:=$LEA_DESKTOP_MEM}"; : "${cpus:=$LEA_DESKTOP_CPUS}"
    else
        : "${mem:=$LEA_MEM}"; : "${cpus:=$LEA_CPUS}"
    fi

    lea_vm_running "$name" && die "$name is already up -- 'showcase.sh down --name $name' first"

    # THE ONLY STEP ON THIS PATH THAT NEEDS ROOT, and on vsock it is not
    # taken. lea_net_ready creates a bridge, a tap per slot, a MASQUERADE rule
    # and ip_forward -- four host-wide changes, all through sudo. A vsock
    # instance needs none of them, which is what makes an unprivileged run on
    # a cluster node possible (DEVELOPMENT.md section 7).
    if [[ $tr == ip ]]; then
        info "== $name: host network =="
        lea_net_ready "$((index + 1))"
        info "  bridge $LEA_BRIDGE and tap$index: ready"
    else
        info "== $name: transport =="
        info "  vsock CID $(lea_vsock_cid "$index") on $(lea_vsock_sock "$index") -- no bridge, no tap, no sudo"
    fi

    info "== $name: backends =="
    local -a vhu=()
    if [[ $input -eq 1 ]]; then
        lea_backend_start "$name" input || { lea_backend_stop "$name"; return 1; }
        vhu+=(--vhu "$(lea_vhu_arg "$name" input)")
    fi
    if [[ $compute -eq 1 ]]; then
        lea_backend_start "$name" nvrm ${cap:+--vram-limit "$cap"} \
            ${prof:+--vram-profile "$prof"} ${vtype:+--vgpu-type "$vtype"} \
            || { lea_backend_stop "$name"; return 1; }
        vhu+=(--vhu "$(lea_vhu_arg "$name" nvrm)")
    fi

    info "== $name: VM ($os, $tr) =="
    local -a vmopts=(--index "$index" --mem "$mem" --cpus "$cpus" --guest "$os" --transport "$tr")
    [[ ${#gameopt[@]} -gt 0 ]] && vmopts+=("${gameopt[@]}")
    [[ $fresh -eq 1 ]] && vmopts+=(--fresh)
    # A per-instance base image: the desktop rig overlays the desktop-baked
    # image while vm0 keeps the compute one. Only consulted when the disk is
    # (re)created -- an existing overlay keeps its backing file.
    [[ -n $base ]] && vmopts+=(--base "$base")
    if [[ $console -eq 1 ]]; then
        lea_vm_start "$name" "${vmopts[@]}" --console "${vhu[@]}"
        lea_backend_stop "$name"
        return 0
    fi
    lea_vm_start "$name" "${vmopts[@]}" "${vhu[@]}" || {
        error "$name: VM start failed"
        lea_backend_stop "$name"
        return 1
    }
    [[ $compute -eq 1 ]] || return 0

    if [[ $provision -eq 1 ]]; then
        info "== $name: provisioning (userspace, probes, nvrm_nodes.ko) =="
        local -a sopt=(); [[ $torch -eq 1 ]] && sopt+=(--with-torch)
        lea_guest_setup "$name" "${sopt[@]}" >"$dir/setup.log" 2>&1 \
            && info "  $name: provisioned" \
            || { error "$name: provisioning failed -- $dir/setup.log"
                 tail -5 "$dir/setup.log" >&2
                 # SAY HOW TO GET OUT OF IT. The VM is still running and
                 # `up` refuses a running instance, so the next thing anybody
                 # types is the wrong thing. Reported 2026-08-21 by somebody
                 # who was left with a booted guest, no modules, and no
                 # obvious way forward.
                 error "the VM is still UP and no modules are loaded, because
loading is the step after this one. To retry provisioning:
  scripts/showcase.sh down --name $name --force && scripts/showcase.sh up --name $name
The disk is fine -- '--fresh' is only needed if you want to discard it."
                 return 1; }
    fi
    if [[ $load -eq 1 ]]; then
        info "== $name: guest module (virtio_nvrm.ko) =="
        lea_guest_build_nvrm "$name" ${pin:+--max-pin-mib "$pin"} \
            >"$dir/load.log" 2>&1 \
            && info "  $name: modules loaded" \
            || { error "$name: loading failed -- $dir/load.log"; tail -3 "$dir/load.log" >&2; return 1; }
    fi
    if [[ $display -eq 1 ]]; then
        # SECOND CLASS, and said out loud rather than discovered: the display
        # path stages NVIDIA's GL/EGL/Vulkan userspace and builds
        # nvidia-modeset.ko and nvidia-drm.ko INSIDE the guest against its
        # kernel headers (lea_display_stage, lea_guest_build_nvkms). On NixOS
        # neither has an equivalent yet -- the modules would have to come from
        # boot.extraModulePackages like the two Leandro ones. Refused here
        # rather than half-attempted three functions later.
        [[ $tr == vsock ]] && { error "$name: --display/--session needs a display, and the vsock transport
       exists to run without one -- no network device, no X, no streaming.
       Use --transport ip for a display rig."; return 1; }
        [[ $os == nixos ]] && { error "$name: --display/--session is not implemented for a NixOS guest.
       The compute path is (showcase.sh up --guest nixos); the display path
       still needs an in-guest NVKMS build, which NixOS does not do. Use an
       Ubuntu instance for the display rig."; return 1; }
        info "== $name: virtual display =="
        if [[ -n $session ]]; then
            local -a dopt=(); [[ $steam -eq 1 ]] && dopt+=(--with-steam)
        [[ $wayland -eq 1 ]] && dopt+=(--wayland)
            lea_desktop_up "$name" --session "$session" "${dopt[@]}" || return 1
        else
            lea_display_up "$name" || return 1
        fi
    fi
    info "  $name ($ip): up"
    return 0
}

# lea_rig_down NAME [--force] -- VM first, then backends. Also cleans up
# after a half-finished up: it does not care which parts came up. Refuses
# (return 2) while another LIVE script owns the instance -- see
# lea_inst_owner; --force overrides.
lea_rig_down() {
    local name=$1 dir owner force=0
    [[ ${2:-} == --force ]] && force=1
    dir=$(lea_inst_dir "$name")
    [[ -d $dir ]] || return 0
    if [[ $force -eq 0 ]] && owner=$(lea_inst_owner "$name"); then
        error "$name is in use by pid $owner ($(tr '\0' ' ' < "/proc/$owner/cmdline" 2>/dev/null | cut -c1-60)) -- not touched (--force to override)"
        return 2
    fi
    if lea_vm_running "$name"; then
        lea_vm_stop "$name"
    else
        rm -f "$dir/ch.pid"
    fi
    lea_backend_stop "$name"
    # The hybrid vsock socket is this slot's host-side endpoint and it goes
    # down with the VM -- the same way `net down` removes the tap.
    #
    # ONLY FOR A VSOCK INSTANCE, and that guard is not decoration. The socket
    # is keyed on the INDEX, and two instance directories may legitimately
    # share an index as long as only one of them RUNS (_lea_index_taken
    # refuses the running case, not the stored one). Without the guard,
    # taking down a stopped instance -- `lea_fleet_down` sweeps vm0..vm7 on
    # every bench exit, `build.sh bake` downs `bake` at index 1 before it
    # discovers the collision -- unlinks the live occupant's endpoint out
    # from under it. The VM keeps running and every later lea_ssh silently
    # falls back to an address a vsock guest does not have.
    if [[ $(lea_transport_of "$name") == vsock ]]; then
        local _idx; _idx=$(lea_guest_idx "$name")
        [[ -n $_idx ]] && rm -f "$(lea_vsock_sock "$_idx")"
    fi
    rm -f "$dir/owner"
    info "$name: down"
}

# lea_rig_down_on_exit NAME -- stop that rig on the way out unless
# LEA_KEEP_VM=1 (the gates' --keep-vm).
lea_rig_down_on_exit() {
    lea_on_exit "[[ \${LEA_KEEP_VM:-0} -eq 1 ]] || lea_rig_down $(printf '%q' "$1") >/dev/null 2>&1"
}

# lea_rig_status [NAME...] -- one line per instance (all of them by default).
lea_rig_status() {
    local -a names=("$@")
    [[ ${#names[@]} -gt 0 ]] || mapfile -t names < <(lea_inst_list)
    printf "%-12s %-3s %-7s %-6s %-16s %-6s %-5s %-6s %-6s %s\n" NAME IDX GUEST TRANSP IP TAP VM NVRM INPUT MODULE
    local n vmup mod
    for n in "${names[@]}"; do
        lea_inst "$n" 2>/dev/null || continue
        vmup=down; lea_vm_running "$n" && vmup=up
        mod=-
        if [[ $vmup == up ]]; then
            # tr -d: the guest answer arrives with a newline, and an unstripped
            # one broke this table into two lines per VM.
            mod=$(lea_ssh "$INST_IP" 'lsmod | grep -c "^virtio_nvrm "' 2>/dev/null | tr -dc '0-9')
            case "${mod:-}" in
                1) mod=loaded ;;
                "") mod=unreachable ;;
                *) mod="no($mod)" ;;
            esac
        fi
        # On vsock the address column is a LABEL, not a reachable address:
        # the guest has no network device. Shown all the same, because it is
        # the handle every command still takes.
        printf "%-12s %-3s %-7s %-6s %-16s %-6s %-5s %-6s %-6s %s\n" "$n" "$INST_IDX" "$INST_GUEST" \
            "$INST_TRANSPORT" "$INST_IP" "$([[ $INST_TRANSPORT == ip ]] && echo "$INST_TAP" || echo -)" \
            "$vmup" "$(_lea_backend_alive "$n" nvrm)" "$(_lea_backend_alive "$n" input)" "$mod"
    done
}

# lea_foreign_rigs [OWN...] -- names the RUNNING instances that are not the
# caller's own. A gate or a bench that tears its rig down must not touch
# anybody else's; and 2026-08-07 it did, measured: a gate run at 22:39 sent
# SIGTERM to the desktop instance's backend. cloud-hypervisor stayed up, so
# it did not look like a kill at all -- the guest simply hung forever in
# nvrm_xfer_run on the next RM call. Returns 0 when it printed something.
lea_foreign_rigs() {
    local n own found=1 dir
    for n in $(lea_inst_list); do
        for own in "$@"; do [[ $n == "$own" ]] && continue 2; done
        dir=$(lea_inst_dir "$n")
        if lea_running "$dir/ch.pid" || lea_running "$dir/nvrm.pid" || lea_running "$dir/input.pid"; then
            echo "$n"
            found=0
        fi
    done
    return $found
}

# ---- N rigs: the fleet ------------------------------------------------------
# Members are vm0..vm<N-1>. vm0 keeps its own full disk; members 1..N-1 are
# THIN qcow2 OVERLAYS on LEA_FLEET_BASE, a frozen copy of a fully provisioned
# disk -- which is why that base must never be written again, and why the
# overlays are disposable.
_lea_fleet_overlay() {
    local i=$1 fresh=$2 os=${3:-ubuntu} tr=${4:-ip}
    lea_inst "vm$i"
    INST_TRANSPORT=$tr
    local was=$INST_GUEST
    local rootfs=$INST_DIR/rootfs.qcow2 seed=$INST_DIR/seed.img

    # A DISK BELONGS TO AN OS, and that has to be decided BEFORE the new one
    # is written down. _lea_inst_save erases exactly the difference
    # lea_vm_start would otherwise notice, so recording first and asking
    # afterwards makes the member boot a NixOS kernel against an Ubuntu root
    # filesystem. Measured 2026-08-18: it lands in emergency mode with
    # "Cannot open access to console, the root account is locked", which
    # reads like a broken image rather than a mixed-up disk.
    local changed=0
    [[ $os != "$was" && -f $rootfs ]] && changed=1
    INST_GUEST=$os
    _lea_inst_save

    # WHICH BASE a fleet member overlays, and why the two guests differ here.
    # An Ubuntu member hangs off LEA_FLEET_BASE, a frozen copy of a FULLY
    # PROVISIONED disk, and it is a HARD requirement: provisioning an Ubuntu
    # guest means an apt run and a 2.5 GiB torch download per member.
    # A NixOS member needs almost none of that -- the image IS the
    # provisioning -- so the image itself is a working base and the frozen
    # one is only an optimisation. What it saves is the torch venv, which
    # cannot be in the image: the gate and this bench compare against the
    # HOST's reference venv (vendor/hostvenv, torch 2.13.0+cu130), so it has
    # to be the same pip wheels rather than nixpkgs' torch.
    local fbase=$LEA_FLEET_BASE
    if [[ $os == nixos ]]; then
        lea_nixos_image || return 1
        if [[ -f $LEA_NIXOS_FLEET_BASE ]]; then
            fbase=$LEA_NIXOS_FLEET_BASE
        else
            fbase=$LEA_NIXOS_DIR/rootfs.qcow2
            info "  vm$i: no $(basename "$LEA_NIXOS_FLEET_BASE") -- overlaying the image itself;
       this member provisions from scratch, torch venv included. Freeze a
       provisioned NixOS disk there to skip that (scripts/lib/config.sh)."
        fi
    fi

    # Member 0 IS the standard dev VM (vm/vm0/) and keeps its own disk: the
    # gpu gate addresses that instance by name and a fleet run has no
    # business throwing its state away. Its equivalence to members 1..N-1 is
    # by construction -- LEA_FLEET_BASE is a frozen COPY of a provisioned
    # vm0.
    #
    # THAT RELATIONSHIP DOES NOT EXIST FOR A NIXOS FLEET. Its frozen base is
    # a provisioned image, not a copy of anybody's vm0, so member 0 is an
    # ordinary member there and hangs off the same base as the rest -- which
    # is what makes all N of them the same guest, and a fleet whose member 0
    # is a different guest from the other three measures nothing.
    [[ $i -eq 0 && $os == ubuntu && $changed -eq 0 ]] && return 0
    [[ ($fresh -eq 1 || $changed -eq 1) && -f $rootfs ]] && rm -f "$rootfs" "$seed"
    [[ -f $rootfs ]] && return 0
    # `return`, not `die`: the caller tears the half-built fleet down, and an
    # exit here would jump straight past that cleanup.
    [[ -f $fbase ]] || {
        error "$fbase is missing -- it is the frozen base every fleet
       overlay hangs off. Create it from a fully provisioned dev VM
       (stopped!):  cp $LEA_VM_DIR/vm0/rootfs.qcow2 $fbase"
        return 1
    }
    info "  vm$i: creating overlay on $(basename "$fbase")"
    qemu-img create -q -f qcow2 -F qcow2 -b "$fbase" "$rootfs" "$LEA_DISK_SIZE" || return 1
    # Drop the stale seed with it, so a fresh one carrying the CURRENT
    # identity and a new instance-id is built (which is what makes
    # cloud-init re-run on a disk that has already booted once). A NixOS
    # member has no seed at all -- its identity is on the kernel command
    # line -- and rm on a file that is not there is not an error.
    rm -f "$seed"
}

# lea_fleet_up N [--fresh] [--no-load] [--mem MiB] -- reports success only
# once every member answers over SSH and is provisioned. PER MEMBER:
# LEA_VRAM_LIMIT_MIB_<i> beats LEA_VRAM_LIMIT_MIB, and LEA_VRAM_PROFILE_MIB_<i>
# beats LEA_VRAM_PROFILE_MIB the same way. A cap that is the same for
# everyone answers "does the cap work"; a cap that differs per member
# answers "does one tenant's cap hold while the neighbour has none", which
# is the question a cap exists for. The two policies are per member as
# well, so a fleet can run one of each -- the backend refuses only when
# BOTH are set on the SAME member.
lea_fleet_up() {
    local count=$1; shift
    local fresh=0 load=1 mem=$LEA_MEM os=ubuntu tr=ip i cap_var cap prof_var prof
    while [[ $# -gt 0 ]]; do
        case $1 in
            --fresh)     fresh=1; shift ;;
            --no-load)   load=0; shift ;;
            --mem)       mem=$2; shift 2 ;;
            --guest)     os=$2; shift 2 ;;
            --transport) tr=$2; shift 2 ;;
            *) die "lea_fleet_up: unknown option $1" ;;
        esac
    done
    [[ $count -ge 1 && $count -le $LEA_MAX_VMS ]] \
        || die "--count must be between 1 and $LEA_MAX_VMS (LEA_MAX_VMS in scripts/lib/config.sh)"
    info "== fleet of $count ($os, $tr) =="
    for i in $(seq 0 $((count - 1))); do
        _lea_fleet_overlay "$i" "$fresh" "$os" "$tr" || { error "vm$i: overlay failed"; lea_fleet_down; return 1; }
        cap_var="LEA_VRAM_LIMIT_MIB_$i"; cap="${!cap_var:-${LEA_VRAM_LIMIT_MIB:-}}"
        prof_var="LEA_VRAM_PROFILE_MIB_$i"; prof="${!prof_var:-${LEA_VRAM_PROFILE_MIB:-}}"
        # Not --fresh here: the overlay step above already recreated the disk
        # and seed when asked, and lea_vm_start keeps what exists.
        local -a lopt=(); [[ $load -eq 0 ]] && lopt+=(--no-load)
        lea_rig_up "vm$i" --index "$i" --mem "$mem" --guest "$os" --transport "$tr" \
            ${cap:+--vram-limit "$cap"} ${prof:+--vram-profile "$prof"} "${lopt[@]}" \
            >"$LEA_VM_DIR/vm$i/up.log" 2>&1 \
            && info "  vm$i ($(lea_ip "$i")): up" \
            || { error "vm$i: failed -- $LEA_VM_DIR/vm$i/up.log"; tail -8 "$LEA_VM_DIR/vm$i/up.log" >&2
                 error "fleet incomplete -- cleaning up"; lea_fleet_down; return 1; }
    done
    lea_fleet_status
}

# lea_fleet_count -- how many members are up, counted from vm0 upwards.
# "how many" is a fact about the rig, not a preference: exec against four
# members while two are up produced two real results and two "No route to
# host", which reads like half the fleet crashed.
lea_fleet_count() {
    local i n=0
    for i in $(seq 0 $((LEA_MAX_VMS - 1))); do
        lea_vm_running "vm$i" || break
        n=$((n + 1))
    done
    echo "$n"
}

# lea_fleet_down -- every slot, not just the running ones: a failed `up`
# may have left a backend or a pidfile behind at any index.
lea_fleet_down() {
    local i
    for i in $(seq 0 $((LEA_MAX_VMS - 1))); do
        [[ -d $LEA_VM_DIR/vm$i ]] && lea_rig_down "vm$i" >/dev/null
    done
    info "fleet down."
}

lea_fleet_status() {
    local i n; n=$(lea_fleet_count)
    local -a names=()
    for i in $(seq 0 $((n > 0 ? n - 1 : 0))); do [[ -d $LEA_VM_DIR/vm$i ]] && names+=("vm$i"); done
    [[ ${#names[@]} -gt 0 ]] && lea_rig_status "${names[@]}" || info "no fleet member is up"
}

# lea_fleet_exec COMMAND... -- the same command on every running member
# SIMULTANEOUSLY -- the core of the parallel measurement. Output per member
# in vm/vm<i>/exec.log, status in exec.rc.
lea_fleet_exec() {
    local n i; n=$(lea_fleet_count)
    [[ $n -gt 0 ]] || die "no fleet member is up"
    local -a pids=()
    for i in $(seq 0 $((n - 1))); do
        ( lea_ssh "$(lea_ip "$i")" "$@" >"$LEA_VM_DIR/vm$i/exec.log" 2>&1
          echo "$?" > "$LEA_VM_DIR/vm$i/exec.rc" ) &
        pids+=($!)
    done
    wait "${pids[@]}"
    for i in $(seq 0 $((n - 1))); do
        printf "== vm%s (%s, rc=%s)\n" "$i" "$(lea_ip "$i")" "$(cat "$LEA_VM_DIR/vm$i/exec.rc" 2>/dev/null)"
        cat "$LEA_VM_DIR/vm$i/exec.log"
    done
}

# ---- the rig state ----------------------------------------------------------
# WHY THIS EXISTS (three mismeasurements in a single day): `nvidia-smi -pm`
# was Disabled, and that alone moved the native reference from 132 to 209 ms
# -- 58 %. The wrong number almost went into the documentation as a target
# figure. An environment setting that shifts a reference by 58 % must not
# depend on whether somebody happened to set it.
#
# The line therefore belongs in EVERY measurement CSV, and a comparison that
# mixes lines from different states has to FAIL LOUDLY rather than quietly
# average across them. That is what `bench.sh summary` enforces.
#
# Format (stable, sorted, no spaces inside values):
#   RIG driver=610.43.03 persistence=1 persistenced=1 governor=performance
#       ch=v53.0 pcie=gen1x16 gpu=NVIDIA_GeForce_RTX_2070
_lea_rig_norm() { tr -d '[:space:]' <<<"${1:-}" | tr ' ' '_'; }
lea_rig_state() {
    local driver pm persistence persistenced governor ch gpu pcie
    driver=$(nvidia-smi --query-gpu=driver_version --format=csv,noheader 2>/dev/null | head -1)
    driver=$(_lea_rig_norm "${driver:-unknown}")
    pm=$(nvidia-smi --query-gpu=persistence_mode --format=csv,noheader 2>/dev/null | head -1)
    case "$(_lea_rig_norm "$pm")" in Enabled) persistence=1 ;; Disabled) persistence=0 ;; *) persistence='?' ;; esac
    pgrep -x nvidia-persiste >/dev/null 2>&1 && persistenced=1 || persistenced=0
    governor=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo none)
    ch=$(tr -d '[:space:]' < "$LEA_ROOT/CH_VERSION" 2>/dev/null || echo unknown)
    gpu=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1 | tr ' ' '_')
    # PCIe link, as genNxM. It belongs in the line because everything that
    # crosses the bus depends on it -- and managed memory crosses it on EVERY
    # access (the pages stay in system RAM, LOCATION=PCI). The link drops to
    # gen1 when idle and rises under load, so a number read at the wrong
    # moment is a number from a different machine.
    pcie=$(nvidia-smi --query-gpu=pcie.link.gen.current,pcie.link.width.current \
           --format=csv,noheader,nounits 2>/dev/null | head -1 | tr -d ' ')
    pcie="gen${pcie%,*}x${pcie#*,}"
    printf 'RIG driver=%s persistence=%s persistenced=%s governor=%s ch=%s pcie=%s gpu=%s\n' \
        "$driver" "$persistence" "$persistenced" "$governor" "$ch" "${pcie:-unknown}" "${gpu:-unknown}"
}

# lea_rig_check -- is the rig ready to measure? Prints the RIG line, then
# only things that DISTORT A COMPARISON, not matters of taste. Returns 1
# when it is not.
lea_rig_check() {
    local line driver persistence persistenced want vend rc=0
    line=$(lea_rig_state); echo "$line"
    driver=$(sed -n 's/.*driver=\([^ ]*\).*/\1/p' <<<"$line")
    persistence=$(sed -n 's/.*persistence=\([^ ]*\).*/\1/p' <<<"$line")
    persistenced=$(sed -n 's/.*persistenced=\([^ ]*\).*/\1/p' <<<"$line")
    want=$(lea_want_driver)
    if [[ -n $want && $driver != "$want" ]]; then
        echo "RIG-ERROR: driver $driver, expected $want (DRIVER_VERSION)" >&2
        rc=1
    fi
    # The vendored headers must be the same version too, or every struct
    # offset in this repository is a guess.
    vend=$(git -C "$LEA_ROOT/vendor/open-gpu-kernel-modules" describe --tags --exact-match 2>/dev/null || echo none)
    if [[ ! -d $LEA_ROOT/vendor/open-gpu-kernel-modules ]]; then
        # NOT A CHECKOUT. In a store or package install there is no vendor/
        # and there cannot be: it is 170 MB of NVIDIA source that only the
        # BUILD needs. What that check is really asking -- "were these
        # binaries built against the headers of the driver now running" -- is
        # asked directly there instead, by comparing the package MANIFEST's
        # driver against the node's (bench.sh slurm's preflight). Said out
        # loud rather than skipped quietly.
        echo "RIG-NOTE: no vendor/ here (a package install); the driver match is checked against the MANIFEST instead." >&2
    elif [[ $vend != "$want" ]]; then
        echo "RIG-ERROR: vendor/open-gpu-kernel-modules is $vend, expected $want (scripts/build.sh vendor)" >&2
        rc=1
    fi
    if [[ $persistence != 1 ]]; then
        if [[ ${LEA_RIG_UNMANAGED:-0} -eq 1 ]]; then
            # This host's GPU settings are not the caller's to change -- a
            # batch node. The state stays in the RIG line beside every
            # number, and the aggregator refuses to compare across it
            # (bench.sh slurm --collect); what it must not do is silently
            # pass for a state that costs 58 %.
            echo "RIG-WARNING: persistence mode is off and this run cannot change it (LEA_RIG_UNMANAGED)." >&2
            echo "             Absolute numbers are up to 58 % worse and comparable ONLY to other" >&2
            echo "             runs whose rig line also says persistence=0." >&2
        else
            echo "RIG-ERROR: persistence mode is off. That shifts the native reference by 58 %." >&2
            echo "           sudo nvidia-smi -pm 1   (permanently: systemctl enable --now nvidia-persistenced)" >&2
            rc=1
        fi
    fi
    # persistenced is a WARNING, not an error: -pm 1 alone already carries
    # most of it; the daemon was worth another 42 ms on top (132 vs 174).
    if [[ $persistenced != 1 ]]; then
        echo "RIG-WARNING: nvidia-persistenced is not running -- absolute values will be higher." >&2
    fi
    return "$rc"
}

# ---- after a crash: clean ---------------------------------------------------
# Every function here cleans up after itself; nothing cleans up after a
# script that DIED. What stays behind then is a backend without a VM, a
# socket without a listener, and a pidfile whose process is long gone --
# and the last one is the worst of the three, because every other script
# reads a pidfile as "this is running".
#
# WARNING: `pkill -x`, never `pkill -f`. A `-f` pattern also matches this
# script's own command line and any editor that happens to have the source
# open (three incidents, last one an exit 144 in the middle of a
# measurement). And `comm` is truncated to 15 characters, so a binary has
# to be killed under BOTH spellings.
lea_kill_backends() {
    pkill -x vhost-user-nvr   >/dev/null 2>&1 || true
    pkill -x vhost-user-nvrm  >/dev/null 2>&1 || true
    pkill -x vhost-user-inpu  >/dev/null 2>&1 || true
    pkill -x vhost-user-input >/dev/null 2>&1 || true
    return 0
}

# lea_rig_clean [--dry-run] [--force]
#   --dry-run   list what would go, change nothing
#   --force     also tear RUNNING instances down
# WARNING: a running rig is not garbage. Without --force this refuses as
# soon as an instance's cloud-hypervisor is alive -- otherwise a cleanup in
# the wrong terminal pulls four VMs out from under a measurement that has
# been running for an hour. --dry-run makes the SAME decision as a real
# run, so that a preview is a preview and not a different program.
lea_rig_clean() {
    local dry=0 force=0 cleaned=0 banner=0
    while [[ $# -gt 0 ]]; do
        case $1 in
            --dry-run) dry=1; shift ;;
            --force)   force=1; shift ;;
            *) die "lea_rig_clean: unknown option $1" ;;
        esac
    done
    _banner() {
        [[ $banner -eq 1 ]] && return 0
        banner=1
        [[ $dry -eq 1 ]] && lea_head "dry run -- would do the following" || lea_head "cleaning"
    }
    _did()  { _banner; cleaned=$((cleaned + 1)); echo "  $*"; }
    _kept() { _banner; echo "  $*"; }

    # 0. running instances -- asked of the pidfiles, not over SSH, because a
    #    cleanup must also work when the guests no longer answer.
    local -a running=()
    local n
    for n in $(lea_inst_list); do lea_vm_running "$n" && running+=("$n"); done
    if [[ ${#running[@]} -gt 0 && $force -eq 0 ]]; then
        error "instance(s) ${running[*]} are up -- refusing to clean.
       A running rig is not garbage. Stop it yourself (showcase.sh down),
       or repeat with --force if you really want this to tear it down."
        return 3
    fi

    # 1. VMs, gracefully: a hard kill leaves EMPTY SSH host keys behind and
    #    sshd then refuses to start on the next boot.
    for n in "${running[@]}"; do
        _did "stop instance $n (VM first, then its backends)"
        [[ $dry -eq 0 ]] && lea_rig_down "$n" --force >/dev/null 2>&1
    done
    # A cloud-hypervisor without a pidfile of ours cannot be stopped
    # gracefully from here, and this does not signal it -- that is exactly
    # what produces the empty host keys. Report it and let a human decide.
    local pid
    for pid in $(pgrep -x cloud-hyperviso 2>/dev/null); do   # comm is 15 chars
        grep -qs "^$pid\$" "$LEA_VM_DIR"/*/ch.pid 2>/dev/null && continue
        warn "cloud-hypervisor pid $pid is running with no pidfile of ours.
       Not touched: killing a VM by signal leaves empty SSH host keys
       behind. Stop it by hand once you know which VM it is."
    done

    # 2. backends -- by pidfile where there is one, by name for the strays.
    local backends
    backends=$(
        { pgrep -x vhost-user-nvrm; pgrep -x vhost-user-nvr;
          pgrep -x vhost-user-inpu; pgrep -x vhost-user-input; } 2>/dev/null | sort -un
    )
    local cmd
    for pid in $backends; do
        # WARNING: `2>/dev/null` on the tr does NOT silence this -- the
        # shell reports a failed REDIRECTION itself, before tr ever runs,
        # and a process that exited between `pgrep` above and this line is
        # the normal case rather than an exception. Test for readability.
        cmd=""
        [[ -r /proc/$pid/cmdline ]] && cmd=$(tr '\0' ' ' < "/proc/$pid/cmdline" | sed 's/ *$//')
        _did "kill backend pid $pid (${cmd:-already gone})"
    done
    if [[ -n $backends && $dry -eq 0 ]]; then
        lea_kill_backends
        sleep 0.3
    fi

    # 3. sockets and fifos nobody holds. A socket someone listens on only
    #    stays if that someone SURVIVES this run -- compared against the
    #    plan (the backends above), not against the live system, so that
    #    --dry-run and the real run agree.
    _sock_holders() {
        local abs; abs=$(readlink -f "$1")
        # ss -lxpH prints  users:(("vhost-user-nvrm",pid=3935035,fd=10))
        ss -lxpH 2>/dev/null | awk -v p="$abs" '$5 == p { print }' | grep -o 'pid=[0-9]*' | cut -d= -f2
    }
    # The holder check for a fifo CANNOT be ss: a fifo is not a socket. Walk
    # /proc/*/fd instead, which is what actually holds it.
    _fifo_holders() {
        local abs p; abs=$(readlink -f "$1")
        for p in /proc/[0-9]*; do
            p=${p#/proc/}
            readlink -f "/proc/$p/fd/"* 2>/dev/null | grep -qxF "$abs" && echo "$p"
        done
    }
    # The vsock sockets are slot-keyed and live directly under LEA_VM_DIR
    # (short path -- AF_UNIX sun_path is 108 bytes), so they need their own
    # glob beside the per-instance ones rather than being caught by them.
    local f h survivor
    for f in "$LEA_VM_DIR"/*/*.sock "$LEA_VM_DIR"/*/*.fifo "$LEA_VM_DIR"/vsock*.sock; do
        [[ -S $f || -p $f ]] || continue
        survivor=""
        if [[ -S $f ]]; then
            for h in $(_sock_holders "$f"); do grep -qx "$h" <<<"$backends" || survivor="$h"; done
        else
            for h in $(_fifo_holders "$f"); do grep -qx "$h" <<<"$backends" || survivor="$h"; done
        fi
        [[ -n $survivor ]] && { _kept "keeping $f -- pid $survivor holds it and is not ours"; continue; }
        _did "remove ${f##*.} $f"
        [[ $dry -eq 0 ]] && rm -f "$f"
    done

    # 4. pidfiles with dead content -- worse than none, because every
    #    script reads them as "running".
    local p alive
    for p in "$LEA_VM_DIR"/*/*.pid "$LEA_VM_DIR"/*.pid; do
        [[ -f $p ]] || continue
        alive=0
        while read -r pid; do
            [[ $pid =~ ^[0-9]+$ ]] || continue
            kill -0 "$pid" 2>/dev/null && alive=1
        done < "$p"
        [[ $alive -eq 1 ]] && { _kept "keeping $p -- its process is alive"; continue; }
        _did "remove stale pidfile $p (pid $(tr '\n' ' ' < "$p" | sed 's/ *$//'))"
        [[ $dry -eq 0 ]] && rm -f "$p"
    done

    if [[ $cleaned -eq 0 ]]; then
        echo "nothing to clean"
    else
        [[ $dry -eq 1 ]] && echo "$cleaned item(s) would be cleaned" || echo "$cleaned item(s) cleaned"
    fi
    return 0
}
