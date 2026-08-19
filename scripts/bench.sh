#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# The measurement track: what the boundary costs, how the card is shared,
# what a stream delivers -- always against a reference, never against a
# feeling.
#
#   scripts/bench.sh transport [--hours H] [--minutes M] [--until HH:MM] [--out DIR]
#                              [--no-native] [--smoke] [--loads "a b c"] [--progress]
#   scripts/bench.sh summary <messungen.csv>
#   scripts/bench.sh fleet   [--reps N] [--stages "1 2 4"] [--iters N] [--out DIR]
#                            [--keep-fleet] [--load convburn|managed|ping] [--smoke]
#                            [--guest ubuntu|nixos] [--transport ip|vsock]
#   scripts/bench.sh render  [--count N] [--stages "1 2 4"] [--reps R] [--size WxH]
#                            [--out DIR] [--provision-only]
#   scripts/bench.sh stream  [--capture x11|portal] [--encoder software|nvenc]
#                            [--session xvfb|desktop] [--name NAME | --ip A.B.C.D]
#                            [--seconds N] [--bitrate Kbps] [--res WxH] [--fps N]
#                            [--content gears|gears-nv|none] [--out DIR] [--display :N]
#   scripts/bench.sh diag    [--out DIR]
#   scripts/bench.sh slurm   [--package DIR] [--out DIR] [--counts "1 2 4"]
#                            [--cells "gate fleet"] [--partition P] [--gres G]
#                            [--account A] [--time HH:MM:SS] [--mem MiB]
#                            [--cpus N] [--submit]
#   scripts/bench.sh slurm   --run CELL [--package DIR] [--out DIR]
#   scripts/bench.sh slurm   --collect DIR
#   scripts/bench.sh vk      [--name NAME] [--res WxH] [--out DIR] [--display :N]
#                            [--present immediate|fifo|both] [--seconds N]
#                            [--scenes "a b c"]
#
# TRANSPORT: what the path through the module costs, against the bare card.
# Two variants, spelled as the CSV's own tokens -- `nativ` on the host, no
# VM (what the card itself can do) and `modul` in the VM over virtio-nvrm
# (the production path); the gap is
# the price of the VM boundary including the module. INTERLEAVED rather
# than block by block: the card drives the host desktop on the side and gets
# warmer under load (measured: 62 -> 72 degrees, boost drops), so running
# variant A for two hours and then B would measure the temperature. BOTH
# CPU times: forwarding moves work into the host backend, and measuring only
# the guest wall clock makes the transport look cheaper than it is. Runs
# unattended until the deadline; every line goes into the CSV immediately;
# if the VM or the backend dies, the rig is rebuilt and the incident logged.
# --progress prints one line per measurement so the waiting is observable.
#
# SUMMARY evaluates a transport CSV: MEDIAN and p10/p90, not mean and
# standard deviation -- the distribution is skewed (one compaction run and a
# single value shoots up). It ABORTS when the run mixes persistence states:
# a comparison across them is not one (persistence alone once moved the
# native reference by 58 %).
#
# FLEET: scaling over several VMs on ONE GPU -- the same workload at the
# same time in 1, then 2, then 4 VMs. --guest picks what the members are;
# without it, whatever member 0 already is, so that a fleet brought up with
# `showcase.sh up --count 4 --guest nixos` is not silently rebuilt as Ubuntu. Loads: convburn (throughput and
# fairness), managed (managed memory on N VMs at once, with
# oversubscription, which is MEANT to fail loudly), ping (ioctlping and
# mmapping under contention). The fleet is started ONCE with 4 VMs; stages
# 1/2 leave the rest booted but idle, so every stage measures the same boot
# environment. Aggregate throughput = sum(iters/time); fairness = spread of
# ms/it between the VMs in % of the median; acc must be ONE distinct value.
#
# RENDER: what N guests get when they all RENDER at once -- `glmark2
# --off-screen` (renders into an FBO: no compositor, no presentation, no X
# in the measuring path; and no /dev/dri needed, NVIDIA's GL reaches the card
# over /dev/nvidiactl alone) on a RUNNING fleet (showcase.sh up --count N).
# The host-native score is measured in the same run when the host has
# glmark2 and a display; otherwise the 2026-08-07 reference is printed
# (21,486 native, 21,277 in one guest, 99.0 %). Per-member VRAM caps are
# handed to the fleet (LEA_VRAM_LIMIT_MIB_1=512 showcase.sh up --count 2).
#
# STREAM: one Moonlight session, measured, so that two encoders can be
# compared on the same rig, the same content, minutes apart. Sunshine in the
# guest captures the X root (`x11`, frames through system memory, NVENC
# reachable) or the desktop portal (`portal`); Moonlight on THIS host
# receives; the verdict comes from Moonlight's own session summary. --session
# `desktop` takes the session that is ALREADY running (showcase.sh up
# --display --session gnome, or the display gate's X on :7 via --display);
# `xvfb` starts a software X server with openbox and owns the content, the
# arrangement for a COMPARISON. A non-black-pixel count qualifies every
# number: 102 non-black pixels of 2,073,600 is an ENCODER number, not a
# stream. Exit 3 = the run happened but is not a measurement. WARNING:
# `moonlight` opens a window in the operator's own session (windowed).
# It brings its OWN Sunshine config and PUTS THE GUEST'S BACK: a Sunshine
# that was already running is restarted the way it was started (DISPLAY and
# XAUTHORITY read from its own environ) and its config file restored, on
# every exit path -- so a desktop from `showcase.sh up --session gnome`
# still streams after a measurement.
#
# DIAG: where does the time go during setup? Counts what crosses the
# boundary during cuInit and context creation with LEA_DEBUG=2 in the
# backend -- ioctls in total and per escape, window mappings, backend CPU.
# The log is itself expensive, so the WALL CLOCK of this run is NOT a
# measurement and is deliberately not printed.
#
# VK: vkmark on the virtual display (X on :7 inside the desktop instance,
# brought up if absent, or a session display via --display :0), six scenes
# from `clear` to sixteen blurred desktop windows, with the same vkmark on
# the host as the reference when it is installed. TWO PRESENT MODES, because
# they answer different questions: `fifo` asks whether the virtual display
# holds its 60 Hz (16.67 ms, and the run says "at the cap" when it does),
# `immediate` takes the pacing away and measures what a frame COSTS over the
# boundary. Frame time is the primary column; FPS under fifo is a property
# of the display, not of the path. Measured 2026-08-18 (RTX 2070, two runs):
# fifo 16.39-16.67 ms in the guest; immediate 0.262-0.375 ms guest against
# 0.107-0.229 ms host, a factor of 1.6 to 2.6. Nothing vkmark has gets near
# the 60 Hz budget on this card -- the cap was a present mode, not a limit.
# And not even a fixed one: the same fifo pass on the BARE rig X on :7, with
# no compositor and no session, sits at 10.000 ms / 100 FPS for five of the
# six scenes (twice, 2026-08-18), which the run marks "OFF the cap".
#
# Every measurement checks the rig state first (lea_rig_check) and writes
# it beside the data: a measurement against a wrongly configured rig costs
# more than no measurement, because it looks like a result. HOW TO WAIT:
# each subcommand holds vm/bench-<name>.pid; wait on the FILE, never on
# `pgrep -f` (lea_hold_pidfile has the long version).
set -uo pipefail
LEA_ROOT=${LEA_ROOT:-$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd -P)}
# shellcheck source=scripts/lib/rig.sh
source "$LEA_ROOT/scripts/lib/rig.sh"

usage() { lea_usage_from_header; exit "${1:-0}"; }

CMD=${1:-}
case $CMD in
    transport|summary|fleet|render|stream|diag|vk|slurm) shift ;;
    -h|--help|"") usage 0 ;;
    *) error "unknown subcommand: $CMD"; usage 2 ;;
esac

# ---- shared ---------------------------------------------------------------
LOG=""
log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "${LOG:-/dev/null}"; }
TICK=$(getconf CLK_TCK)
backend_cpu_ms() {  # <pid> -> utime+stime in ms
    local t
    t=$(awk '{print $14+$15}' "/proc/$1/stat" 2>/dev/null || echo 0)
    echo $(( ${t:-0} * 1000 / TICK ))
}
# WARNING: rig state FIRST and into the file. --check prints the RIG line
# itself and appends findings -- one call is enough.
bench_rig_check() {  # <outdir>
    if ! lea_rig_check > "$1/rig.txt" 2>&1; then
        error "rig not ready to measure --"; cat "$1/rig.txt"; exit 1
    fi
    cat "$1/rig.txt"
}
# Temperature/clock/load BEFORE every measurement -- so that drift and
# foreign load (the desktop hangs off the same card) are visible in the
# result instead of making it scatter inexplicably. Persistence in the SAME
# query: it once moved the native reference by 58 %, and with it in the CSV
# per line, the evaluation can enforce a single state instead of hoping.
GPU_T=""; GPU_C=""; GPU_U=""; GPU_P=0
gpu_state() {
    local s
    s=$(nvidia-smi --query-gpu=temperature.gpu,clocks.sm,utilization.gpu,persistence_mode \
        --format=csv,noheader,nounits 2>/dev/null | tr -d ' ')
    GPU_T=${s%%,*}; s=${s#*,}; GPU_C=${s%%,*}; s=${s#*,}; GPU_U=${s%%,*}
    GPU_P=${s#*,}; [[ $GPU_P == Enabled ]] && GPU_P=1 || GPU_P=0
    : "${GPU_T:=}" "${GPU_C:=}" "${GPU_U:=}" "${GPU_P:=0}"
}
# The one instance the single-VM measurements use: the standard dev VM.
NAME=vm0
IP=$(lea_ip 0)

# ============================================================================
# transport
# ============================================================================
do_transport() {
    local hours="" minutes="" until_="" out="" native=1 smoke=0 loads="" progress=0
    while [[ $# -gt 0 ]]; do
        case $1 in
            --hours)     hours=$2; shift 2 ;;
            --minutes)   minutes=$2; shift 2 ;;
            --until)     until_=$2; shift 2 ;;
            --out)       out=$2; shift 2 ;;
            --no-native) native=0; shift ;;
            --smoke)     smoke=1; shift ;;
            --loads)     loads=$2; shift 2 ;;
            --progress)  progress=1; shift ;;
            -h|--help)   usage 0 ;;
            *) error "transport: unknown option $1"; usage 2 ;;
        esac
    done
    # Claimed before the rig is built, dropped by an EXIT trap -- so a
    # waiter sees the run from its first second, not its first measurement.
    lea_hold_pidfile "$LEA_VM_DIR/bench-transport.pid"
    local start deadline
    start=$(date +%s)
    if [[ -n $until_ ]]; then
        deadline=$(date -d "$until_" +%s 2>/dev/null) || die "--until $until_ unreadable"
        [[ $deadline -le $start ]] && deadline=$(date -d "tomorrow $until_" +%s)
    elif [[ -n $hours ]]; then
        # Reject fractions explicitly instead of truncating them to 0
        # quietly: `--hours 0.25` once produced a deadline of NOW, the whole
        # rig was built and zero rounds measured. Use --minutes.
        [[ $hours =~ ^[0-9]+$ ]] || die "--hours wants a whole number (else --minutes)"
        deadline=$(( start + hours * 3600 ))
    elif [[ -n $minutes ]]; then
        [[ $minutes =~ ^[0-9]+$ ]] || die "--minutes wants a whole number"
        deadline=$(( start + minutes * 60 ))
    elif [[ $smoke -eq 1 ]]; then
        deadline=$(( start + 240 ))
    else
        deadline=$(( start + 8 * 3600 ))
    fi
    out=${out:-$LEA_VM_DIR/bench-transport-$(date +%Y%m%d-%H%M%S)}
    mkdir -p "$out"
    bench_rig_check "$out"
    local CSV=$out/messungen.csv
    LOG=$out/run.log
    local NVRM_PIDF=$LEA_VM_DIR/$NAME/nvrm.pid
    # Loads. Short enough that a round takes a few minutes -- that keeps the
    # interleaving distance small against the temperature drift.
    local -a WORKLOADS=(ioctlping mmapping smi smibusy cuinit ctx kernel convburn rl mm)
    [[ $smoke -eq 1 ]] && WORKLOADS=(ioctlping mmapping smi cuinit kernel convburn)
    [[ -n $loads ]] && read -r -a WORKLOADS <<< "$loads"
    export NVCB_ITERS=${NVCB_ITERS:-100} NVRL_EPISODES=${NVRL_EPISODES:-50}
    export NVMM_SIZES=${NVMM_SIZES:-2048,4096} NVMM_ITERS=${NVMM_ITERS:-10}
    # LEA_DEBUG stays OFF: every eprintln line in the hot path would be a
    # measurement error nobody sees in the result.
    unset LEA_DEBUG

    [[ -f $CSV ]] || echo "time,round,variant,load,metric,value,gpu_temp_c,gpu_clock_mhz,gpu_util_pct,host_cpu_ms,ok,persistence" > "$CSV"
    row() { echo "$(date +%FT%T),$1,$2,$3,$4,$5,$GPU_T,$GPU_C,$GPU_U,$6,$7,$GPU_P" >> "$CSV"; }

    local REPAIRS=0
    rig_up() {
        lea_rig_down "$NAME" >/dev/null 2>&1
        # A fresh backend per VM start: one serves exactly ONE connection.
        lea_rig_up "$NAME" --index 0 >>"$LOG" 2>&1 || { log "ERROR: the rig did not come up"; return 1; }
        lea_guest_tar "$NAME" "$LEA_ROOT/scripts/guest" '$HOME/gpu' bench-one.sh >>"$LOG" 2>&1 \
            && lea_ssh "$IP" 'chmod +x ~/gpu/bench-one.sh' \
            || { log "ERROR: bench-one.sh did not reach the guest"; return 1; }
        log "rig is up."
    }
    healthy() { lea_running "$NVRM_PIDF" && lea_ssh "$IP" true >/dev/null 2>&1; }
    trap 'log "aborted -- cleaning up"; lea_rig_down "$NAME" >/dev/null 2>&1; exit 130' INT TERM

    emit_rows() {   # <round> <variant> <load> <host_cpu_ms> <output>
        local rnd=$1 v=$2 w=$3 hcpu=$4 out_=$5 ok=0 line k val kv
        local -a seen=()
        [[ $out_ == *"VAL ok 1"* ]] && ok=1
        while IFS= read -r line; do
            case $line in
                "MEAS "*)
                    for kv in $line; do
                        case $kv in
                            wall_ms=*)      row "$rnd" "$v" "$w" wall_ms "${kv#*=}" "$hcpu" "$ok"; seen+=("$kv") ;;
                            guest_cpu_ms=*) row "$rnd" "$v" "$w" guest_cpu_ms "${kv#*=}" "$hcpu" "$ok" ;;
                        esac
                    done ;;
                "VAL "*)
                    k=$(awk '{print $2}' <<<"$line"); val=$(awk '{print $3}' <<<"$line")
                    [[ $k == ok ]] || { row "$rnd" "$v" "$w" "$k" "$val" "$hcpu" "$ok"
                                        [[ ${#seen[@]} -lt 4 ]] && seen+=("$k=$val"); } ;;
                "TAIL "*) echo "  [$v/$w] ${line#TAIL }" >> "$LOG" ;;
                "ERR "*)  log "  [$v/$w] $line" ;;
            esac
        done <<<"$out_"
        [[ $ok -eq 1 ]] || log "  [$v/$w] FAILED (round $rnd)"
        [[ $progress -eq 1 ]] && printf '[%s] r%-3s %-6s %-10s %s%s\n' \
            "$(date +%H:%M:%S)" "$rnd" "$v" "$w" "${seen[*]:-—}" "$([[ $ok -eq 1 ]] || echo '  FAILED')"
        return 0
    }
    measure_guest() {   # <round> <variant> <load>
        local rnd=$1 v=$2 w=$3 out_ c0 c1 pid
        gpu_state
        pid=$(cat "$NVRM_PIDF" 2>/dev/null)
        c0=$(backend_cpu_ms "$pid")
        out_=$(lea_ssh "$IP" "cd ~/gpu && NVCB_ITERS=$NVCB_ITERS NVRL_EPISODES=$NVRL_EPISODES \
              NVMM_SIZES=$NVMM_SIZES NVMM_ITERS=$NVMM_ITERS ./bench-one.sh $v $w" 2>&1)
        c1=$(backend_cpu_ms "$pid")
        emit_rows "$rnd" "$v" "$w" "$(( c1 - c0 ))" "$out_"
    }
    measure_native() {  # <round> <load>   -- no guest, no VM
        local rnd=$1 w=$2 out_
        gpu_state
        out_=$(NVCB_ITERS=$NVCB_ITERS NVRL_EPISODES=$NVRL_EPISODES NVMM_SIZES=$NVMM_SIZES NVMM_ITERS=$NVMM_ITERS \
               "$LEA_ROOT/scripts/guest/bench-one.sh" nativ-host "$w" 2>&1)
        emit_rows "$rnd" nativ "$w" 0 "$out_"
    }

    log "target: $CSV"
    log "deadline: $(date -d "@$deadline" +%F' '%T)  (now $(date +%T))"
    log "loads: ${WORKLOADS[*]}  |  convburn ${NVCB_ITERS} it., rlprobe ${NVRL_EPISODES} ep., mm N=${NVMM_SIZES}"
    local foreign
    if foreign=$(lea_foreign_rigs "$NAME"); then
        die "instances of another rig are running: $(tr '\n' ' ' <<<"$foreign") -- stop them first (showcase.sh down --name X)"
    fi
    if foreign=$(lea_inst_owner "$NAME"); then die "$NAME is in use by pid $foreign -- not taking it over"; fi
    lea_require_built "$LEA_BIN_DIR/vhost-user-nvrm" || die "$LEA_BUILD_PROBLEM"
    rig_up || { lea_rig_down "$NAME" >/dev/null 2>&1; exit 1; }
    # A RELATIVE deadline counts from now, not from the start: building the
    # rig takes minutes, and "measure for two hours" means two hours of
    # MEASURING. An absolute deadline (--until) stays where it is.
    if [[ -z $until_ ]]; then
        deadline=$(( $(date +%s) + (deadline - start) ))
        log "measuring window starts now, ends $(date -d "@$deadline" +%F' '%T)"
    fi
    local ROUND=0 w HOSTPY=""
    while [[ $(date +%s) -lt $deadline ]]; do
        ROUND=$(( ROUND + 1 ))
        log "round $ROUND (modul$([[ $native -eq 1 ]] && echo ' + nativ'))"
        for w in "${WORKLOADS[@]}"; do
            [[ $(date +%s) -lt $deadline ]] || break
            measure_guest "$ROUND" modul "$w"
            if [[ $native -eq 1 ]]; then
                case $w in
                    convburn|rl|mm|smibusy)
                        # The native torch reference comes from vendor/hostvenv
                        # (the same wheel version as in the guest). If it is
                        # missing, that is logged ONCE -- a quiet gap in the
                        # table could not be explained later.
                        if [[ -z $HOSTPY ]]; then
                            HOSTPY=$LEA_ROOT/vendor/hostvenv/bin/python
                            [[ -x $HOSTPY ]] || HOSTPY=python3
                            "$HOSTPY" -c "import torch" 2>/dev/null \
                                || { log "  nativ: no torch on the host -> convburn/rl/mm without a native column"; HOSTPY=no; }
                        fi
                        [[ $HOSTPY != no ]] && measure_native "$ROUND" "$w" ;;
                    *) measure_native "$ROUND" "$w" ;;
                esac
            fi
        done
        if ! healthy; then
            REPAIRS=$(( REPAIRS + 1 ))
            log "rig broken after round $ROUND -- repair $REPAIRS"
            row "$ROUND" rig incident repair "$REPAIRS" 0 0
            [[ $REPAIRS -gt 10 ]] && { log "too many repairs -- stopping"; break; }
            rig_up || { log "rebuild failed -- stopping"; break; }
        fi
    done
    log "$ROUND rounds measured, $REPAIRS repairs."
    lea_rig_down "$NAME" >/dev/null 2>&1
    do_summary "$CSV" | tee "$out/summary.txt"
    log "summary: $out/summary.txt"
}

