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
  [DllImport("user32.dll", SetLastError=true, CharSet=CharSet.Auto)] public static extern IntPtr FindWindow(string cls, string win);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  public struct RECT { public int Left, Top, Right, Bottom; }
  public const uint LEFTDOWN = 0x0002, LEFTUP = 0x0004, RIGHTDOWN = 0x0008, RIGHTUP = 0x0010;
  public static void LeftClick(int x, int y){ SetCursorPos(x,y); System.Threading.Thread.Sleep(120);
    mouse_event(LEFTDOWN,0,0,0,IntPtr.Zero); System.Threading.Thread.Sleep(40); mouse_event(LEFTUP,0,0,0,IntPtr.Zero); }
  public static void RightClick(int x, int y){ SetCursorPos(x,y); System.Threading.Thread.Sleep(120);
    mouse_event(RIGHTDOWN,0,0,0,IntPtr.Zero); System.Threading.Thread.Sleep(40); mouse_event(RIGHTUP,0,0,0,IntPtr.Zero); }
}
"@

# The open tray context menu is a top-level window of class "#32768". Find it and
# read its exact on-screen rect — the reliable way to crop tightly to the menu
# (UIAutomation doesn't expose these popups here, and the heuristic box clipped
# the right-hand percentages).
function Get-PopupMenuRect {
  $h = [Win32]::FindWindow("#32768", $null)
  if ($h -ne [IntPtr]::Zero) {
    $r = New-Object Win32+RECT
    if ([Win32]::GetWindowRect($h, [ref]$r)) {
      $w = $r.Right - $r.Left; $ht = $r.Bottom - $r.Top
      if ($w -ge 40 -and $ht -ge 30) { return @{ x = $r.Left; y = $r.Top; w = $w; h = $ht } }
    }
  }
  return $null
}

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

$AUTO = [System.Windows.Automation.AutomationElement]
$SCOPE = [System.Windows.Automation.TreeScope]
$CTP = [System.Windows.Automation.AutomationElement]::ControlTypeProperty
$CT = [System.Windows.Automation.ControlType]

function New-Cond([object]$controlType) {
  New-Object System.Windows.Automation.PropertyCondition($CTP, $controlType)
}

# Screen-center of an element (fallback when GetClickablePoint throws — dead or
# off-screen tray icons don't have a clickable point).
function Center-Of($el) {
  try {
    $r = $el.Current.BoundingRectangle
    if ($r.Width -ge 4 -and $r.Height -ge 4 -and -not [double]::IsInfinity($r.X)) {
      return @{ x = [int]($r.X + $r.Width/2); y = [int]($r.Y + $r.Height/2) }
    }
  } catch { }
  return $null
}

# Find usagio's tray icon. Notification-area icons live as Button children of the
# taskbar's notification ToolBars ("User/System Promoted Notification Area"),
# which a flat RootElement Button-descendant search misses — so walk ToolBars and
# also the raw Button descendants, matching the icon by its "usagio" tooltip
# (Name). Among matches prefer the one with the RIGHTMOST on-screen rect (the
# live icon sits in the notification area; stale/dead duplicates, if any, sort
# out by taking the last). Returns {x,y,name} of a real on-screen point.
function Find-TrayIcon {
  $root = $AUTO::RootElement
  $cands = @()
  # 1) Buttons under every ToolBar (the notification area is a ToolBar).
  foreach ($tb in $root.FindAll($SCOPE::Descendants, (New-Cond $CT::ToolBar))) {
    foreach ($b in $tb.FindAll($SCOPE::Children, (New-Cond $CT::Button))) { $cands += $b }
  }
  # 2) All Button descendants (covers shells that expose icons directly).
  foreach ($b in $root.FindAll($SCOPE::Descendants, (New-Cond $CT::Button))) { $cands += $b }
  $best = $null; $bestX = -1
  foreach ($b in $cands) {
    $n = $null; try { $n = $b.Current.Name } catch { }
    if ($n -and $n.ToLower().Contains("usagio")) {
      $pt = $null
      try { $p = $b.GetClickablePoint(); $pt = @{ x = [int]$p.X; y = [int]$p.Y } } catch { $pt = Center-Of $b }
      if ($pt -and $pt.x -gt $bestX) { $best = @{ x = $pt.x; y = $pt.y; name = $n }; $bestX = $pt.x }
    }
  }
  return $best
}

