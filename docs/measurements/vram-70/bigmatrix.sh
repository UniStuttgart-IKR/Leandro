#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# EVERY policy and size this card can be cut into, against a workload set
# that spans the interesting axis: things that fit, things that adapt, and
# things that refuse to.
#
# (glmark2 is NOT here: it answers "Could not initialize canvas" in a
#  compute guest, under Xvfb and xvfb-run alike, because there is no NVIDIA
#  GLX for it to bind -- this repo's own glmark2 figure was measured on the
#  DISPLAY rig, which is a different and much heavier setup.)
#   bmw27        Cycles, 386 MB peak, fits anywhere -> seconds
#   classroom    Cycles, 1437 MB peak, ADAPTS       -> seconds (the star)
#   nvenc        the encoder                        -> fps
#   convburn     torch, does NOT adapt, it OOMs     -> ms/iteration
#   vrampress    the ceiling itself                 -> MiB held
set -uo pipefail
LEA_ROOT=/home/silas/git/Leandro
source "$LEA_ROOT/scripts/lib/rig.sh"
source "$LEA_ROOT/scripts/lib/matrix.sh"
OUT=$1; mkdir -p "$OUT"
IP=192.168.100.10
say() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$OUT/run.txt"; }
res() { printf '%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" >> "$OUT/results.tsv"; }
lea_matrix_serial || exit 1
[[ -f $OUT/results.tsv ]] || printf 'row\tguest_mib\tworkload\tvalue\n' > "$OUT/results.tsv"

stage_blender() {   # the tarball lives on the host; guests keep it once unpacked
    lea_ssh "$1" 'test -x ~/bench/blender-4.2.9-linux-x64/blender' 2>/dev/null && return 0
    tar -C /tmp/claude-1000/-home-silas-git-Leandro/f3fc191a-453c-4f97-8ba3-de299d29d9e3/scratchpad/blender \
        -cf - blender.tar.xz classroom bmw27/bmw27_gpu.blend render.py 2>/dev/null \
        | lea_ssh "$1" 'mkdir -p ~/bench && tar -C ~/bench -xf - && cd ~/bench && tar xf blender.tar.xz && mv bmw27/bmw27_gpu.blend . 2>/dev/null; true'
}

row() {   # row TAG ENVSPEC
    local tag=$1 spec=$2 t
    say "=== $tag ($spec) ==="
    "$LEA_ROOT/scripts/showcase.sh" down --name vm0 --force >/dev/null 2>&1
    ( export LEA_VRAM_LIMIT_MIB="" LEA_VRAM_PROFILE_MIB="" LEA_VGPU_TYPE=""
      eval "export $spec"
      "$LEA_ROOT/scripts/showcase.sh" up --name vm0 --index 0 --mem 4096 ) \
        > "$OUT/up-$tag.txt" 2>&1 || { say "  UP FAILED"; res "$tag" "?" "up" "failed"; return 1; }
    stage_blender "$IP"
    local told
    told=$(lea_ssh "$IP" 'nvidia-smi --query-gpu=memory.total --format=csv,noheader,nounits' 2>/dev/null | tr -d ' ')
    say "  guest sees ${told:-?} MiB"

    ( while :; do nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits; sleep 1; done ) \
        > "$OUT/card-$tag.csv" &
    local sampler=$!

    # 2. bmw27 -- fits in every profile on this card.
    t=$(lea_ssh "$IP" 'cd ~/bench && timeout 600 ./blender-*/blender -b bmw27_gpu.blend -P render.py -o /tmp/b_ -f 1 2>&1 | grep -oE "^Time: [0-9:.]+"' 2>/dev/null | tail -1 | sed 's/^Time: //')
    say "  bmw27 ${t:-FAILED}"; res "$tag" "$told" bmw27 "${t:-fail}"

    # 3. classroom -- 1437 MB peak. THE row that says what a too-small
    #    profile costs a workload that can adapt instead of dying.
    lea_ssh "$IP" "cd ~/bench && timeout 900 ./blender-*/blender -b classroom/classroom.blend -P render.py -o /tmp/c_ -f 1 2>&1 | tail -25" \
        > "$OUT/classroom-$tag.txt" 2>&1
    t=$(grep -oE '^Time: [0-9:.]+' "$OUT/classroom-$tag.txt" | tail -1 | sed 's/^Time: //')
    say "  classroom ${t:-DID NOT FINISH}"; res "$tag" "$told" classroom "${t:-fail}"

    # 3b. The same scene at 4K, run only where it can bite. The scene's
    #     textures and BVH are fixed; the render buffers are not, so this
    #     pushes the peak past what 4Q and below can hold.
    if [[ ${told:-99999} -lt 3500 ]]; then
        lea_ssh "$IP" "cd ~/bench && timeout 1200 ./blender-*/blender -b classroom/classroom.blend -P render.py -o /tmp/c4_ -f 1 -- --render-percentage 200 2>&1 | tail -25" \
            > "$OUT/classroom4k-$tag.txt" 2>&1
        t=$(grep -oE '^Time: [0-9:.]+' "$OUT/classroom4k-$tag.txt" | tail -1 | sed 's/^Time: //')
        say "  classroom-4k ${t:-DID NOT FINISH}"; res "$tag" "$told" classroom4k "${t:-fail}"
    fi

    # 4. NVENC, and with it the encoder-capacity question.
    t=$(lea_ssh "$IP" 'timeout 300 ffmpeg -hide_banner -loglevel error -stats -f lavfi -i testsrc=size=1920x1080:rate=60 -frames:v 300 -c:v h264_nvenc -pix_fmt yuv420p -y /tmp/enc.mp4 2>&1 | tail -1 | grep -oE "fps= *[0-9]+" | grep -oE "[0-9]+"' 2>/dev/null | tail -1)
    say "  nvenc ${t:-FAILED} fps"; res "$tag" "$told" nvenc_fps "${t:-fail}"

    # 5. torch, which does NOT adapt: it either runs or raises OOM.
    t=$(lea_ssh "$IP" 'cd ~/gpu && timeout 600 venv/bin/python convburn.py 2>&1 | tail -2 | tr "\n" " "' 2>/dev/null)
    say "  convburn ${t:-FAILED}"; res "$tag" "$told" convburn "${t:-fail}"

    # 6. and the ceiling, for the record.
    t=$(lea_ssh "$IP" 'cd ~/gpu && timeout 200 ./vrampress --max --seconds 15 2>&1 | grep -oE "grown to [0-9]+ MiB|cuInit [0-9]+|cuCtxCreate [0-9]+"' 2>/dev/null | head -1)
    say "  vrampress ${t:-FAILED}"; res "$tag" "$told" vrampress "${t:-fail}"

    kill $sampler 2>/dev/null
    say "  card peak $(sort -n "$OUT/card-$tag.csv" | tail -1) MiB"
}

row uncapped     'LEA_X=1'
row grid-8Q      'LEA_VGPU_TYPE=8Q'
row grid-4Q      'LEA_VGPU_TYPE=4Q'
row grid-2Q      'LEA_VGPU_TYPE=2Q'
row grid-1Q      'LEA_VGPU_TYPE=1Q'
row limit-3072   'LEA_VRAM_LIMIT_MIB=3072'
row limit-2048   'LEA_VRAM_LIMIT_MIB=2048'
row limit-1280   'LEA_VRAM_LIMIT_MIB=1280'
row limit-1024   'LEA_VRAM_LIMIT_MIB=1024'
row limit-768    'LEA_VRAM_LIMIT_MIB=768'
row limit-512    'LEA_VRAM_LIMIT_MIB=512'
row limit-384    'LEA_VRAM_LIMIT_MIB=384'
row profile-3328 'LEA_VRAM_PROFILE_MIB=3328'
row profile-1536 'LEA_VRAM_PROFILE_MIB=1536'
"$LEA_ROOT/scripts/showcase.sh" down --name vm0 --force >/dev/null 2>&1
say "=== matrix done ==="
