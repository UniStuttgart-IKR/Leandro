# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# Shared helpers for the host-side scripts. Not a script -- source it:
#   source "$LEA_ROOT/scripts/lib/common.sh"
#
# What every entry point needs and none should own a copy of: output, the
# SSH option set, waiting for a guest or a socket, the exit-command stack,
# the "is it built" check, and the gate output contract. The rig itself
# (network, backends, VMs, fleets) is scripts/lib/rig.sh; what goes INTO a
# guest is scripts/lib/provision.sh. Both source this file.
#
# ONE library layer, not a tree: rig.sh and provision.sh depend on this
# file and on config.sh, never on each other in a cycle.

# Sourced twice in one shell is normal (an entry point sources rig.sh, which
# sources this, and the entry point sources this too). Redefining the
# functions would be harmless; re-running the array declarations below
# would NOT -- it would empty the exit-command stack under a live trap.
[[ -n ${_LEA_COMMON_LOADED:-} ]] && return 0
_LEA_COMMON_LOADED=1

# The configuration is a separate file because it is data, not code. Source
# it here when nobody has, so one `source .../lib/common.sh` suffices.
# LEA_GATEWAY is the probe because config.sh DERIVES it -- an environment
# that merely overrides one LEA_* value does not set it.
if [[ -z ${LEA_GATEWAY:-} ]]; then
    # shellcheck source=scripts/lib/config.sh
    source "$(dirname "${BASH_SOURCE[0]}")/config.sh"
fi

# lea_cd_root -- change to the repository root, or die trying. Every path in
# config.sh is absolute, so this is for the few things that are relative by
# nature: cargo, make -C probe, git ls-files.
lea_cd_root() { cd "$LEA_ROOT" || die "cannot cd to $LEA_ROOT"; }

# ---- output ---------------------------------------------------------------
# Plain and greppable. Errors go to stderr so a caller can separate them
# from a command's real output.
#
# COLOUR. One palette for every script in this tree, decided once per
# process, and OFF unless the output is going to a terminal: every gate,
# bench and provisioning step here is also read out of a log file, and an
# escape sequence in a log is noise that every later grep has to know about.
# stdout and stderr are decided separately -- a script whose stdout is
# redirected into a log still writes its errors to a terminal, and painting
# those would be the one case where colour helps most.
#
# `NO_COLOR` (the convention, no value needed) turns it off; `LEA_COLOR=1`
# forces it on for a pipe that ends in a pager, `LEA_COLOR=0` off.
#
# What is painted is deliberately little: a verdict (PASS/FAIL), a warning,
# an error, and a section heading. Colouring every line would leave nothing
# standing out, which is the state this started from.
if [[ -n ${LEA_COLOR:-} ]]; then
    _LEA_COLOUR=$LEA_COLOR; _LEA_COLOUR_ERR=$LEA_COLOR
elif [[ -n ${NO_COLOR:-} ]]; then
    _LEA_COLOUR=0; _LEA_COLOUR_ERR=0
else
    [[ -t 1 ]] && _LEA_COLOUR=1 || _LEA_COLOUR=0
    [[ -t 2 ]] && _LEA_COLOUR_ERR=1 || _LEA_COLOUR_ERR=0
fi
if [[ $_LEA_COLOUR -eq 1 || $_LEA_COLOUR_ERR -eq 1 ]]; then
    LEA_B=$'\e[1m'; LEA_DIM=$'\e[2m'; LEA_GRN=$'\e[32m'; LEA_RED=$'\e[31m'
    LEA_YEL=$'\e[33m'; LEA_CYA=$'\e[36m'; LEA_R=$'\e[0m'
else
    LEA_B=""; LEA_DIM=""; LEA_GRN=""; LEA_RED=""; LEA_YEL=""; LEA_CYA=""; LEA_R=""
fi
# The prefix carries the colour, never the message: a reader greps for
# "WARNING:" and a terminal shows it in yellow, and both get what they came
# for.
_lea_say_err() {   # _lea_say_err COLOUR PREFIX MESSAGE...
    local c=$1 p=$2; shift 2
    if [[ $_LEA_COLOUR_ERR -eq 1 ]]; then echo "${c}${p}${LEA_R} $*" >&2
    else echo "${p} $*" >&2; fi
}
info()  { echo "$*"; }
warn()  { _lea_say_err "$LEA_YEL" "WARNING:" "$@"; }
error() { _lea_say_err "$LEA_RED" "ERROR:" "$@"; }
die()   { error "$*"; exit 1; }

# lea_head TEXT... -- a section heading in the human transcript. The `== x ==`
# shape predates the colour and stays, so a log reads the same as a terminal.
lea_head() {
    if [[ $_LEA_COLOUR -eq 1 ]]; then echo "${LEA_B}== $* ==${LEA_R}"
    else echo "== $* =="; fi
}

# lea_usage_from_header -- print the calling script's header comment as help.
#
# The SPDX block is metadata for a tool, not help for a reader, so it is
# skipped BY NAME rather than by counting lines. `NR>2` was right while the
# block was one line; it grew to three during the licence pass, and every
# --help in this repository then opened with two SPDX-FileCopyrightText
# lines. A count has to be revisited whenever a copyright holder is added
# or removed, which is exactly when nobody is looking at the help text.
lea_usage_from_header() {
    awk 'NR>1 { if ($0 !~ /^#/) exit; sub(/^# ?/, ""); if ($0 ~ /^SPDX-/) next; print }' "$0"
}

