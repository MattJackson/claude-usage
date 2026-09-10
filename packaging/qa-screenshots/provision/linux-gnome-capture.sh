#!/bin/bash
# Runs INSIDE the real GNOME session (as the desktop user, with DISPLAY + a live
# DBUS session bus). Downloaded from S3 by the user-data. Enables Ubuntu's
# appindicator extension so usagio's tray icon appears in the GNOME top panel,
# then per fixture: swap state.json, (re)launch usagio, click the tray icon,
# screenshot the real desktop, upload full + crop to S3.
set -uo pipefail
S3="${S3:?}"; FIX="${FIX:?}"
AWS="$(command -v aws || echo /snap/bin/aws)"
log(){ echo "$(date -Is) gnome-capture: $*"; }

# Ubuntu ships appindicator support; make sure it's on for this session.
gnome-extensions enable ubuntu-appindicators@ubuntu.com 2>/dev/null || true
gnome-extensions enable appindicatorsupport@rgcjonas.gmail.com 2>/dev/null || true
# A real wallpaper + dark theme so the shot reads like a desktop, not a void.
gsettings set org.gnome.desktop.background picture-uri 'file:///usr/share/backgrounds/warty-final-ubuntu.png' 2>/dev/null || true
gsettings set org.gnome.desktop.background picture-uri-dark 'file:///usr/share/backgrounds/warty-final-ubuntu.png' 2>/dev/null || true
gsettings set org.gnome.desktop.interface color-scheme 'prefer-dark' 2>/dev/null || true
log "session user=$(whoami) display=$DISPLAY geom=$(xdotool getdisplaygeometry 2>/dev/null)"
log "extensions: $(gnome-extensions list --enabled 2>/dev/null | tr '\n' ',')"

STATE="$HOME/.config/usagio/state.json"; mkdir -p "$HOME/.config/usagio"
shot(){ gnome-screenshot -f "$1" 2>/dev/null || import -window root "$1" 2>/dev/null || scrot "$1" 2>/dev/null; }

W=$(xdotool getdisplaygeometry 2>/dev/null | awk '{print $1}'); W=${W:-1920}
# Best-known x-offset (from screen right edge) of the usagio appindicator icon,
# tuned from the first-fixture probe sweep. y is the panel mid-line (~15px).
CLICK_DX="${CLICK_DX:-88}"
first=1
for name in $FIX; do
  log "=== fixture $name ==="
  "$AWS" s3 cp "$S3/state-$name.json" "$STATE" || log "WARN no state-$name"
  pkill -x usagio 2>/dev/null || true; sleep 2
  ( usagio menubar >"/tmp/usagio-$name.log" 2>&1 & )
  sleep 9

  # Dump the GNOME top-panel StatusNotifier area for debugging the icon slot.
  if [ "$first" = 1 ]; then
    ( xdotool search --name '' getwindowname %@ 2>/dev/null | head -50 ) >/tmp/_wins.txt 2>&1 || true
    "$AWS" s3 cp "/tmp/usagio-$name.log" "$S3/_usagio-first.log" 2>/dev/null || true
    "$AWS" s3 cp /tmp/_wins.txt "$S3/_wins.txt" 2>/dev/null || true
  fi

  # A standalone gnome-shell (no session manager) with no open windows starts
  # in the Activities OVERVIEW. Press Escape (twice, with the pointer settled on
  # the desktop) to drop to the plain desktop before shooting.
  xdotool mousemove 960 540 2>/dev/null || true
  xdotool key Escape 2>/dev/null || true; sleep 1
  xdotool key Escape 2>/dev/null || true; sleep 1

  # usagio's appindicator sits at the FAR top-right of the GNOME panel (the
  # observed icon "U 47%" centered near x≈W-85, y≈15). appindicator menus are
  # Clutter actors, invisible to xdotool search, so we can't detect the open
  # menu — instead, for the FIRST fixture, sweep candidate x-offsets and upload
  # a probe shot per offset so the winning click position can be read off S3.
  if [ "$first" = 1 ]; then
    for dx in 60 75 88 100 115 130; do
      xdotool key Escape 2>/dev/null || true; sleep 1
      xdotool mousemove $((W-dx)) 15 2>/dev/null || true
      xdotool click 1 2>/dev/null || true; sleep 2
      shot "/tmp/probe-$dx.png"
      "$AWS" s3 cp "/tmp/probe-$dx.png" "$S3/_probe-dx-$dx.png" 2>/dev/null || true
    done
    xdotool key Escape 2>/dev/null || true; sleep 1
  fi

  # Best-known offset for the real shot: click the usagio icon to open its menu.
  xdotool mousemove $((W-CLICK_DX)) 15 2>/dev/null || true
  xdotool click 1 2>/dev/null || true; sleep 2

  shot "/tmp/linux-$name-full.png"
  "$AWS" s3 cp "/tmp/linux-$name-full.png" "$S3/linux-$name-full.png" 2>/dev/null || true
  # Provisional crop == full; final tight crop happens on the Mac from -full.
  cp "/tmp/linux-$name-full.png" "/tmp/linux-$name.png" 2>/dev/null || true
  "$AWS" s3 cp "/tmp/linux-$name.png" "$S3/linux-$name.png" 2>/dev/null || true
  xdotool key Escape 2>/dev/null || true
  first=0
done

for l in /tmp/usagio-*.log; do "$AWS" s3 cp "$l" "$S3/$(basename "$l")" 2>/dev/null || true; done
echo "done $(date -Is)" >/tmp/_done; "$AWS" s3 cp /tmp/_done "$S3/_done" 2>/dev/null || true
log "DONE"
