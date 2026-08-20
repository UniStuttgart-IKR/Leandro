#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# In the GUEST: write CS2's video settings to the lowest that still renders,
# and cap the frame rate at the virtual display's rate.
#
#   scripts/guest/cs2-settings.sh [--fps N] [--width W] [--height H] [--dry-run]
#
# WHY THIS EXISTS, and it is not about looking good: the guest renders into
# a virtual display that runs at LEA_VDISPLAY_HZ (60 by default) and is
# captured, encoded by NVENC and streamed. Every frame CS2 renders ABOVE
# that rate is work the card does twice over -- once for a frame nobody
# receives, once for the boundary that carries it -- and on a shared card
# that is throughput taken from whatever else runs on it. `fps_max` at the
# display rate is the single most effective setting here; the quality
# settings below matter less and are set low because the demonstration is
# about the boundary, not about the picture.
#
# WARNING: this writes settings for a CS2 that is INSTALLED. It finds the
# Steam user directory itself; with no CS2 and no Steam login it writes the
# autoexec and says what is missing, so it can be run before the install and
# again after it. It never starts Steam.
#
# The two files, and why both:
#   cs2_video.txt   the video settings CS2 reads at startup. Under
#                   userdata/<id>/730/local/cfg/, per Steam account.
#   autoexec.cfg    console commands, run at every start. Under
#                   game/csgo/cfg/ in the installation. fps_max lives here
#                   because it survives a settings reset in the menu.
set -uo pipefail

FPS=${LEA_VDISPLAY_HZ:-60}
W=1280
H=720
DRY=0
while [[ $# -gt 0 ]]; do
    case $1 in
        --fps)     FPS=$2; shift 2 ;;
        --width)   W=$2; shift 2 ;;
        --height)  H=$2; shift 2 ;;
        --dry-run) DRY=1; shift ;;
        -h|--help)
            awk 'NR>1 { if ($0 !~ /^#/) exit; sub(/^# ?/, ""); if ($0 ~ /^SPDX-/) next; print }' "$0"
            exit 0 ;;
        *) echo "cs2-settings.sh: unknown option $1" >&2; exit 2 ;;
    esac
done

STEAM=""
for d in "$HOME/.local/share/Steam" "$HOME/.steam/steam" "$HOME/.steam/root"; do
    [[ -d $d ]] && { STEAM=$d; break; }
done
if [[ -z $STEAM ]]; then
    # The directory is created by the FIRST run of Steam, not by installing
    # it, so "no directory" and "not installed" are different states and
    # only one of them is fixed by apt.
    if command -v steam >/dev/null 2>&1; then
        echo "Steam is installed but has never been started: no \$HOME/.local/share/Steam yet." >&2
        echo "  Start it once (nvidia-run steam), log in, then run this again." >&2
    else
        echo "Steam is not installed -- bake the image with --with-steam." >&2
    fi
    exit 1
fi

# The video settings block. Values are CS2's own names; 0 is the lowest for
# every one of these except csm_quality and shader_quality, where 0 is
# "low" and there is nothing below it.
video_settings() {
    cat <<EOC
"video.cfg"
{
	"setting.defaultres"		"$W"
	"setting.defaultresheight"	"$H"
	"setting.fullscreen"		"1"
	"setting.nowindowborder"	"0"
	"setting.coop_cursor_enable"	"0"
	"setting.high_dpi"		"0"
	"setting.mat_vsync"		"0"
	"setting.mat_triplebuffered"	"0"
	"setting.mat_motionblur_enabled"	"0"
	"setting.csm_quality_level"	"0"
	"setting.shader_quality"	"0"
	"setting.model_texture_quality"	"0"
	"setting.texture_filtering_mode"	"0"
	"setting.particle_quality"	"0"
	"setting.shadow_quality"	"0"
	"setting.ao_quality"		"0"
	"setting.hdr_type"		"0"
	"setting.aa_mode"		"0"
	"setting.fsr_mode"		"0"
	"setting.dynamic_shadows"	"0"
}
EOC
}

# fps_max is the one that matters for the boundary; the rest keep a menu
# reset from undoing the important half.
autoexec() {
    cat <<EOC
// Written by scripts/guest/cs2-settings.sh -- see that file for why.
// The virtual display runs at ${FPS} Hz; frames above it are rendered,
// captured and then thrown away.
fps_max ${FPS}
fps_max_ui ${FPS}
mat_vsync 0
r_drawtracers_firstperson 0
cl_disablehtmlmotd 1
cl_forcepreload 1
EOC
}

wrote=0

# ---- video settings, per Steam account -------------------------------------
shopt -s nullglob
users=("$STEAM"/userdata/*/)
shopt -u nullglob
if [[ ${#users[@]} -eq 0 ]]; then
    echo "no Steam account has logged in yet (userdata/ is empty)."
    echo "  Log in once, then run this again for the video settings."
else
    for u in "${users[@]}"; do
        cfg="$u/730/local/cfg"
        if [[ $DRY -eq 1 ]]; then
            echo "would write $cfg/cs2_video.txt"
        else
            mkdir -p "$cfg" || continue
            video_settings > "$cfg/cs2_video.txt" || continue
            echo "wrote $cfg/cs2_video.txt (${W}x${H}, everything at its lowest)"
            wrote=$((wrote + 1))
        fi
    done
fi

# ---- autoexec, in the installation -----------------------------------------
game=$(find "$STEAM/steamapps/common" -maxdepth 4 -type d -name csgo 2>/dev/null | head -1)
if [[ -z $game ]]; then
    echo "CS2 is not installed (no steamapps/common/.../csgo)."
    echo "  Install it, then run this again for the autoexec."
elif [[ $DRY -eq 1 ]]; then
    echo "would write $game/cfg/autoexec.cfg"
else
    mkdir -p "$game/cfg" && autoexec > "$game/cfg/autoexec.cfg" \
        && { echo "wrote $game/cfg/autoexec.cfg (fps_max ${FPS})"; wrote=$((wrote + 1)); }
fi

echo
echo "Launch options to set in Steam (right-click CS2 -> Properties):"
echo "  -novid -nojoy -fullscreen -w $W -h $H +fps_max $FPS +exec autoexec"
echo
echo "Two instances on one card: give each one its own fps cap and keep the"
echo "sum under what the display path can carry -- two clients at ${FPS} fps"
echo "each is 2x${FPS} frames through one encoder."
[[ $wrote -eq 0 ]] && echo "(nothing written yet -- see the notes above)"
exit 0
