#!/bin/bash
# cloud-init user-data for the Linux capture VM. Runs as root at first
# boot. The orchestrator (capture-linux.sh) substitutes @@VERSION@@,
# @@S3_URI@@, and @@FIXTURES@@ before base64-encoding this and handing it
# to `aws ec2 run-instances --user-data`.
#
# Flow: install a minimal X stack (Xvfb + openbox + xfce4-panel for a real
# StatusNotifier host), install the released usagio .deb, and for each
# fixture: render state.json, (re)start `usagio menubar`, left-click the
# tray icon to drop its menu, screenshot the full virtual root, upload PNG
# to S3. Finally drop a `_done` sentinel so the orchestrator stops polling.
#
# IMPORTANT: the panel (StatusNotifierHost) and usagio (StatusNotifierItem)
# MUST share ONE D-Bus session bus or the icon never docks. We start a
# single bus up front and export it to every child.
set -ux

VERSION="@@VERSION@@"          # e.g. v0.5.5
S3_URI="@@S3_URI@@"            # e.g. s3://bucket/qa-runs/<runid>/linux
FIXTURES="@@FIXTURES@@"        # space-separated list e.g. "healthy mixed ..."

log() { echo "[$(date -u +%H:%M:%S)] $*"; }

export DEBIAN_FRONTEND=noninteractive
apt-get update -y
apt-get install -y --no-install-recommends \
  xvfb x11-utils xdotool imagemagick openbox xfce4-panel \
  awscli curl ca-certificates python3 dbus-x11 fonts-dejavu-core \
  libayatana-appindicator3-1 libwebkit2gtk-4.1-0 x11-xserver-utils

# ImageMagick's default policy disables some coders but PNG is fine; make
# sure nothing blocks a root-window import.
sed -i 's/rights="none" pattern="PNG"/rights="read|write" pattern="PNG"/' \
  /etc/ImageMagick-6/policy.xml 2>/dev/null || true

# Install usagio from the release .deb.
DEB_URL="https://github.com/MattJackson/usagio/releases/download/${VERSION}/usagio_${VERSION#v}_amd64.deb"
curl -fsSL -o /tmp/usagio.deb "$DEB_URL"
apt-get install -y /tmp/usagio.deb || { apt-get -f install -y; dpkg -i /tmp/usagio.deb; }
USAGIO_BIN="$(command -v usagio || echo /usr/bin/usagio)"
log "usagio: $USAGIO_BIN ($($USAGIO_BIN --version 2>&1 | head -1))"

# Fetch render_fixture.py + fixtures from the repo at the release tag so
# NOW+<duration> tokens resolve correctly at capture time.
RAWBASE="https://raw.githubusercontent.com/MattJackson/usagio/${VERSION}/packaging/screenshots"
mkdir -p /opt/usagio-qa/fixtures
curl -fsSL -o /opt/usagio-qa/render_fixture.py "$RAWBASE/render_fixture.py"
for f in $FIXTURES; do
  curl -fsSL -o "/opt/usagio-qa/fixtures/$f.json" "$RAWBASE/fixtures/$f.json"
done

# --- One shared D-Bus session bus for panel + usagio -----------------
eval "$(dbus-launch --sh-syntax)"
export DBUS_SESSION_BUS_ADDRESS DBUS_SESSION_BUS_PID
log "DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS"

export DISPLAY=:99
SCREEN_W=1600
SCREEN_H=1000
PANEL_H=34
Xvfb $DISPLAY -screen 0 ${SCREEN_W}x${SCREEN_H}x24 -nolisten tcp &
sleep 3
openbox &
sleep 1
# A pleasant solid backdrop so a menu-less frame is visually obvious.
xsetroot -solid "#243447" || true

