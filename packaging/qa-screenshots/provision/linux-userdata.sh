#!/bin/bash
# cloud-init user-data for the Linux capture VM. Runs as root at first
# boot. The orchestrator (capture-linux.sh) substitutes @@VERSION@@,
# @@S3_URI@@, and @@FIXTURES@@ before base64-encoding this and handing it
# to `aws ec2 run-instances --user-data`.
#
# Flow: install a minimal X stack (Xvfb + openbox + xfce4-panel for a real
# systray host), install the released usagio .deb, and for each fixture:
# render state.json, restart usagio menubar, click the tray, screenshot
# the full virtual root, upload PNG to S3. Finally, drop a `_done` sentinel
# so the orchestrator knows to stop polling.
set -eux

VERSION="@@VERSION@@"          # e.g. v0.5.4
S3_URI="@@S3_URI@@"            # e.g. s3://bucket/qa-runs/<runid>/linux
FIXTURES="@@FIXTURES@@"        # space-separated list e.g. "healthy weekly-locked ..."

export DEBIAN_FRONTEND=noninteractive
apt-get update -y
apt-get install -y --no-install-recommends \
  xvfb x11-utils xdotool imagemagick openbox xfce4-panel \
  awscli curl ca-certificates python3 dbus-x11 \
  libayatana-appindicator3-1 libwebkit2gtk-4.1-0

# Install usagio from the release .deb.
DEB_URL="https://github.com/MattJackson/usagio/releases/download/${VERSION}/usagio_${VERSION#v}_amd64.deb"
curl -fsSL -o /tmp/usagio.deb "$DEB_URL"
apt-get install -y /tmp/usagio.deb || { apt-get -f install -y; dpkg -i /tmp/usagio.deb; }

# Fetch render_fixture.py + fixtures from the repo at the release tag so
# NOW+<duration> tokens resolve correctly at capture time.
RAW="https://raw.githubusercontent.com/MattJackson/usagio/${VERSION}/packaging/screenshots"
mkdir -p /opt/usagio-qa/fixtures
curl -fsSL -o /opt/usagio-qa/render_fixture.py "$RAW/render_fixture.py"
for f in $FIXTURES; do
  curl -fsSL -o "/opt/usagio-qa/fixtures/$f.json" "$RAW/fixtures/$f.json"
done

export DISPLAY=:99
SCREEN_W=1920
SCREEN_H=1080
Xvfb $DISPLAY -screen 0 ${SCREEN_W}x${SCREEN_H}x24 &
sleep 2
openbox &
sleep 1
# xfce4-panel provides a real StatusNotifierHost so `tray-icon` docks
# properly. Panel-1 is autoconfigured top-edge by default; good enough.
dbus-launch xfce4-panel --disable-wm-check >/tmp/panel.log 2>&1 &
sleep 3

mkdir -p /root/.config/usagio
STATE=/root/.config/usagio/state.json

for name in $FIXTURES; do
  python3 /opt/usagio-qa/render_fixture.py \
    "/opt/usagio-qa/fixtures/${name}.json" "$STATE"

  # Restart usagio menubar so it re-reads state.json cleanly. (It does
  # mtime-poll, but restart is the more deterministic path for a one-shot
  # capture.)
  pkill -x usagio || true
  sleep 1
  HOME=/root usagio menubar >/tmp/usagio-${name}.log 2>&1 &
  sleep 4

  # Click the tray icon. The xfce4 systray sits top-right on the panel;
  # tray-icon apps dock into it in insertion order. With only usagio
  # running there's usually one icon; probe a few likely x offsets.
  for tx in $((SCREEN_W - 20)) $((SCREEN_W - 40)) $((SCREEN_W - 60)); do
    xdotool mousemove $tx 15 click 1 || true
    sleep 0.4
  done
  sleep 1

  RAW=/tmp/${name}-raw.png
  import -display $DISPLAY -window root "$RAW"

  aws s3 cp "$RAW" "$S3_URI/linux-${name}.png"

  # Dismiss anything we opened before the next fixture.
  xdotool key Escape || true
  xdotool mousemove $((SCREEN_W - 5)) $((SCREEN_H - 5)) || true
done

aws s3 cp /var/log/cloud-init-output.log "$S3_URI/_cloud-init.log" || true
echo "done at $(date -u)" | aws s3 cp - "$S3_URI/_done"