# ============================================================================
# summary
# ============================================================================
do_summary() {
    local CSV=${1:?CSV file}
    # The watchdog against a mismeasurement that already went through once:
    # persistence is in the CSV per line (column 12); if it varies within a
    # run, this aborts instead of quietly averaging.
    local _pers _rig
    _pers=$(awk -F, 'NR>1 && NF>=12 && $12 != "" {print $12}' "$CSV" | sort -u | tr '\n' ' ')
    if [[ $(wc -w <<<"$_pers") -gt 1 ]]; then
        error "this run mixes persistence states ($_pers). A comparison across them is not one -- measure again."
        return 1
    fi
    _rig=$(dirname "$CSV")/rig.txt
    if [[ -f $_rig ]]; then echo "Rig: $(head -1 "$_rig")"; echo
    elif [[ -z $_pers ]]; then echo "Rig: UNKNOWN (CSV predates rig capture -- do not compare with newer ones)"; echo; fi
    # Reads by COLUMN POSITION, not by header name.
    awk -F, '
    NR > 1 && $11 == 1 {
        key = $4 SUBSEP $5 SUBSEP $3          # load, metric, variant
        v[key] = v[key] " " $6
        loads[$4 SUBSEP $5] = 1
    }
    function median(list,   a, n, i, j, t) {
        n = split(list, a, " ")
        if (n == 0) return ""
        for (i = 1; i <= n; i++) for (j = i+1; j <= n; j++) if (a[j]+0 < a[i]+0) { t=a[i]; a[i]=a[j]; a[j]=t }
        return a[int((n+1)/2)] + 0
    }
    function count(list,   a) { return split(list, a, " ") }
    END {
        printf "%-9s %-16s %5s %12s %12s %11s\n", "load", "metric", "n", "modul", "nativ", "modul/nativ"
        printf "%-9s %-16s %5s %12s %12s %11s\n", "----", "------", "-", "-----", "-----", "-----------"
        for (lm in loads) {
            split(lm, p, SUBSEP)
            load = p[1]; metric = p[2]
            m = median(v[load SUBSEP metric SUBSEP "modul"])
            n = median(v[load SUBSEP metric SUBSEP "nativ"])
            cnt = count(v[load SUBSEP metric SUBSEP "modul"])
            rel = (n + 0 != 0 && m != "" && n != "") ? sprintf("%.2fx", m / n) : ""
            printf "%-9s %-16s %5d %12s %12s %11s\n", load, metric, cnt, \
                   (m == "" ? "-" : sprintf("%.3f", m)), (n == "" ? "-" : sprintf("%.3f", n)), rel
        }
    }' "$CSV" | (read -r h1; read -r h2; echo "$h1"; echo "$h2"; sort)
    echo
    echo "Spread (modul, p10 / median / p90) -- far apart means: the number is noise, not a result"
    awk -F, '
    NR > 1 && $11 == 1 && $3 == "modul" { v[$4 SUBSEP $5] = v[$4 SUBSEP $5] " " $6 }
    function q(list, p,   a, n, i, j, t) {
        n = split(list, a, " ")
        for (i = 1; i <= n; i++) for (j = i+1; j <= n; j++) if (a[j]+0 < a[i]+0) { t=a[i]; a[i]=a[j]; a[j]=t }
        return a[int(p * (n - 1)) + 1] + 0
    }
    END { for (k in v) { split(k, p, SUBSEP)
          printf "  %-9s %-16s %10.3f %10.3f %10.3f\n", p[1], p[2], q(v[k],0.1), q(v[k],0.5), q(v[k],0.9) } }' "$CSV" | sort
    echo
    echo "Incidents and failures:"
    awk -F, 'NR>1 && $11!=1 { bad[$3" "$4]++ } NR>1 && $4=="incident" { rig++ }
             END { for (k in bad) printf "  %-20s %d failed measurements\n", k, bad[k];
                   if (rig) printf "  rig repairs: %d\n", rig;
                   if (!length(bad) && !rig) print "  none" }' "$CSV"
    echo
    echo "GPU drift over the run (temperature/clock at the first and last measuring point):"
    awk -F, 'NR==2 {printf "  start: %s C, %s MHz\n", $7, $8} END {printf "  end:   %s C, %s MHz\n", $7, $8}' "$CSV"
}

