# Submit an update to the Microsoft Store from this machine.
#
#   installer\store-submit.ps1 -ProductId 9NBLGGH4XXXX
#   installer\store-submit.ps1
#   installer\store-submit.ps1 -ProductId 9NBLGGH4XXXX -Msix installer\NanoTorrent.msix
#   installer\store-submit.ps1 -DryRun
#
# The first form is the usual one. It needs the Store ID and the package
# identity, and rather than typing them every time, put them in
#
#   installer\store-settings.local.txt
#
# which is git-ignored:
#
#   STORE_PRODUCT_ID    = 9NBLGGH4XXXX
#   STORE_IDENTITY_NAME = 12345Publisher.NanoTorrent
#   STORE_PUBLISHER     = CN=00000000-0000-0000-0000-000000000000
#
# None of those three is a credential - all three are in the manifest of the
# package the Store ships - but this repository is public and the workflow
# keeps them as GitHub secrets, so they do not belong in a committed file
# either. A parameter beats the environment, and the environment beats this
# file, so a one-off submission can still override any of them.
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

    # Pick up a run that died part-way instead of starting it over.
    #
    #   build     all of it (the default)
    #   upload    the package is already built; upload it and carry on
    #   listings  the package is already uploaded; rewrite the listings on
    #   commit    the draft on the Store is right; commit it and wait
    #   wait      only watch a submission that is already committed
    #
    # The phases do not depend on each other's leftovers: `listings` re-reads
    # the draft from the Store, and `commit` and `wait` need nothing local at
    # all, so resuming never relies on temporary files a dead run left behind.
    #
    # This is about more than saving a build. The `upload` phase DELETES the
    # pending submission, so re-running a failed wait from the top would
    # destroy the very submission it was waiting on.
    [ValidateSet('build', 'upload', 'listings', 'commit', 'wait')]
    [string]$From = 'build',

    # Partner Center > Product identity > Name and Publisher. A package whose
    # identity does not match the reservation is rejected at upload, so these
    # are as required as the product ID - they are simply usually set once, in
    # the environment, rather than typed every time.
    [string]$IdentityName = $env:STORE_IDENTITY_NAME,
    [string]$Publisher = $env:STORE_PUBLISHER,
    # Resolved in the body rather than here, so that "given on the command
    # line" can be told apart from "left at its default" - the settings file
    # has to be able to set it, and cannot if the default already filled it in.
    [string]$PublisherDisplayName,

    # Where those settings live. Beside this script, not in the profile
    # folder: it belongs to the checkout, and a second clone should not
    # silently inherit the first one's Store identity.
    [string]$ConfigPath,

    # Leave the listing's screenshots alone. The default is to replace them
    # with `images/*.png`, which is what keeps them from going stale one
    # language at a time - but a submission that is only a rebuild does not
    # need the Store to re-review seven images.
    [switch]$KeepScreenshots,

    # Print every step without running any of them.
    [switch]$DryRun,

    # Commit without asking. A submission goes to certification and is public
    # when it passes, so the default is to ask first.
    [switch]$Yes
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

if (-not $ConfigPath) {
    $ConfigPath = Join-Path $PSScriptRoot "store-settings.local.txt"
}

# `NAME = value` per line, `#` comments, blank lines ignored. Deliberately not
# a .ps1 to dot-source: this file holds three strings, and a settings file that
# can run code is a settings file that eventually does.
function Read-LocalSettings([string]$path) {
    $found = @{}
    if (-not (Test-Path $path)) { return $found }
    foreach ($line in Get-Content -LiteralPath $path) {
        $text = $line.Trim()
        if (-not $text -or $text.StartsWith('#')) { continue }
        $at = $text.IndexOf('=')
        if ($at -lt 1) { continue }
        $name = $text.Substring(0, $at).Trim()
        $value = $text.Substring($at + 1).Trim()
        # Quotes are what anyone writes out of habit, and none of these values
        # legitimately begins and ends with one.
        if ($value.Length -ge 2 -and
            ((($value[0] -eq '"') -and ($value[-1] -eq '"')) -or
             (($value[0] -eq "'") -and ($value[-1] -eq "'")))) {
            $value = $value.Substring(1, $value.Length - 2)
        }
        $found[$name] = $value
    }
    return $found
}

