# Generates packaging/windows/usagio.ico from the usagio brand-mark PNG,
# for the Windows distributable (taskbar-pinned shortcut icon).
#
# Windows-only: runs on the `windows-latest` GitHub Actions runner as part
# of the "Build usagio.exe distributable" step in .github/workflows/
# release.yml / rc-release.yml. Uses .NET's built-in System.Drawing (no
# extra tool install needed on the Windows runner). System.Drawing can't
# parse SVG directly, so the source is packaging/windows/logo-512.png -- a
# static 512x512 PNG checked into this repo, rasterized once locally from
# the same web/public/logo.svg brand mark via `rsvg-convert` (the macOS/
# Linux icon pipelines rasterize that SVG fresh on every release instead;
# Windows runners don't have librsvg available without an extra install
# step, so a checked-in PNG is the simpler tradeoff here -- regenerate it
# by hand with `rsvg-convert -w 512 -h 512 web/public/logo.svg -o
# packaging/windows/logo-512.png` if the brand mark ever changes).
#
# Usage:
#   pwsh packaging/windows/generate-ico.ps1 -SourcePng packaging/windows/logo-512.png -OutIco <path>
param(
    [Parameter(Mandatory = $true)][string]$SourcePng,
    [Parameter(Mandatory = $true)][string]$OutIco
)

Add-Type -AssemblyName System.Drawing

if (-not (Test-Path $SourcePng)) {
    Write-Error "source PNG not found: $SourcePng"
    exit 1
}

$sizes = @(16, 24, 32, 48, 64, 128, 256)
$srcImage = [System.Drawing.Image]::FromFile((Resolve-Path $SourcePng))

# Build an ICO container by hand: ICONDIR header + one ICONDIRENTRY per size,
# each entry's image data itself a PNG (Vista+ supports PNG-in-ICO, which
# System.Drawing.Bitmap.Save can produce directly -- avoids needing raw BMP/
# DIB encoding for every size).
$ms = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter($ms)

# ICONDIR: reserved(2)=0, type(2)=1 (icon), count(2)
$bw.Write([UInt16]0)
$bw.Write([UInt16]1)
$bw.Write([UInt16]$sizes.Count)

$imageBytesList = @()
foreach ($size in $sizes) {
    $bmp = New-Object System.Drawing.Bitmap($size, $size)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.DrawImage($srcImage, 0, 0, $size, $size)
    $g.Dispose()

    $pngStream = New-Object System.IO.MemoryStream
    $bmp.Save($pngStream, [System.Drawing.Imaging.ImageFormat]::Png)
    $imageBytesList += , $pngStream.ToArray()
    $bmp.Dispose()
}

# Header is 6 bytes + 16 bytes per entry; image data follows immediately after.
$headerSize = 6 + (16 * $sizes.Count)
$offset = $headerSize
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $size = $sizes[$i]
    $bytes = $imageBytesList[$i]
    $dim = if ($size -ge 256) { 0 } else { $size }  # 0 means 256 in ICO format
    $bw.Write([Byte]$dim)      # width
    $bw.Write([Byte]$dim)      # height
    $bw.Write([Byte]0)         # color palette
    $bw.Write([Byte]0)         # reserved
    $bw.Write([UInt16]1)       # color planes
    $bw.Write([UInt16]32)      # bits per pixel
    $bw.Write([UInt32]$bytes.Length)
    $bw.Write([UInt32]$offset)
    $offset += $bytes.Length
}
foreach ($bytes in $imageBytesList) {
    $bw.Write($bytes)
}

$bw.Flush()
[System.IO.File]::WriteAllBytes($OutIco, $ms.ToArray())
$srcImage.Dispose()
$bw.Dispose()
$ms.Dispose()

Write-Host "wrote $OutIco"
