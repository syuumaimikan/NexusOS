<#
.SYNOPSIS
    Drives the nexus-collab binary through a whole session and checks what it
    does, including what it refuses to do.

.DESCRIPTION
    The crate's own tests cover the library: timestamps, path coverage, atomic
    writes, backups. What they cannot cover is the program -- argument parsing,
    exit codes, and the messages a person actually reads, which is the half
    somebody relies on at three in the morning when the state file is wrong.

    So this runs the real binary against a scratch directory. Nothing here
    touches the repository's own .ai_collaboration.

    The refusals are the point. A tool that merely *usually* declines to break
    the other agent's lock is a tool that will break it, and the protocol names
    that as the one thing neither agent may do.
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$RepoRoot = Split-Path -Parent $PSScriptRoot
$failures = @()
$checks = 0

Write-Host '==> Building nexus-collab' -ForegroundColor Cyan
Push-Location $RepoRoot
try {
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & cargo build -p nexus-collab 2>&1 | ForEach-Object { Write-Host "    $_" -ForegroundColor DarkGray }
    $ErrorActionPreference = $previous
    if ($LASTEXITCODE -ne 0) { throw "nexus-collab did not build (exit $LASTEXITCODE)" }
} finally { Pop-Location }

$Exe = Join-Path $RepoRoot 'target\debug\nexus-collab.exe'
if (-not (Test-Path $Exe)) { throw "no binary at $Exe" }

# A directory of its own, so that a test can never damage the real one.
$Scratch = Join-Path ([System.IO.Path]::GetTempPath()) "nexus-collab-session-$PID"
if (Test-Path $Scratch) { Remove-Item $Scratch -Recurse -Force }
New-Item -ItemType Directory -Force -Path (Join-Path $Scratch '.ai_collaboration') | Out-Null

$state = @'
{
  "schema_version": 1,
  "updated_at": "2026-09-14T12:00:00+00:00",
  "agents": {
    "claude_code": {
      "status": "idle",
      "current_task": null,
      "active_paths": [],
      "last_activity": "2026-09-14T12:00:00+00:00"
    },
    "gpt6_astra": {
      "status": "working",
      "current_task": "ASTRA-1",
      "active_paths": ["shared/nexus-ai/"],
      "last_activity": "2026-09-14T12:00:00+00:00"
    }
  },
  "tasks": [
    {"id": "ASTRA-1", "status": "working", "owner": "gpt6_astra",
     "summary": "something of theirs", "dependencies": []}
  ],
  "recent_changes": []
}
'@
# WriteAllText rather than Set-Content: Windows PowerShell's `-Encoding utf8`
# writes a byte-order mark, and a fixture that differs from a real file in a
# way the test did not intend is a test that fails for the wrong reason. (The
# tool reads past a BOM now, because real files do have them -- but the fixture
# should still be the thing it is meant to be.)
$utf8 = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllText((Join-Path $Scratch '.ai_collaboration\STATE.json'), $state, $utf8)

# Astra holds a lock, written the way their tools write one: created_at and no
# heartbeat, which this has to read.
#
# Dated *now*, not with a fixed date. A lock nothing has touched for four hours
# is stale, and a fixed date in a test fixture becomes stale the day after it
# is written -- at which point every reading command below reports a problem
# and exits 2, and the test fails for a reason that has nothing to do with the
# thing it is testing. Staleness gets its own check further down, with a lock
# made old on purpose.
New-Item -ItemType Directory -Force -Path (Join-Path $Scratch '.ai_collaboration\locks') | Out-Null
$now = [DateTime]::UtcNow.ToString('yyyy-MM-ddTHH:mm:ss+00:00')
$theirs = @"
{
  "agent": "gpt6_astra",
  "task": "ASTRA-1",
  "paths": ["shared/nexus-ai/", "docs/AI/"],
  "created_at": "$now"
}
"@
[System.IO.File]::WriteAllText((Join-Path $Scratch '.ai_collaboration\locks\ASTRA-1.json'), $theirs, $utf8)

<#
.SYNOPSIS
    Run the binary in the scratch directory and check what came back.