# --- Deterministic xfce4-panel layout --------------------------------
# One top panel, full width, with the systray/statusnotifier plugin as the
# FIRST (leftmost) plugin so usagio's icon lands at a known position
# (~x=16, y=PANEL_H/2), then a clock so the panel reads as a real panel.
mkdir -p /root/.config/xfce4/xfconf/xfce-perchannel-xml
cat > /root/.config/xfce4/xfconf/xfce-perchannel-xml/xfce4-panel.xml <<XFCONF
<?xml version="1.0" encoding="UTF-8"?>
<channel name="xfce4-panel" version="1.0">
  <property name="configver" type="int" value="2"/>
  <property name="panels" type="array">
    <value type="int" value="1"/>
    <property name="panel-1" type="empty">
      <property name="position" type="string" value="p=6;x=0;y=0"/>
      <property name="length" type="uint" value="100"/>
      <property name="position-locked" type="bool" value="true"/>
      <property name="size" type="uint" value="$PANEL_H"/>
      <property name="plugin-ids" type="array">
        <value type="int" value="1"/>
        <value type="int" value="2"/>
        <value type="int" value="3"/>
      </property>
    </property>
  </property>
  <property name="plugins" type="empty">
    <property name="plugin-1" type="string" value="systray">
      <property name="square-icons" type="bool" value="true"/>
      <property name="show-frame" type="bool" value="false"/>
      <property name="menu-is-primary" type="bool" value="true"/>
    </property>
    <property name="plugin-2" type="string" value="separator">
      <property name="expand" type="bool" value="true"/>
      <property name="style" type="uint" value="0"/>
    </property>
    <property name="plugin-3" type="string" value="clock"/>
  </property>
</channel>
XFCONF

# xfconfd must be running for the panel to read the seeded XML.
/usr/lib/x86_64-linux-gnu/xfce4/xfconf/xfconfd 2>/dev/null &
sleep 1
xfce4-panel --disable-wm-check >/tmp/panel.log 2>&1 &
sleep 5

mkdir -p /root/.config/usagio
STATE=/root/.config/usagio/state.json
ICON_X=16
ICON_Y=$((PANEL_H/2))

capture_shot() { # $1=outfile
  import -display $DISPLAY -window root "$1" 2>/tmp/import.log || \
    { xwd -root -display $DISPLAY | convert xwd:- "$1"; }
}

for name in $FIXTURES; do
  log "=== fixture: $name ==="
  python3 /opt/usagio-qa/render_fixture.py \
    "/opt/usagio-qa/fixtures/${name}.json" "$STATE" 2>&1 || \
    { log "render_fixture failed for $name"; continue; }

  pkill -x usagio || true
  sleep 1
  HOME=/root DISPLAY=$DISPLAY DBUS_SESSION_BUS_ADDRESS="$DBUS_SESSION_BUS_ADDRESS" \
    "$USAGIO_BIN" menubar >/tmp/usagio-${name}.log 2>&1 &
  sleep 6   # let the SNI item register + dock into the panel

  # Debug: dump window tree + a pre-click frame on the first fixture only.
  if [[ "$name" == "$(echo $FIXTURES | awk '{print $1}')" ]]; then
    xwininfo -root -tree -display $DISPLAY > /tmp/xwininfo.txt 2>&1 || true
    capture_shot /tmp/linux-_preclick.png
    aws s3 cp /tmp/linux-_preclick.png "$S3_URI/linux-_preclick.png" || true
    aws s3 cp /tmp/xwininfo.txt "$S3_URI/_xwininfo.txt" || true
    aws s3 cp /tmp/panel.log "$S3_URI/_panel.log" || true
    aws s3 cp /tmp/usagio-${name}.log "$S3_URI/_usagio-first.log" || true
  fi

  # Left-click the tray icon to open the appindicator menu.
  xdotool mousemove $ICON_X $ICON_Y click 1
  sleep 2

  RAW=/tmp/linux-${name}.png
  capture_shot "$RAW"
  aws s3 cp "$RAW" "$S3_URI/linux-${name}.png" || log "upload failed $name"

  # Dismiss the menu before the next fixture.
  xdotool key Escape || true
  xdotool mousemove $((SCREEN_W/2)) $((SCREEN_H/2)) click 1 || true
  sleep 1
done

aws s3 cp /var/log/cloud-init-output.log "$S3_URI/_cloud-init.log" || true
echo "done at $(date -u)" | aws s3 cp - "$S3_URI/_done" || true
log "ALL DONE"
