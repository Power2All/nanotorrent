# Generates the two assets the NSIS script needs but can't consume directly:
#   installer.bmp - the welcome/finish image, drawn from res/app.png (NSIS can't
#                   use PNG; it wants a 164x314 BMP).
#   readme.txt    - README.md flattened to plain scrollable text (NSIS can't
#                   render Markdown).
# Called by build-installer.bat. Kept ASCII-only (glyph patterns use \uXXXX) so
# Windows PowerShell 5.1, which reads .ps1 as the system codepage, parses it.

param(
    [string]$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')),
    [string]$Out  = $PSScriptRoot
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

# --- installer.bmp from app.png ---
$png = [System.Drawing.Image]::FromFile((Join-Path $Root 'res\app.png'))
$bmp = New-Object System.Drawing.Bitmap 164, 314
$g   = [System.Drawing.Graphics]::FromImage($bmp)
$g.Clear([System.Drawing.Color]::FromArgb(24, 24, 24))
$g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
$g.DrawImage($png, 22, 44, 120, 120)
$font = New-Object System.Drawing.Font('Segoe UI', 15, [System.Drawing.FontStyle]::Bold)
$g.DrawString('NanoTorrent', $font, [System.Drawing.Brushes]::White, 12, 178)
$font.Dispose(); $g.Dispose()
$bmp.Save((Join-Path $Out 'installer.bmp'), [System.Drawing.Imaging.ImageFormat]::Bmp)
$png.Dispose(); $bmp.Dispose()

# --- readme.txt from README.md (strip Markdown, ASCII-ize a few glyphs) ---
$md = Get-Content -Raw (Join-Path $Root 'README.md')
$t = $md `
    -replace '(?m)^\s{0,3}#{1,6}\s*', '' `
    -replace '(?m)^\s{0,3}>\s?', '' `
    -replace '\*\*', '' `
    -replace '`', '' `
    -replace '\[([^\]]+)\]\([^)]+\)', '$1'
# UTF-8 with BOM (PowerShell 5.1's utf8) - NSIS Unicode renders it, arrows and
# dashes included, so no glyph substitution is needed.
Set-Content -Path (Join-Path $Out 'readme.txt') -Value $t -Encoding utf8

Write-Host "Generated installer.bmp and readme.txt"
