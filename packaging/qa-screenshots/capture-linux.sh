#!/usr/bin/env bash
# Boot an Ubuntu 22.04 EC2 instance, run the Linux capture (via cloud-init
# user-data — see provision/linux-userdata.sh), download the PNGs from S3,
# terminate the instance. Invoked by capture-all.sh with USAGIO_QA_* env
# vars pre-set.
#
# Standalone use: export the same USAGIO_QA_* vars (see capture-all.sh
# preamble) plus USAGIO_QA_VERSION and USAGIO_QA_OUT_DIR, then run this.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

: "${USAGIO_QA_S3_BUCKET:?}"
: "${USAGIO_QA_INSTANCE_PROFILE:?}"
: "${USAGIO_QA_SECURITY_GROUP:?}"
: "${USAGIO_QA_KEYPAIR:?}"
: "${USAGIO_QA_VERSION:?}"
: "${USAGIO_QA_OUT_DIR:?}"
REGION="${USAGIO_QA_REGION:-us-east-1}"
RUN_ID="${USAGIO_QA_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
KEEP="${USAGIO_QA_KEEP:-0}"

FIXTURES="healthy session-locked weekly-locked mixed"
S3_PREFIX="qa-runs/$RUN_ID/linux"
S3_URI="s3://$USAGIO_QA_S3_BUCKET/$S3_PREFIX"

INSTANCE_ID=""
UD_RENDERED=""
cleanup() {
  if [[ -n "$INSTANCE_ID" && "$KEEP" != "1" ]]; then
    echo "[linux] terminating $INSTANCE_ID"
    aws --region "$REGION" ec2 terminate-instances --instance-ids "$INSTANCE_ID" >/dev/null || true
  elif [[ -n "$INSTANCE_ID" ]]; then
    echo "[linux] --keep-instances: leaving $INSTANCE_ID running"
  fi
  [[ -n "$UD_RENDERED" && -f "$UD_RENDERED" ]] && rm -f "$UD_RENDERED"
}
trap cleanup EXIT

# Resolve the latest Ubuntu 24.04 LTS amd64 AMI in this region via SSM.
# 24.04 ships glibc 2.39 — the released usagio binary requires GLIBC_2.39,
# so 22.04 (glibc 2.35) can't run it.
AMI=$(aws --region "$REGION" ssm get-parameter \
  --name /aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id \
  --query 'Parameter.Value' --output text)
echo "[linux] AMI=$AMI"

# Render user-data with substitutions.
UD_TEMPLATE="$SCRIPT_DIR/provision/linux-userdata.sh"
UD_RENDERED=$(mktemp)
sed \
  -e "s|@@VERSION@@|$USAGIO_QA_VERSION|g" \
  -e "s|@@S3_URI@@|$S3_URI|g" \
  -e "s|@@FIXTURES@@|$FIXTURES|g" \
  "$UD_TEMPLATE" > "$UD_RENDERED"

echo "[linux] launching t3.medium in $REGION"
INSTANCE_ID=$(aws --region "$REGION" ec2 run-instances \
  --image-id "$AMI" \
  --instance-type t3.medium \
  --key-name "$USAGIO_QA_KEYPAIR" \
  --security-group-ids "$USAGIO_QA_SECURITY_GROUP" \
  --iam-instance-profile "Name=$USAGIO_QA_INSTANCE_PROFILE" \
  --user-data "file://$UD_RENDERED" \
  --instance-initiated-shutdown-behavior terminate \
  --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=usagio-qa-linux-$RUN_ID},{Key=Project,Value=usagio-qa},{Key=usagio-qa,Value=$RUN_ID}]" \
  --query 'Instances[0].InstanceId' --output text)
echo "[linux] instance=$INSTANCE_ID"

# Poll S3 for the _done sentinel the user-data script drops. The 24.04 GUI
# stack (Xvfb + xfce4-panel + webkit + imagemagick) is a heavy apt install,
# so allow ~15 min.
DEADLINE=$(( $(date +%s) + 1200 ))
echo "[linux] waiting for s3://$USAGIO_QA_S3_BUCKET/$S3_PREFIX/_done ..."
while true; do
  if aws --region "$REGION" s3 ls "$S3_URI/_done" >/dev/null 2>&1; then
    echo "[linux] done"
    break
  fi
  if [[ $(date +%s) -ge $DEADLINE ]]; then
    echo "[linux] TIMEOUT waiting for capture; grabbing whatever's uploaded" >&2
    break
  fi
  sleep 15
done

# Download whatever PNGs exist.
aws --region "$REGION" s3 cp --recursive --exclude "*" --include "linux-*.png" \
  "$S3_URI/" "$USAGIO_QA_OUT_DIR/" || true

# Also pull the cloud-init log if present — useful for debugging failures.
aws --region "$REGION" s3 cp "$S3_URI/_cloud-init.log" "$USAGIO_QA_OUT_DIR/.linux-cloud-init.log" 2>/dev/null || true

# Count what we got.
n=$(ls "$USAGIO_QA_OUT_DIR"/linux-*.png 2>/dev/null | wc -l | tr -d ' ')
echo "[linux] downloaded $n PNGs to $USAGIO_QA_OUT_DIR"
[[ "$n" -gt 0 ]]
