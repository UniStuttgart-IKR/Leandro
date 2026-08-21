#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# HOW WIDE IS THE GRACEFUL BAND, AND IS IT OURS TO SET?
#
# Measured tonight: at 768 MiB bmw27 (a 386 MB scene) completed in 96 s
# instead of 23 -- it did not crash, it fell back to host memory and paid
# 4x for it. At the same size classroom (980 MB) died. Blender calls that
# path "shared host memory"; on this stack it is PINNED host memory, which
# is bounded by LEA_MAX_PIN_MIB (host, one allocation) and max_pin_mib
# (guest module, cumulative). If those are the ceiling, then how gracefully
# a tenant degrades is a number WE choose.
#
# Three variables, one scene each: the cap, the pin budget, and whether
# managed memory is faked (LEA_MANAGED_COMPAT).
set -uo pipefail
LEA_ROOT=/home/silas/git/Leandro
source "$LEA_ROOT/scripts/lib/rig.sh"; source "$LEA_ROOT/scripts/lib/matrix.sh"
OUT=$1; mkdir -p "$OUT"; IP=192.168.100.10
say() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$OUT/run.txt"; }
lea_matrix_serial || exit 1
printf 'scene\tvram\tpin_host\tpin_guest\tmanaged\ttime\tnote\n' > "$OUT/results.tsv"

one() {  # one SCENE VRAM PINHOST PINGUEST MANAGED
    local scene=$1 vram=$2 ph=$3 pg=$4 mc=$5
    local tag="${scene}-${vram}-p${ph}-g${pg}-m${mc}"
    say "=== $scene at ${vram} MiB, host pin ${ph}, guest pin ${pg}, managed ${mc} ==="
    "$LEA_ROOT/scripts/showcase.sh" down --name vm0 --force >/dev/null 2>&1
    ( export LEA_VRAM_LIMIT_MIB="$vram" LEA_MANAGED_COMPAT="$mc" LEA_MAX_PIN_MIB="$ph"
      "$LEA_ROOT/scripts/showcase.sh" up --name vm0 --index 0 --mem 8192 --max-pin-mib "$pg" ) \
        >"$OUT/up-$tag.txt" 2>&1 || { say "  up failed"; return; }
    local file=classroom/classroom.blend
    [[ $scene == bmw27 ]] && file=bmw27_gpu.blend
    lea_ssh $IP "cd ~/bench && timeout 1800 ./blender-*/blender -b $file -P render.py -o /tmp/g_ -f 1 2>&1 | tail -25" \
        > "$OUT/render-$tag.txt" 2>&1
    local t err
    t=$(grep -oE '^Time: [0-9:.]+' "$OUT/render-$tag.txt" | tail -1 | sed 's/^Time: //')
    err=$(grep -oE "out of GPU[a-z ]*" "$OUT/render-$tag.txt" | tail -1)
    say "  -> ${t:-FAILED}  ${err:-}"
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$scene" "$vram" "$ph" "$pg" "$mc" "${t:-fail}" "${err:-ok}" >> "$OUT/results.tsv"
}

# The band as it stands, reproduced: 768 is slow, 512 is dead.
one bmw27     768  256 1024 0
one bmw27     512  256 1024 0
# ... and with the pin budget raised, which is the hypothesis.
one bmw27     512  2048 4096 0
one classroom 1024 2048 4096 0
one classroom 768  2048 4096 0
# ... and with managed memory faked as well.
one classroom 1024 2048 4096 1
one classroom 768  2048 4096 1
# The control: does a raised pin budget change anything where it already fit?
one classroom 1280 2048 4096 0
"$LEA_ROOT/scripts/showcase.sh" down --name vm0 --force >/dev/null 2>&1
say "=== graceful test done ==="
