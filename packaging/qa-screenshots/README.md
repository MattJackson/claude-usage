# QA screenshot pipeline (on-demand, off-CI)

An on-demand screenshot pipeline for sanity-checking a release. Boots
real cloud VMs (EC2 Ubuntu 22.04 + Windows Server 2022) in parallel,
installs usagio from a published release, seeds `state.json` from each
`packaging/screenshots/fixtures/*.json` variant, launches `usagio menubar`
in a real desktop session, captures full-desktop PNGs, downloads them,
and tears the VMs down. macOS capture is a **separate local script** —
see "macOS: no cheap cloud option" below.

**NOT for CI.** The CI-based pipeline at
`.github/workflows/screenshots.yml` doesn't produce usable GUI shots
(hosted runners have no real desktop). This is a manual maintainer tool
run before / after each release.

## Configured prerequisites (already created in account 457667483187)

These were provisioned once (region `us-east-1`) and are tagged
`Project=usagio-qa` for auditing/cleanup. Reuse them for every run:

| resource | value |
|----------|-------|
| S3 bucket | `usagio-qa-screenshots-457667483187` (private, `qa-runs/` expires after 7 days) |
| IAM instance profile / role | `usagio-qa-capture` (inline policy `usagio-qa-s3`: Get/Put/List on that bucket only) |
| EC2 key pair | `usagio-qa` (private key is NOT committed; regenerate with `aws ec2 create-key-pair` if lost) |
| Security group | `usagio-qa-sg` (`sg-09a2032c2505aa4f2`, default VPC) — inbound 22/3389/5985 from the maintainer IP only, outbound all |

Env vars to run:
```sh
export USAGIO_QA_S3_BUCKET=usagio-qa-screenshots-457667483187
export USAGIO_QA_INSTANCE_PROFILE=usagio-qa-capture
export USAGIO_QA_KEYPAIR=usagio-qa
export USAGIO_QA_SSH_KEY=/path/to/usagio-qa.pem
export USAGIO_QA_SECURITY_GROUP=sg-09a2032c2505aa4f2
```

Linux runs on **Ubuntu 24.04** (glibc 2.39 — the released usagio binary
requires `GLIBC_2.39`, so 22.04 cannot run it). Windows runs on Windows
Server 2022.

## One-time setup (from scratch, if recreating)

