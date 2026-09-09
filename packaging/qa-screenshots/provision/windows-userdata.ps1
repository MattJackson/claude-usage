<powershell>
# cloud-init / EC2Launch user-data for the Windows capture VM. Runs as
# SYSTEM at first boot (Session 0 — no interactive desktop!). The
# orchestrator (capture-windows.sh) substitutes @@VERSION@@, @@S3_URI@@,
# @@FIXTURES@@, and @@ADMIN_PASSWORD@@ before base64-encoding this and
# handing it to `aws ec2 run-instances --user-data`.
#
# Because tray-icon apps can only render in an interactive desktop session
# (Session 1+), this script:
#   1. Configures Administrator autologon and a random password.
#   2. Drops the actual capture script + fixtures to C:\usagio-qa\.
#   3. Registers a scheduled task set to run at that user's logon, in
#      the interactive session.
#   4. Reboots. On next boot the machine autologons, the task fires in
#      the real desktop, captures screenshots, uploads to S3, drops a
#      _done sentinel.
$ErrorActionPreference = "Stop"

$Version   = "@@VERSION@@"
$S3Uri     = "@@S3_URI@@"
$Fixtures  = "@@FIXTURES@@".Split(" ")
$AdminPass = "@@ADMIN_PASSWORD@@"

# --- 1. Autologon --------------------------------------------------------
$net = Get-Command net.exe
& $net user Administrator $AdminPass | Out-Null
$winlogon = "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon"
Set-ItemProperty $winlogon AutoAdminLogon "1"
Set-ItemProperty $winlogon DefaultUserName "Administrator"
Set-ItemProperty $winlogon DefaultPassword $AdminPass
Set-ItemProperty $winlogon DefaultDomainName $env:COMPUTERNAME
# Skip the first-login OOBE + EC2 password-reset behaviour.
Set-ItemProperty "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System" `
    -Name "EnableLUA" -Value 0 -ErrorAction SilentlyContinue

# --- 2. Drop capture script + fixtures ----------------------------------
$Work = "C:\usagio-qa"
New-Item -ItemType Directory -Force -Path $Work | Out-Null
New-Item -ItemType Directory -Force -Path "$Work\fixtures" | Out-Null

$Raw = "https://raw.githubusercontent.com/MattJackson/usagio/$Version/packaging/screenshots"
Invoke-WebRequest -UseBasicParsing "$Raw/render_fixture.py" -OutFile "$Work\render_fixture.py"
foreach ($f in $Fixtures) {
    Invoke-WebRequest -UseBasicParsing "$Raw/fixtures/$f.json" -OutFile "$Work\fixtures\$f.json"
}

# Install usagio silently from the release setup.exe. NSIS default silent
# flag is /S.
$SetupUrl = "https://github.com/MattJackson/usagio/releases/download/$Version/usagio-$Version-x86_64-pc-windows-msvc-setup.exe"
$Setup = "$Work\usagio-setup.exe"
Invoke-WebRequest -UseBasicParsing $SetupUrl -OutFile $Setup
Start-Process -FilePath $Setup -ArgumentList "/S" -Wait

# Install python (needed for render_fixture.py NOW+ token resolution).
$PyUrl = "https://www.python.org/ftp/python/3.11.9/python-3.11.9-amd64.exe"
$Py = "$Work\python-setup.exe"
Invoke-WebRequest -UseBasicParsing $PyUrl -OutFile $Py
Start-Process -FilePath $Py -ArgumentList "/quiet","InstallAllUsers=1","PrependPath=1","Include_test=0" -Wait

# --- 3. Capture script (runs interactively at logon) --------------------
$CaptureBody = @'
$ErrorActionPreference = "Continue"
Start-Transcript -Path C:\usagio-qa\capture.log -Append -Force

$Version  = "__VERSION__"
$S3Uri    = "__S3URI__"
$Fixtures = "__FIXTURES__".Split(" ")

Start-Sleep -Seconds 20   # let the desktop / taskbar settle after logon

# Find installed usagio.exe (NSIS default install path).
$UsagioBin = @(
  "$env:ProgramFiles\usagio\usagio.exe",
  "$env:ProgramFiles(x86)\usagio\usagio.exe",
  "$env:LOCALAPPDATA\Programs\usagio\usagio.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $UsagioBin) {
  Get-ChildItem -Recurse -Path "C:\Program Files" -Filter usagio.exe -ErrorAction SilentlyContinue |
    Select-Object -First 1 | ForEach-Object { $UsagioBin = $_.FullName }
}
Write-Host "usagio.exe: $UsagioBin"

$StateDir = Join-Path $env:APPDATA "usagio"
New-Item -ItemType Directory -Force -Path $StateDir | Out-Null
$StateFile = Join-Path $StateDir "state.json"

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$vs = [System.Windows.Forms.SystemInformation]::VirtualScreen

function Capture-FullScreen([string]$path) {
  $bmp = New-Object System.Drawing.Bitmap $vs.Width, $vs.Height
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($vs.Left, $vs.Top, 0, 0, $vs.Size)
  $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
  $g.Dispose(); $bmp.Dispose()
}

foreach ($name in $Fixtures) {
  & python "C:\usagio-qa\render_fixture.py" "C:\usagio-qa\fixtures\$name.json" $StateFile

  Get-Process usagio -ErrorAction SilentlyContinue | Stop-Process -Force
  Start-Sleep -Seconds 1
  Start-Process -FilePath $UsagioBin -ArgumentList "menubar"
  Start-Sleep -Seconds 5

  # Open the tray-overflow flyout so usagio's icon is visible, then Enter
  # to open its context menu. Best-effort — see README caveats.
  try {
    $shell = New-Object -ComObject WScript.Shell
    $shell.SendKeys("^{ESC}")     # dismiss any Start menu
    Start-Sleep -Milliseconds 300
    [System.Windows.Forms.SendKeys]::SendWait("^{ESC}")
    Start-Sleep -Milliseconds 200
    [System.Windows.Forms.SendKeys]::SendWait("{ESC}")
    # Win+B focuses the notification area.
    [System.Windows.Forms.SendKeys]::SendWait("^{ESC}")
    Start-Sleep -Milliseconds 200
    $shell.SendKeys("^{ESC}")
    Start-Sleep -Milliseconds 300
    [System.Windows.Forms.SendKeys]::SendWait("{ENTER}")
    Start-Sleep -Seconds 1
  } catch {
    Write-Warning "tray click failed: $_"
  }

  $raw = "C:\usagio-qa\windows-$name.png"
  Capture-FullScreen $raw
  & aws s3 cp $raw "$S3Uri/windows-$name.png"

  [System.Windows.Forms.SendKeys]::SendWait("{ESC}")
  [System.Windows.Forms.SendKeys]::SendWait("{ESC}")
}

& aws s3 cp C:\usagio-qa\capture.log "$S3Uri/_capture.log"
"done at $(Get-Date -Format o)" | & aws s3 cp - "$S3Uri/_done"
Stop-Transcript
'@

$CaptureBody = $CaptureBody.Replace("__VERSION__", $Version)
$CaptureBody = $CaptureBody.Replace("__S3URI__", $S3Uri)
$CaptureBody = $CaptureBody.Replace("__FIXTURES__", ($Fixtures -join " "))
Set-Content -Path "$Work\capture.ps1" -Value $CaptureBody -Encoding UTF8

# Install AWS CLI v2 (the AMI has it via SSM agent's copy, but not on PATH
# for interactive sessions).
$AwsUrl = "https://awscli.amazonaws.com/AWSCLIV2.msi"
$Awsi = "$Work\awscli.msi"
Invoke-WebRequest -UseBasicParsing $AwsUrl -OutFile $Awsi
Start-Process -FilePath msiexec -ArgumentList "/i","$Awsi","/qn" -Wait

# --- 4. Scheduled task to run capture at logon --------------------------
$Action = New-ScheduledTaskAction -Execute "powershell.exe" `
    -Argument "-ExecutionPolicy Bypass -NoProfile -File C:\usagio-qa\capture.ps1"
$Trigger = New-ScheduledTaskTrigger -AtLogOn -User "Administrator"
$Principal = New-ScheduledTaskPrincipal -UserId "Administrator" -LogonType Interactive -RunLevel Highest
$Settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -StartWhenAvailable -ExecutionTimeLimit (New-TimeSpan -Minutes 20)
Register-ScheduledTask -TaskName "UsagioQACapture" -Action $Action -Trigger $Trigger `
    -Principal $Principal -Settings $Settings -Force

# --- 5. Reboot to trigger autologon + scheduled task -------------------
shutdown /r /t 5 /f
</powershell>
<persist>true</persist>