# ---- SSH ------------------------------------------------------------------
# ONE definition of the option set, used by every consumer (ssh, scp, the
# fleet, the provisioning path).
#
# StrictHostKeyChecking=no + UserKnownHostsFile=/dev/null are deliberate and
# they are the ROOT fix, not a workaround: these guests are short-lived, they
# recycle a fixed set of RFC1918 addresses, and `--fresh` regenerates their
# host keys by design. Verifying them would mean a known_hosts entry that is
# stale more often than not. Sending the entries to /dev/null means nothing
# is ever written to the user's ~/.ssh/known_hosts -- so there is nothing to
# clean up afterwards, and no clear-known-hosts crutch is needed.
#
# WARNING: this trades away MITM protection. That is acceptable HERE because
# the path never leaves the host bridge. Do not copy these options to
# anything that crosses a real network.
# ---- the two transports ----------------------------------------------------
# THE ADDRESS IS THE INSTANCE HANDLE, on both transports, and that is what
# keeps this to four functions instead of 183 edits.
#
# `lea_ip` is bijective -- IP = LEA_NET_PREFIX.(LEA_IP_FIRST + index) -- so an
# address recovers the index it was made from, and the index names the
# instance's host-side endpoints by formula (lea_vsock_sock in config.sh).
# On the vsock transport the guest has NO address at all; the one its callers
# pass is therefore a pure label, and resolving it here is what lets every
# call site go on passing the same thing it always passed.
#
# WHAT IT COSTS: nothing worth choosing between. The one bulk transfer over
# this path is the NVIDIA userspace payload, which lea_guest_tar streams
# through ssh. Measured 2026-08-19, the same 494 MiB staged into the same
# NixOS guest image on the same host, the two transports interleaved so drift
# cannot favour one:
#
#     vsock  1.75 s (282 MiB/s)   1.58 s (313 MiB/s)
#     ip     1.68 s (294 MiB/s)   1.56 s (317 MiB/s)
#
# -- a 1-4 % spread, inside the run-to-run noise of either. The compute
# measurement is likewise untouched: `bench.sh fleet` over four NixOS guests
# at stages 1/2/4 gave 32.41/26.22/47.24 ms/it over vsock against
# 33.59/26.99/49.00 over ip the same afternoon, with the identical single acc
# value. Choose the transport for what it removes -- the bridge, the taps,
# the NAT rule and every sudo -- not for what it costs.
#
# The transport is decided by ONE fact: whether that index's hybrid vsock
# socket exists. It exists exactly while a VM started with --vsock is running,
# because cloud-hypervisor creates it and lea_rig_down removes it -- so there
# is no second record of the transport to fall out of step with the first.
# (vm/<name>/transport is the instance's own memory of what it was BROUGHT UP
# as; this is what it IS right now, and only one of the two can be checked
# without knowing the instance's name.)

