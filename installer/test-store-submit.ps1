# Tests for store-submit.ps1's Invoke-Step and Test-Phase, which cannot be
# exercised by running the real thing: every path through it ends at a live
# Store submission.
#
# Both functions are lifted out of the script by AST rather than copied, so
# this cannot quietly drift from what actually runs. Nothing here touches the
# network or Partner Center.
#
#   pwsh -NoProfile -File installer/test-store-submit.ps1
$ErrorActionPreference = 'Stop'
$src = Join-Path $PSScriptRoot "store-submit.ps1"

$ast = [System.Management.Automation.Language.Parser]::ParseFile($src, [ref]$null, [ref]$null)
foreach ($want in @('Invoke-Step', 'Test-Phase', 'Get-InFlightState', 'Select-DispatchedRun', 'Test-Windows')) {
    $fn = $ast.FindAll({
            param($n)
            $n -is [System.Management.Automation.Language.FunctionDefinitionAst]
        }, $true) | Where-Object { $_.Name -eq $want } | Select-Object -First 1
    if ($null -eq $fn) { throw "$want not found in $src" }
    . ([scriptblock]::Create($fn.Extent.Text))
}

# The context those two read. Kept in step with the script by the last test
# below, which reads the real list back out of it.
$PhaseOrder = @('build', 'upload', 'listings', 'commit', 'wait')
$From = 'build'
$StartPhase = 0
$DryRun = $false

$fails = 0
$pass = 0

function Check([string]$name, [scriptblock]$body) {
    try {
        & $body
        $script:pass++
        Write-Output "  PASS  $name"
    } catch {
        $script:fails++
        Write-Output "  FAIL  $name -> $($_.Exception.Message)"
    }
}

# A native command that exited with $code, which is all Invoke-Step looks at.
# Set directly rather than by running `cmd /c exit`, which only Windows has -
# these tests run wherever the submission does, and that includes Linux.
function Exit-Native([int]$code) { $global:LASTEXITCODE = $code }

Write-Output "Invoke-Step"

# 1. The ordinary case: one attempt, succeeds, runs the body exactly once.
Check "succeeds first time, body runs once" {
    $script:n = 0
    Invoke-Step "ok" -Do { $script:n++; Exit-Native 0 } 6> $null
    if ($script:n -ne 1) { throw "body ran $($script:n) times" }
}

# 2. A flaky step: fails twice, then succeeds. This is the whole point.
Check "retries a flaky step and stops once it works" {
    $script:n = 0
    Invoke-Step "flaky" -Attempts 3 -BackoffSeconds @(0.01) -Do {
        $script:n++
        if ($script:n -lt 3) { Exit-Native 7 } else { Exit-Native 0 }
    } 6> $null
    if ($script:n -ne 3) { throw "body ran $($script:n) times, expected 3" }
}

# 3. Exhausting the attempts still throws, and names the exit code.
Check "gives up after the last attempt" {
    $script:n = 0
    $msg = $null
    try {
        Invoke-Step "doomed" -Attempts 3 -BackoffSeconds @(0.01) -Do {
            $script:n++; Exit-Native 9
        } 6> $null
    } catch { $msg = $_.Exception.Message }
    if ($script:n -ne 3) { throw "body ran $($script:n) times, expected 3" }
    if ($msg -notlike "*doomed failed with exit code 9*") { throw "message was: $msg" }
}

# 4. Default is NO retry - a step that changes state must not run twice just
#    because someone forgot a parameter.
Check "does not retry by default" {
    $script:n = 0
    try {
        Invoke-Step "once" -Do { $script:n++; Exit-Native 3 } 6> $null
    } catch { }
    if ($script:n -ne 1) { throw "body ran $($script:n) times, expected 1" }
}

# 5. The pre-existing message is unchanged for the non-retried steps.
Check "keeps the original exit-code message" {
    $msg = $null
    try {
        Invoke-Step "Build the MSIX" -Do { Exit-Native 1 } 6> $null
    } catch { $msg = $_.Exception.Message }
    if ($msg -ne "Build the MSIX failed with exit code 1") { throw "message was: $msg" }
}