#>
function Test-Collab {
    param(
        [string]$What,
        [string[]]$Arguments,
        [int]$Expect = 0,
        [string[]]$Says = @(),
        [string[]]$DoesNotSay = @()
    )
    $script:checks++
    Push-Location $Scratch
    try {
        # Through files rather than `2>&1 |`. Windows PowerShell wraps each
        # line a native program writes to stderr in its own ErrorRecord, and a
        # message that spans two lines comes back with the second one decorated
        # out of recognition -- which cost a real check here: the refusal that
        # names a mailbox puts the name on its second line, and the test said
        # the tool never printed it when the tool printed it perfectly well.
        $outFile = [System.IO.Path]::GetTempFileName()
        $errFile = [System.IO.Path]::GetTempFileName()
        $run = Start-Process -FilePath $Exe -ArgumentList $Arguments -NoNewWindow -Wait -PassThru `
            -RedirectStandardOutput $outFile -RedirectStandardError $errFile `
            -WorkingDirectory $Scratch
        # `-Raw` on an empty file gives $null, not '', and a command that says
        # nothing is an ordinary outcome here.
        $said = Get-Content $outFile -Raw
        $complained = Get-Content $errFile -Raw
        if ($null -eq $said) { $said = '' }
        if ($null -eq $complained) { $complained = '' }
        $output = $said + $complained
        $code = $run.ExitCode
        Remove-Item $outFile, $errFile -Force -ErrorAction SilentlyContinue
    } finally { Pop-Location }

    $wrong = @()
    if ($code -ne $Expect) { $wrong += "exited $code, expected $Expect" }
    foreach ($phrase in $Says) {
        if (-not $output.Contains($phrase)) { $wrong += "never said '$phrase'" }
    }
    foreach ($phrase in $DoesNotSay) {
        if ($output.Contains($phrase)) { $wrong += "said '$phrase', and should not have" }
    }

    if ($wrong.Count -eq 0) {
        Write-Host "    ok   $What" -ForegroundColor DarkGray
    } else {
        foreach ($why in $wrong) { $script:failures += "$What : $why" }
        Write-Host "    FAIL $What" -ForegroundColor Red
        foreach ($line in ($output -split "`n" | Select-Object -First 6)) {
            Write-Host "         $line" -ForegroundColor DarkRed
        }
    }
}

$env:NEXUS_AGENT = 'claude_code'

Write-Host ''
Write-Host '==> Reading' -ForegroundColor Cyan
Test-Collab 'status reads the directory' @('status') -Says @('gpt6_astra', 'ASTRA-1')
Test-Collab 'a good state has nothing wrong with it' @('check') -Says @('nothing wrong')
Test-Collab 'tasks can be listed' @('task', 'list') -Says @('ASTRA-1')
Test-Collab 'and filtered by owner' @('task', 'list', '--owner', 'claude_code') `
    -DoesNotSay @('ASTRA-1')
Test-Collab 'json comes out as json' @('lock', 'list', '--json') -Says @('"agent": "gpt6_astra"')

Write-Host ''
Write-Host '==> What it refuses' -ForegroundColor Cyan
Test-Collab 'it will not release another agent''s lock' `
    @('lock', 'release', 'ASTRA-1') -Expect 1 `
    -Says @('will not release', 'gpt6_astra')
Test-Collab 'it will not take a path another agent holds' `
    @('lock', 'take', 'CLAUDE-1', 'shared/nexus-ai/src/lib.rs') -Expect 1 `
    -Says @('clashes with', 'gpt6_astra')
Test-Collab 'a name that merely starts the same is not inside' `
    @('lock', 'take', 'CLAUDE-1', 'shared/nexus-ai-core/src/lib.rs') -Expect 0
# ...and given straight back, so the count below is about the session's own
# lock and not about this one.
Test-Collab 'and that lock can be given back' `
    @('lock', 'release', 'CLAUDE-1') -Expect 0
Test-Collab 'it will not add a task that is already there' `
    @('task', 'add', 'ASTRA-1', '--summary', 'mine now') -Expect 1 `
    -Says @('already a task')
Test-Collab 'it will not set a task that is not there' `
    @('task', 'set', 'NOBODY-9', '--status', 'completed') -Expect 1 `
    -Says @('no task called NOBODY-9')
Test-Collab 'a lock on nothing claims nothing' `
    @('lock', 'take', 'CLAUDE-2') -Expect 1
Test-Collab 'an unknown command says so rather than doing nothing quietly' `
    @('frobnicate') -Expect 1 -Says @('no `frobnicate` command')

Write-Host ''
Write-Host '==> A session' -ForegroundColor Cyan
Test-Collab 'a lock can be taken' `
    @('lock', 'take', 'CLAUDE-7', 'kernel/', 'user/nexus-view/') `
    -Says @('claude_code holds')
Test-Collab 'a task can be recorded' `
    @('task', 'add', 'CLAUDE-7', '--summary', 'a thing', '--priority', 'high') `
    -Says @('CLAUDE-7 added')
Test-Collab 'where you are working can be said' `
    @('paths', 'set', 'kernel/', 'user/nexus-view/') -Says @('claude_code is working in')
