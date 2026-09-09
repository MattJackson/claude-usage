#!/usr/bin/env bash
# Boot a Windows Server 2022 EC2 instance, run the Windows capture (via
# EC2Launch user-data — see provision/windows-userdata.ps1), download the
# PNGs from S3, terminate the instance. Invoked by capture-all.sh with
# USAGIO_QA_* env vars pre-set.
set -euo pipefail

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
S3_PREFIX="qa-runs/$RUN_ID/windows"
S3_URI="s3://$USAGIO_QA_S3_BUCKET/$S3_PREFIX"

# Random admin password (used only for autologon on this throwaway VM;
# never leaves the user-data blob). Built with openssl to avoid the
# `tr </dev/urandom | head` SIGPIPE that trips `set -o pipefail`.
ADMIN_PW="Qa$(openssl rand -hex 9)9!"

INSTANCE_ID=""
UD_RENDERED=""
cleanup() {
  if [[ -n "$INSTANCE_ID" && "$KEEP" != "1" ]]; then
    echo "[windows] terminating $INSTANCE_ID"
    aws --region "$REGION" ec2 terminate-instances --instance-ids "$INSTANCE_ID" >/dev/null || true
  elif [[ -n "$INSTANCE_ID" ]]; then
    echo "[windows] --keep-instances: leaving $INSTANCE_ID running"
  fi
  [[ -n "$UD_RENDERED" && -f "$UD_RENDERED" ]] && rm -f "$UD_RENDERED"
}
trap cleanup EXIT

# Pre-render each fixture to a concrete state.json HERE (this Mac has a working
# Python) and upload to S3, instead of running render_fixture.py on the VM — the
# VM's Python env was unreliable (sys.prefix resolved to system32, "No module
# named 'encodings'"), so fixtures silently didn't apply. The VM just downloads
# the ready state.json per fixture. Relative "NOW+" tokens resolve to ~now here,
# a few minutes before the VM reads them; negligible for day/hour countdowns.
RENDER_PY="$SCRIPT_DIR/../screenshots/render_fixture.py"
FIX_DIR="$SCRIPT_DIR/../screenshots/fixtures"
PY=$(command -v python3 || command -v python)
echo "[windows] pre-rendering fixtures locally with $PY"
for f in $FIXTURES; do
  tmp_state=$(mktemp)
  "$PY" "$RENDER_PY" "$FIX_DIR/$f.json" "$tmp_state"
  aws --region "$REGION" s3 cp "$tmp_state" "$S3_URI/state-$f.json" >/dev/null
  rm -f "$tmp_state"
  echo "[windows]   uploaded state-$f.json"
done

AMI=$(aws --region "$REGION" ssm get-parameter \
  --name /aws/service/ami-windows-latest/Windows_Server-2022-English-Full-Base \
  --query 'Parameter.Value' --output text)
echo "[windows] AMI=$AMI"

UD_TEMPLATE="$SCRIPT_DIR/provision/windows-userdata.ps1"
UD_RENDERED=$(mktemp)
sed \
  -e "s|@@VERSION@@|$USAGIO_QA_VERSION|g" \
  -e "s|@@S3_URI@@|$S3_URI|g" \
  -e "s|@@FIXTURES@@|$FIXTURES|g" \
  -e "s|@@ADMIN_PASSWORD@@|$ADMIN_PW|g" \
  "$UD_TEMPLATE" > "$UD_RENDERED"

echo "[windows] launching t3.medium in $REGION"
INSTANCE_ID=$(aws --region "$REGION" ec2 run-instances \
  --image-id "$AMI" \
  --instance-type t3.medium \
  --security-group-ids "$USAGIO_QA_SECURITY_GROUP" \
  --iam-instance-profile "Name=$USAGIO_QA_INSTANCE_PROFILE" \
  --user-data "file://$UD_RENDERED" \
  --block-device-mappings 'DeviceName=/dev/sda1,Ebs={VolumeSize=50,VolumeType=gp3}' \
  --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=usagio-qa-windows-$RUN_ID},{Key=Project,Value=usagio-qa},{Key=usagio-qa,Value=$RUN_ID}]" \
  --query 'Instances[0].InstanceId' --output text)
echo "[windows] instance=$INSTANCE_ID"

# Windows boots + installs runtime (setup.exe, python, AWS CLI) + reboots
# + autologons + runs capture. In practice 10-18 min. Deadline 25 min.
DEADLINE=$(( $(date +%s) + 2100 ))
echo "[windows] waiting for s3://$USAGIO_QA_S3_BUCKET/$S3_PREFIX/_done ..."
while true; do
  if aws --region "$REGION" s3 ls "$S3_URI/_done" >/dev/null 2>&1; then
    echo "[windows] done"
    break
  fi
  if [[ $(date +%s) -ge $DEADLINE ]]; then
    echo "[windows] TIMEOUT waiting for capture; grabbing whatever's uploaded" >&2
    break
  fi
  sleep 20
done

aws --region "$REGION" s3 cp --recursive --exclude "*" --include "windows-*.png" \
  "$S3_URI/" "$USAGIO_QA_OUT_DIR/" || true

aws --region "$REGION" s3 cp "$S3_URI/_capture.log" "$USAGIO_QA_OUT_DIR/.windows-capture.log" 2>/dev/null || true

n=$(ls "$USAGIO_QA_OUT_DIR"/windows-*.png 2>/dev/null | wc -l | tr -d ' ')
echo "[windows] downloaded $n PNGs to $USAGIO_QA_OUT_DIR"
[[ "$n" -gt 0 ]]