# 6. A thrown error on the final attempt comes out untouched, keeping the type
#    and position the wrapper cannot reproduce.
Check "rethrows a real exception rather than restating it" {
    $msg = $null
    try {
        Invoke-Step "throwing" -Do { throw "the original words" } 6> $null
    } catch { $msg = $_.Exception.Message }
    if ($msg -ne "the original words") { throw "message was: $msg" }
}

# 7. An exception is retried too, not only a bad exit code.
Check "retries a step that throws, then succeeds" {
    $script:n = 0
    Invoke-Step "throwing then fine" -Attempts 3 -BackoffSeconds @(0.01) -Do {
        $script:n++
        if ($script:n -lt 2) { throw "not yet" }
        Exit-Native 0
    } 6> $null
    if ($script:n -ne 2) { throw "body ran $($script:n) times, expected 2" }
}

# 8. Backoff runs out of entries gracefully - the last delay repeats.
Check "reuses the last backoff when attempts outnumber delays" {
    $script:n = 0
    $t = [Diagnostics.Stopwatch]::StartNew()
    try {
        Invoke-Step "many" -Attempts 4 -BackoffSeconds @(0.01, 0.02) -Do {
            $script:n++; Exit-Native 1
        } 6> $null
    } catch { }
    $t.Stop()
    if ($script:n -ne 4) { throw "body ran $($script:n) times, expected 4" }
    if ($t.Elapsed.TotalSeconds -gt 5) { throw "waited $($t.Elapsed.TotalSeconds)s - backoff ignored?" }
}

# 9. DryRun still short-circuits, and must not run the body at all.
Check "dry run does not execute the body" {
    $script:n = 0
    $DryRun = $true
    Invoke-Step "dry" -Attempts 3 -Do { $script:n++ } 6> $null
    $DryRun = $false
    if ($script:n -ne 0) { throw "body ran $($script:n) times under -DryRun" }
}

Write-Output ""
Write-Output "Test-Phase and -From"

# 10. Starting at the beginning runs everything.
Check "-From build runs every phase" {
    $script:From = 'build'; $script:StartPhase = 0
    foreach ($p in $PhaseOrder) {
        if (-not (Test-Phase $p)) { throw "$p was skipped" }
    }
}

# 11. Resuming skips what is behind and runs what is at or ahead of it. This is
#     the property that keeps `-From wait` from deleting the submission.
Check "-From commit skips build/upload/listings, keeps commit and wait" {
    $script:From = 'commit'; $script:StartPhase = $PhaseOrder.IndexOf('commit')
    foreach ($p in @('build', 'upload', 'listings')) {
        if (Test-Phase $p) { throw "$p should have been skipped" }
    }
    foreach ($p in @('commit', 'wait')) {
        if (-not (Test-Phase $p)) { throw "$p should have run" }
    }
}

# 12. The last phase on its own: nothing before it may run, upload least of all.
Check "-From wait runs only the wait" {
    $script:From = 'wait'; $script:StartPhase = $PhaseOrder.IndexOf('wait')
    foreach ($p in @('build', 'upload', 'listings', 'commit')) {
        if (Test-Phase $p) { throw "$p should have been skipped" }
    }
    if (-not (Test-Phase 'wait')) { throw "wait should have run" }
}

# 13. A skipped step must not execute its body - the whole point of resuming.
Check "a skipped step does not run its body" {
    $script:From = 'wait'; $script:StartPhase = $PhaseOrder.IndexOf('wait')
    $script:n = 0
    Invoke-Step "would delete the submission" -Phase upload -Do { $script:n++ } 6> $null
    if ($script:n -ne 0) { throw "body ran $($script:n) times despite being skipped" }
}

# 14. ...and a step at or after the start still does.
Check "a step at the resume point still runs" {
    $script:From = 'wait'; $script:StartPhase = $PhaseOrder.IndexOf('wait')
    $script:n = 0
    Invoke-Step "poll" -Phase wait -Do { $script:n++; Exit-Native 0 } 6> $null
    if ($script:n -ne 1) { throw "body ran $($script:n) times, expected 1" }
}

$script:From = 'build'; $script:StartPhase = 0

