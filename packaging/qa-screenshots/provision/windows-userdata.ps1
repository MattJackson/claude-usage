<powershell>
# cloud-init / EC2Launch user-data for the Windows capture VM. Runs as
# SYSTEM at first boot (Session 0 — no interactive desktop!). The
# orchestrator (capture-windows.sh) substitutes @@VERSION@@, @@S3_URI@@,
# @@FIXTURES@@, and @@ADMIN_PASSWORD@@ before base64-encoding this and
# handing it to `aws ec2 run-instances --user-data`.
#
# Because tray-icon apps only render their menu in an interactive desktop
# session (Session 1+), this script:
#   1. Configures Administrator autologon.
#   2. Drops the capture script + fixtures to C:\usagio-qa\.
#   3. Registers a scheduled task set to run at that user's logon in the
#      interactive session.
#   4. Reboots. On next boot the machine autologons, the task fires on the
#      real desktop, captures screenshots, uploads to S3, drops _done.
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
# Disable UAC so the interactive session runs unelevated prompts cleanly.
Set-ItemProperty "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System" `
    -Name "EnableLUA" -Value 0 -ErrorAction SilentlyContinue
# Force ALL notification-area icons to always show on the taskbar (no
# overflow flyout) for the DEFAULT user hive, so usagio's icon is directly
# clickable. Also set it for the current (Administrator) profile below.
reg add "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\Explorer" /v NoAutoTrayNotify /t REG_DWORD /d 1 /f | Out-Null

# --- 2. Drop capture script + fixtures ----------------------------------
$Work = "C:\usagio-qa"
New-Item -ItemType Directory -Force -Path $Work | Out-Null
New-Item -ItemType Directory -Force -Path "$Work\fixtures" | Out-Null

[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$Raw = "https://raw.githubusercontent.com/MattJackson/usagio/$Version/packaging/screenshots"
Invoke-WebRequest -UseBasicParsing "$Raw/render_fixture.py" -OutFile "$Work\render_fixture.py"
foreach ($f in $Fixtures) {
    Invoke-WebRequest -UseBasicParsing "$Raw/fixtures/$f.json" -OutFile "$Work\fixtures\$f.json"
}

# Install usagio silently from the release setup.exe (NSIS silent = /S).
$SetupUrl = "https://github.com/MattJackson/usagio/releases/download/$Version/usagio-$Version-x86_64-pc-windows-msvc-setup.exe"
$Setup = "$Work\usagio-setup.exe"
Invoke-WebRequest -UseBasicParsing $SetupUrl -OutFile $Setup
Start-Process -FilePath $Setup -ArgumentList "/S" -Wait
Start-Sleep -Seconds 5

# Python for render_fixture.py NOW+ token resolution.
$PyUrl = "https://www.python.org/ftp/python/3.11.9/python-3.11.9-amd64.exe"
$Py = "$Work\python-setup.exe"
Invoke-WebRequest -UseBasicParsing $PyUrl -OutFile $Py
Start-Process -FilePath $Py -ArgumentList "/quiet","InstallAllUsers=1","PrependPath=1","Include_test=0" -Wait

# AWS CLI v2 (on PATH for the interactive session).
$AwsUrl = "https://awscli.amazonaws.com/AWSCLIV2.msi"
$Awsi = "$Work\awscli.msi"
Invoke-WebRequest -UseBasicParsing $AwsUrl -OutFile $Awsi
Start-Process -FilePath msiexec -ArgumentList "/i","$Awsi","/qn" -Wait

# Diagnostic checkpoint: prove first-boot provisioning finished and that the
# AWS CLI can reach S3 from SYSTEM. If nothing else uploads, this marker tells
# us the capture (post-reboot, interactive session) is where it broke.
$AwsExe = "C:\Program Files\Amazon\AWSCLIV2\aws.exe"
if (Test-Path $AwsExe) {
    $usagioExe = (Get-ChildItem -Recurse -Path "C:\Program Files","C:\Program Files (x86)","$env:LOCALAPPDATA\Programs" -Filter usagio.exe -ErrorAction SilentlyContinue | Select-Object -First 1).FullName
    "firstboot done $(Get-Date -Format o); usagio=$usagioExe; python=$(Get-Command python -ErrorAction SilentlyContinue)" | & $AwsExe s3 cp - "$S3Uri/_firstboot.txt"
}

# --- 3. Capture script (runs interactively at logon) --------------------
$CaptureBody = @'
$ErrorActionPreference = "Continue"
Start-Transcript -Path C:\usagio-qa\capture.log -Append -Force

$Version  = "__VERSION__"
$S3Uri    = "__S3URI__"
$Fixtures = "__FIXTURES__".Split(" ")
$Aws = "C:\Program Files\Amazon\AWSCLIV2\aws.exe"
if (-not (Test-Path $Aws)) { $Aws = "aws" }

# Checkpoint: the interactive capture session actually started.
"capture started $(Get-Date -Format o) as $(whoami)" | & $Aws s3 cp - "$S3Uri/_capture-started.txt" 2>$null

# Force all tray icons to always show for THIS user, then restart Explorer
# so the notification area has no overflow.
$exp = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer"
New-ItemProperty -Path $exp -Name "EnableAutoTray" -Value 0 -PropertyType DWord -Force | Out-Null
Stop-Process -Name explorer -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 8

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Win32 {
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e);
  public const uint LEFTDOWN = 0x0002, LEFTUP = 0x0004, RIGHTDOWN = 0x0008, RIGHTUP = 0x0010;
  public static void LeftClick(int x, int y){ SetCursorPos(x,y); System.Threading.Thread.Sleep(120);
    mouse_event(LEFTDOWN,0,0,0,IntPtr.Zero); System.Threading.Thread.Sleep(40); mouse_event(LEFTUP,0,0,0,IntPtr.Zero); }
  public static void RightClick(int x, int y){ SetCursorPos(x,y); System.Threading.Thread.Sleep(120);
    mouse_event(RIGHTDOWN,0,0,0,IntPtr.Zero); System.Threading.Thread.Sleep(40); mouse_event(RIGHTUP,0,0,0,IntPtr.Zero); }
}
"@

$vs = [System.Windows.Forms.SystemInformation]::VirtualScreen
function Capture-FullScreen([string]$path) {
  $bmp = New-Object System.Drawing.Bitmap $vs.Width, $vs.Height
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($vs.Left, $vs.Top, 0, 0, $vs.Size)
  $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
  $g.Dispose(); $bmp.Dispose()
}

# Crop a rectangle (screen coords) out of a full-screen grab and save it. Clamps
# to the virtual screen so an off-screen menu bound can't throw.
function Capture-Region([string]$path, [int]$x, [int]$y, [int]$w, [int]$h) {
  $x = [Math]::Max($vs.Left, $x); $y = [Math]::Max($vs.Top, $y)
  if ($x + $w -gt $vs.Right)  { $w = $vs.Right  - $x }
  if ($y + $h -gt $vs.Bottom) { $h = $vs.Bottom - $y }
  if ($w -lt 8 -or $h -lt 8) { return $false }
  $bmp = New-Object System.Drawing.Bitmap $w, $h
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($x, $y, 0, 0, (New-Object System.Drawing.Size($w, $h)))
  $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
  $g.Dispose(); $bmp.Dispose()
  return $true
}

# Find the bounds of the open context menu (the tray icon's popup). tray-icon's
# menu surfaces as a top-level ControlType.Menu (Win32 #32768) popup; grab the
# first visible one with a real on-screen rectangle. Returns a hashtable of
# {x,y,w,h} in screen coords, or $null if no menu is open — which lets the
# caller fall back to a heuristic crop derived from the tray-icon location.
function Get-MenuBounds {
  $root = [System.Windows.Automation.AutomationElement]::RootElement
  $cond = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
    [System.Windows.Automation.ControlType]::Menu)
  $menus = $root.FindAll([System.Windows.Automation.TreeScope]::Descendants, $cond)
  foreach ($m in $menus) {
    try {
      $r = $m.Current.BoundingRectangle
      if ($r.Width -ge 40 -and $r.Height -ge 30 -and -not [double]::IsInfinity($r.X)) {
        return @{ x = [int]$r.X; y = [int]$r.Y; w = [int]$r.Width; h = [int]$r.Height }
      }
    } catch { }
  }
  return $null
}

# Walk the UIAutomation tree for a tray button whose Name mentions usagio.
function Find-TrayIcon {
  $root = [System.Windows.Automation.AutomationElement]::RootElement
  $cond = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
    [System.Windows.Automation.ControlType]::Button)
  $btns = $root.FindAll([System.Windows.Automation.TreeScope]::Descendants, $cond)
  foreach ($b in $btns) {
    $n = $b.Current.Name
    if ($n -and $n.ToLower().Contains("usagio")) {
      try {
        $p = $b.GetClickablePoint()
        return @{ x = [int]$p.X; y = [int]$p.Y; name = $n }
      } catch { }
    }
  }
  return $null
}

$UsagioBin = @(
  "$env:ProgramFiles\usagio\usagio.exe",
  "${env:ProgramFiles(x86)}\usagio\usagio.exe",
  "$env:LOCALAPPDATA\Programs\usagio\usagio.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $UsagioBin) {
  $UsagioBin = (Get-ChildItem -Recurse -Path "C:\Program Files","C:\Program Files (x86)","$env:LOCALAPPDATA\Programs" -Filter usagio.exe -ErrorAction SilentlyContinue | Select-Object -First 1).FullName
}
Write-Host "usagio.exe: $UsagioBin  virtualscreen: $($vs.Width)x$($vs.Height)"

$StateDir = Join-Path $env:APPDATA "usagio"
New-Item -ItemType Directory -Force -Path $StateDir | Out-Null
$StateFile = Join-Path $StateDir "state.json"

$first = $true
foreach ($name in $Fixtures) {
  Write-Host "=== fixture: $name ==="
  # `-I` (isolated): ignore any inherited PYTHONHOME/PYTHONPATH. Without it the
  # scheduled-task environment resolved sys.prefix to C:\Windows\system32 and
  # python died with "No module named 'encodings'", so the fixture never
  # applied and every screenshot showed the same empty state.
  # Download the fixture's pre-rendered state.json (rendered on the maintainer's
  # Mac and uploaded before boot — the VM's Python was unreliable). No Python on
  # the VM anymore.
  & $Aws s3 cp "$S3Uri/state-$name.json" $StateFile
  if ($LASTEXITCODE -ne 0) { Write-Warning "could not fetch state-$name.json (exit $LASTEXITCODE)" }

  # Kill usagio, then restart Explorer so orphaned tray icons from the previous
  # fixture (a force-killed process can't remove its own notification-area icon,
  # so dead "usagio" icons pile up and confuse UIAutomation targeting) are
  # cleared. After the restart only the freshly-launched usagio owns an icon.
  Get-Process usagio -ErrorAction SilentlyContinue | Stop-Process -Force
  Start-Sleep -Seconds 1
  Stop-Process -Name explorer -Force -ErrorAction SilentlyContinue
  Start-Sleep -Seconds 6
  Start-Process -FilePath $UsagioBin -ArgumentList "menubar"
  Start-Sleep -Seconds 7

  # Always grab a FULL-DESKTOP shot first (before opening the menu) — it's the
  # ground truth for debugging what the VM actually shows, and we derive the
  # auto-crop from it. Uploaded as windows-<name>-full.png for every fixture.
  $full = "C:\usagio-qa\windows-$name-full.png"

  # ALWAYS dump every UIAutomation Button name (with a bounding-rect tag) so we
  # can see exactly what's enumerable — including the notification-area tray
  # icons — whether or not targeting succeeds. Written per fixture.
  $allBtns = [System.Windows.Automation.AutomationElement]::RootElement.FindAll(
    [System.Windows.Automation.TreeScope]::Descendants,
    (New-Object System.Windows.Automation.PropertyCondition(
      [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
      [System.Windows.Automation.ControlType]::Button)))
  $dump = @()
  foreach ($b in $allBtns) {
    try { $r = $b.Current.BoundingRectangle; $dump += ("{0} @ {1},{2} {3}x{4}" -f $b.Current.Name, [int]$r.X, [int]$r.Y, [int]$r.Width, [int]$r.Height) } catch { $dump += $b.Current.Name }
  }
  Set-Content "C:\usagio-qa\uia-buttons-$name.txt" ($dump -join "`n")
  & $Aws s3 cp "C:\usagio-qa\uia-buttons-$name.txt" "$S3Uri/_uia-buttons-$name.txt"

  $icon = Find-TrayIcon
  if ($icon) {
    Write-Host "tray icon '$($icon.name)' at $($icon.x),$($icon.y)"
    # tray-icon shows the menu on left-click by default on Windows.
    [Win32]::LeftClick($icon.x, $icon.y)
    Start-Sleep -Milliseconds 900
    # Some builds map the menu to right-click; if the left click only
    # activated, a right-click brings up the context menu without harm.
    [Win32]::RightClick($icon.x, $icon.y)
    Start-Sleep -Seconds 1
  } else {
    Write-Warning "usagio tray icon not found via UIAutomation; falling back to Win+B"
    $ws = New-Object -ComObject WScript.Shell
    $ws.SendKeys("{ESC}")
    # Win+B then Enter (best-effort).
    [System.Windows.Forms.SendKeys]::SendWait("{ENTER}")
    Start-Sleep -Seconds 1
  }

  # Full desktop (menu open) — for debugging + crop derivation.
  Capture-FullScreen $full
  & $Aws s3 cp $full "$S3Uri/windows-$name-full.png"

  # Auto-crop to the open menu. Prefer the menu's real UIAutomation bounds;
  # fall back to a heuristic box anchored on the tray icon (menu opens up-left
  # of a bottom-right tray icon). Uploaded as windows-<name>.png — the shot the
  # website uses.
  $raw = "C:\usagio-qa\windows-$name.png"
  $pad = 12
  $mb = Get-MenuBounds
  $cropped = $false
  if ($mb) {
    Write-Host "menu bounds: $($mb.x),$($mb.y) $($mb.w)x$($mb.h)"
    $cropped = Capture-Region $raw ($mb.x - $pad) ($mb.y - $pad) ($mb.w + 2*$pad) ($mb.h + 2*$pad)
  }
  if (-not $cropped -and $icon) {
    # Heuristic: menu rises above-left of the icon. Grab a generous box.
    $cw = 460; $ch = 520
    $cropped = Capture-Region $raw ($icon.x - $cw + 40) ($icon.y - $ch) $cw $ch
  }
  if (-not $cropped) { Capture-FullScreen $raw }  # last resort: full frame
  & $Aws s3 cp $raw "$S3Uri/windows-$name.png"

  # Dismiss the menu.
  [System.Windows.Forms.SendKeys]::SendWait("{ESC}")
  [System.Windows.Forms.SendKeys]::SendWait("{ESC}")
  Start-Sleep -Milliseconds 500
  $first = $false
}

& $Aws s3 cp C:\usagio-qa\capture.log "$S3Uri/_capture.log"
"done at $(Get-Date -Format o)" | & $Aws s3 cp - "$S3Uri/_done"
Stop-Transcript
'@

$CaptureBody = $CaptureBody.Replace("__VERSION__", $Version)
$CaptureBody = $CaptureBody.Replace("__S3URI__", $S3Uri)
$CaptureBody = $CaptureBody.Replace("__FIXTURES__", ($Fixtures -join " "))
Set-Content -Path "$Work\capture.ps1" -Value $CaptureBody -Encoding UTF8

# --- 4. Scheduled task to run capture at logon --------------------------
$Action = New-ScheduledTaskAction -Execute "powershell.exe" `
    -Argument "-ExecutionPolicy Bypass -NoProfile -WindowStyle Hidden -File C:\usagio-qa\capture.ps1"
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