# Dump the notification-area structure (toolbars + their button names/rects) for
# diagnosis — this is what actually contains the tray icons.
function Dump-TrayStructure([string]$path) {
  $root = $AUTO::RootElement
  $lines = @()
  foreach ($tb in $root.FindAll($SCOPE::Descendants, (New-Cond $CT::ToolBar))) {
    $tn = $null; try { $tn = $tb.Current.Name } catch { }
    $lines += "TOOLBAR: $tn"
    foreach ($b in $tb.FindAll($SCOPE::Children, (New-Cond $CT::Button))) {
      $bn = $null; $r = $null
      try { $bn = $b.Current.Name; $r = $b.Current.BoundingRectangle } catch { }
      $lines += ("  BTN: {0} @ {1},{2}" -f $bn, [int]$r.X, [int]$r.Y)
    }
  }
  Set-Content $path ($lines -join "`n")
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

# Launch usagio ONCE. Instead of kill+relaunch per fixture (which orphaned tray
# icons) or restarting Explorer (which tore down the UIAutomation tree and left
# it enumerating zero buttons), we start usagio a single time and change the
# fixture by rewriting state.json — usagio's poll/redraw loop re-reads it and
# updates the menu live. One stable tray icon, no orphans, intact UIA tree.
Get-Process usagio -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2
Start-Process -FilePath $UsagioBin -ArgumentList "menubar"
Start-Sleep -Seconds 8

$first = $true
foreach ($name in $Fixtures) {
  Write-Host "=== fixture: $name ==="
  # Pre-rendered on the maintainer's Mac and uploaded before boot (the VM's
  # Python was unreliable). Rewrite state.json; usagio picks up the change.
  & $Aws s3 cp "$S3Uri/state-$name.json" $StateFile
  if ($LASTEXITCODE -ne 0) { Write-Warning "could not fetch state-$name.json (exit $LASTEXITCODE)" }
  # Give usagio a couple of redraw ticks to re-read state.json and rebuild the
  # tray menu before we open it.
  Start-Sleep -Seconds 5

  $full = "C:\usagio-qa\windows-$name-full.png"

  # Dump the notification-area toolbar structure (what actually holds tray icons)
  # every fixture, for diagnosis.
  Dump-TrayStructure "C:\usagio-qa\uia-tray-$name.txt"
  & $Aws s3 cp "C:\usagio-qa\uia-tray-$name.txt" "$S3Uri/_uia-tray-$name.txt"

  # Locate usagio's tray icon. UIAutomation does NOT expose notification-area
  # tray icons on this Windows Server (they live in explorer's ToolbarWindow32,
  # which UIA descendant search doesn't surface — only taskbar buttons show up),
  # so Find-TrayIcon is best-effort. The reliable path is a POSITION click: with
  # EnableAutoTray=0 every tray icon is visible and usagio's is the left-most
  # custom icon in the notification area, a fixed offset from the bottom-right on
  # this fixed AMI/resolution. Measured at ~ (screenW-209, screenH-20).
  $icon = Find-TrayIcon
  if ($icon) {
    Write-Host "tray icon '$($icon.name)' at $($icon.x),$($icon.y) (UIA)"
    $ix = $icon.x; $iy = $icon.y
  } else {
    $ix = $vs.Right - 209; $iy = $vs.Bottom - 20
    Write-Host "tray icon via fixed position $ix,$iy (UIA did not expose it)"
  }
  # tray-icon shows the menu on left-click on Windows. Move first (some shells
  # need a hover to register the icon), then click.
  [Win32]::LeftClick($ix, $iy)
  Start-Sleep -Milliseconds 1200
  # If the left click only activated/toggled, a right-click still brings up the
  # context menu. Harmless if the menu is already open (it just reopens).
  [Win32]::RightClick($ix, $iy)
  Start-Sleep -Seconds 1

  # Full desktop (menu open) — for debugging + crop derivation.
  Capture-FullScreen $full
  & $Aws s3 cp $full "$S3Uri/windows-$name-full.png"

  # Auto-crop to the open menu. Prefer the menu's real UIAutomation bounds;
  # fall back to a heuristic box anchored on the tray icon (menu opens up-left
  # of a bottom-right tray icon). Uploaded as windows-<name>.png — the shot the
  # website uses.
  $raw = "C:\usagio-qa\windows-$name.png"
  $pad = 10
  # Prefer the real #32768 popup-menu window rect; fall back to UIA menu bounds.
  $mb = Get-PopupMenuRect
  if (-not $mb) { $mb = Get-MenuBounds }
  $cropped = $false
  if ($mb) {
    Write-Host "menu rect: $($mb.x),$($mb.y) $($mb.w)x$($mb.h)"
    $cropped = Capture-Region $raw ($mb.x - $pad) ($mb.y - $pad) ($mb.w + 2*$pad) ($mb.h + 2*$pad)
  }
  if (-not $cropped) {
    # Heuristic: the menu rises above-left of the icon at ($ix,$iy). Grab a
    # generous box anchored there.
    $cw = 460; $ch = 520
    $cropped = Capture-Region $raw ($ix - $cw + 40) ($iy - $ch) $cw $ch
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
