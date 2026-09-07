# Submit an update to the Microsoft Store from this machine.
#
#   installer\store-submit.ps1 -ProductId 9NBLGGH4XXXX
#   installer\store-submit.ps1 -ProductId 9NBLGGH4XXXX -Msix installer\NanoTorrent.msix
#   installer\store-submit.ps1 -ProductId 9NBLGGH4XXXX -DryRun
#
# Does what .github/workflows/store-publish.yml does, without GitHub: build the
# package, upload it, rewrite the "What's new" text for every language from
# MS_Store_Release_Info, then commit the submission for certification.
#
# One-time setup on this machine:
#
#   winget install Microsoft.DotNet.DesktopRuntime.9
#   winget install "Microsoft Store Developer CLI"
#   msstore                      # first run walks through signing in
#
# Sign in with the Microsoft Entra ID account associated with the Partner
# Center account, NOT a personal Microsoft account - the CLI rejects an MSA.
#
# Kept ASCII-only and 5.1-compatible, like the other scripts here.

[CmdletBinding()]
param(
    # Partner Center > Product identity > Store ID. Twelve characters, starts
    # with a 9. Falls back to the environment so it need not be retyped.
    [string]$ProductId = $env:STORE_PRODUCT_ID,

    # Skip the build and submit this package instead. The identity below is
    # then whatever that package was built with, so it is not asked for.
    [string]$Msix,

    # Partner Center > Product identity > Name and Publisher. A package whose
    # identity does not match the reservation is rejected at upload, so these
    # are as required as the product ID - they are simply usually set once, in
    # the environment, rather than typed every time.
    [string]$IdentityName = $env:STORE_IDENTITY_NAME,
    [string]$Publisher = $env:STORE_PUBLISHER,
    [string]$PublisherDisplayName = $(
        if ($env:STORE_PUBLISHER_DISPLAY_NAME) { $env:STORE_PUBLISHER_DISPLAY_NAME } else { "Power2All" }
    ),

    # Print every step without running any of them.
    [switch]$DryRun,

    # Commit without asking. A submission goes to certification and is public
    # when it passes, so the default is to ask first.
    [switch]$Yes
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

if (-not $ProductId) {
    throw "no -ProductId, and STORE_PRODUCT_ID is not set. Partner Center > Product identity > Store ID."
}

function Invoke-Step {
    param([string]$What, [scriptblock]$Do)
    Write-Host ""
    Write-Host "==> $What" -ForegroundColor Cyan
    if ($DryRun) {
        Write-Host "    (dry run) $($Do.ToString().Trim())" -ForegroundColor DarkGray
        return
    }
    & $Do
    if ($LASTEXITCODE -ne 0 -and $null -ne $LASTEXITCODE) {
        throw "$What failed with exit code $LASTEXITCODE"
    }
}

# The version the listings have to describe. Read from the manifest rather than
# passed in, because the one thing worse than a stale listing is a listing that
# describes a version nobody shipped.
$version = (Select-String -Path (Join-Path $root "Cargo.toml") -Pattern '^version = "(.+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
Write-Host "NanoTorrent $version -> Store product $ProductId"

if (-not $DryRun -and -not (Get-Command msstore -ErrorAction SilentlyContinue)) {
    throw "msstore is not on PATH. Install it with: winget install `"Microsoft Store Developer CLI`""
}

# Asked BEFORE the build. `msstore info` exits non-zero when the CLI has never
# been configured, and finding that out at the upload step costs a full release
# build and a makeappx run first.
#
# stderr goes to $null rather than through 2>&1: merging a native command's
# stderr into the pipeline under ErrorActionPreference=Stop turns each line into
# a NativeCommandError, which would fail here for the wrong reason.
if (-not $DryRun) {
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & msstore info > $null 2> $null
    $configured = ($LASTEXITCODE -eq 0)
    $ErrorActionPreference = $prev

    if (-not $configured) {
        throw @"
msstore is installed but has not been configured on this machine yet.

Run this once, then try again:
  msstore reconfigure

It asks for the tenant, seller and client IDs and a client secret, all from
Partner Center. Sign in with the Microsoft Entra ID account associated with the
Partner Center account - the CLI refuses a personal Microsoft account.
"@
    }
}

# ---------------------------------------------------------------------------
# 1. The package
# ---------------------------------------------------------------------------

if (-not $Msix) {
    # Checked BEFORE the build, not after it. build-msix.ps1 warns about a
    # placeholder identity and carries on, which is right for a local test
    # build and wrong here: the package would be rejected at upload, having
    # cost a full release build first.
    $missing = @()
    if (-not $IdentityName) { $missing += "-IdentityName (or STORE_IDENTITY_NAME)" }
    if (-not $Publisher) { $missing += "-Publisher (or STORE_PUBLISHER)" }
    if ($missing) {
        throw @"
missing the Store package identity: $($missing -join ', ')

Both come from Partner Center > Product identity, next to the Store ID:
  Name      -> -IdentityName        e.g. 12345Publisher.NanoTorrent
  Publisher -> -Publisher           e.g. CN=<guid>

They are usually set once in the environment rather than typed each time:
  `$env:STORE_IDENTITY_NAME = '...'
  `$env:STORE_PUBLISHER     = '...'

A package built without them carries a placeholder identity and Partner
Center refuses it on upload.
"@
    }

    Invoke-Step "Build the MSIX" {
        & (Join-Path $PSScriptRoot "build-msix.ps1") `
            -IdentityName $IdentityName `
            -Publisher $Publisher `
            -PublisherDisplayName $PublisherDisplayName `
            -NoSign
    }
    # By exact name, not by globbing. build-msix.ps1 writes
    # NanoTorrent-<version>-x64.msix and this folder keeps one per release, so
    # `*.msix | Select-Object -First 1` picks whichever sorts first - the
    # OLDEST version present - and would upload that to the Store.
    $Msix = Join-Path $PSScriptRoot "NanoTorrent-$version-x64.msix"
    if (-not $DryRun -and -not (Test-Path $Msix)) {
        throw "expected $Msix, but the build did not produce it"
    }
}
Write-Host "package: $Msix"

# ---------------------------------------------------------------------------
# 2. Upload it, but leave the submission in draft
# ---------------------------------------------------------------------------
#
# --noCommit matters, and so does the order. For an app that is already
# published, `msstore publish` DELETES the pending draft and creates a new one
# from the last published submission - so any listing edits made first would be
# thrown away here. Package first, metadata second, commit last.

Invoke-Step "Upload the package (left as a draft)" {
    msstore publish $Msix -id $ProductId --noCommit
}

# ---------------------------------------------------------------------------
# 3. The listings
# ---------------------------------------------------------------------------
#
# `msstore publish` only ever uploads the package. Release notes come from
# MS_Store_Release_Info, which is why they no longer have to be pasted into
# Partner Center by hand, once per language.

$draft = Join-Path $env:TEMP "nanotorrent-submission.json"
$updated = Join-Path $env:TEMP "nanotorrent-submission-updated.json"

Invoke-Step "Read the draft submission" {
    msstore submission get $ProductId | Out-File -Encoding utf8 $draft
    if (-not (Test-Path $draft) -or (Get-Item $draft).Length -eq 0) {
        throw "msstore submission get returned nothing"
    }
}

Invoke-Step "Fold in this version's What's new, for every language" {
    & (Join-Path $PSScriptRoot "store-whatsnew.ps1") `
        -SubmissionPath $draft -OutPath $updated -Version $version
}

# --payload, not the JSON inline. Windows caps a command line at about 32,767
# characters and this submission carries 41 languages of descriptions and
# release notes, which is far past it - inline fails with "The filename or
# extension is too long". The CLI's own help singles this case out.
Invoke-Step "Send the listings back" {
    msstore submission update $ProductId --payload $updated
}

# ---------------------------------------------------------------------------
# 4. Commit
# ---------------------------------------------------------------------------

if (-not $Yes -and -not $DryRun) {
    Write-Host ""
    Write-Host "The package is uploaded and the listings are written, still as a draft."
    Write-Host "Committing sends it to certification; it goes live when that passes."
    $answer = Read-Host "Commit the submission? [y/N]"
    if ($answer -notmatch '^(y|yes)$') {
        Write-Host ""
        Write-Host "Left as a draft. Review it in Partner Center, then either press Submit"
        Write-Host "there or re-run this with -Yes. To throw it away:"
        Write-Host "  msstore submission delete $ProductId"
        return
    }
}

Invoke-Step "Commit the submission" {
    msstore submission publish $ProductId
}

Invoke-Step "Wait for it" {
    msstore submission poll $ProductId
}

Write-Host ""
Write-Host "Submitted. Certification takes a while; Partner Center emails the result." -ForegroundColor Green
