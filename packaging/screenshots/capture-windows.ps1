# Capture the full set of "hero" screenshot variations of usagio's system
# tray icon on Windows, one per fixture in packaging/screenshots/fixtures/,
# using real usagio UI + mocked account data (no hand-drawn mockups).
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
# So for every "menu open" variant this script best-effort attempts to open
# the tray flyout (and, for "settings", drill into the Settings submenu)
# via keyboard automation before capturing a full-screen shot; if that
# doesn't work reliably, the capture still succeeds showing the taskbar.
#
# For a screenshot that actually shows the open dropdown, run this on your
# own Windows machine — see README.md.
#
# Requires: python (for render_fixture.py / postprocess.py + Pillow).
# Usage: capture-windows.ps1 -OutDir <output-dir>

param(
    [string]$OutDir = "."
)

$ErrorActionPreference = "Stop"
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$FixturesDir = Join-Path $ScriptDir "fixtures"
$StateDir = Join-Path $env:APPDATA "usagio"
$StateFile = Join-Path $StateDir "state.json"
$BackupFile = $null
$UsagioProc = $null
$RawDir = Join-Path $env:TEMP ("usagio-shots-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $RawDir | Out-Null

# name, fixture, mode ("tray" | "menu" | "settings")
$Variants = @(
    @{ Name = "tray";                Fixture = "healthy";        Mode = "tray" },
    @{ Name = "menu-healthy";        Fixture = "healthy";        Mode = "menu" },
    @{ Name = "menu-locked";         Fixture = "weekly-locked";  Mode = "menu" },
    @{ Name = "menu-session-locked"; Fixture = "session-locked"; Mode = "menu" },
    @{ Name = "menu-mixed";          Fixture = "mixed";          Mode = "menu" },
    @{ Name = "settings";            Fixture = "healthy";        Mode = "settings" }
)

function Cleanup {
    if ($UsagioProc -and -not $UsagioProc.HasExited) {
        Stop-Process -Id $UsagioProc.Id -Force -ErrorAction SilentlyContinue
    }
    if ($BackupFile -and (Test-Path $BackupFile)) {
        Move-Item -Force $BackupFile $StateFile
    } elseif (Test-Path $StateFile) {
        Remove-Item -Force $StateFile
    }
    Remove-Item -Recurse -Force $RawDir -ErrorAction SilentlyContinue
}

try {
    New-Item -ItemType Directory -Force -Path $StateDir | Out-Null
    if (Test-Path $StateFile) {
        $BackupFile = [System.IO.Path]::GetTempFileName()
        Copy-Item -Force $StateFile $BackupFile
    }

    $PythonCmd = Get-Command python -ErrorAction SilentlyContinue
    if (-not $PythonCmd) { $PythonCmd = Get-Command python3 -ErrorAction SilentlyContinue }
    if (-not $PythonCmd) { throw "no 'python' or 'python3' on PATH (needed for render_fixture.py / postprocess.py)" }
    $Python = $PythonCmd.Source

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

    Copy-Item -Force (Join-Path $FixturesDir "healthy.json") $StateFile
    $UsagioProc = Start-Process -FilePath $UsagioBin.Source -ArgumentList "menubar" -PassThru
    Start-Sleep -Seconds 3

    Add-Type -AssemblyName System.Windows.Forms
    Add-Type -AssemblyName System.Drawing
    $screen = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds

    function Capture-FullScreen([string]$path) {
        $bmp = New-Object System.Drawing.Bitmap $screen.Width, $screen.Height
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        $g.CopyFromScreen($screen.Left, $screen.Top, 0, 0, $screen.Size)
        $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
        $g.Dispose()
        $bmp.Dispose()
    }

    foreach ($v in $Variants) {
        & $Python (Join-Path $ScriptDir "render_fixture.py") (Join-Path $FixturesDir "$($v.Fixture).json") $StateFile
        Start-Sleep -Milliseconds 1500

        $clicked = $false
        if ($v.Mode -eq "menu" -or $v.Mode -eq "settings") {
            try {
                # Win+B focuses the tray notification area; Enter opens the
                # focused icon's context menu/flyout. Best-effort — flaky in
                # CI since focus order in the tray isn't guaranteed.
                $shell = New-Object -ComObject WScript.Shell
                $shell.SendKeys("^{ESC}")
                Start-Sleep -Milliseconds 300
                $shell.SendKeys("%{ESC}")
                Start-Sleep -Milliseconds 300
                $shell.SendKeys("^{ESC}")
                Start-Sleep -Milliseconds 300
                [System.Windows.Forms.SendKeys]::SendWait("{ENTER}")
                Start-Sleep -Seconds 1
                $clicked = $true
                if ($v.Mode -eq "settings") {
                    # Best-effort: arrow down toward a "Settings" row and
                    # press Enter to drill in. Exact position isn't
                    # guaranteed without an accessibility-tree query.
                    [System.Windows.Forms.SendKeys]::SendWait("{DOWN 2}{ENTER}")
                    Start-Sleep -Milliseconds 500
                }
            } catch {
                Write-Warning "[$($v.Name)] could not synthesize a tray click reliably in this session: $_"
            }
        }
        if (-not $clicked -and ($v.Mode -eq "menu" -or $v.Mode -eq "settings")) {
            Write-Warning "[$($v.Name)] tray menu did not open; capturing taskbar-only instead."
        }

        $raw = Join-Path $RawDir "windows-$($v.Name)-raw.png"
        Capture-FullScreen $raw

        if ($clicked) {
            [System.Windows.Forms.SendKeys]::SendWait("{ESC}")
            [System.Windows.Forms.SendKeys]::SendWait("{ESC}")
        }

        # Windows tray/taskbar sits bottom-right; anchor there with generous
        # margin, higher up for the taller open-flyout variants.
        $anchorY = 0.89
        if ($v.Mode -ne "tray") { $anchorY = 0.72 }
        & $Python (Join-Path $ScriptDir "postprocess.py") $raw (Join-Path $OutDir "windows-$($v.Name).png") `
            --anchor-x 0.875 --anchor-y $anchorY --crop-width-frac 0.62 --bg "20,20,20"
    }

    Write-Output "Wrote $($Variants.Count) Windows hero screenshots to $OutDir"
} finally {
    Cleanup
}