# ============================================================================
# fleet
# ============================================================================
do_fleet() {
    local reps=3 stages="1 2 4" iters=100 out="" keep=0 load=convburn guest="" transport=""
    while [[ $# -gt 0 ]]; do
        case $1 in
            --reps)   reps=$2; shift 2 ;;
            --stages) stages=$2; shift 2 ;;
            --iters)  iters=$2; shift 2 ;;
            --out)    out=$2; shift 2 ;;
            --keep-fleet) keep=1; shift ;;
            --load)   load=$2; shift 2 ;;
            --guest)  guest=$2; shift 2 ;;
            --transport) transport=$2; shift 2 ;;
            --smoke)  reps=1; stages="1 2"; iters=20; shift ;;
            -h|--help) usage 0 ;;
            *) error "fleet: unknown option $1"; usage 2 ;;
        esac
    done
    # WHICH GUEST the fleet is made of. This function brings the fleet up
    # itself, so it has to decide -- and defaulting to a fixed value would
    # silently REBUILD an existing NixOS fleet as Ubuntu on the next run. The
    # default is therefore what member 0 already is, which is also what makes
    # `showcase.sh up --count 4 --guest nixos` and then `bench.sh fleet` do
    # the obvious thing.
    [[ -n $guest ]] || guest=$(lea_guest_os vm0)
    [[ -n $transport ]] || transport=$(lea_transport_of vm0)
    lea_hold_pidfile "$LEA_VM_DIR/bench-fleet.pid"
    out=${out:-$LEA_VM_DIR/bench-fleet-$(date +%Y%m%d-%H%M%S)}
    mkdir -p "$out"
    bench_rig_check "$out"
    local CSV=$out/parallel.csv
    LOG=$out/run.log
    [[ -f $CSV ]] || echo "time,stage,rep,vm,metric,value" > "$CSV"
    row() { echo "$(date +%FT%T),$1,$2,$3,$4,$5" >> "$CSV"; }   # stage rep vm metric value
    local n_max=0 s
    for s in $stages; do [[ $s -gt $n_max ]] && n_max=$s; done
    [[ $n_max -ge 1 && $n_max -le $LEA_MAX_VMS ]] || die "--stages wants numbers 1..$LEA_MAX_VMS"
    lea_vm_running vm0 && die "the standard VM (vm0) is running -- it is fleet member 0. showcase.sh down first."
    lea_require_built "$LEA_BIN_DIR/vhost-user-nvrm" || die "$LEA_BUILD_PROBLEM"
    local GUEST_CMD
    # The run in the guest: module path, no LD_PRELOAD. convburn's output line:
    #   convburn: ... time=2.437s 24.37ms/it acc=1.065710664e+00 vram=352MiB
    case $load in
        convburn) GUEST_CMD='cd ~/gpu && LD_LIBRARY_PATH=$PWD/nv/lib NVCB_ITERS=__ITERS__ timeout 600 venv/bin/python convburn.py' ;;
        # NVMG_STAGE=3 includes the oversubscription; what changes across
        # stages is the NUMBER of VMs trying it at once.
        managed)  GUEST_CMD='cd ~/gpu && LD_LIBRARY_PATH=$PWD/nv/lib NVPROBE_PTX=$PWD/kernels.ptx NVMG_STAGE=3 timeout 600 ./managedprobe' ;;
        # Both transport figures in ONE run, under the same contention.
        ping)     GUEST_CMD='cd ~/gpu && LD_LIBRARY_PATH=$PWD/nv/lib timeout 300 ./ioctlping __ITERS__ && timeout 300 ./mmapping 200 20 65536' ;;
        *) die "unknown load: $load (convburn|managed|ping)" ;;
    esac
    log "target: $CSV  (guest=$guest transport=$transport load=$load reps=$reps stages='$stages' iters=$iters)"
    # The guest OS belongs beside the RIG line, not only in the log: a fleet
    # measured on NixOS and one measured on Ubuntu are two measurements, and
    # a directory that does not say which is a directory nobody can compare
    # against anything later.
    echo "GUEST $guest transport=$transport" >> "$out/rig.txt"
    # The managed switch belongs on the HOST, before the backend starts
    # (session.rs reads it there). Set in the guest it does nothing.
    [[ $load == managed ]] && export LEA_MANAGED_COMPAT=1
    log "starting the fleet ($n_max VMs, loading modules)${LEA_MANAGED_COMPAT:+, MANAGED_COMPAT=1} ..."
    lea_fleet_up "$n_max" --guest "$guest" --transport "$transport" >>"$LOG" 2>&1 || { log "ERROR: the fleet did not come up"; exit 1; }
    [[ $keep -eq 1 ]] || lea_on_exit "lea_fleet_down >/dev/null 2>&1"

    local S R gstate SAMP SPID i t0 t1 agg ok c1 rc L t msit b b1 b2 v fair st
    local -a C0 PIDS
    for S in $stages; do
        for R in $(seq 1 "$reps"); do
            gstate=$(nvidia-smi --query-gpu=temperature.gpu,clocks.sm --format=csv,noheader,nounits | tr -d ' ')
            row "$S" "$R" host gpu_temp_c "${gstate%%,*}"
            row "$S" "$R" host gpu_clock_mhz "${gstate#*,}"
            # 1 s sampler: the VRAM and host RAM maximum DURING the run.
            SAMP=$out/samp-$S-$R.txt; : > "$SAMP"
            ( while :; do
                  echo "$(nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits | tr -d ' ') \
                        $(free -m | awk '/^Mem:/{print $3}')" >> "$SAMP"
                  sleep 1
              done ) & SPID=$!
            for i in $(seq 0 $((S - 1))); do
                C0[i]=$(backend_cpu_ms "$(cat "$LEA_VM_DIR/vm$i/nvrm.pid" 2>/dev/null)")
            done
            t0=$(date +%s%N)
            for i in $(seq 0 $((S - 1))); do
                ( lea_ssh "$(lea_ip "$i")" "${GUEST_CMD//__ITERS__/$iters}" > "$out/s$S-r$R-vm$i.log" 2>&1
                  echo $? > "$out/s$S-r$R-vm$i.rc" ) &
                PIDS[i]=$!
            done
            wait "${PIDS[@]:0:$S}"
            t1=$(date +%s%N)
            kill "$SPID" 2>/dev/null; wait "$SPID" 2>/dev/null
            row "$S" "$R" host wall_total_s "$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.3f",(b-a)/1e9}')"
            row "$S" "$R" host vram_max_mib     "$(awk '{if($1>m)m=$1}END{print m+0}' "$SAMP")"
            row "$S" "$R" host host_ram_max_mib "$(awk '{if($2>m)m=$2}END{print m+0}' "$SAMP")"
            agg=0; ok=1
            for i in $(seq 0 $((S - 1))); do
                c1=$(backend_cpu_ms "$(cat "$LEA_VM_DIR/vm$i/nvrm.pid" 2>/dev/null)")
                row "$S" "$R" "$i" backend_cpu_ms "$(( c1 - ${C0[i]} ))"
                rc=$(cat "$out/s$S-r$R-vm$i.rc" 2>/dev/null || echo 99)
                row "$S" "$R" "$i" rc "$rc"
                L=$out/s$S-r$R-vm$i.log
                # grab RE NAME -- if the value is missing, NOTHING is written.
                # A missing line is visible in the CSV, an invented 0 is not.
                grab() { local g; g=$(grep -oP "$1" "$L" | head -1); [[ -n $g ]] && row "$S" "$R" "$i" "$2" "$g"; }
                case $load in
                    convburn)
                        t=$(grep -oP '(?<=time=)[0-9.]+' "$L" | head -1)
                        msit=$(grep -oP '[0-9.]+(?=ms/it)' "$L" | head -1)
                        grab '(?<=time=)[0-9.]+'   time_s
                        grab '[0-9.]+(?=ms/it)'    ms_per_it
                        grab '(?<=acc=)[0-9.e+-]+' acc
                        [[ $rc -eq 0 && -n $msit ]] || { ok=0; log "  stage $S rep $R vm$i FAILED (rc=$rc)"; }
                        [[ -n $t ]] && agg=$(awk -v a="$agg" -v z="$t" -v n="$iters" 'BEGIN{printf "%.3f", a + n/z}')
                        ;;
                    managed)
                        # One correctness column per probe stage. `bad` is the
                        # number of wrong elements -- anything but 0 means
                        # "quietly computed the wrong thing", worse than a crash.
                        for st in 1 2 3; do
                            b=$(grep -oP "stage$st .*bad=\\K[0-9]+" "$L" | head -1)
                            [[ -n $b ]] && row "$S" "$R" "$i" "stage${st}_bad" "$b"
                            grep -q "stage$st ok" "$L" && row "$S" "$R" "$i" "stage${st}_ok" 1 \
                                                      || row "$S" "$R" "$i" "stage${st}_ok" 0
                        done
                        grab '(?<=oversub )[0-9]+' oversub_mib
                        # The pool warning from the host is the actual reason
                        # for this measurement (docs/OPEN-QUESTIONS.md, item 4).
                        row "$S" "$R" "$i" pool_warnings \
                            "$(grep -c 'not in any pool' "$LEA_VM_DIR/vm$i/nvrm.log" 2>/dev/null || echo 0)"
                        # The success criterion is NOT "stage3 ok". Oversubscription
                        # does not carry across the boundary and is meant to fail
                        # LOUDLY (measured: 0 of 14 runs ok, already with ONE VM).
                        # What is correct: stages 1 and 2 compute correctly (bad=0),
                        # stage 3 fails visibly instead of computing the wrong thing.
                        b1=$(grep -oP "stage1 .*bad=\\K[0-9]+" "$L" | head -1)
                        b2=$(grep -oP "stage2 .*bad=\\K[0-9]+" "$L" | head -1)
                        if [[ ${b1:-x} == 0 && ${b2:-x} == 0 ]]; then
                            grep -q "stage3 ok" "$L" && row "$S" "$R" "$i" oversub_holds 1 || row "$S" "$R" "$i" oversub_holds 0
                        else
                            ok=0; log "  stage $S rep $R vm$i managed computed WRONG (bad1=${b1:-?} bad2=${b2:-?})"
                        fi
                        ;;
                    ping)
                        grab '(?<=ioctlping )n=[0-9]+ fails=\K[0-9]+' ioctl_fails
                        v=$(grep -oP 'ioctlping .*p50_us=\K[0-9.]+' "$L" | head -1); [[ -n $v ]] && row "$S" "$R" "$i" ioctl_p50_us "$v"
                        v=$(grep -oP 'ioctlping .*p99_us=\K[0-9.]+' "$L" | head -1); [[ -n $v ]] && row "$S" "$R" "$i" ioctl_p99_us "$v"
                        v=$(grep -oP 'mmapping .*p50_us=\K[0-9.]+' "$L" | head -1); [[ -n $v ]] && row "$S" "$R" "$i" mmap_p50_us "$v"
                        v=$(grep -oP 'mmapping .*p90_us=\K[0-9.]+' "$L" | head -1); [[ -n $v ]] && row "$S" "$R" "$i" mmap_p90_us "$v"
                        { grep -q "ioctlping .*fails=0" "$L" && grep -q "mmapping .*fails=0" "$L"; } \
                            || { ok=0; log "  stage $S rep $R vm$i ping FAILED (rc=$rc)"; }
                        ;;
                esac
            done
            row "$S" "$R" host agg_it_per_s "$agg"
            # Fairness: spread of the ms/it in % of the median (only when S > 1).
            if [[ $S -gt 1 && $load == convburn ]]; then
                fair=$(grep -h -oP '[0-9.]+(?=ms/it)' "$out"/s"$S"-r"$R"-vm*.log | sort -n | awk '
                    {a[NR]=$1} END{ if(NR<2){print "";exit}
                    m=a[int((NR+1)/2)]; printf "%.1f", (a[NR]-a[1])/m*100 }')
                [[ -n $fair ]] && row "$S" "$R" host fairness_spread_pct "$fair"
            fi
            row "$S" "$R" host ok "$ok"
            log "stage $S rep $R: agg=${agg} it/s  (ok=$ok)"
        done
    done
    if [[ $keep -eq 0 ]]; then log "stopping the fleet ..."; lea_fleet_down >>"$LOG" 2>&1; fi
    {
    echo
    echo "Scaling (median over $reps repetitions, $load $iters it., module path):"
    awk -F, '
    NR > 1 { key = $2 SUBSEP $5; v[key] = v[key] " " $6; stages[$2] = 1 }
    function med(list,   a, n, i, j, t) {
        n = split(list, a, " "); if (n == 0) return ""
        for (i = 1; i <= n; i++) for (j = i+1; j <= n; j++) if (a[j]+0 < a[i]+0) { t=a[i]; a[i]=a[j]; a[j]=t }
        return a[int((n+1)/2)] + 0
    }
    END {
        printf "  %-6s %10s %12s %12s %10s %10s %12s\n", "VMs", "ms/it", "agg it/s", "fairness%", "VRAM MiB", "RAM MiB", "backend ms"
        for (s in stages) {
            if (s == "host") continue
            printf "  %-6s %10.2f %12.2f %12s %10s %10s %12s\n", s, \
                med(v[s SUBSEP "ms_per_it"]), med(v[s SUBSEP "agg_it_per_s"]), \
                (v[s SUBSEP "fairness_spread_pct"] == "" ? "-" : sprintf("%.1f", med(v[s SUBSEP "fairness_spread_pct"]))), \
                med(v[s SUBSEP "vram_max_mib"]), med(v[s SUBSEP "host_ram_max_mib"]), med(v[s SUBSEP "backend_cpu_ms"])
        }
    }' "$CSV" | (read -r h; echo "$h"; sort -n)
    echo
    echo "Correctness: distinct acc values over ALL stages/VMs (must be exactly 1):"
    awk -F, '$5 == "acc" {print "  " $6}' "$CSV" | sort -u
    echo
    echo "Failures:"
    awk -F, '$5 == "ok" && $6 != 1 {n++; print "  stage " $2 " rep " $3} END { if (!n) print "  none" }' "$CSV"
    } | tee "$out/summary.txt"
    log "done: $out"
}

# ============================================================================
# render
# ============================================================================
do_render() {
    local count=0 stages="" reps=3 size=1920x1080 out="" provonly=0
    while [[ $# -gt 0 ]]; do
        case $1 in
            --count)  count=$2; shift 2 ;;
            --stages) stages=$2; shift 2 ;;
            --reps)   reps=$2; shift 2 ;;
            --size)   size=$2; shift 2 ;;
            --out)    out=$2; shift 2 ;;
            --provision-only) provonly=1; shift ;;
            -h|--help) usage 0 ;;
            *) error "render: unknown option $1"; usage 2 ;;
        esac
    done
    : "${out:=$LEA_VM_DIR/bench-render-$(date +%Y%m%d-%H%M%S)}"
    mkdir -p "$out"
    lea_hold_pidfile "$LEA_VM_DIR/bench-render.pid"
    bench_rig_check "$out"
    # How many members are actually up? Asking beats assuming.
    local live i ip r
    live=$(lea_fleet_count)
    [[ $count -eq 0 ]] && count=$live
    [[ $live -ge 1 ]] || die "no fleet is running -- showcase.sh up --count N"
    [[ $count -le $live ]] || die "asked for $count members, $live are up"
    [[ -z $stages ]] && stages=$(seq 1 "$count" | tr '\n' ' ')
    info "fleet: $live up, using $count, stages: $stages"

    # Each member needs three things it does not have from the compute
    # image: a software X server (the GLX context; not in the measuring
    # path), glmark2, and NVIDIA's GL userspace. In parallel, because 197
    # MiB per member is the bulk of it.
    provision() {
        local i=$1 ip; ip=$(lea_ip "$i")
        lea_ssh "$ip" "command -v glmark2 >/dev/null && command -v Xvfb >/dev/null" 2>/dev/null \
            || lea_ssh "$ip" "sudo apt-get install -y -q xvfb glmark2-x11 >/dev/null 2>&1"
        lea_ssh "$ip" "test -f /opt/nvrm-gl/env.sh" 2>/dev/null || lea_gl_stage "vm$i" >/dev/null 2>&1
        lea_ssh "$ip" "pgrep -x Xvfb >/dev/null || (setsid nohup Xvfb :1 -screen 0 ${size}x24 \
            +extension GLX >/tmp/xvfb.log 2>&1 </dev/null &); sleep 2
            DISPLAY=:1 xdpyinfo >/dev/null 2>&1" 2>/dev/null
    }
    info "provisioning $count members (parallel)"
    for i in $(seq 0 $((count - 1))); do provision "$i" & done
    wait
    # The premise, checked per member and NOT assumed: which renderer will
    # draw. ASK THE PROGRAM THAT WILL MEASURE -- glmark2 prints its own
    # GL_RENDERER. And name what must NOT be there rather than a wanted
    # vendor string: under a VRAM cap the renderer reads "Leandro RTX
    # 2070-256M/PCIe/SSE2".
    local -A CAP=()
    : > "$out/caps.csv"
    for i in $(seq 0 $((count - 1))); do
        ip=$(lea_ip "$i")
        r=$(lea_ssh "$ip" ". /opt/nvrm-gl/env.sh; export DISPLAY=:1 __NV_PRIME_RENDER_OFFLOAD=1 __GLX_VENDOR_LIBRARY_NAME=nvidia
            timeout 60 glmark2 --off-screen --size 64x64 -b build:duration=1 2>&1 | sed -n 's/^ *GL_RENDERER: *//p' | head -1" 2>/dev/null)
        printf '  vm%-2s %-16s %s\n' "$i" "$ip" "${r:-<none>}"
        case ${r:-none} in
            *llvmpipe*|*softpipe*|*virgl*|*swrast*|*Mesa*|none) die "vm$i renders on '${r:-<none>}', not on the card -- refusing to measure" ;;
        esac
        # The cap, ASKED OF THE GUEST: nvidia-smi there reports the capped total.
        CAP[$i]=$(lea_ssh "$ip" "nvidia-smi --query-gpu=memory.total --format=csv,noheader" 2>/dev/null | tr -d ' ')
        echo "$i,${CAP[$i]:-?}" >> "$out/caps.csv"
    done
    [[ $provonly -eq 1 ]] && { info "provisioned only, as asked"; exit 0; }

    local CSV=$out/scores.csv
    echo "stage,rep,vm,score,renderer,vram_cap_mib" > "$CSV"
    one_run() {   # <vm index> <stage> <rep>
        local i=$1 st=$2 rep=$3 ip o score
        ip=$(lea_ip "$i")
        o=$(lea_ssh "$ip" ". /opt/nvrm-gl/env.sh
            export DISPLAY=:1 __NV_PRIME_RENDER_OFFLOAD=1 __GLX_VENDOR_LIBRARY_NAME=nvidia
            glmark2 --off-screen --size $size 2>&1" 2>/dev/null)
        echo "$o" > "$out/stage$st-rep$rep-vm$i.txt"
        score=$(sed -n 's/.*glmark2 Score: *\([0-9]*\).*/\1/p' <<<"$o" | tail -1)
        echo "$st,$rep,$i,${score:-NA},NVIDIA,${CAP[$i]:-?}" >> "$CSV"
        printf '    vm%-2s score %s\n' "$i" "${score:-FAILED}"
    }
    local st rep
    for st in $stages; do
        [[ $st -le $count ]] || { warn "stage $st > $count members -- skipped"; continue; }
        for rep in $(seq 1 "$reps"); do
            info "stage $st, rep $rep -- $st guest(s) rendering at once"
            # SIMULTANEOUSLY: started together and waited for together.
            for i in $(seq 0 $((st - 1))); do one_run "$i" "$st" "$rep" & done
            wait
        done
    done
    # The host-native reference, in THIS run when the host can: glmark2 and
    # a display to create the GLX context in. Otherwise the last measured
    # numbers are printed, dated.
    local host_ref="" hs
    if command -v glmark2 >/dev/null 2>&1 && [[ -n ${DISPLAY:-} ]]; then
        info "host-native reference ($reps x glmark2 --off-screen on this display)"
        for rep in $(seq 1 "$reps"); do
            hs=$(glmark2 --off-screen --size "$size" 2>&1 | sed -n 's/.*glmark2 Score: *\([0-9]*\).*/\1/p' | tail -1)
            host_ref="$host_ref ${hs:-NA}"
        done
        echo "host$host_ref" > "$out/host-native.txt"
    fi
    info "fleet left up (this never tears it down)"
    python3 - "$CSV" "$host_ref" <<'PY'
import csv, sys, collections, statistics
rows = list(csv.DictReader(open(sys.argv[1])))
host = [int(x) for x in sys.argv[2].split() if x.isdigit()]
ok = [r for r in rows if r["score"] not in ("NA", "")]
by = collections.defaultdict(list)
for r in ok:
    by[int(r["stage"])].append(int(r["score"]))
per_vm = collections.defaultdict(list)
for r in ok:
    per_vm[(int(r["stage"]), int(r["vm"]))].append(int(r["score"]))
caps = {int(r["vm"]): r["vram_cap_mib"] for r in ok}
print()
print("== glmark2 --off-screen, N guests on ONE card, simultaneously ==")
print(f"  {'guests':>6} {'runs':>5} {'median':>8} {'sum':>9} {'per guest':>10} {'spread p10/p90':>18}")
base = None
for st in sorted(by):
    v = sorted(by[st])
    med = statistics.median(v)
    total = med * st
    if base is None:
        base = med
    p10 = v[max(0, int(len(v) * 0.1))] if v else 0
    p90 = v[min(len(v) - 1, int(len(v) * 0.9))] if v else 0
    print(f"  {st:>6} {len(v):>5} {med:>8.0f} {total:>9.0f} {med/base*100:>9.0f}% {p10:>8.0f}/{p90:<8.0f}")
print()
print("  fairness within a stage -- min/max across guests, per stage:")
for st in sorted(by):
    meds = {vm: statistics.median(s) for (s2, vm), s in per_vm.items() if s2 == st}
    if len(meds) < 2:
        continue
    lo, hi = min(meds.values()), max(meds.values())
    detail = "  ".join(f"vm{vm}={m:.0f}(cap {caps.get(vm,'?')})" for vm, m in sorted(meds.items()))
    print(f"    {st} guests: spread {hi/lo:.2f}x   {detail}")
print()
if host:
    hm = statistics.median(host)
    one = statistics.median(by[1]) if 1 in by else None
    rel = f", one guest {one:.0f} ({one/hm*100:.1f} %)" if one else ""
    print(f"  reference, measured in this run: host native {hm:.0f}{rel}")
else:
    print("  reference (measured 2026-08-07): host native 21486, one guest 21277 (99.0 %)")
    print("  (no glmark2 or no DISPLAY on this host -- the reference was not re-measured)")
PY
    info "raw: $out/  csv: $CSV"
}