# _lea_idx_of_ip <ip> -- the instance index that address belongs to.
_lea_idx_of_ip() {
    local last=${1##*.} idx
    [[ $1 == "$LEA_NET_PREFIX".* ]] || return 1
    [[ $last =~ ^[0-9]+$ ]] || return 1
    idx=$((last - LEA_IP_FIRST))
    [[ $idx -ge 0 && $idx -lt $LEA_MAX_VMS ]] || return 1
    echo "$idx"
}

# _lea_vsock_of_ip <ip> -- that instance's live vsock socket, or nothing.
_lea_vsock_of_ip() {
    local idx sock
    idx=$(_lea_idx_of_ip "$1") || return 1
    sock=$(lea_vsock_sock "$idx")
    [[ -S $sock ]] || return 1
    echo "$sock"
}

# lea_ssh_opts [ip] -- THE option set, and with an address the transport too.
#
# Given an address whose instance is on vsock, this appends the ProxyCommand
# that carries ssh over it. ssh does not resolve a hostname when a
# ProxyCommand is set, so the label never reaches a resolver.
lea_ssh_opts() {
    printf '%s\n' \
        -i "$LEA_SSH_KEY" \
        -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null \
        -o ConnectTimeout=5 \
        -o LogLevel=ERROR
    local _lea_sock
    if [[ -n ${1:-} ]] && _lea_sock=$(_lea_vsock_of_ip "$1"); then
        # Single-quoted inside the value: ssh hands the whole string to
        # /bin/sh, and LEA_VM_DIR on a cluster is somebody else's path.
        # Port 22 is where systemd's ssh generator puts sshd's AF_VSOCK
        # socket (see DEVELOPMENT.md section 7).
        printf '%s\n' -o "ProxyCommand='$LEA_VSOCK_CONNECT' '$_lea_sock' 22"
    fi
}

# lea_ssh <ip> [command...] -- run a command in a guest (or open a shell).
lea_ssh() {
    local ip=$1; shift
    local -a opts; mapfile -t opts < <(lea_ssh_opts "$ip")
    ssh "${opts[@]}" "$LEA_GUEST_USER@$ip" "$@"
}

# lea_scp <source...> <target> -- same options, for file transfer. Guest
# paths are written as <ip>:<path> and rewritten to user@<ip>:<path> here.
lea_scp() {
    # `_lea_args`, not `args`: shellcheck follows `source` into this file and
    # carries a local array name into every script that has a scalar of the
    # same name. Same reason as `_lea_absent` in lea_require_tools.
    local -a _lea_args=()
    local a _lea_ip=""
    for a in "$@"; do
        if [[ $a == *:* && $a != /* && $a != .* ]]; then
            # The guest end of the copy, in <ip>:<path> form -- and the only
            # place the transport can be read off a scp argument list.
            [[ -n $_lea_ip ]] || _lea_ip=${a%%:*}
            _lea_args+=("$LEA_GUEST_USER@$a")
        else
            _lea_args+=("$a")
        fi
    done
    local -a opts; mapfile -t opts < <(lea_ssh_opts "$_lea_ip")
    scp "${opts[@]}" "${_lea_args[@]}"
}

# lea_guest IP CMD... -- lea_ssh with BatchMode and the guest's CRs stripped.
#
# The CR strip is why this exists beside lea_ssh: a value read out of
# /sys through ssh carries a trailing \r, and `[[ $VD == 1 ]]` is then false
# on a guest that answered 1. Every caller used to append `| tr -d '\r'` and
# the ones that forgot failed in a way that reads like a broken module.
lea_guest() {
    local ip=$1 rc; shift
    local -a opts
    mapfile -t opts < <(lea_ssh_opts "$ip")
    ssh "${opts[@]}" -o BatchMode=yes "$LEA_GUEST_USER@$ip" "$@" | tr -d '\r'
    # PIPESTATUS, not $?: $? would be tr's, which is 0 whatever ssh did.
    rc=${PIPESTATUS[0]}
    return "$rc"
}

# lea_wait_ssh <ip> [tries] [pidfile] -- wait until the guest answers.
# With a pidfile, gives up early if the VM process has died: waiting the
# full timeout on a VM that is already gone hides the real error.
#
# Transport-agnostic by construction: every attempt goes through lea_ssh,
# which resolves the transport afresh each time. That matters on vsock, where
# the socket does not exist until cloud-hypervisor has created it -- the first
# few attempts of a cold start find no socket and fall through to a failed
# connection, which is the same "not up yet" this loop already handles.
lea_wait_ssh() {
    local ip=$1 tries=${2:-60} pidf=${3:-}
    local _
    for _ in $(seq "$tries"); do
        if lea_ssh "$ip" true 2>/dev/null; then
            return 0
        fi
        if [[ -n $pidf && -f $pidf ]] && ! kill -0 "$(cat "$pidf")" 2>/dev/null; then
            return 2      # VM process gone -- do not keep waiting
        fi
        sleep 2
    done
    return 1
}

# lea_wait_socket PATH [TRIES=50] [SLEEP=0.2] -- wait for a unix socket.
lea_wait_socket() {
    local path=$1 tries=${2:-50} nap=${3:-0.2}
    local _
    for _ in $(seq "$tries"); do
        [[ -S $path ]] && return 0
        sleep "$nap"
    done
    return 1
}

# ---- NVIDIA userspace on the host -----------------------------------------
# WHERE the driver's userspace half lives is NOT a constant. It was hard-coded
# to /usr/lib, which is true on Arch and Fedora, false on Debian
# (x86_64-linux-gnu) and false on NixOS, where it sits under
# /run/opengl-driver/lib and the store. A hard-coded path there reports
# "libcuda missing" on a machine whose driver is installed perfectly.
#
# Asked in this order: an explicit override, then the dynamic linker (which
# is right by construction wherever ldconfig is maintained), then the known
# distribution locations.
#
# lea_nvidia_libdir <version> -> directory containing libcuda.so.<version>
lea_nvidia_libdir() {
    local want=$1 d cand

    # 1. Explicit override -- the escape hatch for anything unusual.
    if [[ -n ${LEA_NVIDIA_LIB_DIR:-} ]]; then
        [[ -f $LEA_NVIDIA_LIB_DIR/libcuda.so.$want ]] && { echo "$LEA_NVIDIA_LIB_DIR"; return 0; }
        return 1
    fi

    # 2. Ask the dynamic linker, then resolve the symlink chain: libcuda.so.1
    #    points at the versioned file, and its directory is what we want.
    if command -v ldconfig >/dev/null 2>&1; then
        cand=$(ldconfig -p 2>/dev/null | awk '/libcuda\.so\.1 /{print $NF; exit}')
        if [[ -n $cand && -e $cand ]]; then
            d=$(dirname "$(realpath "$cand" 2>/dev/null || echo "$cand")")
            [[ -f $d/libcuda.so.$want ]] && { echo "$d"; return 0; }
        fi
    fi

    # 3. Known locations. /run/opengl-driver/lib is the NixOS one and the
    #    reason this list exists at all.
    for d in /usr/lib /usr/lib64 /usr/lib/x86_64-linux-gnu \
             /run/opengl-driver/lib /usr/lib/nvidia /opt/nvidia/lib64; do
        [[ -f $d/libcuda.so.$want ]] && { echo "$d"; return 0; }
    done
    return 1
}

# lea_nvidia_bin <name> -> full path of an NVIDIA tool (nvidia-smi)
lea_nvidia_bin() {
    local n=$1 p d
    p=$(command -v "$n" 2>/dev/null) && { echo "$p"; return 0; }
    for d in /usr/bin /run/current-system/sw/bin /usr/local/bin; do
        [[ -x $d/$n ]] && { echo "$d/$n"; return 0; }
    done
    return 1
}

# lea_driver_version -- the version of the running host driver, or nothing.
lea_driver_version() {
    grep -oE '[0-9]+\.[0-9]+\.[0-9]+' /proc/driver/nvidia/version 2>/dev/null | head -1
}

# lea_want_driver -- the version this tree targets (DRIVER_VERSION).
lea_want_driver() { tr -d '[:space:]' < "$LEA_ROOT/DRIVER_VERSION"; }

# ---- local.env -------------------------------------------------------------
# lea_local_env_set VAR VALUE [FILE] -- make VAR this checkout's default by
# writing `: "${VAR:=VALUE}"` into local.env, which config.sh sources before
# its own defaults.
#
# REPLACES an existing entry for VAR rather than appending a second one, and
# that is not tidiness: the file is written in the `: "${VAR:=...}"` idiom,
# so the FIRST assignment wins and a second line for the same variable is
# dead text that CONTRADICTS the one in force. A commented-out entry counts
# as an existing one -- local.env.example ships LEA_BASE_IMAGE commented out,
# and a copy of it must end up with the value in place of the placeholder,
# not below it.
#
# Refuses when the file cannot be written: in a Nix store install LEA_ROOT is
# read-only by construction, and there the answer is the environment (the
# wrapper in nix/packages/leandro-scripts.nix sets the LEA_* variables), not
# a file.
lea_local_env_set() {
    local var=$1 value=$2 file=${3:-$LEA_ROOT/local.env} dir tmp
    [[ -n $var && -n $value ]] || { error "lea_local_env_set: need VAR and VALUE"; return 2; }
    dir=$(dirname "$file")
    if [[ -e $file ]]; then
        [[ -w $file ]] || { error "$file is not writable -- LEA_ROOT is read-only here (a store install?).
Set $var in the environment instead: export $var=$value"; return 1; }
    else
        [[ -w $dir ]] || { error "$dir is not writable -- LEA_ROOT is read-only here (a store install?).
Set $var in the environment instead: export $var=$value"; return 1; }
    fi
    tmp=$(mktemp "$file.XXXXXX") || return 1
    if [[ -e $file ]]; then
        # Bracket expressions rather than backslash escapes: `\{` in a
        # dynamic ERE is undefined by POSIX and differs between awks.
        awk -v pat="^[[:space:]]*#?[[:space:]]*:[[:space:]]*\"[$][{]${var}[:]=" \
            -v line=": \"\${$var:=$value}\"" '
            $0 ~ pat { if (!done) { print line; done = 1 } ; next }
            { print }
            END { if (!done) print line }
        ' "$file" > "$tmp" || { rm -f "$tmp"; return 1; }
    else
        # The shape of local.env.example, without inventing the answers that
        # are not ours to give.
        {
            echo "# SPDX-License-Identifier: MIT"
            echo "# This checkout's answers to \"where do the artefacts go\", read by"
            echo "# scripts/lib/config.sh before its defaults. local.env.example has the"
            echo "# full list and the reasoning."
            echo ": \"\${$var:=$value}\""
        } > "$tmp" || { rm -f "$tmp"; return 1; }
    fi
    # Keep the mode of the file that was there (mktemp makes 0600).
    [[ -e $file ]] && chmod --reference="$file" "$tmp" 2>/dev/null
    mv -f "$tmp" "$file" || { rm -f "$tmp"; return 1; }
    return 0
}

# ---- processes and pidfiles -----------------------------------------------
# lea_mac <ip> -- locally administered MAC, last byte = last IP octet. Keeps
# a guest's MAC stable across restarts without a table to maintain.
lea_mac() { printf '02:00:c0:a8:64:%02x' "${1##*.}"; }

# lea_running <pidfile> -- true if that pidfile names a live process.
lea_running() {
    [[ -f $1 ]] && kill -0 "$(cat "$1" 2>/dev/null)" 2>/dev/null
}

# lea_hold_pidfile <pidfile> -- claim it for THIS shell and drop it on exit.
#
# WARNING: this exists so that nothing has to wait on a COMMAND LINE. The
# tempting form
#
#     until ! pgrep -f "bash ./scripts/bench.sh"; do sleep 15; done
#
# never terminates, and the reason is not a sloppy pattern: the pattern
# stands verbatim in the waiting shell's own command line, and `pgrep`
# excludes only ITSELF, never its parent. The loop matches the waiter
# forever while the script it waits for has long finished. Tightening the
# regex hides that; it does not fix it. The rule is therefore NOT "write a
# better pattern" but "do not match on command lines at all" -- neither for
# waiting nor for killing (rig.sh kills by pidfile, and `pkill -x` only in
# `clean`, same family of bug).
#
# The waiting side is then:
#
#     until ! lea_running vm/bench.pid; do sleep 15; done
#
# `set -u` safe, and it appends to an existing EXIT trap rather than
# replacing one.
lea_hold_pidfile() {
    local f=$1
    # A pidfile that is ALREADY live belongs to another run. Taking it over
    # silently is the worst outcome available: the older run's EXIT trap
    # would then delete the file while this one still runs, and every
    # waiter reads that as "finished". Say so; the run continues, because
    # two deliberate parallel runs are legitimate and refusing would be a
    # second convention.
    if [[ -f $f ]] && kill -0 "$(cat "$f" 2>/dev/null)" 2>/dev/null; then
        warn "$f is held by PID $(cat "$f") -- a second run is starting."
        warn "  A waiter on this file sees ONE run, not two."
    fi
    mkdir -p "$(dirname "$f")"
    echo $$ > "$f"
    lea_on_exit "_lea_release_pidfile $(printf '%q' "$f")"
}

# _lea_release_pidfile <pidfile> -- drop it, but only while it still names US.
#
# Without that test, a run whose pidfile was taken over (the warning above)
# would delete the successor's file on its way out -- the same lie, one step
# later. Always returns 0: it runs from an EXIT trap, where a non-zero status
# would be the last thing `set -e` sees.
_lea_release_pidfile() {
    [[ $(cat "$1" 2>/dev/null) == "$$" ]] && rm -f "$1"
    return 0
}

# ---- exit handling --------------------------------------------------------
# ONE EXIT trap for the whole shell, with a stack of commands behind it.
#
# The obvious alternative -- read the old trap back with `trap -p`, strip the
# wrapper with sed and paste it into a new one -- is what lea_hold_pidfile did
# until 2026-08-18, and it is subtly broken: `trap -p` prints its command
# SHELL-QUOTED, so an embedded quote comes back as '\'' and pasting that into
# a fresh `trap "..."` stores the escape sequence rather than the quote. The
# pidfile handler contained exactly such a quote, so a second composition
# would have run `cat \vm/x.pid\`. Nothing composed twice, so nothing broke;
# the gate safety net below would have been the second one.
#
# So the previous trap is read back ONCE, un-escaped by letting the shell
# parse it (eval of the quoted form, which is what that form is for), and
# pushed onto the stack like everything else.
declare -a _LEA_EXIT_CMDS=()
# The exit status the shell will finally leave with. A handler may RAISE it
# (the gate safety net does); nothing may lower it silently.
_LEA_EXIT_RC=0

# _lea_exit_run <pending-status> -- run the stack in LIFO order, then leave.
_lea_exit_run() {
    local rc=$1 i
    _LEA_EXIT_RC=$rc
    for ((i = ${#_LEA_EXIT_CMDS[@]} - 1; i >= 0; i--)); do
        # `|| true`: a handler that returns non-zero must not decide the
        # script's exit status, and under `set -e` must not stop the ones
        # after it -- teardown is exactly where half-done is worst.
        eval "${_LEA_EXIT_CMDS[i]}" || true
    done
    [[ $_LEA_EXIT_RC == "$rc" ]] || exit "$_LEA_EXIT_RC"
    return 0
}

# lea_on_exit "<command string>" -- run it when this shell exits, LIFO.
lea_on_exit() {
    local q prev
    if [[ -z ${_LEA_EXIT_TRAP_SET:-} ]]; then
        _LEA_EXIT_TRAP_SET=1
        q=$(trap -p EXIT)
        if [[ -n $q ]]; then
            q=${q#trap -- }
            q=${q% EXIT}
            eval "prev=$q"
            # First on the stack, so it runs LAST: a `trap cleanup EXIT`
            # written before this call is the caller's own teardown, and
            # tearing the rig down before the handlers that report on it
            # would print the verdict about a rig that is already gone.
            [[ -n ${prev:-} ]] && _LEA_EXIT_CMDS+=("$prev")
        fi
        trap '_lea_exit_run $?' EXIT
    fi
    _LEA_EXIT_CMDS+=("$1")
}

# ---- preconditions --------------------------------------------------------
# lea_require_tools TOOL... -- die naming EVERY missing one, not just the first.
lea_require_tools() {
    local t
    # `_lea_absent`, not `missing`: shellcheck follows `source` into this file
    # and carries a local array name into every script that has a scalar of
    # the same name, reporting SC2178 in files that are perfectly correct.
    local -a _lea_absent=()
    for t in "$@"; do
        command -v "$t" >/dev/null 2>&1 || _lea_absent+=("$t")
    done
    [[ ${#_lea_absent[@]} -eq 0 ]] && return 0
    # All of them at once on purpose: reporting one per run turns installing
    # four packages into four failed runs.
    die "missing tool(s): ${_lea_absent[*]} -- install them (Arch: pacman -S ${_lea_absent[*]}, Debian/Ubuntu: apt-get install ${_lea_absent[*]})"
}

# The reason the last lea_require_built call failed, as one line a gate can
# hand straight to lea_gate_skip. Empty after a successful call.
LEA_BUILD_PROBLEM=""

# _lea_newer_source REFERENCE -- the first workspace source newer than it.
#
# `-quit` after the first hit: the answer is the same whether one file or
# forty are newer, and this runs before every gate. vendor/ is deliberately
# out of the walk -- `build.sh vendor` rewrites those headers whether or not
# their content changed, and a re-fetch would then look like an edit.
_lea_newer_source() {
    (
        cd "$LEA_ROOT" 2>/dev/null || return 0
        find crates Cargo.toml Cargo.lock -type f \
            \( -name '*.rs' -o -name '*.h' -o -name '*.toml' -o -name '*.lock' \) \
            -newer "$1" -print -quit 2>/dev/null
    )
}

# lea_require_built PATH... -- every artifact present, and none of them older
# than the source it was built from. Returns 1 and sets LEA_BUILD_PROBLEM.
#
# Deliberately does NOT die: a gate turns this into `skip` (a precondition
# that was not met), which is a different verdict from a failed check.
#
# THE STALENESS HALF exists because taking `cargo build --release` out of the
# gates -- so that a gate measures the rig and not the workspace -- traded one
# failure mode for another. A MISSING binary is loud. A binary from before
# this morning's edit to session.rs is not: the gate runs, every stage passes,
# and it reports green about code nobody is running. That is the one shape in
# which a gate lies rather than fails, so it is worth an mtime comparison.
#
# LEA_ALLOW_STALE=1 downgrades it to a warning and a fact, for the case where
# it is deliberate: measuring a binary built from a tree that has since moved
# on. The escape hatch is the point -- without one, the way past this check is
# `touch`, and a check people learn to defeat is worse than no check at all.
#
# A binary from a Nix store (LEA_BIN_DIR outside LEA_ROOT) has no source
# tree to compare against; only presence is checked then.
lea_require_built() {
    local p abs m oldest="" oldest_path="" newer rc=0
    LEA_BUILD_PROBLEM=""
    for p in "$@"; do
        [[ $p == /* ]] && abs=$p || abs=$LEA_ROOT/$p
        if [[ ! -x $abs ]]; then
            echo "$abs missing -- run: scripts/build.sh cargo" >&2
            rc=1
            continue
        fi
        m=$(stat -c %Y "$abs" 2>/dev/null) || continue
        # The OLDEST of them is the one that decides: a fresh binary beside a
        # stale one still means the run would measure the stale one.
        [[ -z $oldest || $m -lt $oldest ]] && { oldest=$m; oldest_path=$abs; }
    done
    if [[ $rc -ne 0 ]]; then
        LEA_BUILD_PROBLEM="host binaries not built -- run: scripts/build.sh cargo"
        return 1
    fi
    [[ -n $oldest_path ]] || return 0
    if [[ -n $_LEA_GATE_NAME ]]; then
        lea_gate_fact binaries_built \
            "$(date -d "@$oldest" '+%Y-%m-%dT%H:%M:%S' 2>/dev/null || echo "$oldest")"
    fi
    [[ $oldest_path == "$LEA_ROOT"/* ]] || return 0
    newer=$(_lea_newer_source "$oldest_path")
    [[ -z $newer ]] && return 0
    [[ -n $_LEA_GATE_NAME ]] && lea_gate_fact stale_source "$newer"
    if [[ ${LEA_ALLOW_STALE:-0} -eq 1 ]]; then
        warn "$newer is newer than ${oldest_path#"$LEA_ROOT"/} -- measuring a stale binary on purpose (LEA_ALLOW_STALE=1)"
        return 0
    fi
    LEA_BUILD_PROBLEM="${oldest_path#"$LEA_ROOT"/} is older than $newer -- run: scripts/build.sh cargo (LEA_ALLOW_STALE=1 to measure it anyway)"
    echo "$LEA_BUILD_PROBLEM" >&2
    return 1
}

# ---- the gate output contract ---------------------------------------------
# What a gate used to end in was
#
#   GATE-RESULT name=gpu status=pass stages=6 failed=0 duration=67s detail=".."
#
# which a human reads well and a script parses badly: `detail` is free text
# with spaces and quotes in it, and everything a gate MEASURED (a checksum, a
# frame rate, a node name) was inside that text or nowhere. The contract now
# is exactly one JSON object as the last line of stdout:
#
#   {"gate":"gpu","result":"pass","dur_s":67,"facts":{...}}
#   {"gate":"gpu","result":"fail","dur_s":31,"reason":"..","facts":{...}}
#   {"gate":"gpu","result":"skip","dur_s":0,"reason":"..","facts":{}}
#
# and the rules are:
#
#   - stdout carries the JSON and NOTHING else. All human text goes to
#     stderr. lea_gate_begin arranges that without any gate rewriting its
#     echoes: it dups stdout to fd 3 and points stdout at stderr, so every
#     later `echo` in the script lands on stderr and only _lea_gate_emit
#     writes to fd 3.
#   - exit 0 pass, 1 fail, 2 skip. `skip` means a PRECONDITION was not met
#     (no GPU, binaries not built, rig not ready). It is NOT for "a stage
#     could not measure" -- inside a running gate that stays a fail, because
#     a skipped stage inside a green gate is a success claim without a reader.
#   - `reason` is omitted on pass; `facts` is always present, possibly empty.
#   - a gate that dies before lea_gate_finish still emits a line, via the
#     EXIT trap lea_gate_begin installs. A clean-looking exit 0 without a
#     verdict is reported as a FAIL, not a pass.
#
# Reserved fact keys the library writes: `stages` (stage names in run order),
# `failed` (the ones that failed), `detail` (the free text that used to be
# the tail of GATE-RESULT). Everything else comes from lea_gate_fact.
_LEA_GATE_NAME=""
_LEA_GATE_T0=0
_LEA_GATE_DONE=0
declare -a _LEA_GATE_STAGES=()
declare -a _LEA_GATE_FAILED=()
declare -a _LEA_GATE_FACT_KEYS=()
declare -a _LEA_GATE_FACT_VALS=()

# _lea_json_str VALUE -- the value as a JSON string, quotes and all.
_lea_json_str() {
    local s=$1
    s=${s//\\/\\\\}
    s=${s//\"/\\\"}
    s=${s//$'\n'/\\n}
    s=${s//$'\r'/\\r}
    s=${s//$'\t'/\\t}
    # Whatever control characters are left have no escape worth inventing
    # and would make the line unparseable. Dropped rather than mangled.
    s=$(printf '%s' "$s" | tr -d '\000-\010\013\014\016-\037')
    printf '"%s"' "$s"
}

# _lea_json_val VALUE -- a bare number when it looks like one, else a string.
_lea_json_val() {
    if [[ $1 =~ ^-?[0-9]+(\.[0-9]+)?$ ]]; then
        printf '%s' "$1"
    else
        _lea_json_str "$1"
    fi
}

# _lea_json_arr ITEM... -- a JSON array of strings.
_lea_json_arr() {
    local first=1 i
    printf '['
    for i in "$@"; do
        [[ $first -eq 1 ]] || printf ','
        first=0
        _lea_json_str "$i"
    done
    printf ']'
}

# lea_gate_begin NAME -- start a gate: clock, fd 3, and the safety net.
lea_gate_begin() {
    _LEA_GATE_NAME=$1
    _LEA_GATE_T0=$(date +%s)
    _LEA_GATE_DONE=0
    _LEA_GATE_STAGES=()
    _LEA_GATE_FAILED=()
    _LEA_GATE_FACT_KEYS=()
    _LEA_GATE_FACT_VALS=()
    # fd 3 is the real stdout from here on; stdout itself becomes stderr, so
    # no existing echo in a gate has to be touched. Command substitutions are
    # unaffected -- they get their own pipe -- and child scripts inherit the
    # arrangement, which is what we want: their chatter is human output too.
    exec 3>&1 1>&2
    lea_on_exit "_lea_gate_atexit"
}

# _lea_gate_atexit -- the safety net: no verdict emitted means the gate died.
_lea_gate_atexit() {
    [[ $_LEA_GATE_DONE -eq 1 ]] && return 0
    [[ -z $_LEA_GATE_NAME ]] && return 0
    # The status the shell was ABOUT to leave with, reported verbatim. Not
    # the forced one below: on a signal the pending status is 0 (the kill
    # itself succeeded) while the shell goes on to exit 143, and a reason
    # line naming a number this function invented is worse than none.
    local pending=$_LEA_EXIT_RC
    # A gate that exits 0 without a verdict is NOT a pass. `set -e` on an
    # unchecked command, a `die` in a helper and a SIGTERM all land here, and
    # a clean-looking status says nothing at all about the measurement.
    [[ $_LEA_EXIT_RC -eq 0 ]] && _LEA_EXIT_RC=1
    _lea_gate_emit fail "exited before the verdict (pending status $pending)"
}

# lea_gate_say HEADING... -- a section separator in the human transcript.
lea_gate_say() {
    echo
    lea_head "$@"
}

# lea_gate_pass STAGE REASON -- record a stage that answered correctly.
lea_gate_pass() {
    _LEA_GATE_STAGES+=("$1")
    echo "  GATE $1: ${LEA_GRN}PASS${LEA_R}  ($2)"
}

# lea_gate_fail STAGE REASON -- record a stage that did not.
lea_gate_fail() {
    _LEA_GATE_STAGES+=("$1")
    _LEA_GATE_FAILED+=("$1")
    echo "  GATE $1: ${LEA_RED}FAIL${LEA_R}  ($2)"
}

# lea_gate_fact KEY VALUE -- one measured number or string for the JSON line.
lea_gate_fact() {
    local k=$1 v=${2:-} i
    for i in "${!_LEA_GATE_FACT_KEYS[@]}"; do
        if [[ ${_LEA_GATE_FACT_KEYS[i]} == "$k" ]]; then
            _LEA_GATE_FACT_VALS[i]=$v
            return 0
        fi
    done
    _LEA_GATE_FACT_KEYS+=("$k")
    _LEA_GATE_FACT_VALS+=("$v")
}

# _lea_gate_emit RESULT [REASON] -- write the one JSON line to fd 3.
_lea_gate_emit() {
    local result=$1 reason=${2:-} dur i
    _LEA_GATE_DONE=1
    dur=$(( $(date +%s) - _LEA_GATE_T0 ))
    {
        printf '{"gate":%s,"result":"%s","dur_s":%d' \
            "$(_lea_json_str "$_LEA_GATE_NAME")" "$result" "$dur"
        [[ $result != pass ]] && printf ',"reason":%s' "$(_lea_json_str "$reason")"
        printf ',"facts":{"stages":'
        _lea_json_arr "${_LEA_GATE_STAGES[@]}"
        printf ',"failed":'
        _lea_json_arr "${_LEA_GATE_FAILED[@]}"
        for i in "${!_LEA_GATE_FACT_KEYS[@]}"; do
            printf ',%s:%s' \
                "$(_lea_json_str "${_LEA_GATE_FACT_KEYS[i]}")" \
                "$(_lea_json_val "${_LEA_GATE_FACT_VALS[i]}")"
        done
        printf '}}\n'
    } >&3
}

# lea_gate_skip REASON -- a precondition was not met. Emits and exits 2.
lea_gate_skip() {
    local reason=${1:-precondition not met}
    echo
    echo "${_LEA_GATE_NAME^^}-GATE: SKIP -- $reason"
    _lea_gate_emit skip "$reason"
    exit 2
}

# lea_gate_finish [DETAIL] -- the verdict. Emits and exits 0 (pass) or 1.
#
# Deliberately `exit`, not `return`: the gpu and display gates run without
# `set -e` (they do their own cleanup), and a mere return value would be
# lost there -- the gate would report PASS though a stage had failed. The
# EXIT traps of the calling script still run.
lea_gate_finish() {
    local detail=${1:-} result=pass
    [[ ${#_LEA_GATE_FAILED[@]} -gt 0 ]] && result=fail
    [[ -n $detail ]] && lea_gate_fact detail "$detail"
    echo
    if [[ $result == pass ]]; then
        echo "${_LEA_GATE_NAME^^}-GATE: PASS (${_LEA_GATE_STAGES[*]:-none})"
        _lea_gate_emit pass
        exit 0
    fi
    echo "${_LEA_GATE_NAME^^}-GATE: FAIL -- ${_LEA_GATE_FAILED[*]}"
    _lea_gate_emit fail "failed: ${_LEA_GATE_FAILED[*]}"
    exit 1
}

# ---- the trace reader -------------------------------------------------------
# ONE place that knows what a trace line looks like, on the shell side. The
# tracer writes two formats now (crates/nvrm-trace/src/log.rs): the legacy
# TSV and JSONL, both rendered from the same record, and every shell
# consumer reads whichever it is handed through this function instead of
# knowing.
#
# WHAT IT EMITS: the MEASUREMENT stream, as the canonical TSV columns, for
# the six positional kinds --
#
#   open      <dev> <fd>
#   ioctl     <dev> <nr> <sub> <size> <psize> <ret> <status> <fd>
#   mmap      <dev> <fd> <len> <off> <addr>
#   read      <dev> <fd> <ret>
#   poll      <dev> <fd> <revents>
#   eventreg  <fd> <prev>
#
# and DROPS the diagnostic kinds (`nvos*`, `ctrlout`, `cardinfo`, `uvm*`,
# `memparams`) and the `#` provenance header. That is not a loss: log.rs
# has always called those diagnostic lines and not measurements, no shell
# consumer has ever read one, and the ones that matter -- `ctrlout`,
# `cardinfo` -- are read by probe/python/traceread.py, which models every
# kind.
#
# AWK AND NOT jq, deliberately: this runs inside the guest, where the
# staged tree is scripts/lib, probe/matrix, probe/bin and the tracer, and
# nothing has promised jq is installed. The JSON it parses is not arbitrary
# JSON -- it is what render_json in log.rs writes -- and the reader below
# extracts by key rather than by position, so a field added in the middle
# does not move anything.
#
# lea_trace_stream FILE
lea_trace_stream() {
    [[ -f ${1-} ]] || return 0
    awk '
    # The value of key K on this JSON line, or "\002" if the key is absent.
    # Quoted strings come back unquoted; numbers, null, true and false come
    # back as written. Escapes inside a string are left ESCAPED -- nothing
    # the tracer writes today contains one, and a reader that silently
    # unescaped would be a second, divergent implementation of push_json_str.
    function jval(line, k,    s, out, i, c, esc) {
        if (match(line, "\"" k "\":") == 0) return "\002"
        s = substr(line, RSTART + RLENGTH)
        if (substr(s, 1, 1) != "\"") {
            if (match(s, /^[^,}]+/)) return substr(s, RSTART, RLENGTH)
            return "\002"
        }
        out = ""; esc = 0
        for (i = 2; i <= length(s); i++) {
            c = substr(s, i, 1)
            if (esc)        { out = out c; esc = 0; continue }
            if (c == "\\")  { out = out c; esc = 1; continue }
            if (c == "\"")  break
            out = out c
        }
        return out
    }
    # `null` is the JSON spelling of the TSV `-`, which is what every
    # selector downstream tests for.
    function v(line, k,    x) {
        x = jval(line, k)
        if (x == "\002" || x == "null") return "-"
        return x
    }
    /^#/ { next }
    /^\{/ {
        t = jval($0, "t")
        if (t == "ioctl")
            print t "\t" v($0,"dev") "\t" v($0,"nr") "\t" v($0,"sub") "\t" \
                  v($0,"size") "\t" v($0,"psize") "\t" v($0,"ret") "\t" \
                  v($0,"status") "\t" v($0,"fd")
        else if (t == "open")
            print t "\t" v($0,"dev") "\t" v($0,"fd")
        else if (t == "mmap")
            print t "\t" v($0,"dev") "\t" v($0,"fd") "\t" v($0,"len") "\t" \
                  v($0,"off") "\t" v($0,"addr")
        else if (t == "read")
            print t "\t" v($0,"dev") "\t" v($0,"fd") "\t" v($0,"ret")
        else if (t == "poll")
            print t "\t" v($0,"dev") "\t" v($0,"fd") "\t" v($0,"revents")
        else if (t == "eventreg")
            print t "\t" v($0,"fd") "\t" v($0,"prev")
        next
    }
    # Legacy TSV: the same six kinds, already in these columns.
    $1=="ioctl" || $1=="open" || $1=="mmap" || $1=="read" || $1=="poll" || $1=="eventreg"
    ' "$1"
}

# lea_trace_file DIR PROBE -- the trace to read, JSONL first.
#
# Both formats are written during the migration and the equivalence gate
# proves they agree, so preferring the new one is what actually exercises
# it. A trace directory from before the migration has no `.jsonl` and falls
# back without saying anything, because there is nothing wrong with it.
lea_trace_file() {
    local d=$1 p=$2
    if [[ -f $d/$p.jsonl ]]; then echo "$d/$p.jsonl"; else echo "$d/$p.tsv"; fi
}