The orchestrator uses S3 as the transport for PNGs coming off the VMs
(so it doesn't have to open SSH-in on Windows or wire up WinRM). You
create the AWS resources once, export a handful of env vars, and re-use
them for every subsequent run.

1. **S3 bucket** — any name; the orchestrator writes under
   `s3://<bucket>/qa-runs/<run-id>/`. Add a lifecycle rule that expires
   objects after ~7 days so nothing lingers.
   ```
   aws s3 mb s3://your-usagio-qa-bucket
   ```

2. **IAM instance profile** — both VMs assume this. Needs
   `s3:PutObject` + `s3:AbortMultipartUpload` on the bucket.
   ```
   Trust policy: sts:AssumeRole for ec2.amazonaws.com
   Inline policy:
     Effect: Allow
     Action: s3:PutObject, s3:AbortMultipartUpload
     Resource: arn:aws:s3:::your-usagio-qa-bucket/qa-runs/*
   ```
   Then create an instance profile with the same name and attach the
   role.

3. **EC2 keypair** — used for the Linux VM. (Windows uses no inbound
   access; it reports back via S3 only.)
   ```
   aws ec2 create-key-pair --key-name usagio-qa --query KeyMaterial \
     --output text > ~/.ssh/usagio-qa.pem
   chmod 600 ~/.ssh/usagio-qa.pem
   ```

4. **Security group** — outbound-all, inbound SSH from your IP (Linux
   only; Windows doesn't need inbound).
   ```
   aws ec2 create-security-group --group-name usagio-qa \
     --description "usagio QA screenshot VMs"
   aws ec2 authorize-security-group-ingress --group-name usagio-qa \
     --protocol tcp --port 22 --cidr $(curl -s https://checkip.amazonaws.com)/32
   ```

5. **Env vars** — put these in `~/.zshrc` / direnv / whatever:
   ```sh
   export USAGIO_QA_S3_BUCKET=your-usagio-qa-bucket
   export USAGIO_QA_INSTANCE_PROFILE=usagio-qa-vm     # from step 2
   export USAGIO_QA_KEYPAIR=usagio-qa                 # from step 3
   export USAGIO_QA_SSH_KEY=$HOME/.ssh/usagio-qa.pem  # from step 3
   export USAGIO_QA_SECURITY_GROUP=sg-XXXXXXXX        # from step 4
   ```

## Running

```sh
# Linux + Windows only (default). Downloads to output/v0.5.4/.
packaging/qa-screenshots/capture-all.sh v0.5.4

# Include macOS (only works if you're on a Mac).
packaging/qa-screenshots/capture-all.sh v0.5.4 --with-macos

# Different region.
packaging/qa-screenshots/capture-all.sh v0.5.4 --region eu-west-1

# Skip teardown for post-mortem (SSH in and poke around).
packaging/qa-screenshots/capture-all.sh v0.5.4 --keep-instances
```

Output lands at `packaging/qa-screenshots/output/<version>/<os>-<variant>.png`
— 4 fixtures × (2 or 3) OSes = 8 or 12 PNGs.

## Expected cost per run

At us-east-1 on-demand prices, ~10 min wall time, teardown enabled:

| leg     | instance   | ~cost / run |
|---------|-----------|-------------|
| Linux   | t3.medium | ~$0.007     |
| Windows | t3.medium | ~$0.02      |
| S3 PUT + storage | (trivial) | < $0.001 |
| **Total (no macOS)** | | **~$0.03** |

macOS would add ~$25 minimum (mac1.metal has a 24-hour minimum
allocation), which is why it's local-only.

## macOS: no cheap cloud option

- **EC2 mac1.metal / mac2.metal / mac2-m2.metal**: real Mac hardware, but
  the [24-hour minimum dedicated-host allocation][ec2-mac] means the
  cheapest possible screenshot run is ~$25 (mac2 at $0.65/hr × 24hr +
  data). Not worth it for a 12-image sanity check.
- **MacStadium / Anka Build Cloud / MacInCloud**: dedicated / monthly
  subscription models; not on-demand pricing.
- **GitHub Actions macos-latest**: no interactive desktop; can't grant
  Accessibility permission non-interactively; same headless-tray problem
  as the CI pipeline this repo is replacing.

So `capture-macos-local.sh` runs on your own Mac. It's a stripped-down
version of the existing `packaging/screenshots/capture-macos.sh` — one
`usagio menubar` launch, four fixtures swapped under it, four full-desktop
`screencapture -x` shots. Requires Accessibility permission granted to
Terminal/iTerm (System Settings > Privacy & Security > Accessibility).

If that changes and someone builds a real on-demand Mac option under
~$0.50/run, add `capture-macos.sh` (parallel to the Linux/Windows scripts)
and wire it into `capture-all.sh`. Until then, don't.

[ec2-mac]: https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/ec2-mac-instances.html#mac-instance-considerations

## What each script does

- **`capture-all.sh`** — orchestrator. Parses args, launches
  `capture-linux.sh` + `capture-windows.sh` in parallel (and
  `capture-macos-local.sh` if `--with-macos`), waits on both, prints a
  summary.
- **`capture-linux.sh`** — resolves the latest Ubuntu 22.04 AMI via SSM,
  renders `provision/linux-userdata.sh` with the version + S3 URI +
  fixture list, runs `aws ec2 run-instances`, polls S3 for a `_done`
  sentinel, downloads all `linux-*.png`, terminates the instance. On
  EXIT, always attempts terminate — so a mid-flight failure still cleans
  up.
- **`capture-windows.sh`** — same pattern with the Windows Server 2022
  AMI and `provision/windows-userdata.ps1`.
- **`capture-macos-local.sh`** — runs `usagio menubar` locally, swaps
  four fixtures under it, `screencapture -x` each one to
  `output/<version>/macos-*.png`.
- **`provision/linux-userdata.sh`** — the cloud-init blob. Installs
  Xvfb + openbox + xfce4-panel (for a real StatusNotifierHost tray),
  installs the release `.deb`, loops fixtures, uploads PNGs to S3.
- **`provision/windows-userdata.ps1`** — the EC2Launch blob. Configures
  Administrator autologon, downloads + installs usagio's `setup.exe`
  silently, drops a per-logon scheduled task with the capture script,
  reboots. Post-reboot the auto-logged-in interactive session runs
  the capture and uploads to S3.

## Fixtures

Reuses `packaging/screenshots/fixtures/*.json` at the requested release
tag (fetched via `raw.githubusercontent.com` from inside the VM — no local
upload). Four variants are captured per OS:

- `healthy` — all accounts healthy
- `session-locked` — one account with a session-scope limit hit
- `weekly-locked` — one account with a weekly-scope limit hit
- `mixed` — one healthy + one session-locked + one weekly-locked + one
  needing re-login

The `NOW+<duration>` tokens in the locked fixtures are resolved at
capture time by the same `render_fixture.py` used by
`packaging/screenshots/`.

## Full-desktop, not tight-crop

Every PNG is a full virtual-screen capture — menu bar (macOS), taskbar
(Windows), or top panel (Linux) is always visible so cropping / hero-frame
work can happen downstream against the raw shot, without ever needing
another cloud run. **No cropping happens in this pipeline.**

## Debugging a failed run

Both cloud legs upload their capture logs to S3 alongside the PNGs, and
`capture-*.sh` copies them locally too:

- `output/<version>/.linux-cloud-init.log` — full `cloud-init-output.log`
  from the Linux VM.
- `output/<version>/.windows-capture.log` — PowerShell transcript from
  the Windows capture task.

If a leg times out with `_done` never appearing, the log will usually
tell you which install / launch step hung. To poke around live, add
`--keep-instances`, then:

- Linux: `ssh -i $USAGIO_QA_SSH_KEY ubuntu@<public-ip>` — the VM has a
  live Xvfb `:99` you can point `x11vnc` at.
- Windows: `aws ssm start-session --target <instance-id>` gives you a
  Session-0 PowerShell; Fleet Manager > "Remote Desktop" gives you the
  interactive desktop where the capture task ran.

Don't forget to terminate manually when done: `aws ec2 terminate-instances --instance-ids <id>`.

## Known-flaky pieces (iterate here first)

- **Windows tray-click sequence.** The Win+B / SendKeys sequence in
  `provision/windows-userdata.ps1` opening the tray flyout is the
  least-reliable single step. If the icon lands in the overflow flyout
  it may need an extra Tab before Enter. Test with `--keep-instances`
  and iterate the SendKeys chain in the script.
- **Linux xfce4-panel systray dock coords.** `xdotool` is guessing
  where usagio's tray icon lands; the fallback is a "no menu open, just
  tray icon visible" shot.
- **usagio install path on Windows.** The NSIS `/S` silent install
  should drop `usagio.exe` under `C:\Program Files\usagio\` but the
  script also walks `Program Files (x86)` and `%LOCALAPPDATA%\Programs`
  and falls back to a recursive `Get-ChildItem` search. If a future
  installer changes the path, add it to the candidate list.