# ============================================================================
# stream
# ============================================================================

# SUNSHINE IS BORROWED, NOT OWNED. This measurement needs a Sunshine of its
# own -- its config decides the capture method and the encoder, which IS the
# measurement -- but a desktop brought up by `showcase.sh up --session gnome`
# already has one, started by lea_desktop_up inside the GNOME session and
# reading ~/.config/sunshine/sunshine.conf. That is the very file written
# below, and until 2026-08-18 the run ended with a bare `pkill -x sunshine`:
# the desktop was left with no Sunshine and a config it never wrote, and
# `showcase.sh demo --only desktop` then reported "Sunshine not listening"
# three scripts away from the cause.
#
# So the state is RECORDED before anything is touched and put back on the
# way out, on every exit path: whether one was running, how it was started
# (DISPLAY and XAUTHORITY out of its own /proc/<pid>/environ, its config
# argument out of its cmdline -- asked, not assumed), and the config file it
# read. Restarting it is the same line lea_desktop_up uses.
#
# NOT restored, deliberately: ~/.config/sunshine/creds.json and the pairing
# state. `--creds` here rewrites the web-UI login, and the pairing is shared
# on purpose (re-pairing for every measurement is four more moving parts).
# Both are Sunshine's own state, not the desktop's, and the demo does not
# read them.
_LEA_SUN_WAS_UP=0
_LEA_SUN_DISPLAY=""
_LEA_SUN_XAUTH=""
_LEA_SUN_CONF=""
_LEA_SUN_RESTORED=0

# lea_stream_sunshine_save <ip> <confdir>
lea_stream_sunshine_save() {
    local ip=$1 dir=$2 out line
    _LEA_SUN_WAS_UP=0; _LEA_SUN_DISPLAY=""; _LEA_SUN_XAUTH=""
    _LEA_SUN_CONF=""; _LEA_SUN_RESTORED=0
    out=$(lea_ssh "$ip" '
        p=$(pgrep -x sunshine | head -1)
        if [ -n "$p" ]; then
            echo up=1
            # `sudo cat`, not `sudo tr < file`: the redirect belongs to the
            # calling shell, so the second form reads the file unprivileged
            # and fails. Privileged it must be -- the Sunshine .deb sets file
            # capabilities (cap_sys_admin,cap_sys_nice on 2026.516.143833),
            # which clears the process dumpable flag, so /proc/<pid>/environ
            # belongs to root even for the user who started it.
            sudo cat /proc/$p/environ 2>/dev/null | tr "\0" "\n" \
                | sed -n "s/^DISPLAY=/display=/p; s/^XAUTHORITY=/xauth=/p"
            # sunshine takes its config file as a POSITIONAL argument; the
            # desktop path starts it without one, which means the default
            # ~/.config/sunshine/sunshine.conf. First non-option wins.
            tr "\0" "\n" < /proc/$p/cmdline 2>/dev/null | tail -n +2 \
                | grep -v "^-" | head -1 | sed "s|^|conf=|"
        else
            echo up=0
        fi
        if [ -f '"$dir"'/sunshine.conf ]; then
            cp -a '"$dir"'/sunshine.conf '"$dir"'/sunshine.conf.lea-bench-saved
        fi
        exit 0' 2>/dev/null)
    while IFS= read -r line; do
        case $line in
            up=1)      _LEA_SUN_WAS_UP=1 ;;
            display=*) _LEA_SUN_DISPLAY=${line#display=} ;;
            xauth=*)   _LEA_SUN_XAUTH=${line#xauth=} ;;
            conf=*)    _LEA_SUN_CONF=${line#conf=} ;;
        esac
    done <<<"$out"
    if [[ $_LEA_SUN_WAS_UP -eq 1 ]]; then
        info "sunshine: one is already running (DISPLAY=${_LEA_SUN_DISPLAY:-<unset>}${_LEA_SUN_CONF:+, conf $_LEA_SUN_CONF}) -- it is put back at the end"
    else
        info "sunshine: none running -- this run owns it and leaves it stopped"
    fi
}

# lea_stream_sunshine_restore <ip> <confdir> -- undo everything the run did
# to Sunshine. Idempotent: it is called explicitly at the end AND from the
# exit stack, because `die` in the middle is the case that broke the desktop.
lea_stream_sunshine_restore() {
    local ip=$1 dir=$2 genv="env"
    [[ ${_LEA_SUN_RESTORED:-0} -eq 1 ]] && return 0
    _LEA_SUN_RESTORED=1
    lea_ssh "$ip" "pkill -x sunshine; sleep 2
        if [ -f $dir/sunshine.conf.lea-bench-saved ]; then
            mv -f $dir/sunshine.conf.lea-bench-saved $dir/sunshine.conf
        else
            rm -f $dir/sunshine.conf
        fi" >/dev/null 2>&1
    [[ $_LEA_SUN_WAS_UP -eq 1 ]] || return 0
    [[ -n $_LEA_SUN_DISPLAY ]] && genv+=" DISPLAY=$_LEA_SUN_DISPLAY"
    [[ -n $_LEA_SUN_XAUTH ]]   && genv+=" XAUTHORITY=$_LEA_SUN_XAUTH"
    info "sunshine: restarting the one that was here before"
    lea_ssh "$ip" "sh -c 'setsid nohup $genv sunshine $_LEA_SUN_CONF >/tmp/lea-sunshine.out 2>&1 </dev/null &'
        for i in \$(seq 1 60); do
            (exec 3<>/dev/tcp/127.0.0.1/47989) 2>/dev/null && break
            sleep 1
        done
        pgrep -x sunshine >/dev/null || { echo 'sunshine did not come back'; exit 1; }
        ss -ltn | grep -qE ':(47984|47989|47990) ' || { echo 'sunshine is back but not listening'; exit 1; }" \
        || warn "Sunshine could not be put back -- the desktop has none now.
  Bring it back with: ./scripts/showcase.sh up --name <name> --index 5 --session gnome
  (or by hand: setsid nohup $genv sunshine $_LEA_SUN_CONF &)"
}

do_stream() {
    local capture=x11 encoder=nvenc session=desktop secs=60 bitrate=20000 res=1920x1080 fps=60
    local content=gears out="" ip="" name="" disp_override=""
    while [[ $# -gt 0 ]]; do
        case $1 in
            --capture)  capture=$2; shift 2 ;;
            --session)  session=$2; shift 2 ;;
            --encoder)  encoder=$2; shift 2 ;;
            --seconds)  secs=$2; shift 2 ;;
            --bitrate)  bitrate=$2; shift 2 ;;
            --res)      res=$2; shift 2 ;;
            --fps)      fps=$2; shift 2 ;;
            --content)  content=$2; shift 2 ;;
            --out)      out=$2; shift 2 ;;
            --ip)       ip=$2; shift 2 ;;
            --name)     name=$2; shift 2 ;;
            --display)  disp_override=$2; shift 2 ;;
            -h|--help)  usage 0 ;;
            *) error "stream: unknown option $1"; usage 2 ;;
        esac
    done
    if [[ -z $ip ]]; then lea_inst "${name:-desktop}"; ip=$INST_IP; fi
    # THE TWO CAPTURE METHODS, and why the choice is not free:
    #   x11     X11-SHM on the X root, frames through system memory
    #           (`cuda_ram_t`), no DRM FD -- NVENC works. Under a rootless
    #           Xwayland the root holds nothing; on the NVIDIA X (:7 in the
    #           measured desktop) it holds the desktop.
    #   portal  xdg-desktop-portal + pipewire. Real desktop content AND no
    #           DRM FD -- the pair that isolates the encoder. GNOME's own
    #           portal backend; a permission dialog nobody can click stops an
    #           unpatched Sunshine here (measured).
    # (wlr-export-dmabuf wanted a DRM FD for the CUDA device and belonged to
    # the sway-on-virtio-gpu arrangement, which is gone with crosvm.)
    case $capture in x11|portal) ;; *) die "--capture wants x11 or portal" ;; esac
    case $session in xvfb|desktop) ;; *) die "--session wants xvfb or desktop" ;; esac
    [[ $session == xvfb && $capture != x11 ]] && die "--session xvfb only makes sense with --capture x11"
    command -v moonlight >/dev/null || die "moonlight missing -- pacman -S moonlight-qt"
    : "${out:=$LEA_VM_DIR/bench-stream-$(date +%Y%m%d-%H%M%S)-$session-$capture-$encoder}"
    mkdir -p "$out"
    lea_hold_pidfile "$LEA_VM_DIR/bench-stream.pid"
    local SUN_USER=lea SUN_PASS=leastream SUN_PIN=4321
    # The state file is deliberately NOT per-run: pairing survives a
    # Sunshine restart, and re-pairing for every measurement is four more
    # moving parts.
    local SUN_DIR=/home/$LEA_GUEST_USER/.config/sunshine
    info "stream: capture=$capture encoder=$encoder ${res}@${fps} ${bitrate}kbps ${secs}s -> $out"
    # Before ANY guest state is touched, and undone on every exit path.
    lea_stream_sunshine_save "$ip" "$SUN_DIR"
    lea_on_exit "lea_stream_sunshine_restore $(printf '%q' "$ip") $(printf '%q' "$SUN_DIR")"

    # The Wayland socket has to be ABSENT, not empty, for the X11 path:
    # misc.cpp:1144 only looks at DISPLAY when WAYLAND_DISPLAY is unset.
    # :0 is gdm3's X (--session desktop with GNOME), :1 the Xvfb; --display
    # names the display gate's X on :7 -- a second truth about that number
    # here is exactly what the gate must not inherit.
    local gdisp=:0
    [[ $session == xvfb ]] && gdisp=:1
    [[ -n $disp_override ]] && gdisp=$disp_override
    guest_env() {
        if [[ $capture == x11 ]]; then
            echo "env -u WAYLAND_DISPLAY XDG_RUNTIME_DIR=/run/user/1000 DISPLAY=$gdisp"
        else
            # GNOME's socket is wayland-0 and its desktop name is GNOME -- the
            # portal backend is picked by XDG_CURRENT_DESKTOP.
            echo "env XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 DISPLAY=:0 XDG_CURRENT_DESKTOP=GNOME"
        fi
    }
    info "guest: $session + content"
    if [[ $session == desktop ]]; then
        # Take what is there. Starting a session under a session is how you
        # end up measuring two compositors.
        lea_ssh "$ip" "pgrep -x gnome-shell >/dev/null || pgrep -x Xorg >/dev/null" 2>/dev/null \
            || die "no desktop session in $ip -- showcase.sh up --name desktop --index 5 --display --session gnome"
        info "  using the session that is already running"
    else
        # A real X server, so the root window is the desktop, plus a window
        # manager, because Sunshine streams a desktop and an X server without
        # one shows a bare root. Xvfb has no DRI, so GL inside it is llvmpipe
        # unless offloaded (gears-nv).
        lea_ssh "$ip" "export XDG_RUNTIME_DIR=/run/user/1000
            pgrep -x Xvfb >/dev/null || (setsid nohup Xvfb $gdisp -screen 0 ${res}x24 +extension GLX +extension RANDR \
                >/tmp/xvfb.log 2>&1 </dev/null &)
            sleep 3
            DISPLAY=$gdisp xdpyinfo >/dev/null 2>&1 || { echo 'Xvfb did not answer'; exit 1; }
            pgrep -x openbox >/dev/null || (setsid nohup env DISPLAY=$gdisp openbox >/tmp/openbox.log 2>&1 </dev/null &)
            sleep 2" \
            || die "guest: no Xvfb (apt install xvfb x11-utils openbox)"
    fi
    if [[ $capture == portal ]]; then
        # GNOME brings its OWN portal backend and screencast implementation.
        # Nothing is restarted here: restarting a portal under a running
        # compositor is how you lose the session it belongs to.
        lea_ssh "$ip" "systemctl --user is-active xdg-desktop-portal pipewire | tr '\n' ' '" \
            || warn "portal or pipewire not active in the session"
    fi
    local gears_env=""
    # gears-nv: the same glxgears with NVIDIA PRIME render offload -- the
    # frames are drawn by the card instead of by llvmpipe. Same program,
    # same window, same capture path; only the renderer differs (measured:
    # 2,958 FPS on llvmpipe against 49,046 offloaded). The variables have to
    # reach glxgears itself; in Sunshine's environment they do nothing.
    [[ $content == gears-nv ]] && gears_env=". /opt/nvrm-gl/env.sh; export __NV_PRIME_RENDER_OFFLOAD=1 __GLX_VENDOR_LIBRARY_NAME=nvidia;"
    if [[ $content == gears || $content == gears-nv ]]; then
        lea_ssh "$ip" "$gears_env export XDG_RUNTIME_DIR=/run/user/1000 DISPLAY=$gdisp
            pgrep -x glxgears >/dev/null || (setsid nohup glxgears >/tmp/gears.log 2>&1 </dev/null &)
            sleep 3
            pgrep -x glxgears >/dev/null || { echo 'glxgears did not start'; exit 1; }" \
            || die "guest: no glxgears -- install mesa-utils, or use --content none"
    else
        # A leftover animation from an earlier run would be measured as if it
        # belonged to this one.
        lea_ssh "$ip" "pkill -x glxgears; sleep 1" >/dev/null 2>&1
    fi
    # Is there anything to capture? An encoder comparison on different
    # picture CONTENT is not one. Grab a still first and count -- and WHICH
    # renderer drew it, asked rather than assumed.
    local renderer="" nonblack=""
    if [[ $content == gears || $content == gears-nv ]]; then
        renderer=$(lea_ssh "$ip" "$gears_env export DISPLAY=$gdisp
            glxinfo -B 2>/dev/null | sed -n 's/^OpenGL renderer string: //p'" 2>/dev/null)
        info "guest: content rendered by ${renderer:-<unknown>}"
    fi
    if [[ $capture == x11 ]]; then
        # xwd, not `import -window root`: under a compositing manager the root
        # carries a full-screen guard window and import returns black for a
        # screen that has content. %w %h rather than %[fx:w*h], because every
        # large %[fx:] value comes back as "2.0736e+06".
        nonblack=$(lea_ssh "$ip" "export DISPLAY=$gdisp
            command -v xwd >/dev/null || exit 0
            xwd -root -silent 2>/dev/null | convert xwd:- -colorspace Gray -threshold 0 -format '%[fx:mean] %w %h' info: 2>/dev/null" 2>/dev/null \
            | awk '{printf "%d", $1 * $2 * $3}')
        if [[ -n $nonblack ]]; then
            info "guest: $nonblack non-black pixels on the X root"
            # 0.1 % of 1080p is ~2000 -- below that nothing recognisable is on screen.
            [[ $nonblack -lt 2000 ]] && warn "the captured root is (near) black -- this measures the ENCODER, not a stream"
        else
            info "guest: non-black pixel count unavailable (no imagemagick)"
        fi
    fi
    info "guest: sunshine ($capture/$encoder)"
    lea_ssh "$ip" "mkdir -p $SUN_DIR
        cat > $SUN_DIR/sunshine.conf <<EOC
min_log_level = 1
capture = $capture
encoder = $encoder
log_path = $SUN_DIR/sunshine.log
credentials_file = $SUN_DIR/creds.json
file_state = $SUN_DIR/state.json
EOC
        pkill -x sunshine; sleep 2
        : > $SUN_DIR/sunshine.log
        sunshine --creds $SUN_USER $SUN_PASS $SUN_DIR/sunshine.conf >/dev/null 2>&1
        $(guest_env) setsid nohup sunshine $SUN_DIR/sunshine.conf >$SUN_DIR/stdout.log 2>&1 </dev/null &
        # Not a fixed sleep: encoder probing takes ~1 s over x11 and ~20 s over
        # the portal, because every probe opens its own screencast session.
        for i in \$(seq 1 60); do
            (exec 3<>/dev/tcp/127.0.0.1/47989) 2>/dev/null && break
            sleep 1
        done
        pgrep -x sunshine >/dev/null || { echo 'sunshine did not start'; exit 1; }
        (exec 3<>/dev/tcp/127.0.0.1/47989) 2>/dev/null || { echo 'sunshine never opened 47989'; exit 1; }" \
        || die "guest: no sunshine"
    # What encoder did Sunshine settle on? The measurement's premise, read
    # back from Sunshine's own log rather than assumed.
    local found got
    found=$(lea_ssh "$ip" "grep -a 'Found H.264 encoder' $SUN_DIR/sunshine.log | tail -1")
    info "guest: ${found:-<no encoder line>}"
    case "$found" in *h264_nvenc*) got=nvenc ;; *libx264*) got=software ;; *) got=unknown ;; esac
    [[ $got == "$encoder" ]] || warn "asked for $encoder, Sunshine chose $got -- the run measures $got"
    [[ $got == unknown ]] && warn "no encoder line at all -- Sunshine may have died during probing"
    # Pairing: two pair attempts at once produce "Incorrect PIN" while the
    # API still says {"status":true}. One at a time, and only if needed.
    if ! timeout 25 moonlight list "$ip" >/dev/null 2>&1; then
        info "pairing"
        ( timeout 40 moonlight pair "$ip" --pin $SUN_PIN >"$out/pair.log" 2>&1 ) &
        local pairpid=$!
        sleep 4
        curl -sk -u "$SUN_USER:$SUN_PASS" -H 'Content-Type: application/json' \
            -d "{\"pin\":\"$SUN_PIN\",\"name\":\"host\"}" "https://$ip:47990/api/pin" >>"$out/pair.log" 2>&1
        wait $pairpid
        timeout 25 moonlight list "$ip" >/dev/null 2>&1 || die "pairing failed -- $out/pair.log"
    fi
    # Guest CPU over the session, /proc/stat before and after.
    local cpu0 cpu1 mlpid alive=1 vcpus
    vcpus=$(lea_ssh "$ip" nproc 2>/dev/null | tr -dc '0-9'); vcpus=${vcpus:-$LEA_CPUS}
    cpu0=$(lea_ssh "$ip" "grep '^cpu ' /proc/stat")
    info "streaming ${secs}s"
    ( timeout $((secs + 30)) moonlight stream "$ip" Desktop --resolution "$res" --fps "$fps" --bitrate "$bitrate" \
        --display-mode windowed --no-vsync --no-quit-after >"$out/moonlight.log" 2>&1 ) &
    mlpid=$!
    sleep "$secs"
    cpu1=$(lea_ssh "$ip" "grep '^cpu ' /proc/stat")
    # Quitting the app on the host side is what makes Moonlight print its
    # session summary. Killing the client skips it.
    timeout 25 moonlight quit "$ip" >>"$out/moonlight.log" 2>&1
    wait $mlpid 2>/dev/null
    # A Sunshine that dies during the stream leaves the encoder line standing
    # and the measurement looking valid. Ask the process, not the log.
    lea_ssh "$ip" "pgrep -x sunshine >/dev/null" 2>/dev/null || alive=0
    if [[ $alive -eq 0 ]]; then
        warn "Sunshine is GONE at the end of the session -- this run is not a measurement."
        lea_ssh "$ip" "tail -25 $SUN_DIR/sunshine.log" 2>/dev/null | sed 's/^/    /'
    fi
    lea_ssh "$ip" "grep -aiE 'fatal|error|couldn.t|failed' $SUN_DIR/sunshine.log | tail -8" > "$out/sunshine-complaints.txt" 2>/dev/null
    lea_ssh "$ip" "cat $SUN_DIR/sunshine.log" > "$out/sunshine.log" 2>/dev/null
    lea_stream_sunshine_restore "$ip" "$SUN_DIR"

    python3 - "$out" "$capture" "$got" "$bitrate" "$res" "$fps" "$secs" "$alive" "$session" "${nonblack:--1}" "${renderer:-unknown}" <<'PY'