Test-Collab 'and what you are doing' `
    @('state', 'working', '--task', 'CLAUDE-7') -Says @('claude_code is working')
Test-Collab 'a heartbeat touches your own locks' `
    @('heartbeat') -Says @('1 lock(s) refreshed')
Test-Collab 'a task can be finished with evidence' `
    @('task', 'set', 'CLAUDE-7', '--status', 'completed', '--verification', 'it ran') `
    -Says @('status=completed')
Test-Collab 'and the lock given back' `
    @('lock', 'release', 'CLAUDE-7') -Says @('CLAUDE-7 released')

Write-Host ''
Write-Host '==> Backups and recovery' -ForegroundColor Cyan
$backups = Join-Path $Scratch '.ai_collaboration\backups'
if (Test-Path $backups) {
    $count = (Get-ChildItem $backups -Filter 'STATE-*.json').Count
    $checks++
    # Six commands above wrote the state; the first had nothing to copy.
    if ($count -ge 5) {
        Write-Host "    ok   every change left a copy behind ($count)" -ForegroundColor DarkGray
    } else {
        $failures += "only $count backups for six writes; changes are being lost"
        Write-Host "    FAIL only $count backups for six writes" -ForegroundColor Red
    }
} else {
    $failures += 'nothing was ever backed up'
    Write-Host '    FAIL nothing was ever backed up' -ForegroundColor Red
}

# A backup that does not validate must not go back over a state that does.
$bad = Join-Path $backups 'STATE-20200101T000000Z-00.json'
[System.IO.File]::WriteAllText($bad, '{"schema_version": 1, "agents": {}, "tasks": []}', $utf8)
Test-Collab 'a backup that does not validate is refused' `
    @('recover', 'STATE-20200101T000000Z-00.json') -Expect 1 `
    -Says @('does not validate')
Test-Collab 'and a backup that is not there is refused' `
    @('recover', 'STATE-nonesuch.json') -Expect 1 -Says @('no backup called')

Write-Host ''
Write-Host '==> A state that has gone wrong' -ForegroundColor Cyan
$stateFile = Join-Path $Scratch '.ai_collaboration\STATE.json'
$broken = (Get-Content $stateFile -Raw) -replace '"schema_version": 1', '"schema_version": 99'
[System.IO.File]::WriteAllText($stateFile, $broken, $utf8)
Test-Collab 'check finds a schema it does not understand' `
    @('check') -Expect 2 -Says @('schema_version is 99')
Test-Collab 'and nothing will write over it' `
    @('heartbeat') -Expect 1 -Says @('not usable')

# Recovery puts a good one back, which is the whole point of having them.
$good = Get-ChildItem $backups -Filter 'STATE-2026*.json' |
    Sort-Object Name | Select-Object -Last 1
Test-Collab 'and recovery puts a good one back' `
    @('recover', $good.Name) -Says @('restored from')
Test-Collab 'after which it is usable again' @('heartbeat') -Says @('still here')

Write-Host ''
Write-Host '==> A lock whose holder has stopped' -ForegroundColor Cyan
# Four hours untouched is stale. The tool has to say so loudly -- and still
# refuse to do anything about it, which is the whole point.
$old = [DateTime]::UtcNow.AddHours(-9).ToString('yyyy-MM-ddTHH:mm:ss+00:00')
$abandoned = @"
{
  "agent": "gpt6_astra",
  "task": "ASTRA-1",
  "paths": ["shared/nexus-ai/", "docs/AI/"],
  "created_at": "$old"
}
"@
[System.IO.File]::WriteAllText((Join-Path $Scratch '.ai_collaboration\locks\ASTRA-1.json'), $abandoned, $utf8)

Test-Collab 'a stale lock is reported' @('check') -Expect 2 -Says @('untouched for 9 hours')
Test-Collab 'and status says so too' @('status') -Expect 2 -Says @('STALE')
Test-Collab 'and it is still not this program''s to break' `
    @('lock', 'release', 'ASTRA-1') -Expect 1 -Says @('will not release')
Test-Collab 'nor to take the ground out from under' `
    @('lock', 'take', 'CLAUDE-8', 'shared/nexus-ai/src/lib.rs') -Expect 1 `
    -Says @('stale', 'not this program''s to break')