# 15. The phase list here, the one in the script and the values -From accepts
#     all have to agree, or a resume point silently does nothing.
Check "the phase list matches the script and the ValidateSet" {
    $text = Get-Content -Raw $src
    $m = [regex]::Match($text, '\$PhaseOrder\s*=\s*@\(([^)]*)\)')
    if (-not $m.Success) { throw "no `$PhaseOrder in the script" }
    $inScript = ([regex]::Matches($m.Groups[1].Value, "'([a-z]+)'") |
        ForEach-Object { $_.Groups[1].Value })
    if (($inScript -join ',') -ne ($PhaseOrder -join ',')) {
        throw "script has $($inScript -join ','), test has $($PhaseOrder -join ',')"
    }
    $v = [regex]::Match($text, "\[ValidateSet\(('build'[^)]*)\)\]\s*\r?\n\s*\[string\]\`$From")
    if (-not $v.Success) { throw "no ValidateSet on -From" }
    $inSet = ([regex]::Matches($v.Groups[1].Value, "'([a-z]+)'") |
        ForEach-Object { $_.Groups[1].Value })
    if (($inSet -join ',') -ne ($PhaseOrder -join ',')) {
        throw "-From accepts $($inSet -join ','), phases are $($PhaseOrder -join ',')"
    }
}

# 16. Every Invoke-Step in the script names a phase, or it would default to
#     'build' and run during every resume - including the destructive upload.
Check "every step in the script declares its phase" {
    $text = Get-Content -Raw $src
    $bad = @()
    foreach ($m in [regex]::Matches($text, 'Invoke-Step\s+"([^"]+)"([^\r\n]*)')) {
        if ($m.Groups[2].Value -notmatch '-Phase\s+\w+') {
            $bad += $m.Groups[1].Value
        }
    }
    if ($bad.Count -gt 0) { throw "no -Phase on: $($bad -join '; ')" }
}

Write-Output ""
Write-Output "Get-InFlightState"

# The two strings below are verbatim `msstore submission status` output, copied
# from real runs against this product. They are the whole reason this function
# can be trusted: the difference between them is one word, and reading it the
# wrong way either blocks every ordinary release or deletes a live one.

# 17. Nothing pending. The status line belongs to the LAST PUBLISHED
#     submission, so it must not be read as something in flight - otherwise no
#     release could ever start.
Check "no pending submission is not in flight" {
    $text = @"
Could not find a Pending Submission, but found the Last Published Submission.
Retrieving Last Published Submission
Submission Status = Published
"@
    $got = Get-InFlightState $text
    if ($null -ne $got) { throw "said '$got', expected nothing" }
}

# 17b. The case that makes the "Found Pending Submission" check earn its place:
#      no pending submission at all, but the LAST PUBLISHED one is mid-rollout
#      and so reports a state that IS on the busy list. Matching on the status
#      alone would refuse to start a new submission here, which is wrong - the
#      previous release rolling out is no reason to block the next one.
Check "a busy status on the last PUBLISHED submission is not in flight" {
    $text = @"
Could not find a Pending Submission, but found the Last Published Submission.
Retrieving Last Published Submission
Submission Status = Release
"@
    $got = Get-InFlightState $text
    if ($null -ne $got) { throw "said '$got' - would block a release with nothing pending" }
}

# 18. The case that actually happened: committed, mid-certification. Uploading
#     over this would pull back a release Microsoft already has.
Check "a committed submission is in flight" {
    $text = @"
Found Pending Submission.
Retrieving Pending Submission
Submission Status = CommitStarted
"@
    $got = Get-InFlightState $text
    if ($got -ne 'CommitStarted') { throw "said '$got', expected CommitStarted" }
}

# 19. A pending draft that was never committed is exactly what the upload phase
#     is meant to replace, so it must NOT be treated as in flight.
Check "an uncommitted draft is not in flight" {
    $text = @"
Found Pending Submission.
Retrieving Pending Submission
Submission Status = PendingCommit
"@
    $got = Get-InFlightState $text
    if ($null -ne $got) { throw "said '$got', expected nothing - this blocks normal releases" }
}

# 20. The later stages of certification are in flight too, not just the first.
Check "every post-commit state counts as in flight" {
    foreach ($s in @('CommitStarted', 'PreProcessing', 'Certification',
            'Release', 'Publishing', 'PendingPublication')) {
        $text = "Found Pending Submission.`nSubmission Status = $s"
        if ((Get-InFlightState $text) -ne $s) { throw "$s was not caught" }
    }
}