$settings = Read-LocalSettings $ConfigPath
function Get-Setting([string]$name) {
    if ($settings.ContainsKey($name)) { return $settings[$name] }
    return $null
}

# Parameter, then environment, then the file. The parameter defaults above
# already folded the environment in, so anything still empty falls to the file.
if (-not $ProductId) { $ProductId = Get-Setting 'STORE_PRODUCT_ID' }
if (-not $IdentityName) { $IdentityName = Get-Setting 'STORE_IDENTITY_NAME' }
if (-not $Publisher) { $Publisher = Get-Setting 'STORE_PUBLISHER' }
if (-not $PublisherDisplayName) { $PublisherDisplayName = $env:STORE_PUBLISHER_DISPLAY_NAME }
if (-not $PublisherDisplayName) { $PublisherDisplayName = Get-Setting 'STORE_PUBLISHER_DISPLAY_NAME' }
if (-not $PublisherDisplayName) { $PublisherDisplayName = "Power2All" }

if (-not $ProductId) {
    throw @"
no Store product ID.

Give it as -ProductId, set STORE_PRODUCT_ID, or put it in
$ConfigPath :

  STORE_PRODUCT_ID    = 9NBLGGH4XXXX
  STORE_IDENTITY_NAME = 12345Publisher.NanoTorrent
  STORE_PUBLISHER     = CN=00000000-0000-0000-0000-000000000000

All three are on Partner Center > Product identity. That file is git-ignored.
"@
}

# The phases in the order they run; -From names the first one to actually do.
$PhaseOrder = @('build', 'upload', 'listings', 'commit', 'wait')
$StartPhase = $PhaseOrder.IndexOf($From)

function Test-Phase([string]$name) {
    return ($PhaseOrder.IndexOf($name) -ge $StartPhase)
}

# Read `msstore submission status` output and answer one question: is there a
# submission Microsoft already has, which deleting would pull back? Returns the
# state's name, or $null when there is nothing in flight.
#
# Text rather than a structured answer because the CLI gives no other. Two
# things it has to get right, both of them checked by the tests:
#
#   "Could not find a Pending Submission, but found the Last Published
#    Submission ... Submission Status = Published"  ->  nothing in flight,
#    because that status belongs to the LAST PUBLISHED submission, not a
#    pending one. Reading it as in-flight would block every normal run.
#
#   "Found Pending Submission ... Submission Status = CommitStarted"  ->  in
#    flight. Deleting that is the accident this exists to prevent.
function Get-InFlightState([string]$statusText) {
    if ($statusText -notmatch 'Found Pending Submission') { return $null }
    $m = [regex]::Match($statusText, 'Submission Status\s*=\s*(\w+)')
    if (-not $m.Success) { return $null }
    # The states a submission reaches only after it has been committed.
    $busy = @('CommitStarted', 'PreProcessing', 'Certification', 'Release',
        'Publishing', 'PendingPublication')
    if ($busy -contains $m.Groups[1].Value) { return $m.Groups[1].Value }
    return $null
}

