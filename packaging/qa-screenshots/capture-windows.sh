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