# 21. Garbage in, nothing out - a guard that throws on unexpected output would
#     block releases for a CLI message change.
Check "unparseable output is not in flight" {
    foreach ($text in @('', 'something went wrong', 'Found Pending Submission.')) {
        $got = Get-InFlightState $text
        if ($null -ne $got) { throw "said '$got' for: $text" }
    }
}

# 22. The guards around the upload phase, read out of the script itself.
#
#     `$Msix` is resolved, and the in-flight check that refuses to clobber a
#     submission already in certification is made, under an `if` that has to be
#     true exactly when the upload phase runs. A guard of
#     `(Test-Phase 'upload') -and -not (Test-Phase 'listings')` reads as if it
#     says that and is false for EVERY -From, because Test-Phase means "this
#     phase is at or after the start", not "this is the phase". That shipped:
#     the package path stayed empty, msstore fell back to the working directory
#     and failed with "We could not find a project publisher", and the in-flight
#     check silently never ran at all.
Check "every upload-phase guard is true exactly when upload runs" {
    $ifs = $ast.FindAll({
            param($n)
            $n -is [System.Management.Automation.Language.IfStatementAst]
        }, $true)

    $guards = @()
    foreach ($i in $ifs) {
        $cond = $i.Clauses[0].Item1.Extent.Text
        if ($cond -match "Test-Phase\s+'upload'") { $guards += $cond }
    }
    if ($guards.Count -lt 2) {
        throw "found $($guards.Count) upload guards in store-submit.ps1, expected at least 2"
    }

    foreach ($cond in $guards) {
        foreach ($from in $PhaseOrder) {
            $script:StartPhase = $PhaseOrder.IndexOf($from)
            $script:DryRun = $false
            $want = ($PhaseOrder.IndexOf('upload') -ge $script:StartPhase)
            $got = [bool](& ([scriptblock]::Create($cond)))
            if ($got -ne $want) {
                throw "-From $from : '$cond' gave $got, expected $want"
            }
        }
    }
    $script:StartPhase = 0
}

Write-Output ""
Write-Output "Select-DispatchedRun"

# `gh run list --json databaseId,headSha,createdAt`, as it prints it. The
# dispatch happened at 12:00; this machine asks for anything from 11:59 on.
$since = [datetime]::Parse('2026-09-30T11:59:00Z', [Globalization.CultureInfo]::InvariantCulture,
    [Globalization.DateTimeStyles]::AdjustToUniversal)
$runs = @"
[
  {"createdAt":"2026-09-30T12:00:04Z","databaseId":502,"headSha":"bbbb"},
  {"createdAt":"2026-09-30T12:00:02Z","databaseId":501,"headSha":"aaaa"},
  {"createdAt":"2026-09-29T08:00:00Z","databaseId":400,"headSha":"aaaa"}
]
"@

# The run for this commit, not simply the newest - 502 is someone else's.
Check "picks the run for this commit, not the newest" {
    $got = Select-DispatchedRun $runs 'aaaa' $since
    if ($got -ne 501) { throw "picked $got, expected 501" }
}

# Yesterday's build of the same commit is not the one just started. Adopting
# it would be harmless only by luck - its package might be from a different
# workflow version entirely.
Check "an older run of the same commit is not adopted" {
    $old = @"
[{"createdAt":"2026-09-29T08:00:00Z","databaseId":400,"headSha":"aaaa"}]
"@
    $got = Select-DispatchedRun $old 'aaaa' $since
    if ($null -ne $got) { throw "picked $got, expected nothing yet" }
}

# gh prints `[]` until the dispatch shows up; that is "keep polling".
Check "no runs yet is nothing, not an error" {
    $got = Select-DispatchedRun '[]' 'aaaa' $since
    if ($null -ne $got) { throw "picked $got" }
}

Check "Test-Windows answers for the platform this runs on" {
    $want = ($PSVersionTable.PSEdition -eq 'Desktop') -or [bool]$IsWindows
    if ((Test-Windows) -ne $want) { throw "said $(Test-Windows)" }
}

Write-Output ""
Write-Output "$pass passed, $fails failed"
# Explicit, because the last native call above leaves $LASTEXITCODE non-zero
# and the shell would otherwise report a clean run as a failure.
if ($fails -gt 0) { exit 1 } else { exit 0 }