function Invoke-Step {
    param(
        [string]$What,
        [scriptblock]$Do,
        # Which phase this step belongs to. Steps before the one -From names
        # are announced and skipped, so the log still shows the whole shape of
        # a run rather than starting abruptly in the middle.
        [ValidateSet('build', 'upload', 'listings', 'commit', 'wait')]
        [string]$Phase = 'build',
        # Total attempts. Stays at 1 for everything that changes Store state in
        # a way a second run would double up on - only the package upload sets
        # it higher, and the comment there says why that one is safe.
        [int]$Attempts = 1,
        # Waited between attempts, indexed by the attempt just failed; the last
        # entry repeats if there are more attempts than delays. A parameter so
        # the test can pass fractions of a second instead of a minute.
        [double[]]$BackoffSeconds = @(15, 45)
    )
    if (-not (Test-Phase $Phase)) {
        Write-Host ""
        Write-Host "==> $What" -ForegroundColor DarkGray
        Write-Host "    skipped (-From $From)" -ForegroundColor DarkGray
        return
    }
    Write-Host ""
    Write-Host "==> $What" -ForegroundColor Cyan
    if ($DryRun) {
        Write-Host "    (dry run) $($Do.ToString().Trim())" -ForegroundColor DarkGray
        return
    }
    for ($attempt = 1; $attempt -le $Attempts; $attempt++) {
        $failure = $null
        try {
            & $Do
            if ($LASTEXITCODE -ne 0 -and $null -ne $LASTEXITCODE) {
                $failure = "exit code $LASTEXITCODE"
            }
        } catch {
            # On the last time round let the original error out untouched: it
            # carries the position and type that this wrapper cannot reproduce,
            # which is what every non-retried step relied on before.
            if ($attempt -eq $Attempts) { throw }
            $failure = $_.Exception.Message
        }
        if (-not $failure) { return }
        if ($attempt -eq $Attempts) { throw "$What failed with $failure" }
        $wait = $BackoffSeconds[[Math]::Min($attempt - 1, $BackoffSeconds.Count - 1)]
        Write-Host "    attempt $attempt of $Attempts failed ($failure); retrying in $wait s" -ForegroundColor Yellow
        Start-Sleep -Seconds $wait
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

if (-not $Msix -and (Test-Phase 'build')) {
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

They are usually written once into
  $ConfigPath
rather than typed each time:
  STORE_IDENTITY_NAME = ...
  STORE_PUBLISHER     = ...
That file is git-ignored. The environment still works too.

A package built without them carries a placeholder identity and Partner
Center refuses it on upload.
"@
    }

    Invoke-Step "Build the MSIX" -Phase build -Do {
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
}

if (-not (Test-Phase 'build')) {
    Write-Host ""
    Write-Host "==> Build the MSIX" -ForegroundColor DarkGray
    Write-Host "    skipped (-From $From)" -ForegroundColor DarkGray
}

# Resolved out here rather than in the build block above: resuming at `upload`
# skips the build but still has to know which package to send, and the phases
# after it need no package at all.
if ((Test-Phase 'upload') -and -not (Test-Phase 'listings')) {
    if (-not $Msix) {
        $Msix = Join-Path $PSScriptRoot "NanoTorrent-$version-x64.msix"
    }
    if (-not $DryRun -and -not (Test-Path $Msix)) {
        throw "expected $Msix, but the build did not produce it"
    }
    Write-Host "package: $Msix"
}

# ---------------------------------------------------------------------------
# 2. Upload it, but leave the submission in draft
# ---------------------------------------------------------------------------
#
# --noCommit matters, and so does the order. For an app that is already
# published, `msstore publish` DELETES the pending draft and creates a new one
# from the last published submission - so any listing edits made first would be
# thrown away here. Package first, metadata second, commit last.

# The one retried step. `msstore publish` deletes the pending submission and
# then creates a new one from the last published submission, and the create
# fails outright when the delete has not propagated yet: the CLI prints
# "Error while creating submission. Please try again." and exits -1, having
# already done the delete. That leaves the product with no pending submission,
# which is exactly the state a fresh attempt wants.
#
# Running it again is safe for the same reason it is racy: it begins by
# deleting whatever it finds, so there is no half-made submission for a second
# attempt to trip over, and --noCommit means no retry can ever put something
# in front of certification.
# Look before deleting. `msstore publish` opens by removing the pending
# submission, which is right when that is an uncommitted draft and ruinous when
# it is one already sent for certification: the delete would pull back a
# release that is part-way through. The states below are the ones that mean
# Microsoft already has it.
if ((Test-Phase 'upload') -and -not (Test-Phase 'listings') -and -not $DryRun) {
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $state = (& msstore submission status $ProductId 2>&1 | Out-String)
    $ErrorActionPreference = $prev
    $inFlight = Get-InFlightState $state
    if ($inFlight) {
        throw @"
this product already has a submission in flight ($inFlight).

Uploading now would delete it, pulling back a release that is already part of
the way through certification. Nothing has been changed.

If you are picking up after the wait died, the submission itself is fine -
it just stopped being watched. Watch it again with:

  installer\store-submit.ps1 -From wait

If you really do mean to replace it, withdraw it in Partner Center first.
"@
    }
}

try {
    Invoke-Step "Upload the package (left as a draft)" -Phase upload -Attempts 3 -Do {
        msstore publish $Msix -id $ProductId --noCommit
    }
} catch {
    throw @"
$($_.Exception.Message)

All three attempts failed, so this is not the delete/create race the retry is
there for. The usual other cause is a submission already in certification:
Partner Center will not create a second one alongside it, and msstore reports
that with the same "Please try again". Check which it is:

  msstore submission status $ProductId

A pending submission means certification is still running - wait for it, or
withdraw it in Partner Center, then run this again. No pending submission and
a published one means the service simply refused; try once more.
"@
}

# ---------------------------------------------------------------------------
# 3. The listings
# ---------------------------------------------------------------------------
#
# `msstore publish` only ever uploads the package. Release notes come from
# MS_Store_Release_Info and screenshots from images/, which is why neither has
# to be attached in Partner Center by hand, once per language.

$draft = Join-Path $env:TEMP "nanotorrent-submission.json"
$updated = Join-Path $env:TEMP "nanotorrent-submission-updated.json"
$final = Join-Path $env:TEMP "nanotorrent-submission-final.json"
$shots = Join-Path $env:TEMP "nanotorrent-listing-images.zip"

Invoke-Step "Read the draft submission" -Phase listings -Do {
    msstore submission get $ProductId | Out-File -Encoding utf8 $draft
    if (-not (Test-Path $draft) -or (Get-Item $draft).Length -eq 0) {
        throw "msstore submission get returned nothing"
    }
}

Invoke-Step "Fold in this version's What's new, for every language" -Phase listings -Do {
    & (Join-Path $PSScriptRoot "store-whatsnew.ps1") `
        -SubmissionPath $draft -OutPath $updated -Version $version
}

# Screenshots are not metadata - their bytes have to be added to the archive
# the package went up in. store-images.ps1 explains why; the short version is
# that `msstore` has no command for it.
if ($KeepScreenshots) {
    $final = $updated
    Write-Host ""
    Write-Host "==> Screenshots left as they are (-KeepScreenshots)" -ForegroundColor Cyan
} else {
    Invoke-Step "Fold in the screenshots, for every language" -Phase listings -Do {
        & (Join-Path $PSScriptRoot "store-images.ps1") `
            -SubmissionPath $updated -OutPath $final -ZipPath $shots -Upload
    }
}

# --payload, not the JSON inline. Windows caps a command line at about 32,767
# characters and this submission carries 76 languages of descriptions and
# release notes, which is far past it - inline fails with "The filename or
# extension is too long". The CLI's own help singles this case out.
Invoke-Step "Send the listings back" -Phase listings -Do {
    msstore submission update $ProductId --payload $final
}

# ---------------------------------------------------------------------------
# 4. Commit
# ---------------------------------------------------------------------------

if ((Test-Phase 'commit') -and -not (Test-Phase 'wait') -and -not $Yes -and -not $DryRun) {
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

Invoke-Step "Commit the submission" -Phase commit -Do {
    msstore submission publish $ProductId
}

# Watching, not doing. By this point the submission is committed and in
# Microsoft's hands, so a poll that stops early takes nothing with it - the
# certification carries on regardless. Failing the script here would report a
# release as broken when all that broke was the watching of it, and would
# invite exactly the re-run that deletes the submission.
try {
    Invoke-Step "Wait for it" -Phase wait -Do {
        msstore submission poll $ProductId
    }
} catch {
    Write-Host ""
    Write-Host "The submission is committed; only the wait stopped early." -ForegroundColor Yellow
    Write-Host "  $($_.Exception.Message)" -ForegroundColor DarkGray
    Write-Host ""
    Write-Host "Nothing needs re-running. Check on it whenever you like:"
    Write-Host "  msstore submission status $ProductId"
    Write-Host "  installer\store-submit.ps1 -From wait"
    Write-Host ""
    Write-Host "Do NOT re-run this script from the top while it is in flight -" -ForegroundColor Yellow
    Write-Host "the upload phase would delete the submission." -ForegroundColor Yellow
    return
}

Write-Host ""
Write-Host "Submitted. Certification takes a while; Partner Center emails the result." -ForegroundColor Green
