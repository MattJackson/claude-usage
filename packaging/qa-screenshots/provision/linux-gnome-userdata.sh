#!/bin/bash
exec >/var/log/usagio-gnome.log 2>&1
set -x
S3="@@S3_URI@@"; VER="@@VERSION@@"; FIX="@@FIXTURES@@"
push(){ command -v aws >/dev/null 2>&1 && aws s3 cp /var/log/usagio-gnome.log "$S3/_userdata.log" 2>/dev/null || true; }
note(){ echo "$(date -Is) STAGE: $*"; push; }

for i in $(seq 1 150); do fuser /var/lib/dpkg/lock-frontend >/dev/null 2>&1 || break; sleep 5; done
export DEBIAN_FRONTEND=noninteractive
apt-get update -y || true

# awscli FIRST so we have logging/uploads even if the GNOME install has trouble.
apt-get install -y awscli curl ca-certificates || snap install aws-cli --classic || true
note "awscli=$(command -v aws) ver=$(aws --version 2>&1 | head -1)"

# GNOME + Xorg-dummy stack. Bulk install, then retry the CORE bits individually
# so one unavailable package name can't abort everything (the previous failure).
GPKGS="gnome-session gnome-shell gnome-screenshot gnome-settings-daemon dbus-x11 \
xserver-xorg-core xserver-xorg-video-dummy xserver-xorg-legacy xinit x11-xserver-utils \
xdotool scrot imagemagick fonts-dejavu-core adwaita-icon-theme gnome-themes-extra \
ubuntu-wallpapers libayatana-appindicator3-1 libwebkit2gtk-4.1-0 libxdo3 \
libgl1-mesa-dri libglx-mesa0 libegl-mesa0 mesa-utils \
gnome-shell-extension-appindicator gnome-shell-extension-ubuntu-appindicators"
apt-get install -y --no-install-recommends $GPKGS || note "bulk apt had failures; retrying core"
for p in gnome-session gnome-shell gnome-screenshot gnome-settings-daemon dbus-x11 \
  xserver-xorg-core xserver-xorg-video-dummy xserver-xorg-legacy xdotool imagemagick scrot \
  ubuntu-wallpapers libayatana-appindicator3-1 libwebkit2gtk-4.1-0 libxdo3 \
  gnome-shell-extension-appindicator; do
  dpkg -s "$p" >/dev/null 2>&1 || apt-get install -y --no-install-recommends "$p" || note "MISSING $p"
done
note "gnome-session=$(command -v gnome-session) Xorg=$(command -v Xorg) shot=$(command -v gnome-screenshot)"

curl -fsSL -o /tmp/usagio.deb "https://github.com/MattJackson/usagio/releases/download/$VER/usagio_${VER#v}_amd64.deb"
apt-get install -y /tmp/usagio.deb || { apt-get -f install -y; dpkg -i /tmp/usagio.deb; apt-get -f install -y; }
note "usagio=$(/usr/bin/usagio --version 2>&1 | head -1)"

id usagioqa || useradd -m -s /bin/bash usagioqa
loginctl enable-linger usagioqa || true

cat >/etc/X11/xorg-dummy.conf <<'XORG'
Section "ServerFlags"
  Option "AutoAddDevices" "false"
EndSection
Section "Device"
  Identifier "d"
  Driver "dummy"
  VideoRam 256000
EndSection
Section "Monitor"
  Identifier "m"
  HorizSync 5.0-1000.0
  VertRefresh 5.0-200.0
  Modeline "1920x1080" 173.00 1920 2048 2248 2576 1080 1083 1088 1120
EndSection
Section "Screen"
  Identifier "s"
  Device "d"
  Monitor "m"
  DefaultDepth 24
  SubSection "Display"
    Depth 24
    Modes "1920x1080"
    Virtual 1920 1080
  EndSubSection
EndSection
XORG
echo 'allowed_users=anybody' >/etc/X11/Xwrapper.config
echo 'needs_root_rights=yes' >>/etc/X11/Xwrapper.config

aws s3 cp "$S3/linux-gnome-capture.sh" /home/usagioqa/run-capture.sh
chmod +x /home/usagioqa/run-capture.sh
chown usagioqa:usagioqa /home/usagioqa/run-capture.sh

note "starting Xorg + gnome-session"
Xorg :99 -config /etc/X11/xorg-dummy.conf -noreset vt8 >/var/log/xorg99.log 2>&1 &
sleep 8
U=$(id -u usagioqa)
install -d -m700 -o usagioqa -g usagioqa /run/user/$U

# DIAGNOSTIC PROBE: run gnome-shell --x11 directly (no gnome-session) for ~18s and
# capture its own stderr — that's the crash reason gnome-session hides ("Oh no").
# Also dump glxinfo so we can see whether GLX/swrast is actually available.
ENVX="DISPLAY=:99 XDG_RUNTIME_DIR=/run/user/$U LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe MESA_LOADER_DRIVER_OVERRIDE=llvmpipe GDK_BACKEND=x11"
su - usagioqa -c "export $ENVX; { echo '== glxinfo =='; glxinfo 2>&1 | head -25; echo '== gnome-shell --x11 probe =='; dbus-run-session -- timeout 18 gnome-shell --x11 --replace; echo \"shell exit=\$?\"; } >/tmp/shell-probe.log 2>&1" || true
aws s3 cp /tmp/shell-probe.log "$S3/_shell-probe.log" 2>/dev/null || true
note "shell probe uploaded"

# Launch gnome-shell DIRECTLY as the X11 compositor (NOT via gnome-session).
# The probe above proved `gnome-shell --x11 --replace` starts fine on the dummy
# display, whereas `gnome-session --session=ubuntu` fails its required-component
# check (no GDM/display-manager) and shows the "Oh no, something has gone wrong"
# screen. gnome-shell alone renders the top panel + loads the appindicator
# extension (enabled by run-capture.sh), which is all we need for the tray shot.
su - usagioqa -c "export $ENVX; dbus-run-session -- bash -lc 'gnome-shell --x11 --replace >/tmp/gnome-session.log 2>&1 & sleep 30; export S3=\"$S3\" FIX=\"$FIX\"; /home/usagioqa/run-capture.sh >/tmp/run-capture.log 2>&1'" &

for i in $(seq 1 50); do
  sleep 20
  aws s3 cp /var/log/usagio-gnome.log "$S3/_userdata.log" 2>/dev/null || true
  aws s3 cp /var/log/xorg99.log "$S3/_xorg.log" 2>/dev/null || true
  { echo "== gnome-session =="; cat /tmp/gnome-session.log 2>/dev/null; echo "== run-capture =="; cat /tmp/run-capture.log 2>/dev/null; echo "== journal (gnome/mutter) =="; journalctl -a --no-pager 2>/dev/null | grep -iE 'gnome-shell|mutter|gnome-session' | tail -40; } >/tmp/sess.log 2>/dev/null || true
  aws s3 cp /tmp/sess.log "$S3/_session.log" 2>/dev/null || true
  aws s3 ls "$S3/_done" >/dev/null 2>&1 && break
done
note "userdata end"