import re, sys, json, pathlib
(out, capture, encoder, bitrate, res, fps, secs, alive, session, nonblack, renderer) = sys.argv[1:12]
log = pathlib.Path(out, "moonlight.log").read_text(errors="replace")
# Moonlight's summary block, one metric per line. Whatever is missing stays
# None -- a missing number is not a zero.
def grab(pattern):
    m = re.search(pattern, log)
    return m.groups() if m else None
stats = {
    "incoming_fps":   grab(r"Incoming frame rate from network: ([\d.]+) FPS"),
    "decoding_fps":   grab(r"Decoding frame rate: ([\d.]+) FPS"),
    "rendering_fps":  grab(r"Rendering frame rate: ([\d.]+) FPS"),
    "host_latency":   grab(r"Host processing latency min/max/average: ([\d.]+)/([\d.]+)/([\d.]+) ms"),
    "decode_time":    grab(r"Average decoding time: ([\d.]+) ms"),
    "net_latency":    grab(r"Average network latency: (\d+) ms"),
    "drop_network":   grab(r"Frames dropped by your network connection: ([\d.]+)%"),
    "drop_jitter":    grab(r"Frames dropped due to network jitter: ([\d.]+)%"),
}
res_json = {k: (list(v) if v else None) for k, v in stats.items()}
# A run is VALID only if the producer survived it and frames actually
# arrived. Without both, the table below is a row of dashes that reads
# like a result.
def first(key):
    v = stats[key]
    return float(v[0]) if v else None
incoming = first("incoming_fps")
valid = alive == "1" and incoming is not None and incoming > 0
res_json.update(capture=capture, encoder=encoder, bitrate=int(bitrate), resolution=res, fps=int(fps),
                seconds=int(secs), sunshine_alive_at_end=(alive == "1"), valid=valid, session=session,
                nonblack_pixels=int(nonblack), renderer=renderer)
pathlib.Path(out, "stats.json").write_text(json.dumps(res_json, indent=2))
def show(label, key, unit="", idx=0):
    v = stats[key]
    print(f"  {label:<34} {'—' if not v else v[idx] + unit}")
print()
print(f"== {session} / {capture} + {encoder}, {res}@{fps}, {bitrate} kbps, {secs}s ==")
print(f"  {'Content rendered by':<34} {renderer}")
if int(nonblack) >= 0:
    w, h = (int(x) for x in res.split("x"))
    print(f"  {'Non-black pixels on capture':<34} {nonblack} of {w * h}"
          + ("   <- an ENCODER number, not a stream" if int(nonblack) < 2000 else ""))
show("Incoming frame rate", "incoming_fps", " FPS")
show("Rendering frame rate", "rendering_fps", " FPS")
v = stats["host_latency"]
print(f"  {'Host processing latency min/max/avg':<34} " + ("—" if not v else "/".join(v) + " ms"))
show("Average decoding time", "decode_time", " ms")
show("Frames dropped, network", "drop_network", " %")
show("Frames dropped, jitter", "drop_jitter", " %")
if not valid:
    print()
    reason = ("Sunshine did not survive the session" if alive != "1"
              else "no frames arrived (Moonlight reported no incoming frame rate)")
    print(f"  RUN INVALID -- {reason}.")
    print(f"  Do not quote these numbers. {out}/sunshine-complaints.txt")
    sys.exit(3)
PY
    local valid=$?      # 3 = the run happened but is not a measurement
    # Guest CPU, printed after the table because it comes from the other side.
    LEA_CPUS=$vcpus python3 - "$cpu0" "$cpu1" "$out" <<'PY'
import sys, json, pathlib, os
a = [int(x) for x in sys.argv[1].split()[1:]]
b = [int(x) for x in sys.argv[2].split()[1:]]
d = [y - x for x, y in zip(a, b)]
total = sum(d)
idle = d[3] + d[4]          # idle + iowait
busy = (total - idle) / total if total else 0
cores = int(os.environ.get("LEA_CPUS", "4"))
print(f"  {'Guest CPU over the session':<34} {busy * 100:.1f} % of {cores} = {busy * cores:.2f} cores")
p = pathlib.Path(sys.argv[3], "stats.json")
s = json.loads(p.read_text())
s["guest_cpu_cores"] = round(busy * cores, 3)
s["guest_cpu_percent"] = round(busy * 100, 2)
p.write_text(json.dumps(s, indent=2))
PY
    echo
    info "raw: $out/moonlight.log  stats: $out/stats.json"
    # The exit code carries the verdict: a caller that only looks at $?
    # must not see 0 for a run that produced no frames.
    exit "$valid"
}

# ============================================================================
# diag
# ============================================================================
do_diag() {
    local out=""
    while [[ $# -gt 0 ]]; do
        case $1 in
            --out) out=$2; shift 2 ;;
            -h|--help) usage 0 ;;
            *) error "diag: unknown option $1"; usage 2 ;;
        esac
    done
    out=${out:-$LEA_VM_DIR/bench-diag-$(date +%Y%m%d-%H%M%S)}
    mkdir -p "$out"
    lea_hold_pidfile "$LEA_VM_DIR/bench-diag.pid"
    LOG=$out/run.log
    local foreign
    if foreign=$(lea_foreign_rigs "$NAME"); then
        die "instances of another rig are running: $(tr '\n' ' ' <<<"$foreign")"
    fi
    if foreign=$(lea_inst_owner "$NAME"); then die "$NAME is in use by pid $foreign -- not taking it over"; fi
    lea_require_built "$LEA_BIN_DIR/vhost-user-nvrm" || die "$LEA_BUILD_PROBLEM"
    trap 'log "aborted"; lea_rig_down "$NAME" >/dev/null 2>&1; exit 130' INT TERM
    log "bringing the rig up (backend with LEA_DEBUG=2 -- every ioctl in the log)"
    lea_rig_down "$NAME" >/dev/null 2>&1
    LEA_DEBUG=2 lea_rig_up "$NAME" --index 0 >>"$LOG" 2>&1 || { log "ERROR: the rig did not come up"; lea_rig_down "$NAME" >/dev/null 2>&1; exit 1; }
    lea_guest_tar "$NAME" "$LEA_ROOT/scripts/guest" '$HOME/gpu' bench-one.sh >>"$LOG" 2>&1 \
        && lea_ssh "$IP" 'chmod +x ~/gpu/bench-one.sh' || { log "ERROR: bench-one.sh"; lea_rig_down "$NAME" >/dev/null 2>&1; exit 1; }
    local BLOG=$LEA_VM_DIR/$NAME/nvrm.log pid
    pid=$(cat "$LEA_VM_DIR/$NAME/nvrm.pid")
    # Delimited by LINE NUMBERS, not by markers in the log: the backend holds
    # the file open and writes at its own offset -- whatever a second process
    # appends gets overwritten at its next word (measured: not one marker
    # survived). run_one SETS the values instead of printing them: a command
    # substitution would be a subshell, and its assignments would be gone.
    local -A FROM TO CPU
    run_one() {   # <load>
        local w=$1 c0 c1
        FROM[$w]=$(( $(wc -l < "$BLOG") + 1 ))
        c0=$(backend_cpu_ms "$pid")
        lea_ssh "$IP" "cd ~/gpu && ./bench-one.sh modul $w" >"$out/modul-$w.out" 2>&1
        c1=$(backend_cpu_ms "$pid")
        # The backend writes line by line and unbuffered; once the reply is
        # in, everything belonging to this run is already in the file.
        TO[$w]=$(wc -l < "$BLOG")
        CPU[$w]=$(( c1 - c0 ))
    }
    local w
    for w in cuinit ctx; do
        run_one "$w"
        log "modul/$w: backend CPU ${CPU[$w]} ms, $(grep -o 'VAL ok [01]' "$out/modul-$w.out" | head -1)"
    done
    cp "$BLOG" "$out/nvrm-backend.log"
    lea_rig_down "$NAME" >/dev/null 2>&1
    # The backend log carries one line per forwarded ioctl
    #   "vhost-user-nvrm: dev <t> nr <nr> cmd <c> ret <r> status <s>"
    # and one "MapPrepare -> window+..." line per window mapping.
    between() { [[ $3 -ge $2 ]] || return 0; sed -n "$2,$3p" "$1"; }
    {
    echo
    echo "What crosses the boundary? (one run per cell, LEA_DEBUG=2)"
    printf "%-8s %-8s %10s %10s %10s %12s\n" load variant ioctls windows RM_ALLOC backend-CPU
    local body
    for w in cuinit ctx; do
        body=$(between "$out/nvrm-backend.log" "${FROM[$w]}" "${TO[$w]}")
        printf "%-8s %-8s %10s %10s %10s %12s\n" "$w" modul \
            "$(grep -c "dev .* nr .* ret " <<<"$body")" "$(grep -c "MapPrepare\|MAP_BLOB" <<<"$body")" \
            "$(grep -c "nr 0x2b " <<<"$body")" "${CPU[$w]}"
    done
    echo
    echo "The most frequent escapes (cuinit):"
    between "$out/nvrm-backend.log" "${FROM[cuinit]}" "${TO[cuinit]}" \
        | grep -oP 'nr 0x[0-9a-f]+' | sort | uniq -c | sort -rn | head -6 | sed 's/^/    /'
    } | tee "$out/summary.txt"
    log "done: $out"
}

