#!/bin/bash
exec >/var/log/usagio-gnome.log 2>&1
set -x
S3="@@S3_URI@@"; VER="@@VERSION@@"; FIX="@@FIXTURES@@"
push(){ aws s3 cp /var/log/usagio-gnome.log "$S3/_userdata.log" 2>/dev/null || true; }

for i in $(seq 1 120); do fuser /var/lib/dpkg/lock-frontend >/dev/null 2>&1 || break; sleep 5; done
export DEBIAN_FRONTEND=noninteractive
apt-get update -y
apt-get install -y --no-install-recommends \
  gnome-session gnome-shell gnome-shell-extension-ubuntu-appindicators gnome-screenshot \
  gnome-settings-daemon dbus-x11 xserver-xorg-core xserver-xorg-video-dummy xserver-xorg-legacy \
  xinit x11-xserver-utils xdotool scrot imagemagick fonts-dejavu-core adwaita-icon-theme \
  gnome-themes-extra ubuntu-wallpapers libayatana-appindicator3-1 libwebkit2gtk-4.1-0 \
  libxdo3 curl ca-certificates awscli
command -v aws && aws --version
push

curl -fsSL -o /tmp/usagio.deb "https://github.com/MattJackson/usagio/releases/download/$VER/usagio_${VER#v}_amd64.deb"
apt-get install -y /tmp/usagio.deb || { apt-get -f install -y; dpkg -i /tmp/usagio.deb; apt-get -f install -y; }
/usr/bin/usagio --version; push

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

Xorg :99 -config /etc/X11/xorg-dummy.conf -noreset vt8 >/var/log/xorg99.log 2>&1 &
sleep 6
U=$(id -u usagioqa)
install -d -m700 -o usagioqa -g usagioqa /run/user/$U
su - usagioqa -c "export DISPLAY=:99 XDG_RUNTIME_DIR=/run/user/$U LIBGL_ALWAYS_SOFTWARE=1 GDK_BACKEND=x11; dbus-run-session -- bash -lc 'gnome-session --session=ubuntu >/tmp/gnome-session.log 2>&1 & sleep 35; export S3=\"$S3\" FIX=\"$FIX\"; /home/usagioqa/run-capture.sh >/tmp/run-capture.log 2>&1'" &

for i in $(seq 1 45); do
  sleep 20
  aws s3 cp /var/log/usagio-gnome.log "$S3/_userdata.log" 2>/dev/null || true
  aws s3 cp /var/log/xorg99.log "$S3/_xorg.log" 2>/dev/null || true
  { echo "== gnome-session =="; cat /tmp/gnome-session.log 2>/dev/null; echo "== run-capture =="; cat /tmp/run-capture.log 2>/dev/null; } >/tmp/sess.log 2>/dev/null || true
  aws s3 cp /tmp/sess.log "$S3/_session.log" 2>/dev/null || true
  aws s3 ls "$S3/_done" >/dev/null 2>&1 && break
done
push
