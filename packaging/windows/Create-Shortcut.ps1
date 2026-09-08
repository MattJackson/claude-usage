# Creates a Start Menu shortcut for usagio.exe with the usagio.ico icon, so
# the user can right-click it and "Pin to taskbar" -- pinning the bare .exe
# directly shows Windows' generic executable icon (usagio.exe has no
# icon resource embedded at compile time), but a .lnk shortcut can point
# IconLocation at a separate .ico file, which the taskbar honors.
#
# Ships alongside usagio.exe and usagio.ico in the release zip
# (dist/usagio-<version>-x86_64-pc-windows-msvc.zip). Run it once after
# unzipping:
#
#   powershell -ExecutionPolicy Bypass -File Create-Shortcut.ps1
#
# It creates "usagio.lnk" in the Start Menu Programs folder (and, if -Desktop
# is passed, also on the Desktop), pointing at whatever directory this
# script itself lives in -- so unzip first, then run it from that directory
# (don't move usagio.exe/usagio.ico afterwards, or re-run this script).
param(
    [switch]$Desktop
)

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$ExePath = Join-Path $ScriptDir 'usagio.exe'
$IcoPath = Join-Path $ScriptDir 'usagio.ico'

if (-not (Test-Path $ExePath)) {
    Write-Error "usagio.exe not found next to this script ($ScriptDir) -- unzip the release archive first"
    exit 1
}
if (-not (Test-Path $IcoPath)) {
    Write-Error "usagio.ico not found next to this script ($ScriptDir)"
    exit 1
}

function New-UsagioShortcut([string]$Dir) {
    $lnkPath = Join-Path $Dir 'usagio.lnk'
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($lnkPath)
    $shortcut.TargetPath = $ExePath
    $shortcut.WorkingDirectory = $ScriptDir
    $shortcut.IconLocation = $IcoPath
    $shortcut.Description = 'usagio -- track usage / limits across every AI coding CLI'
    $shortcut.Save()
    Write-Host "wrote $lnkPath"
}

$startMenuPrograms = [Environment]::GetFolderPath('Programs')
New-UsagioShortcut $startMenuPrograms

if ($Desktop) {
    $desktop = [Environment]::GetFolderPath('Desktop')
    New-UsagioShortcut $desktop
}

Write-Host ""
Write-Host "Right-click the new Start Menu shortcut and choose 'Pin to taskbar' to pin usagio with its icon."