# ============================================================================
# vk -- vkmark on the virtual display
#
# WHAT THIS HAD TO GET PAST FIRST. Until 2026-08-18 it ran two scenes with
# `-p fifo` and the guest came back at exactly 60 FPS / 16.67 ms in both --
# which is the virtual display's refresh, not the cost of drawing anything.
# The comment blamed the display ("the only present mode it offers"). Half
# of that is true and the half that mattered is not:
#
#   mailbox    refused, and by the DEVICE, not the surface: "Selected present
#              mode Mailbox is not supported by the used Vulkan physical
#              device" (measured 2026-08-18 in the desktop guest).
#   immediate  works. Same guest, same X on :0, same scene: fifo gives 60 FPS
#              / 16.670 ms, immediate 3707 FPS / 0.270 ms.
#
# So the cap was a present mode, not a limit -- and no vkmark scene gets
# anywhere near it on this card. At 1920x1080 on an RTX 2070 the heaviest
# scene in the set costs 0.374 ms in the guest and 0.211 ms on the host; a
# frame time above 16.7 ms would need a workload roughly fifty times heavier
# than vkmark has. Both modes are therefore worth running and they answer
# different questions:
#
#   fifo       does the virtual display actually hold its 60 Hz? The number
#              to look for is 16.67 ms, and anything above it is a miss.
#   immediate  what does a frame COST over this boundary? Nothing paces the
#              swapchain, so the frame time is render plus present, and the
#              guest/host ratio is the price of the crossing.
#
# FrameTime is the primary column for the same reason: FPS under fifo is a
# property of the display, frame time under immediate is a property of the
# path.
# ============================================================================
do_vk() {
    local name=desktop res=$LEA_VDISPLAY_SIZE out="" disp=:7 present=both secs=5 scenes=""
    while [[ $# -gt 0 ]]; do
        case $1 in
            --name)    name=$2; shift 2 ;;
            --res)     res=$2; shift 2 ;;
            --out)     out=$2; shift 2 ;;
            --display) disp=$2; shift 2 ;;
            --present) present=$2; shift 2 ;;
            --seconds) secs=$2; shift 2 ;;
            --scenes)  scenes=$2; shift 2 ;;
            -h|--help) usage 0 ;;
            *) error "vk: unknown option $1"; usage 2 ;;
        esac
    done
    case $present in immediate|fifo|both) ;; *) die "--present wants immediate, fifo or both" ;; esac
    # Light to heavy, and the two `desktop` rows differ only in how much they
    # ask of the compositor path -- which is where the boundary sits.
    local -a SCENES
    if [[ -n $scenes ]]; then
        read -r -a SCENES <<<"$scenes"
    else
        SCENES=(
            clear
            shading:shading=phong
            texture
            "effect2d:kernel=blur:background-resolution=1920x1080"
            "desktop:windows=4:background-resolution=1920x1080"
            "desktop:windows=16:window-size=0.5:background-resolution=1920x1080"
        )
    fi
    out=${out:-$LEA_VM_DIR/bench-vk-$(date +%Y%m%d-%H%M%S)}
    mkdir -p "$out"
    lea_hold_pidfile "$LEA_VM_DIR/bench-vk.pid"
    bench_rig_check "$out"
    lea_inst "$name"; local ip=$INST_IP
    lea_vm_running "$name" || die "$name is not running -- showcase.sh up --name $name --index 5 --display"
    lea_ssh "$ip" 'command -v vkmark >/dev/null' 2>/dev/null || die "no vkmark in $name (build.sh bake --with-desktop installs it)"
    # An X on the virtual display, brought up if absent -- the same server the
    # display gate measures on. On a session display (:0 under gdm3) it is
    # already there and --res only sizes the vkmark window.
    lea_ssh "$ip" "DISPLAY=$disp xrandr >/dev/null 2>&1" 2>/dev/null \
        || lea_display_up "$name" --display "$disp" --res "$res" >"$out/display.log" 2>&1 \
        || die "the display rig did not come up -- see $out/display.log"

    # The scene arguments, once, so guest and host run the SAME string.
    local -a ARGS=(); local sc
    for sc in "${SCENES[@]}"; do ARGS+=(-b "$sc:duration=$secs"); done

    local -a MODES=()
    case $present in both) MODES=(fifo immediate) ;; *) MODES=("$present") ;; esac

    local mode
    for mode in "${MODES[@]}"; do
        info "vkmark in $name on $disp at $res, present=$mode, ${#SCENES[@]} scenes x ${secs}s"
        # On gdm3's :0 the session's Xauthority is needed; the rig X on :7 has
        # none. Read it from gnome-shell's environment the way the desktop
        # bring-up does, and leave it empty otherwise. `sudo cat`, because a
        # process with file capabilities (Sunshine is one) hides its environ --
        # gnome-shell does not, but the form costs nothing and never breaks.
        lea_ssh "$ip" "GS=\$(pgrep -x gnome-shell | head -1)
            [ -n \"\$GS\" ] && export XAUTHORITY=\$(tr '\\0' '\\n' < /proc/\$GS/environ | sed -n 's/^XAUTHORITY=//p')
            export DISPLAY=$disp
            timeout 900 vkmark --winsys xcb -p $mode -s $res $(printf '%q ' "${ARGS[@]}") 2>&1 | grep -E '^\[|Score|Error'" \
            > "$out/guest-$mode.txt" 2>&1
        sed 's/^/    guest: /' "$out/guest-$mode.txt"
        if command -v vkmark >/dev/null 2>&1 && [[ -n ${DISPLAY:-} ]]; then
            info "vkmark on the host, same scenes, same resolution, same present mode"
            timeout 900 vkmark --winsys xcb -p "$mode" -s "$res" "${ARGS[@]}" 2>&1 \
                | grep -E '^\[|Score|Error' > "$out/host-$mode.txt"
            sed 's/^/    host:  /' "$out/host-$mode.txt"
        else
            info "no vkmark (or no DISPLAY) on the host -- no reference in this run"
        fi
    done

    python3 - "$out" "$res" "${MODES[@]}" <<'PY'
import re, sys, pathlib
out, res = pathlib.Path(sys.argv[1]), sys.argv[2]
modes = sys.argv[3:]

# "[desktop] background-resolution=1920x1080:windows=4:duration=5: FPS: 3481 FrameTime: 0.287 ms"
LINE = re.compile(r"^\[(\w+)\]\s*(.*?):\s*FPS:\s*(\d+)\s*FrameTime:\s*([\d.]+)\s*ms")

def parse(p):
    d = {}
    if not p.exists():
        return d
    for line in p.read_text().splitlines():
        m = LINE.match(line)
        if not m:
            continue
        # duration is a knob of the harness, not of the scene -- two runs at
        # different --seconds must land on the same row.
        opts = ":".join(o for o in m[2].split(":") if o and not o.startswith("duration="))
        d[f"{m[1]}{':' + opts if opts else ''}"] = (int(m[3]), float(m[4]))
    return d

CAP = 1000.0 / 60.0          # the virtual display's frame budget at 60 Hz
tables = [(m, parse(out / f"guest-{m}.txt"), parse(out / f"host-{m}.txt")) for m in modes]
# One column width for every table, taken from the longest label actually
# printed: a fixed width silently breaks the alignment of exactly the heavy
# scenes this exists to show.
w = max([len(k) for _, g, h in tables for k in set(g) | set(h)] + [len("scene")])
print()
print(f"== vkmark, guest on the virtual display vs host, {res} ==")
print(f"   frame time in ms (primary), FPS beside it; 60 Hz = {CAP:.2f} ms")
for mode, g, h in tables:
    if not g and not h:
        continue
    print()
    print(f"  -- present={mode} --")
    print(f"  {'scene':<{w}} {'guest ms':>9} {'FPS':>6}   {'host ms':>8} {'FPS':>6}   {'g/h':>6}")
    for scene in sorted(set(g) | set(h)):
        gv, hv = g.get(scene), h.get(scene)
        gs = f"{gv[1]:>9.3f} {gv[0]:>6}" if gv else f"{'—':>9} {'—':>6}"
        hs = f"{hv[1]:>8.3f} {hv[0]:>6}" if hv else f"{'—':>8} {'—':>6}"
        rel = f"{gv[1] / hv[1]:>6.2f}" if gv and hv and hv[1] else f"{'—':>6}"
        mark = ""
        if gv and mode == "fifo":
            mark = "  at the cap" if abs(gv[1] - CAP) < 0.5 else "  OFF the cap"
        elif gv and gv[1] > CAP:
            mark = "  above the 60 Hz budget"
        print(f"  {scene:<{w}} {gs}   {hs}   {rel}{mark}")
print()
print("  Measured 2026-08-18 on the development rig (RTX 2070, desktop guest on")
print("  :0 at 1920x1080, the six scenes above, two runs): fifo pins every scene")
print("  at 16.39-16.67 ms in the guest, so the virtual display holds its 60 Hz.")
print("  Under immediate the same scenes cost 0.262-0.375 ms in the guest against")
print("  0.107-0.229 ms on the host -- the crossing multiplies the frame time by")
print("  1.6 to 2.6. No vkmark scene comes near the 16.67 ms budget on this card:")
print("  a frame time above it would need a workload some fifty times heavier.")
print("  A number far outside those bands is a change, not noise.")
print()
print("  And the cap is a property of what PACES the presentation, not of the")
print("  display: on the bare rig X on :7 (no compositor, no session) the same")
print("  fifo pass came back with clear at 16.393 ms and the other five at")
print("  exactly 10.000 ms / 100 FPS -- reproduced twice on 2026-08-18. That is")
print("  what the 'OFF the cap' marker is for; it is a finding, not a fault.")
print("  Read the fifo rows as a display check (16.67 ms means the guest holds")
print("  its 60 Hz) and the immediate rows as the cost of a frame over the")
print("  boundary -- there the g/h column is what the crossing adds.")
print("  The HOST fifo rows are not a 60 Hz check and g/h is meaningless in")
print("  that block: the operator's compositor need not pace vkmark to the")
print("  monitor at all (measured 2026-08-18: 223-256 FPS on a 143 Hz screen),")
print("  so it compares a paced guest against an unpaced host.")
PY
    info "raw: $out/"
}

# ---- slurm ------------------------------------------------------------------
# The cluster harness: generate one job per measurement, run one cell, and
# fan the answers back in.
#
#   scripts/bench.sh slurm [--package DIR] [--out DIR] [--counts "1 2 4"]
#                          [--cells "gate fleet"] [--partition P] [--gres G]
#                          [--account A] [--time HH:MM:SS] [--mem MiB]
#                          [--cpus N] [--submit]
#   scripts/bench.sh slurm --run CELL [--package DIR] [--out DIR]
#   scripts/bench.sh slurm --collect DIR
#
# A CELL is one measurement in one allocation: `gate-<N>` runs the compute
# gate with N guests up, `fleet-<N>` runs the fleet bench up to N. One
# allocation runs one cell, because two cells in one allocation share a GPU
# and neither number means anything afterwards.
#
# IT RUNS WITHOUT SLURM. `--run` is the whole job body and takes no SLURM
# variable it cannot default; that is how this was developed and how it is
# tested, and it is also what an operator should do once by hand before
# queueing a hundred of them.
#
# ---- the four things that are different about a cluster ----------------------
#
# (a) NODE-LOCAL SCRATCH, NOT THE SHARED FILESYSTEM. LEA_VM_DIR holds the
#     qcow2 images and every instance's disk, and those are written with
#     O_DIRECT-ish patterns and held open for the life of a VM. On NFS that is
#     slow; on Lustre or GPFS it is slow AND the file locking a qcow2 wants is
#     either unavailable or a distributed-lock storm. So the job copies the
#     image to $SLURM_TMPDIR (or $TMPDIR) and runs entirely there, and only
#     the RESULTS -- the JSON line, the logs, the rig line -- go back to the
#     shared path. The shared path is for what you keep; the node-local one is
#     for what the run needs.
#
# (b) TWO JOBS ON ONE NODE MUST NOT COLLIDE, and one variable does it.
#     Everything an instance owns hangs off LEA_VM_DIR: the instance
#     directories, the pidfiles (lea_hold_pidfile writes $LEA_VM_DIR/*.pid),
#     the SSH key, the backend sockets, and the vsock endpoint
#     (lea_vsock_sock). Namespacing LEA_VM_DIR by job therefore namespaces
#     all of it at once. The guest CID needs no namespacing at all --
#     measured 2026-08-19, cloud-hypervisor implements vsock in userspace and
#     never opens /dev/vhost-vsock, so there is no host-wide CID space for two
#     jobs to collide in; the socket PATH is the address, and that is already
#     under LEA_VM_DIR.
#
# (c) A FAILED JOB LEAVES FILES, NOT A VM. `--keep` is meaningless here: the
#     allocation ends and the node is reclaimed with everything on it. You get
#     one shot per queue wait, so on ANY failure this collects the out
#     directory, the backend log, the guest serial log, the guest's dmesg if
#     it still answers, and the rig line into ONE tarball on the shared path,
#     before the node takes them away.
#
# (d) THE AGGREGATOR REFUSES INVALID COMPARISONS. `--collect` groups the runs
#     by rig line -- driver, persistence, governor, PCIe width, GPU model --
#     and reports each group separately. Two groups are NOT averaged and not
#     silently concatenated: persistence alone once moved the native reference
#     by 58 % on this project's own hardware, and a table that mixes sites
#     without saying so is worse than no table.
#
# MIG IS REFUSED, and this is the open question rather than an answer: with
# the GPU in MIG mode the RM object surface is different territory -- distinct
# device handles, a partitioned instance tree, and none of it exercised here.
# Nothing in this project has been measured against it, so a MIG node is
# refused with that sentence rather than producing numbers nobody can
# interpret.
do_slurm() {
    local pkg="" out="" counts="1 2 4" cells="gate fleet" run="" collect=""
    local partition="" gres="gpu:1" account="" walltime="" mem="" cpus="" submit=0
    while [[ $# -gt 0 ]]; do
        case $1 in
            --package)   pkg=$(readlink -m "$2"); shift 2 ;;
            --out)       out=$(readlink -m "$2"); shift 2 ;;
            --counts)    counts=$2; shift 2 ;;
            --cells)     cells=$2; shift 2 ;;
            --run)       run=$2; shift 2 ;;
            --collect)   collect=$(readlink -m "$2"); shift 2 ;;
            --partition) partition=$2; shift 2 ;;
            --gres)      gres=$2; shift 2 ;;
            --account)   account=$2; shift 2 ;;
            --time)      walltime=$2; shift 2 ;;
            --mem)       mem=$2; shift 2 ;;
            --cpus)      cpus=$2; shift 2 ;;
            --submit)    submit=1; shift ;;
            -h|--help)   usage 0 ;;
            *) error "slurm: unknown option $1"; usage 2 ;;
        esac
    done
    [[ -n $collect ]] && { _lea_slurm_collect "$collect"; return $?; }
    [[ -n $run ]] && { _lea_slurm_run "$run" "$pkg" "$out"; return $?; }
    _lea_slurm_generate "$out" "$pkg" "$counts" "$cells" \
        "$partition" "$gres" "$account" "$walltime" "$mem" "$cpus" "$submit"
}

# _lea_slurm_budget CELL -> "<minutes> <mem MiB> <cpus>"
#
# MEASURED, not guessed (2026-08-19, this host, NixOS guest over vsock): a
# guest boots to SSH in about 20 s, provisioning stages 494 MiB in under 2 s,
# and the compute gate itself takes 100 s wall. A fleet cell pays the boot per
# member and then runs three stages. The generous part is the walltime: a
# queue that kills a job at the median finishing time wastes the whole queue
# wait, so these are roughly 3x the measured figure and still minutes.
_lea_slurm_budget() {
    local kind=${1%%-*} n=${1##*-} minutes mem cpus
    case $kind in
        gate)  minutes=$((15 + 5 * n)) ;;
        fleet) minutes=$((20 + 10 * n)) ;;
        *)     minutes=30 ;;
    esac
    # LEA_MEM per guest plus room for the host side and the page cache the
    # qcow2 overlays keep hot.
    mem=$((n * LEA_MEM + 4096))
    cpus=$((n * LEA_CPUS + 2))
    echo "$minutes $mem $cpus"
}

# _lea_slurm_cells COUNTS CELLS -> one cell name per line
_lea_slurm_cells() {
    local counts=$1 cells=$2 c n
    for c in $cells; do
        case $c in
            gate|fleet) ;;
            *) die "unknown cell kind '$c' -- gate or fleet" ;;
        esac
        for n in $counts; do
            [[ $n =~ ^[0-9]+$ && $n -ge 1 && $n -le $LEA_MAX_VMS ]] \
                || die "--counts wants numbers 1..$LEA_MAX_VMS, not '$n'"
            # The compute gate measures ONE guest by construction: it is the
            # correctness gate, and its stages address a single instance. So
            # gate cells exist once, not once per count.
            [[ $c == gate && $n -ne 1 ]] && continue
            echo "$c-$n"
        done
    done
}

