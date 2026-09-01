# Builds an MSIX for the Microsoft Store.
#
#   installer\build-msix.ps1                      # self-signed, for local testing
#   installer\build-msix.ps1 -IdentityName 12345Publisher.NanoTorrent `
#                            -Publisher "CN=..." `
#                            -PublisherDisplayName "Power2All" -NoSign
#
# The three identity values come from Partner Center (Product ▸ Product identity)
# once the name is reserved. A package whose Identity does not match the
# reservation is rejected at upload, so the defaults here are only good enough to
# install locally and check that the app runs packaged.
#
# For a Store submission pass -NoSign: Microsoft signs the package itself, and a
# self-signed one would be rejected. Signing is only for sideloading a test build.
#
# Kept ASCII-only so Windows PowerShell 5.1, which reads .ps1 as the system
# codepage, parses it - same constraint as make-assets.ps1.

param(
    # Partner Center: Package/Identity/Name. Falls back to the environment so
    # the real values can live in one place instead of being pasted onto every
    # command line - CI sets the same names from its secrets.
    [string]$IdentityName = $(if ($env:STORE_IDENTITY_NAME) { $env:STORE_IDENTITY_NAME } else { "NanoTorrent.Test" }),

    # Partner Center: Package/Identity/Publisher, the CN=... string Microsoft
    # assigns. For a signed TEST build it must also match the certificate's
    # subject exactly, or makeappx and signtool disagree and Windows refuses to
    # install the package.
    [string]$Publisher = $(if ($env:STORE_PUBLISHER) { $env:STORE_PUBLISHER } else { "CN=NanoTorrent Test" }),

    [string]$PublisherDisplayName = $(if ($env:STORE_PUBLISHER_DISPLAY_NAME) { $env:STORE_PUBLISHER_DISPLAY_NAME } else { "Power2All" }),

    # Skip signing. Use this for a Store submission.
    [switch]$NoSign
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$stage = Join-Path $env:TEMP "nanotorrent-msix"
$assets = Join-Path $stage "Assets"

function Find-SdkTool($name) {
    $bases = @(
        "${env:ProgramFiles(x86)}\Windows Kits\10\bin",
        "$env:ProgramFiles\Windows Kits\10\bin"
    )
    $hits = foreach ($b in $bases) {
        if (Test-Path $b) {
            Get-ChildItem $b -Directory -ErrorAction SilentlyContinue |
                Sort-Object Name -Descending |
                ForEach-Object { Join-Path $_.FullName "x64\$name" } |
                Where-Object { Test-Path $_ }
        }
    }
    $tool = $hits | Select-Object -First 1
    if (-not $tool) { throw "$name not found - install the Windows 10/11 SDK." }
    $tool
}

# --- version: MSIX wants four parts, and the Store requires the last to be 0 --
$cargo = Get-Content (Join-Path $root "Cargo.toml")
$semver = ($cargo | Select-String '^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
$version = "$semver.0"
Write-Host "[1/5] Version $version"

# --- binaries ----------------------------------------------------------------
Write-Host "[2/5] Building release binaries..."
Push-Location $root
# cargo writes progress to stderr, and under ErrorActionPreference=Stop that
# alone is a terminating error. The exit code is the thing that matters.
$prev = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
cargo build --release
$code = $LASTEXITCODE
$ErrorActionPreference = $prev
Pop-Location
if ($code -ne 0) { throw "cargo build failed" }

foreach ($exe in @("nanotorrent-gui.exe", "nanotorrent-cli.exe")) {
    if (-not (Test-Path (Join-Path $root "target\release\$exe"))) {
        throw "$exe not found in target\release"
    }
}

if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Path $assets -Force | Out-Null
Copy-Item (Join-Path $root "target\release\nanotorrent-gui.exe") $stage
Copy-Item (Join-Path $root "target\release\nanotorrent-cli.exe") $stage

# --- logos -------------------------------------------------------------------
# Generated from res/app.png rather than checked in: one 256px source, and the
# Store's required sizes are just scalings of it.
Write-Host "[3/5] Generating logo assets..."
Add-Type -AssemblyName System.Drawing
$srcPath = Join-Path $root "res\app.png"
$sizes = @{
    "StoreLogo.png"         = @(50, 50)
    "Square44x44Logo.png"   = @(44, 44)
    "Square150x150Logo.png" = @(150, 150)
    "Wide310x150Logo.png"   = @(310, 150)
}
# Scale variants Windows picks between on high-DPI displays.
foreach ($s in 100, 125, 150, 200, 400) {
    $sizes["Square44x44Logo.scale-$s.png"] = @([int](44 * $s / 100), [int](44 * $s / 100))
    $sizes["Square150x150Logo.scale-$s.png"] = @([int](150 * $s / 100), [int](150 * $s / 100))
}
# Taskbar / Start list sizes.
foreach ($t in 16, 24, 32, 48, 256) {
    $sizes["Square44x44Logo.targetsize-$t.png"] = @($t, $t)
}

$src = [System.Drawing.Image]::FromFile($srcPath)
foreach ($name in $sizes.Keys) {
    $w, $h = $sizes[$name]
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.Clear([System.Drawing.Color]::Transparent)
    # Square logos take the whole box; the wide tile keeps the icon centred
    # rather than stretched.
    if ($w -eq $h) {
        $g.DrawImage($src, 0, 0, $w, $h)
    } else {
        $side = [Math]::Min($w, $h)
        $g.DrawImage($src, [int](($w - $side) / 2), [int](($h - $side) / 2), $side, $side)
    }
    $bmp.Save((Join-Path $assets $name), [System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose(); $bmp.Dispose()
}
$src.Dispose()
Write-Host "      $($sizes.Count) images"

# --- manifest ----------------------------------------------------------------
Write-Host "[4/5] Writing manifest..."
$manifest = Get-Content (Join-Path $PSScriptRoot "msix\AppxManifest.xml") -Raw
$manifest = $manifest.Replace("{IDENTITY_NAME}", $IdentityName)
$manifest = $manifest.Replace("{PUBLISHER}", $Publisher)
$manifest = $manifest.Replace("{PUBLISHER_DISPLAY_NAME}", $PublisherDisplayName)
$manifest = $manifest.Replace("{VERSION}", $version)
Set-Content (Join-Path $stage "AppxManifest.xml") $manifest -Encoding UTF8

# --- pack --------------------------------------------------------------------
Write-Host "[5/5] Packing..."
$out = Join-Path $PSScriptRoot "NanoTorrent-$semver-x64.msix"
if (Test-Path $out) { Remove-Item $out -Force }
& (Find-SdkTool "makeappx.exe") pack /d $stage /p $out /o
if ($LASTEXITCODE -ne 0) { throw "makeappx failed" }

if (-not $NoSign) {
    # A throwaway certificate, so the package can be installed locally. It is
    # NOT what a Store submission uses - Microsoft signs that itself.
    $cert = Get-ChildItem Cert:\CurrentUser\My |
        Where-Object { $_.Subject -eq $Publisher } | Select-Object -First 1
    if (-not $cert) {
        Write-Host "      creating a self-signed certificate for $Publisher"
        $cert = New-SelfSignedCertificate -Type Custom -Subject $Publisher `
            -KeyUsage DigitalSignature -FriendlyName "NanoTorrent MSIX test" `
            -CertStoreLocation "Cert:\CurrentUser\My" `
            -TextExtension @("2.5.29.37={text}1.3.6.1.5.5.7.3.3", "2.5.29.19={text}")
    }
    & (Find-SdkTool "signtool.exe") sign /fd SHA256 /a /sha1 $cert.Thumbprint $out
    if ($LASTEXITCODE -ne 0) { throw "signtool failed" }
    Write-Host ""
    Write-Host "Signed with a TEST certificate. To install it locally, trust the"
    Write-Host "certificate first (export it and import into Local Machine ->"
    Write-Host "Trusted People), then: Add-AppxPackage '$out'"
}

Write-Host ""
Write-Host "Built: $out"
Write-Host "  Identity : $IdentityName"
Write-Host "  Publisher: $Publisher"
if ($IdentityName -eq "NanoTorrent.Test") {
    Write-Host ""
    Write-Host "NOTE: this used the placeholder identity, so Partner Center will"
    Write-Host "reject it. Reserve the name first, then pass the real values."
}
if (-not $NoSign) {
    Write-Host "For a Store submission, rebuild with -NoSign and the identity"
    Write-Host "values from Partner Center."
}