Write-Host ''
Write-Host '==> Three developers, and more' -ForegroundColor Cyan

# The tool used to have two mailbox directory names written into its source.
# A third developer joined on 2026-09-15 and every one of those lists became
# silently wrong -- `request list` looked in two places out of six and reported
# a clean inbox to somebody who had mail. It now reads the directory.
$boxes = @('claude_to_astra', 'astra_to_claude', 'claude_to_gemini',
           'gemini_to_claude', 'astra_to_gemini', 'gemini_to_astra')
foreach ($box in $boxes) {
    New-Item -ItemType Directory -Force -Path (Join-Path $Scratch ".ai_collaboration\$box") | Out-Null
}
[System.IO.File]::WriteAllText(
    (Join-Path $Scratch '.ai_collaboration\gemini_to_claude\REQUEST_G-1.md'),
    "# from the newest developer`n", $utf8)
[System.IO.File]::WriteAllText(
    (Join-Path $Scratch '.ai_collaboration\astra_to_gemini\REQUEST_A-9.md'),
    "# not addressed to claude`n", $utf8)

Test-Collab 'every mailbox is found, not the two that used to be listed' `
    @('request', 'mailboxes') `
    -Says @('claude_to_gemini', 'gemini_to_astra', 'astra_to_claude')
Test-Collab 'a request from the newest developer is listed' `
    @('request', 'list') -Says @('gemini_to_claude/REQUEST_G-1.md')
Test-Collab 'and so is one that is nobody''s business here' `
    @('request', 'list') -Says @('astra_to_gemini/REQUEST_A-9.md')
Test-Collab '--mine is only what this agent should read' `
    @('request', 'list', '--mine') `
    -Says @('gemini_to_claude/REQUEST_G-1.md') `
    -DoesNotSay @('astra_to_gemini/REQUEST_A-9.md')
Test-Collab 'an empty inbox says so rather than printing nothing' `
    @('request', 'list', '--mine', '--agent', 'nobody_at_all') `
    -Says @('no requests in 0 mailboxes')
Test-Collab 'show finds a request in a mailbox nobody hard-coded' `
    @('request', 'show', 'G-1') -Says @('from the newest developer')

# Releasing somebody else's lock names the mailbox for *those two* agents. It
# used to assume the other party was the only other one there was.
Test-Collab 'the refusal names the right two developers' `
    @('lock', 'release', 'ASTRA-1', '--agent', 'gemini_3_1_pro') `
    -Expect 1 -Says @('gemini_to_astra')

Write-Host ''
Write-Host '==> The event log' -ForegroundColor Cyan

# Two events in the same second used to be one event: the filename was the
# timestamp and the second write replaced the first. Found by watching it
# happen while recording two real events a moment apart.
Test-Collab 'an event is recorded' @('event', 'add', 'the first thing') -Says @('recorded in')
Test-Collab 'and a second one in the same second does not replace it' `
    @('event', 'add', 'the second thing') -Says @('recorded in')
$script:checks++
$events = Get-ChildItem (Join-Path $Scratch '.ai_collaboration\events') -Filter '*.json'
$said = ($events | ForEach-Object { Get-Content $_.FullName -Raw }) -join "`n"
if ($said.Contains('the first thing') -and $said.Contains('the second thing')) {
    Write-Host '    ok   both events survive' -ForegroundColor DarkGray
} else {
    $script:failures += 'both events survive : one of them was overwritten'
    Write-Host '    FAIL both events survive' -ForegroundColor Red
}

Write-Host ''
Write-Host '==> Identity' -ForegroundColor Cyan
$env:NEXUS_AGENT = ''
Test-Collab 'writing with no idea who you are is refused' `
    @('heartbeat') -Expect 1 -Says @('NEXUS_AGENT')
# Exits 2 rather than 0, because the stale lock above is still there and a
# status that said nothing was wrong would be a status that lies to a script.
Test-Collab 'but reading is not' @('status') -Expect 2 -Says @('agents')
$env:NEXUS_AGENT = 'claude_code'

Remove-Item $Scratch -Recurse -Force -ErrorAction SilentlyContinue

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host "Collaboration tool tests passed ($checks checks)." -ForegroundColor Green
    exit 0
} else {
    foreach ($why in $failures) { Write-Host "    FAIL $why" -ForegroundColor Red }
    Write-Host "$($failures.Count) of $checks checks failed." -ForegroundColor Red
    exit 1
}