# _lea_slurm_preflight -- what this node has to be before a number means
# anything. Every failure names the fix; none of them is a warning.
_lea_slurm_preflight() {   # <package dir>
    local pkg=$1 want have mig
    [[ -d $pkg ]] || { error "no package at $pkg -- build one: scripts/build.sh package"; return 1; }
    [[ -f $pkg/MANIFEST ]] || { error "$pkg has no MANIFEST -- it is not a Leandro package"; return 1; }
    [[ -e /dev/kvm ]] || { error "no /dev/kvm on this node.
       The site has to expose it to batch jobs; there is no way around it and
       no software substitute. Ask for KVM, not for nested virtualisation --
       this needs nothing beyond plain KVM."; return 1; }
    [[ -w /dev/kvm ]] || { error "/dev/kvm exists but is not writable by $(id -un).
       The site exposes it to the node but not to the job's user; the usual
       fix is membership of the kvm group in the job's container/cgroup."; return 1; }
    [[ -e /dev/nvidiactl ]] || { error "no /dev/nvidiactl on this node.
       Request a GPU through GRES (--gres=gpu:1); a job without one sees no
       NVIDIA device."; return 1; }

    # THE DRIVER, and this is the one that silently produces wrong numbers
    # rather than an error. vhost-user-nvrm is compiled against ONE driver
    # version -- assert_layout! in crates/nvrm-abi checks every struct offset
    # at COMPILE time, so a binary built for another version does not exist;
    # what exists is a binary whose offsets do not match this node's driver,
    # and the ioctls then succeed with misread fields.
    want=$(sed -n 's/^driver: *//p' "$pkg/MANIFEST" | tr -d '[:space:]')
    have=$(lea_driver_version); have=${have:-none}
    if [[ $have != "$want" ]]; then
        error "this node runs NVIDIA driver '$have'; the package was built for '$want'.
       These must match exactly -- the guest is handed the HOST's libcuda and
       the host backend is compiled against that version's struct layout.
       Fix, on a machine that has the sources and a toolchain:
         scripts/build.sh all --driver $have    # retarget the tree
         scripts/build.sh bake --nixos --with-torch
         scripts/build.sh package --out <pkg-$have>
       and point --package at that one. ONE PACKAGE IS ONE DRIVER VERSION:
       there is no variant selection here and there is deliberately none, on
       a project that targets exactly one version at a time.
       NOTE on where the driver comes from: datacenter/Tesla drivers live
       under a different NVIDIA download path than the desktop ones, and
       vGPU/GRID drivers are not on the public CDN at all -- for those the
       userspace must come off the node itself, which is what the payload
       path already does (lea_payload_stage)."
        return 1
    fi

    # MIG. Refused, and said as an open question rather than a limitation
    # somebody forgot to lift.
    mig=$(nvidia-smi --query-gpu=mig.mode.current --format=csv,noheader 2>/dev/null | head -1 | tr -d '[:space:]')
    if [[ $mig == Enabled ]]; then
        error "this GPU is in MIG mode, and this project has never been measured against it.
       MIG partitions the RM object surface -- different device handles, a
       partitioned instance tree -- and none of that is exercised anywhere in
       this tree. Refusing rather than producing numbers nobody can interpret.
       Run on a node whose GPU is not partitioned, or open the question
       properly (docs/OPEN-QUESTIONS.md)."
        return 1
    fi
    return 0
}

# _lea_slurm_scratch -- the node-local directory this run lives in, namespaced
# per job so two allocations on one node cannot meet. See (a) and (b) above.
_lea_slurm_scratch() {
    local base=${SLURM_TMPDIR:-${TMPDIR:-/tmp}} id
    id=${SLURM_JOB_ID:-local$$}
    [[ -n ${SLURM_ARRAY_TASK_ID:-} ]] && id="$id.$SLURM_ARRAY_TASK_ID"
    # SHORT, deliberately: this holds the vsock sockets, and AF_UNIX sun_path
    # is 108 bytes. A cluster $SLURM_TMPDIR is already deep, so nothing is
    # added below it beyond one component.
    echo "$base/lea-$id"
}

# _lea_slurm_forensics CELL SCRATCH OUT RC -- trap (c). One tarball, on the
# shared path, before the node takes the evidence away.
_lea_slurm_forensics() {
    local cell=$1 scratch=$2 out=$3 rc=$4 tmp n ip
    tmp=$scratch/forensics
    rm -rf "$tmp"; mkdir -p "$tmp"
    {
        echo "cell:      $cell"
        echo "exit:      $rc"
        echo "when:      $(date -Is)"
        echo "host:      $(uname -n)"
        echo "job:       ${SLURM_JOB_ID:-<none>}${SLURM_ARRAY_TASK_ID:+.$SLURM_ARRAY_TASK_ID}"
        echo "scratch:   $scratch"
        echo "kvm:       $([[ -w /dev/kvm ]] && echo writable || echo NO)"
        echo
        echo "# rig"
        lea_rig_state 2>&1
        echo
        echo "# nvidia-smi"
        nvidia-smi 2>&1 | head -20
        echo
        echo "# free / df"
        free -m 2>&1 | head -3
        df -h "$scratch" 2>&1 | tail -2
    } > "$tmp/report.txt" 2>&1
    # Everything each instance left behind, and the guest's own dmesg while it
    # can still be asked for -- after the allocation ends there is nobody to
    # ask.
    for n in $(lea_inst_list 2>/dev/null); do
        mkdir -p "$tmp/$n"
        cp -a "$LEA_VM_DIR/$n"/*.log "$tmp/$n/" 2>/dev/null || true
        cp -a "$LEA_VM_DIR/$n"/{index,guest,transport} "$tmp/$n/" 2>/dev/null || true
        if lea_vm_running "$n"; then
            ip=$(lea_ip "$(lea_guest_idx "$n")")
            timeout 30 lea_ssh "$ip" 'sudo dmesg | tail -200' > "$tmp/$n/guest-dmesg.txt" 2>&1 || true
            timeout 30 lea_ssh "$ip" 'lsmod; ls -l /dev/nvidia*' > "$tmp/$n/guest-state.txt" 2>&1 || true
        fi
    done
    cp -a "$LEA_VM_DIR/out-gpu" "$tmp/out-gpu" 2>/dev/null || true
    cp -a "$scratch"/bench-fleet-* "$tmp/" 2>/dev/null || true
    cp -a "$out/$cell" "$tmp/cell-out" 2>/dev/null || true
    mkdir -p "$out"
    local tarball=$out/$cell.forensics.tar.gz
    tar -C "$tmp" -czf "$tarball" . 2>/dev/null \
        && error "forensics: $tarball ($(du -h "$tarball" | cut -f1))" \
        || error "forensics: could not write $tarball"
    rm -rf "$tmp"
}

# _lea_slurm_run CELL PACKAGE OUT -- the whole job body, SLURM or not.
_lea_slurm_run() {
    local cell=$1 pkg=$2 out=$3
    local kind=${cell%%-*} n=${cell##*-}
    case $kind in gate|fleet) ;; *) die "unknown cell '$cell' -- gate-N or fleet-N" ;; esac
    [[ $n =~ ^[0-9]+$ ]] || die "unknown cell '$cell' -- gate-N or fleet-N"
    [[ -n $pkg ]] || pkg=${LEA_PACKAGE:-}
    [[ -n $pkg ]] || die "--package DIR (or LEA_PACKAGE) -- the cell runs out of a package"
    [[ -n $out ]] || out=$PWD/leandro-results
    mkdir -p "$out"

    # The host's driver libraries, when the caller bound them in. Set here as
    # well as in the generated job so that `--run` by hand behaves the same.
    [[ -z ${LEA_NVIDIA_LIB_DIR:-} && -d /opt/host-nvidia ]] && export LEA_NVIDIA_LIB_DIR=/opt/host-nvidia
    # The native reference the gate's torch stage measures against, carried by
    # the package because a node has no checkout to find one in.
    [[ -d $pkg/hostvenv ]] && export LEA_HOSTVENV=$pkg/hostvenv
    # THE WHEELS' C++/OPENMP RUNTIME, on the host side this time. The gate's
    # torch stage runs the reference venv's python, and a manylinux wheel
    # expects a system libstdc++ that a pure Nix image has on no default path
    # -- the same fact the guest image answers with /opt/nvrm/wheel-runtime.
    # Appended, not prepended: --nv put the driver's libraries first and they
    # must stay first.
    # gcc-lib for libstdc++/libgcc_s/libgomp (torch's _C.so) and zlib for
    # libz (numpy). Found 2026-08-19 by running the gate in the image and
    # reading off what failed, the same way the guest's list was found.
    local _g
    for _g in /nix/store/*-gcc-*-lib/lib /nix/store/*-zlib-*/lib; do
        [[ -d $_g ]] && export LD_LIBRARY_PATH="${LD_LIBRARY_PATH:+$LD_LIBRARY_PATH:}$_g"
    done
    # A CLUSTER NODE'S GPU IS NOT THIS JOB'S TO CONFIGURE. lea_rig_check
    # refuses to measure when persistence mode is off, and on a workstation
    # that is right -- it moved this project's native reference by 58 %. On a
    # batch node the job cannot run `nvidia-smi -pm 1` and has no business
    # trying, so the state becomes a RECORDED FACT instead of a refusal: it is
    # in the rig line beside every number, and --collect refuses to compare
    # runs whose rig lines differ. Nothing is hidden; the decision about
    # comparability just moves to where it can be made.
    export LEA_RIG_UNMANAGED=1
    # OpenSSH runs a ProxyCommand through $SHELL. Inside a container $SHELL is
    # whatever the submitting login had -- /bin/bash, /usr/bin/zsh, a path
    # that need not exist in the image -- and the vsock transport IS a
    # ProxyCommand. The image provides /bin/sh and /bin/bash; naming /bin/sh
    # here covers the rest, and outside a container it is the shell ssh would
    # have used anyway.
    export SHELL=/bin/sh
    _lea_slurm_preflight "$pkg" || return 2

    # (a) NODE-LOCAL. Everything below here is on the node's own disk.
    local scratch; scratch=$(_lea_slurm_scratch)
    export LEA_VM_DIR=$scratch
    export LEA_NIXOS_DIR=$scratch/guest
    export LEA_PROBE_BIN=$pkg/probe-bin
    mkdir -p "$LEA_VM_DIR" "$LEA_NIXOS_DIR" || {
        error "cannot create the node-local scratch $scratch.
       Inside a container that usually means the directory was never bound
       in: the job has to --bind the very path it resolves \$SLURM_TMPDIR /
       \$TMPDIR to, or apptainer auto-creates it read-only."
        return 2
    }
    info "cell $cell: scratch $scratch, results $out"

    # The image is COPIED, not linked: the instance overlays hang off it for
    # the life of the run and a backing file on the shared filesystem is
    # exactly what (a) is about.
    # ENOUGH ROOM ON THAT SCRATCH, asked before the copy rather than
    # discovered halfway through it. The guest image is measured in gigabytes
    # and $TMPDIR is very often a tmpfs -- on this development host /tmp is a
    # 16 GiB tmpfs with 11 GiB free against a 12 GiB image, and filling a
    # tmpfs does not fail a copy, it eats the machine's RAM. Wanted: the
    # image plus room for the instance overlays the run is about to create.
    local need avail
    need=$(du -sLk "$pkg/guest" 2>/dev/null | cut -f1)
    need=$(( ${need:-0} + n * 2 * 1024 * 1024 ))
    avail=$(df -Pk "$scratch" | awk 'NR==2{print $4}')
    if [[ ${avail:-0} -lt $need ]]; then
        error "not enough room on the node-local scratch.
       $scratch has $((avail / 1024 / 1024)) GiB free; this cell needs about
       $((need / 1024 / 1024)) GiB (the guest image plus $n instance overlay(s)).
       That directory comes from \$SLURM_TMPDIR, else \$TMPDIR, else /tmp --
       and on many machines /tmp is a tmpfs, i.e. RAM. Point one of them at
       real disk:  TMPDIR=/big/disk/scratch  (or have the site set
       SLURM_TMPDIR / the job_container/tmpfs plugin)."
        return 2
    fi
    local f t0 t1
    t0=$(date +%s)
    for f in kernel initrd rootfs.qcow2 image.env rootfs.manifest; do
        [[ -f $pkg/guest/$f ]] || continue
        cp -L "$pkg/guest/$f" "$LEA_NIXOS_DIR/$f" || die "staging $f to $scratch failed"
    done
    chmod u+w "$LEA_NIXOS_DIR"/* 2>/dev/null || true
    t1=$(date +%s)
    info "  staged the guest image to node-local scratch in $((t1 - t0))s"

    lea_hold_pidfile "$LEA_VM_DIR/slurm-$cell.pid"
    local rc=0
    case $kind in
        gate)
            "$LEA_ROOT/scripts/test.sh" gpu --instance vm0 --index 0 \
                --guest nixos --transport vsock > "$out/$cell.json" 2> "$out/$cell.log" || rc=$?
            ;;
        fleet)
            "$LEA_ROOT/scripts/bench.sh" fleet --guest nixos --transport vsock \
                --stages "$(_lea_slurm_stages "$n")" --reps "${LEA_SLURM_REPS:-1}" \
                --out "$out/$cell.bench" > "$out/$cell.log" 2>&1 || rc=$?
            _lea_slurm_fleet_json "$cell" "$out" "$rc" > "$out/$cell.json"
            ;;
    esac
    # The rig line beside every measurement -- (d) has nothing to group by
    # without it.
    lea_rig_state > "$out/$cell.rig" 2>/dev/null || true
    {
        echo "host $(uname -n)"
        echo "job ${SLURM_JOB_ID:-local}${SLURM_ARRAY_TASK_ID:+.$SLURM_ARRAY_TASK_ID}"
        echo "package $pkg"
        echo "cell $cell"
    } >> "$out/$cell.rig"

    if [[ $rc -ne 0 ]]; then
        error "cell $cell failed (exit $rc)"
        _lea_slurm_forensics "$cell" "$scratch" "$out" "$rc"
    fi
    # The guests go down either way: on a batch node nothing survives the
    # allocation, and leaving them up only makes the teardown somebody else's
    # timeout.
    lea_fleet_down >/dev/null 2>&1 || true
    lea_rig_down vm0 --force >/dev/null 2>&1 || true
    info "cell $cell: exit $rc"
    return "$rc"
}

# _lea_slurm_stages N -- 1 2 4 ... up to N, which is what the fleet bench
# means by "the same workload in 1, then 2, then N".
_lea_slurm_stages() {
    local n=$1 s=1 out=""
    while [[ $s -le $n ]]; do out="$out $s"; s=$((s * 2)); done
    [[ " $out " == *" $n "* ]] || out="$out $n"
    echo "${out# }"
}

# _lea_slurm_fleet_json CELL OUT RC -- one machine-readable line for a fleet
# cell, in the SHAPE of a gate line so the fan-in has one thing to read.
#
# It is NOT a gate line and does not claim to be: the key is "cell", not
# "gate". The gate output contract in scripts/lib/common.sh is untouched --
# gates still emit exactly what they emitted, and this is a second, separate
# summary for a measurement that never was a gate.
#
# The invariant is carried across verbatim: `acc` must be ONE distinct value
# over every stage and every VM, because a fleet that computed two different
# answers measured nothing worth a table.
_lea_slurm_fleet_json() {
    local cell=$1 out=$2 rc=$3
    local csv=$out/$cell.bench/parallel.csv
    python3 - "$cell" "$rc" "$csv" <<'PY'
import csv, json, sys, os
cell, rc, path = sys.argv[1], int(sys.argv[2]), sys.argv[3]
facts, stages, accs = {}, {}, set()
if os.path.exists(path):
    with open(path) as f:
        for r in csv.DictReader(f):
            m, v, s = r["metric"], r["value"], r["stage"]
            if m == "acc":
                accs.add(v)
            elif m in ("agg_it_per_s", "ms_per_it", "fairness_spread_pct"):
                stages.setdefault(s, {})[m] = v
for s in sorted(stages, key=lambda x: int(x) if x.isdigit() else 0):
    for k, v in stages[s].items():
        facts["vm%s_%s" % (s, k)] = v
facts["distinct_acc"] = len(accs)
if accs:
    facts["acc"] = sorted(accs)[0]
result = "pass"
reason = None
if rc != 0:
    result, reason = "fail", "the fleet bench exited %d" % rc
elif not accs:
    result, reason = "fail", "no acc value in %s -- nothing was measured" % path
elif len(accs) != 1:
    result, reason = "fail", "%d distinct acc values (must be exactly 1): %s" % (
        len(accs), ", ".join(sorted(accs)))
line = {"cell": cell, "result": result, "facts": facts}
if reason:
    line["reason"] = reason
print(json.dumps(line, sort_keys=True))
PY
}

# _lea_slurm_generate -- one sbatch script per cell, plus a submit list.
#
# ONE ALLOCATION RUNS ONE CELL. Two cells in one allocation share a GPU and
# neither number survives that; and a cell that dies takes only its own queue
# slot with it.
_lea_slurm_generate() {
    local out=$1 pkg=$2 counts=$3 cells=$4 partition=$5 gres=$6 account=$7
    local walltime=$8 mem=$9 cpus=${10} submit=${11}
    [[ -n $pkg ]] || die "--package DIR -- the jobs run out of a package (build.sh package)"
    [[ -d $pkg ]] || die "$pkg is not a directory"
    [[ -n $out ]] || out=$PWD/leandro-jobs
    mkdir -p "$out/jobs" "$out/results"

    local cell budget bmin bmem bcpu script list=$out/submit.txt
    : > "$list"
    while read -r cell; do
        budget=$(_lea_slurm_budget "$cell")
        read -r bmin bmem bcpu <<<"$budget"
        [[ -n $walltime ]] && bmin="" 
        [[ -n $mem ]]  && bmem=$mem
        [[ -n $cpus ]] && bcpu=$cpus
        script=$out/jobs/$cell.sbatch
        {
            cat <<SB
#!/bin/bash
# Generated by scripts/bench.sh slurm -- one allocation, one cell.
#SBATCH --job-name=leandro-$cell
#SBATCH --output=$out/results/$cell.slurm.out
#SBATCH --error=$out/results/$cell.slurm.out
#SBATCH --gres=$gres
#SBATCH --cpus-per-task=$bcpu
#SBATCH --mem=${bmem}M
#SBATCH --time=${walltime:-$(printf '%02d:%02d:00' $((bmin / 60)) $((bmin % 60)))}
#SBATCH --nodes=1 --ntasks=1
SB
            [[ -n $partition ]] && echo "#SBATCH --partition=$partition"
            [[ -n $account ]]   && echo "#SBATCH --account=$account"
            cat <<SB

# WHAT THIS NEEDS FROM THE SITE: /dev/kvm exposed to the job, a GPU through
# GRES, apptainer, and a writable node-local scratch.
# NOT needed: root, a bridge, NAT, or nested virtualisation beyond KVM.
set -uo pipefail
PKG=$(printf '%q' "$pkg")
OUT=$(printf '%q' "$out/results")
DRIVER=\$(sed -n 's/^driver: *//p' "\$PKG/MANIFEST" | tr -d '[:space:]')
SIF=\$PKG/sif/leandro-\$DRIVER.sif
# THE EXACT PATH INSIDE THE IMAGE, from the MANIFEST -- never a glob. A
# glob in this script expands on the NODE, where a /nix/store may or may
# not exist and may hold a DIFFERENT leandro-scripts; measured 2026-08-19,
# it resolved to the build host's own store path and apptainer then failed
# on a path that is not in the image. Store paths are copied verbatim into
# the image, so the one the package recorded is the one inside it.
LEANDRO=\$(sed -n 's/^closure: *//p' "\$PKG/MANIFEST" | tr -d '[:space:]')

# apptainer only. singularity's CLI has diverged from it and nothing here
# has ever been run against one, so falling back to it would be claiming a
# second supported runtime on no evidence. Set APPTAINER to point elsewhere.
APPTAINER=\${APPTAINER:-apptainer}
command -v "\$APPTAINER" >/dev/null 2>&1 || {
    echo "no apptainer on this node (set APPTAINER=/path/to/apptainer)" >&2
    exit 1
}

# THE HOST'S DRIVER LIBRARY DIRECTORY, resolved out here and bound in.
# --nv injects the driver's libraries under their SONAME only
# (libcuda.so.1); lea_payload_stage needs the VERSIONED name
# (libcuda.so.\$DRIVER), because that is what it hands the guest and what
# lea_libcuda_check hashes against the host's. Measured 2026-08-19:
# /.singularity.d/libs holds libcuda.so.1 as a real 112 MB file and no
# libcuda.so.610.43.03 at all.
# THE SCRATCH THIS CELL WILL USE, resolved out here and BOUND IN. It has to
# be the same expression the cell uses inside, or the directory it writes to
# is not the one that was mounted -- measured 2026-08-19 through a real
# queue: SLURM does not set SLURM_TMPDIR (that is a site convention, not a
# SLURM variable), the script bound only that, and the cell then tried to
# mkdir under a path apptainer had auto-created read-only. The job died with
# "Read-only file system" and a space check reporting 0 GiB.
SCRATCH=\${SLURM_TMPDIR:-\${TMPDIR:-/tmp}}
mkdir -p "\$SCRATCH" || { echo "cannot create scratch \$SCRATCH" >&2; exit 1; }

HOSTLIB=""
c=\$(ldconfig -p 2>/dev/null | awk '/libcuda\\.so\\.1 /{print \$NF; exit}')
[ -n "\$c" ] && HOSTLIB=\$(dirname "\$(readlink -f "\$c")")
for d in "\$HOSTLIB" /usr/lib64 /usr/lib/x86_64-linux-gnu /usr/lib /run/opengl-driver/lib; do
    [ -n "\$d" ] || continue
    [ -e "\$d/libcuda.so.\$DRIVER" ] && { HOSTLIB=\$d; break; }
done
if [ -z "\$HOSTLIB" ] || [ ! -e "\$HOSTLIB/libcuda.so.\$DRIVER" ]; then
    echo "no NVIDIA userspace for driver \$DRIVER on this node." >&2
    echo "The guest is handed the HOST's libcuda and it must be that exact" >&2
    echo "version. Either this node runs a different driver (then build a" >&2
    echo "package for it) or its userspace is somewhere unusual (then set" >&2
    echo "LEA_NVIDIA_LIB_DIR and bind that directory yourself)." >&2
    exit 1
fi

exec "\$APPTAINER" exec --nv \\
    --bind "\$PKG" --bind "\$OUT" \\
    --bind "\$HOSTLIB":/opt/host-nvidia:ro \\
    --bind "\$SCRATCH" \\
    --env TMPDIR="\$SCRATCH" \\
    --env LEA_NVIDIA_LIB_DIR=/opt/host-nvidia \\
    --env LEA_HOSTVENV="\$PKG/hostvenv" \\
    "\$SIF" \\
    "\$LEANDRO/bin/leandro-bench" slurm \\
        --run $cell --package "\$PKG" --out "\$OUT"
SB
        } > "$script"
        chmod +x "$script"
        echo "$script" >> "$list"
        info "  $cell  (${bcpu} cpus, ${bmem}M, ${walltime:-${bmin}m})"
        # A HINT, not a limit: these are sized for a cluster node, and the
        # machine generating them need not be the one running them. But when
        # it IS -- a single-node SLURM set up to try this out -- a request the
        # node cannot satisfy is not queued, it is rejected outright, and the
        # message SLURM gives ("Requested node configuration is not
        # available") does not say which field was too big.
        local _ncpu _nmem
        _ncpu=$(nproc 2>/dev/null || echo 0)
        _nmem=$(awk '/MemTotal/{print int($2/1024)}' /proc/meminfo 2>/dev/null || echo 0)
        [[ $bcpu -gt ${_ncpu:-0} ]] && warn "$cell asks for $bcpu CPUs; this machine has $_ncpu. Override: --cpus N"
        [[ $bmem -gt ${_nmem:-0} ]] && warn "$cell asks for ${bmem}M; this machine has ${_nmem}M. Override: --mem MiB"
    done < <(_lea_slurm_cells "$counts" "$cells")

    info ""
    info "jobs:    $out/jobs/  ($(wc -l < "$list") cells)"
    info "results: $out/results/"
    info ""
    if [[ $submit -eq 1 ]]; then
        command -v sbatch >/dev/null || die "--submit, but there is no sbatch on this machine"
        local s
        while read -r s; do sbatch "$s"; done < "$list"
    else
        info "Submit them:   while read -r s; do sbatch \"\$s\"; done < $list"
        info "Or run one here, without SLURM:"
        info "  scripts/bench.sh slurm --run $(head -1 "$list" | xargs basename | sed 's/\.sbatch//') --package $pkg --out $out/results"
    fi
    info "Then:          scripts/bench.sh slurm --collect $out/results"
}

# _lea_slurm_collect DIR -- the fan-in, and trap (d).
#
# Reads the LAST line of every <cell>.json (the gate contract puts exactly one
# JSON object there and human text on stderr, so the last line is the verdict
# even when a stage was chatty), pairs it with the <cell>.rig beside it, and
# writes one table plus one machine-readable file.
#
# A CELL THAT PRODUCED NO JSON IS A FAILURE, NOT A GAP. That is the whole
# reason this reads the directory rather than the successes: a job that was
# killed by the queue, ran out of walltime, or died before its gate started
# leaves a .rig or a .log and no verdict, and averaging over what is left
# quietly reports on a subset nobody chose.
#
# RUNS FROM DIFFERENT RIGS ARE NOT COMPARED. The rig line -- driver,
# persistence, persistenced, governor, PCIe link, GPU model -- is the grouping
# key. `bench.sh summary` already aborts when a transport run mixes
# persistence states, because persistence alone once moved the native
# reference by 58 %; across sites there are five more ways to be
# incomparable, so groups are reported side by side and never merged.
_lea_slurm_collect() {
    local dir=$1
    [[ -d $dir ]] || die "no results directory at $dir"
    python3 - "$dir" <<'PY'
import glob, json, os, sys
d = sys.argv[1]
cells = {}
# Every cell that left ANY trace, so a missing verdict is visible as a cell
# rather than as an absence.
# summary.json is THIS function's own output from a previous run. Reading it
# back as a cell invents one named "summary" with no rig and no verdict --
# which then shows up as a failure and as a second, incomparable group.
OURS = ("summary",)
for pat, key in ((".json", "json"), (".rig", "rig"), (".log", "log")):
    for p in glob.glob(os.path.join(d, "*" + pat)):
        name = os.path.basename(p)[: -len(pat)]
        if name.endswith(".slurm"):
            name = name[: -len(".slurm")]
        if name in OURS:
            continue
        cells.setdefault(name, {})[key] = p

def last_json(path):
    obj = None
    try:
        for line in open(path):
            line = line.strip()
            if line.startswith("{") and line.endswith("}"):
                try:
                    obj = json.loads(line)
                except ValueError:
                    pass
    except OSError:
        return None
    return obj

def rig_of(path):
    """The RIG line as an ordered dict, plus the host/job notes beside it."""
    rig, extra = {}, {}
    try:
        for line in open(path):
            line = line.strip()
            if line.startswith("RIG "):
                for kv in line[4:].split():
                    if "=" in kv:
                        k, v = kv.split("=", 1)
                        rig[k] = v
            else:
                p = line.split(None, 1)
                if len(p) == 2:
                    extra[p[0]] = p[1]
    except OSError:
        pass
    return rig, extra

# The fields that decide comparability. Everything here has been observed to
# move a number on this project's own hardware, or is a different device.
#
# PCIe WIDTH, NOT the link generation. The gen is not a property of the
# machine: it drops to gen1 when the card is idle and rises under load, so
# two runs minutes apart on ONE host report gen2x16 and gen3x16 -- measured
# 2026-08-19, and the first version of this grouped by the whole string and
# duly declared a machine incomparable with itself. The width is what the
# slot is wired for and does not move. The gen is still reported per cell,
# because a run that never left gen1 is worth looking at.
KEYS = ("gpu", "driver", "persistence", "persistenced", "governor", "pcie_width")


def rig_key(rig):
    r = dict(rig)
    pcie = r.pop("pcie", "?")
    r["pcie_width"] = pcie.split("x")[-1] if "x" in pcie else "?"
    return r

rows, groups = [], {}
for name in sorted(cells):
    c = cells[name]
    obj = last_json(c["json"]) if "json" in c else None
    rig, extra = rig_of(c["rig"]) if "rig" in c else ({}, {})
    if obj is None:
        result, reason = "NO-VERDICT", "the cell produced no JSON line (killed, out of walltime, or died before the gate started)"
    else:
        result = obj.get("result", "?")
        reason = obj.get("reason")
    rk = rig_key(rig)
    gk = tuple(rk.get(k, "?") for k in KEYS)
    groups.setdefault(gk, []).append(name)
    rows.append({
        "cell": name, "result": result, "reason": reason,
        "rig": rig, "host": extra.get("host"), "job": extra.get("job"),
        "facts": (obj or {}).get("facts", {}),
        "dur_s": (obj or {}).get("dur_s"),
    })

bad = [r for r in rows if r["result"] not in ("pass",)]
print("== cells ==")
print("%-14s %-11s %-16s %-9s %s" % ("CELL", "RESULT", "HOST", "DUR_S", "GPU / DRIVER / PCIE"))
for r in rows:
    rig = r["rig"]
    print("%-14s %-11s %-16s %-9s %s" % (
        r["cell"], r["result"], (r["host"] or "-")[:16],
        r["dur_s"] if r["dur_s"] is not None else "-",
        "%s / %s / pcie %s" % (rig.get("gpu", "?"), rig.get("driver", "?"), rig.get("pcie", "?"))))
    if r["reason"]:
        print("    %s" % r["reason"])

print()
print("== comparability ==")
if len(groups) <= 1:
    (gk,) = tuple(groups) or ((),)
    print("one rig, %d cell(s) -- comparable." % len(rows))
    if gk:
        print("  " + "  ".join("%s=%s" % (k, v) for k, v in zip(KEYS, gk)))
else:
    print("%d DIFFERENT RIGS. These are NOT comparable and are NOT averaged:" % len(groups))
    for i, (gk, names) in enumerate(sorted(groups.items()), 1):
        print("  group %d: %s" % (i, ", ".join(sorted(names))))
        print("           " + "  ".join("%s=%s" % (k, v) for k, v in zip(KEYS, gk)))
    diff = [k for i, k in enumerate(KEYS) if len({g[i] for g in groups}) > 1]
    print("  they differ in: %s" % ", ".join(diff))
    print("  Report them separately, or re-run the odd ones on a matching rig.")
    print("  (persistence alone once moved this project's native reference by 58 %.)")

summary = {
    "cells": rows,
    "comparable": len(groups) <= 1,
    "groups": [{"rig": dict(zip(KEYS, gk)), "cells": sorted(v)} for gk, v in sorted(groups.items())],
    "failed": [r["cell"] for r in bad],
}
outp = os.path.join(d, "summary.json")
with open(outp, "w") as f:
    json.dump(summary, f, indent=2, sort_keys=True)
print()
print("machine-readable: %s" % outp)
if bad:
    print("FAILED: %s" % ", ".join(r["cell"] for r in bad))
sys.exit(1 if bad or len(groups) > 1 else 0)
PY
}

case $CMD in
    transport) do_transport "$@" ;;
    summary)   do_summary "$@" ;;
    fleet)     do_fleet "$@" ;;
    render)    do_render "$@" ;;
    stream)    do_stream "$@" ;;
    diag)      do_diag "$@" ;;
    vk)        do_vk "$@" ;;
    slurm)     do_slurm "$@" ;;
esac
