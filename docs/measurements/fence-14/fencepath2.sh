# SPDX-License-Identifier: MIT
# fencepath2.sh, from the run -- fence-14 policy, 2026-08-21.
#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# OPEN-QUESTIONS 14, second attempt: fencetime and Vulkan live only on the
# DISPLAY rig (provision.sh stages the probes there), so run the vdisplay
# gate with --keep-vm and strace the probe in the guest it leaves standing.
# -T prints how long each syscall took, and the 10.07 ms fallback is a
# syscall that took 10 ms -- which names the path without instrumenting it.
set -uo pipefail
LEA_ROOT=${LEA_ROOT:-/home/silas/git/Leandro}
source "$LEA_ROOT/scripts/lib/rig.sh"
OUT=${1:?outdir}; mkdir -p "$OUT"
say() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$OUT/run.txt"; }

say "display --keep-vm (the full gate: it stages the probes and runs the events stage)"
"$LEA_ROOT/scripts/test.sh" display --keep-vm > "$OUT/vdisplay.txt" 2>&1
rc=$?
say "  gate exit=$rc, $(grep -c '^PASS' "$OUT/vdisplay.txt" || echo 0) PASS, $(grep -c '^FAIL' "$OUT/vdisplay.txt" || echo 0) FAIL"
grep -E "^(PASS|FAIL|SKIP)" "$OUT/vdisplay.txt" | sed 's/^/    /' | tee -a "$OUT/run.txt"

name=display; idx=$(tr -d '[:space:]' < "$LEA_VM_DIR/$name/index" 2>/dev/null || echo 6)
ip=$(lea_ip "$idx")
lea_running "$LEA_VM_DIR/$name/ch.pid" || { say "the gate did not leave a guest up"; exit 1; }
say "guest $name at $ip is up"

say "fencetime, plain, 4 runs -- how often does it fall back?"
for i in 1 2 3 4; do
    say "  run $i: $(lea_ssh "$ip" 'export DISPLAY=:7; ~/fencetime 2>&1 | tail -1' | tr -d '\r' | cut -c1-110)"
done

say "fencetime under strace -T -f (full syscall set)"
lea_ssh "$ip" 'export DISPLAY=:7; cd ~ && strace -f -T -o /tmp/fence.strace ./fencetime 2>&1 | tail -1' \
    > "$OUT/strace-run.txt" 2>&1
say "  $(tr -d '\r' < "$OUT/strace-run.txt" | tail -1 | cut -c1-110)"
lea_ssh "$ip" 'cat /tmp/fence.strace' > "$OUT/fence.strace" 2>/dev/null
say "  $(wc -l < "$OUT/fence.strace") strace lines"

say "and again, so one run's accident is visible as one"
lea_ssh "$ip" 'export DISPLAY=:7; cd ~ && strace -f -T -o /tmp/fence2.strace ./fencetime 2>&1 | tail -1' \
    > "$OUT/strace-run2.txt" 2>&1
lea_ssh "$ip" 'cat /tmp/fence2.strace' > "$OUT/fence2.strace" 2>/dev/null
say "  $(wc -l < "$OUT/fence2.strace") strace lines"

cp "$LEA_VM_DIR/$name/nvrm.log" "$OUT/backend.txt" 2>/dev/null
say "down"
"$LEA_ROOT/scripts/showcase.sh" down --all --force >> "$OUT/down.txt" 2>&1
say "=== fencepath2 done ==="
