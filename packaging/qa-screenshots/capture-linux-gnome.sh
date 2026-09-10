#!/usr/bin/env bash
# Boot an Ubuntu 24.04 EC2 instance, install a REAL GNOME desktop (not the old
# headless Xvfb+xfce panel), install the released usagio .deb, screenshot the
# tray menu on the real GNOME Shell top panel, download the PNGs from S3, and
# terminate the instance. Autonomous like capture-windows.sh: the VM uploads to
# S3 and drops a _done marker; no SSH/inbound needed (only the instance profile's
# S3 permissions + outbound HTTPS).
#
# The point of a real GNOME capture (vs the old Xvfb panel) is proof: it shows
# usagio actually runs on the Ubuntu GNOME desktop we advertise, and surfaces
# real-world issues (appindicator support, menu icons) the way the Windows
# clean-VM capture surfaced real bugs.
#
# Env (same as capture-windows.sh, minus keypair/SSH):
#   USAGIO_QA_S3_BUCKET, USAGIO_QA_INSTANCE_PROFILE, USAGIO_QA_SECURITY_GROUP,
#   USAGIO_QA_VERSION (e.g. v0.5.22), USAGIO_QA_OUT_DIR, [USAGIO_QA_REGION]
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
: "${USAGIO_QA_S3_BUCKET:?}"
: "${USAGIO_QA_INSTANCE_PROFILE:?}"
: "${USAGIO_QA_SECURITY_GROUP:?}"
: "${USAGIO_QA_VERSION:?}"
: "${USAGIO_QA_OUT_DIR:?}"
REGION="${USAGIO_QA_REGION:-us-east-1}"
RUN_ID="${USAGIO_QA_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
KEEP="${USAGIO_QA_KEEP:-0}"

FIXTURES="healthy session-locked weekly-locked mixed"
S3_PREFIX="qa-runs/$RUN_ID/linux-gnome"
S3_URI="s3://$USAGIO_QA_S3_BUCKET/$S3_PREFIX"

INSTANCE_ID=""
UD_RENDERED=""
cleanup() {
  if [[ -n "$INSTANCE_ID" && "$KEEP" != "1" ]]; then
    echo "[gnome] terminating $INSTANCE_ID"
    aws --region "$REGION" ec2 terminate-instances --instance-ids "$INSTANCE_ID" >/dev/null || true
  elif [[ -n "$INSTANCE_ID" ]]; then
    echo "[gnome] --keep-instances: leaving $INSTANCE_ID running"
  fi
  [[ -n "$UD_RENDERED" && -f "$UD_RENDERED" ]] && rm -f "$UD_RENDERED"
}
trap cleanup EXIT

# Pre-render fixtures locally (Mac Python) and upload — same as the Windows path.
RENDER_PY="$SCRIPT_DIR/../screenshots/render_fixture.py"
FIX_DIR="$SCRIPT_DIR/../screenshots/fixtures"
PY=$(command -v python3 || command -v python)
echo "[gnome] pre-rendering fixtures with $PY"
for f in $FIXTURES; do
  tmp=$(mktemp)
  "$PY" "$RENDER_PY" "$FIX_DIR/$f.json" "$tmp"
  aws --region "$REGION" s3 cp "$tmp" "$S3_URI/state-$f.json" >/dev/null
  rm -f "$tmp"
  echo "[gnome]   uploaded state-$f.json"
done

# Latest Ubuntu 24.04 LTS amd64 (Canonical) via SSM public parameter.
AMI=$(aws --region "$REGION" ssm get-parameter \
  --name /aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id \
  --query 'Parameter.Value' --output text)
echo "[gnome] AMI=$AMI"

# Upload the in-session capture script (the VM downloads it — keeps user-data
# small and lets us iterate on the capture logic without re-encoding user-data).
aws --region "$REGION" s3 cp "$SCRIPT_DIR/provision/linux-gnome-capture.sh" "$S3_URI/linux-gnome-capture.sh" >/dev/null
echo "[gnome] uploaded capture script"

UD_TEMPLATE="$SCRIPT_DIR/provision/linux-gnome-userdata.sh"
UD_RENDERED=$(mktemp)
sed \
  -e "s|@@VERSION@@|$USAGIO_QA_VERSION|g" \
  -e "s|@@S3_URI@@|$S3_URI|g" \
  -e "s|@@FIXTURES@@|$FIXTURES|g" \
  "$UD_TEMPLATE" > "$UD_RENDERED"
echo "[gnome] user-data size: $(wc -c < "$UD_RENDERED") bytes (limit 16384)"

echo "[gnome] launching t3.large in $REGION (GNOME needs the RAM)"
INSTANCE_ID=$(aws --region "$REGION" ec2 run-instances \
  --image-id "$AMI" \
  --instance-type t3.large \
  --security-group-ids "$USAGIO_QA_SECURITY_GROUP" \
  --iam-instance-profile "Name=$USAGIO_QA_INSTANCE_PROFILE" \
  --user-data "file://$UD_RENDERED" \
  --block-device-mappings 'DeviceName=/dev/sda1,Ebs={VolumeSize=30,VolumeType=gp3}' \
  --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=usagio-qa-gnome-$RUN_ID},{Key=Project,Value=usagio-qa},{Key=usagio-qa,Value=$RUN_ID}]" \
  --query 'Instances[0].InstanceId' --output text)
echo "[gnome] instance=$INSTANCE_ID"

# GNOME install + boot + session + capture. Ubuntu-desktop install is heavy;
# allow 30 min.
DEADLINE=$(( $(date +%s) + 1800 ))
echo "[gnome] waiting for $S3_URI/_done ..."
while true; do
  if aws --region "$REGION" s3 ls "$S3_URI/_done" >/dev/null 2>&1; then
    echo "[gnome] done"; break
  fi
  if [[ $(date +%s) -ge $DEADLINE ]]; then
    echo "[gnome] TIMEOUT; grabbing whatever's uploaded" >&2; break
  fi
  sleep 25
done

aws --region "$REGION" s3 cp --recursive --exclude "*" --include "linux-*.png" \
  "$S3_URI/" "$USAGIO_QA_OUT_DIR/" || true
aws --region "$REGION" s3 cp "$S3_URI/_capture.log" "$USAGIO_QA_OUT_DIR/.gnome-capture.log" 2>/dev/null || true
n=$(ls "$USAGIO_QA_OUT_DIR"/linux-*.png 2>/dev/null | wc -l | tr -d ' ')
echo "[gnome] downloaded $n PNGs to $USAGIO_QA_OUT_DIR"
