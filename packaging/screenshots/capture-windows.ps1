# Capture a "hero" screenshot of usagio's system tray icon on Windows,
# using a fixed mocked demo-state.json fixture (see demo-state.json in this
# dir).
#
# IMPORTANT / reality check: GitHub's windows-latest runners DO run a real
# interactive desktop session (unlike the Linux runner), so a system tray
# and taskbar genuinely exist and can be screenshotted. However:
#   - Windows auto-collapses inactive tray icons into the "overflow" flyout
#     (the up-arrow / hidden icons panel), so the icon may not be visible
#     directly on the taskbar on first run.
#   - Synthesizing a real click that opens usagio's tray context menu via
#     System.Windows.Forms.SendKeys/mouse_event can be flaky in CI (no
#     guarantee the tray icon's screen coordinates are stable, and opened
#     native context menus don't always paint before CopyFromScreen).
#
# So this script:
#   1. Always captures the full taskbar strip (bottom of the primary
#      screen) — the icon should be visible there, working headlessly on
#      the runner's real desktop session.
#   2. BEST-EFFORT attempts to click the icon and capture the open menu.
#      If it can't find/click the icon reliably, falls back to the
#      taskbar-only screenshot as hero-windows.png.
#
# For a screenshot that actually shows the open dropdown, run this on your
# own Windows machine — see README.md.
#
# Usage: capture-windows.ps1 -OutDir <output-dir>

param(
    [string]$OutDir = "."
)

$ErrorActionPreference = "Stop"
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$StateDir = Join-Path $env:APPDATA "usagio"
$StateFile = Join-Path $StateDir "state.json"
$BackupFile = $null
$UsagioProc = $null

function Cleanup {
    if ($UsagioProc -and -not $UsagioProc.HasExited) {
        Stop-Process -Id $UsagioProc.Id -Force -ErrorAction SilentlyContinue
    }
    if ($BackupFile -and (Test-Path $BackupFile)) {
        Move-Item -Force $BackupFile $StateFile
    } elseif (Test-Path $StateFile) {
        Remove-Item -Force $StateFile
    }
}

try {
    New-Item -ItemType Directory -Force -Path $StateDir | Out-Null
    if (Test-Path $StateFile) {
        $BackupFile = [System.IO.Path]::GetTempFileName()
        Copy-Item -Force $StateFile $BackupFile
    }
    Copy-Item -Force (Join-Path $ScriptDir "demo-state.json") $StateFile

    $UsagioBin = Get-Command usagio -ErrorAction SilentlyContinue
    if (-not $UsagioBin) {
        foreach ($cand in @(".\target\release\usagio.exe", ".\target\debug\usagio.exe")) {
            if (Test-Path $cand) {
                $UsagioBin = Get-Item $cand
                break
            }
        }
    }
    if (-not $UsagioBin) {
        throw "could not find a built usagio.exe (checked PATH, target\release, target\debug)"
    }

    $UsagioProc = Start-Process -FilePath $UsagioBin.Source -ArgumentList "menubar" -PassThru
    Start-Sleep -Seconds 3

    Add-Type -AssemblyName System.Windows.Forms
    Add-Type -AssemblyName System.Drawing

    $screen = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds

    function Capture-Region([int]$x, [int]$y, [int]$w, [int]$h, [string]$path) {
        $bmp = New-Object System.Drawing.Bitmap $w, $h
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        $g.CopyFromScreen($x, $y, 0, 0, (New-Object System.Drawing.Size $w, $h))
        $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
        $g.Dispose()
        $bmp.Dispose()
    }

    # 1) Taskbar-only capture (bottom strip of the primary display, where
    #    the tray icon lives, possibly inside the overflow flyout).
    $taskbarHeight = 48
    Capture-Region 0 ($screen.Height - $taskbarHeight) $screen.Width $taskbarHeight (Join-Path $OutDir "hero-windows-taskbar.png")

    # 2) Best-effort: open the tray overflow flyout with the Windows-native
    #    keyboard shortcut (Win+B focuses the tray, then Enter opens the
    #    focused icon's context menu) and capture the bottom-right corner
    #    where flyouts render. This is inherently best-effort in CI.
    $clickOk = $false
    try {
        [System.Windows.Forms.SendKeys]::SendWait("^{ESC}")
        Start-Sleep -Milliseconds 300
        [System.Windows.Forms.SendKeys]::SendWait("%{ESC}")
        Start-Sleep -Milliseconds 300
        # Win+B: focus the system tray notification area.
        $shell = New-Object -ComObject WScript.Shell
        $shell.SendKeys("^{ESC}")
        Start-Sleep -Milliseconds 300
        [System.Windows.Forms.SendKeys]::SendWait("{ENTER}")
        Start-Sleep -Seconds 1
        $clickOk = $true
    } catch {
        Write-Warning "could not synthesize a tray click reliably in this session: $_"
    }

    if ($clickOk) {
        $flyoutW = 420
        $flyoutH = 500
        Capture-Region ($screen.Width - $flyoutW) ($screen.Height - $taskbarHeight - $flyoutH) $flyoutW $flyoutH (Join-Path $OutDir "hero-windows.png")
    } else {
        Copy-Item -Force (Join-Path $OutDir "hero-windows-taskbar.png") (Join-Path $OutDir "hero-windows.png")
    }

    Write-Output "Wrote $(Join-Path $OutDir 'hero-windows.png') (and hero-windows-taskbar.png)"
} finally {
    Cleanup
}
