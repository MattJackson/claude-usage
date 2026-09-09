#!/usr/bin/env bash
# On-demand QA screenshot pipeline. Boots one EC2 VM per OS (Linux always,
# Windows always, macOS only if --with-macos and only on your own Mac —
# there is no cheap on-demand macOS cloud option; see README.md), captures
# full-desktop screenshots of usagio at the requested release version for
# every fixture variant, downloads them to
# output/<version>/<os>-<variant>.png, and tears the VMs down.
#
# NOT for CI. This is a script the maintainer invokes locally before / after
# a release to sanity-check what the app actually looks like on each OS.
#
# Usage:
#   capture-all.sh <version> [--with-macos] [--region REGION] [--keep-instances]
#
#   <version>          Required. e.g. v0.5.4. Must match a published GitHub
#                      release tag (release assets are the install source).
#   --with-macos       Also run capture-macos-local.sh. Requires that you
#                      are on a Mac; there is no cloud path.
#   --region           AWS region. Default: us-east-1.
#   --keep-instances   Skip teardown (useful when debugging). By default,
#                      instances are ALWAYS terminated even if capture
#                      failed halfway through.
#
# Prereqs (see README.md for the one-time setup checklist):
#   - AWS CLI configured (~/.aws/credentials or AWS_* env vars).
#   - Environment vars:
#       USAGIO_QA_S3_BUCKET   — S3 bucket for PNG transport
#       USAGIO_QA_KEYPAIR     — EC2 keypair name (Linux SSH access)
#       USAGIO_QA_SSH_KEY     — path to matching private key file
#       USAGIO_QA_INSTANCE_PROFILE — IAM instance profile w/ s3:PutObject
#                                    (both VMs use it)
#       USAGIO_QA_SECURITY_GROUP  — security group ID allowing outbound
#                                    HTTPS + inbound SSH from your IP
#                                    (Linux only — Windows uses SSM, no
#                                    inbound needed)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

VERSION=""
WITH_MACOS=0
REGION="us-east-1"
KEEP=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --with-macos) WITH_MACOS=1; shift ;;
    --region) REGION="$2"; shift 2 ;;
    --keep-instances) KEEP=1; shift ;;
    -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
    -*) echo "unknown flag: $1" >&2; exit 2 ;;
    *)
      if [[ -z "$VERSION" ]]; then VERSION="$1"; shift
      else echo "unexpected positional: $1" >&2; exit 2; fi ;;
  esac
done

if [[ -z "$VERSION" ]]; then
  echo "usage: capture-all.sh <version> [--with-macos] [--region REGION] [--keep-instances]" >&2
  exit 2
fi
# Accept "v0.5.4" or "0.5.4". Normalise to the tag form the release URLs use.
[[ "$VERSION" =~ ^[0-9] ]] && VERSION="v$VERSION"

: "${USAGIO_QA_S3_BUCKET:?set USAGIO_QA_S3_BUCKET (see README.md)}"
: "${USAGIO_QA_KEYPAIR:?set USAGIO_QA_KEYPAIR}"
: "${USAGIO_QA_SSH_KEY:?set USAGIO_QA_SSH_KEY}"
: "${USAGIO_QA_INSTANCE_PROFILE:?set USAGIO_QA_INSTANCE_PROFILE}"
: "${USAGIO_QA_SECURITY_GROUP:?set USAGIO_QA_SECURITY_GROUP}"

OUT_DIR="$SCRIPT_DIR/output/$VERSION"
mkdir -p "$OUT_DIR"

# One shared run id so both legs' S3 prefixes never collide.
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-$$"
export USAGIO_QA_RUN_ID="$RUN_ID"
export USAGIO_QA_REGION="$REGION"
export USAGIO_QA_KEEP="$KEEP"
export USAGIO_QA_VERSION="$VERSION"
export USAGIO_QA_OUT_DIR="$OUT_DIR"

echo "=== usagio QA screenshot capture ==="
echo "  version : $VERSION"
echo "  region  : $REGION"
echo "  bucket  : s3://$USAGIO_QA_S3_BUCKET/qa-runs/$RUN_ID/"
echo "  output  : $OUT_DIR"
echo "  macos   : $([[ $WITH_MACOS -eq 1 ]] && echo on || echo off)"
echo

# Launch Linux + Windows legs in parallel. Each script is responsible for
# its own teardown (via its own EXIT trap) so a failure in one doesn't
# leak the other's instance.
LINUX_LOG="$OUT_DIR/.linux.log"
WIN_LOG="$OUT_DIR/.windows.log"

"$SCRIPT_DIR/capture-linux.sh" >"$LINUX_LOG" 2>&1 &
LINUX_PID=$!
"$SCRIPT_DIR/capture-windows.sh" >"$WIN_LOG" 2>&1 &
WIN_PID=$!

echo "linux leg   pid=$LINUX_PID log=$LINUX_LOG"
echo "windows leg pid=$WIN_PID log=$WIN_LOG"
echo "(tail -f either log to watch progress)"
echo

LINUX_RC=0
WIN_RC=0
wait $LINUX_PID || LINUX_RC=$?
wait $WIN_PID || WIN_RC=$?

echo
echo "linux leg   exit=$LINUX_RC"
echo "windows leg exit=$WIN_RC"

if [[ $WITH_MACOS -eq 1 ]]; then
  if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "warn: --with-macos requested but this machine is not a Mac; skipping." >&2
  else
    echo
    echo "=== running local macOS capture ==="
    "$SCRIPT_DIR/capture-macos-local.sh"
  fi
else
  echo
  echo "note: macOS was NOT captured. To add macOS shots to $OUT_DIR:"
  echo "      run   packaging/qa-screenshots/capture-macos-local.sh   on a Mac."
fi

echo
echo "Captured files under $OUT_DIR:"
ls -la "$OUT_DIR" | grep -v '^\.' || true

if [[ $LINUX_RC -ne 0 || $WIN_RC -ne 0 ]]; then
  exit 1
fi
