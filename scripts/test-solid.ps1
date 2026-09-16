<#
.SYNOPSIS
    Starts the three-dimensional drawing from the launcher and checks that what
    it drew was a solid rather than a shape.

.DESCRIPTION
    The renderer's arithmetic has thirty tests on the host, and they say nothing
    about whether this system can put the result on a screen. This drives the
    whole path: a key, a launcher, a compositor that hands out a surface, and a
    program that fills that surface a pixel at a time with no graphics interface
    anywhere beneath it.

    What is checked is the program's own verdict, and that verdict is worth more
    than a count. For every face of every frame it works out from the geometry
    whether the face should be visible -- whether its outward normal leans
    towards the eye -- and requires that to agree with whether the rasteriser
    drew it. Counting drawn against turned away, which is what it used to do,
    passes even when every face is reversed and the picture is the inside of the
    shape.

.PARAMETER Timeout
    How long to wait for each stage, in seconds.

.PARAMETER Shot
    Where to put a screenshot of the solid, if one is wanted.
#>
[CmdletBinding()]
param(
    [int]$Timeout = 240,
    [string]$Shot
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')
. (Join-Path $PSScriptRoot 'capture.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'
$Log = Join-Path $BuildDir 'solid-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-solid.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $Log) { Remove-Item $Log -Force }

$MonitorPort = Get-Random -Minimum 33000 -Maximum 33999
$QemuArgs = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
    -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars -SerialLog $Log `
    -MonitorPort $MonitorPort -Headless

# Say out loud that this run has the machine.
#
# There is one disk image and three agents working in this checkout, and two
# QEMUs on one image do not fail loudly: the second boots and hangs at
# `begin NexusFS` with no error. A lock in the place the protocol already keeps
# them turns an unexplained timeout into a name and a task.
#
# A warning and not a refusal, deliberately. This convention is proposed in
# REQUEST_CLAUDE-20260917-002 and not yet agreed, and a script that blocked on a
# convention nobody had signed up to would only be a new way to fail. A stale
# lock is a reason to ask, never a reason to kill anything.
$LockDir = Join-Path $RepoRoot '.ai_collaboration\locks'
$LockFile = Join-Path $LockDir 'CLAUDE-MACHINE-001.json'
if (Test-Path $LockDir) {
    foreach ($other in (Get-ChildItem $LockDir -Filter '*.json')) {
        $held = Get-Content $other.FullName -Raw | ConvertFrom-Json
        if ($held.paths -contains 'build/nexus-disk.img' -and $other.FullName -ne $LockFile) {
            Write-Host "    note: $($held.agent) holds the machine for $($held.task), since $($held.created_at)" -ForegroundColor DarkYellow
        }
    }
    $now = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    $utf8NoBom = New-Object System.Text.UTF8Encoding $false
    [System.IO.File]::WriteAllText($LockFile, (@{
                agent        = 'claude_code'
                task         = 'CLAUDE-SOLID-001'
                paths        = @('build/nexus-disk.img')
                created_at   = $now
                heartbeat_at = $now
            } | ConvertTo-Json), $utf8NoBom)
}

Write-Host "==> Booting NexusOS with a monitor on port $MonitorPort" -ForegroundColor Cyan
$process = Start-Process -FilePath $QemuExe.Source -ArgumentList $QemuArgs -PassThru -NoNewWindow

function Wait-For {
    param([string]$Text, [int]$Seconds)
    for ($waited = 0; $waited -lt $Seconds; $waited++) {
        Start-Sleep -Seconds 1
        if ($process.HasExited) { return $false }
        if (Test-Path $Log) {
            $sofar = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''
            if ($sofar.Contains($Text)) { return $true }
        }
    }
    return $false
}

$failures = @()
try {
    Write-Host '==> Waiting for the desktop' -ForegroundColor Cyan
    if (-not (Wait-For -Text 'shell: took the strip' -Seconds $Timeout)) {
        throw 'the desktop never appeared'
    }

    $client = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $MonitorPort)
    try {
        $writer = New-Object System.IO.StreamWriter($client.GetStream())
        $writer.AutoFlush = $true
        Start-Sleep -Milliseconds 800

        function Send-Keys {
            param([string[]]$Keys, [int]$Pause = 180)
            foreach ($key in $Keys) {
                $writer.WriteLine("sendkey $key")
                Start-Sleep -Milliseconds $Pause
            }
        }

        Write-Host '==> F3' -ForegroundColor Cyan
        Send-Keys @('f3')
        if (-not (Wait-For -Text 'launch: a window for starting things by name' -Seconds 60)) {
            $failures += 'the launcher never started'
        }

        # "dra" finds "Draw a solid" and nothing else on the list. "Install
        # downloads" has the d and no r after it, which is the kind of near miss
        # worth picking the letters to avoid.
        Write-Host '==> Typing "dra"' -ForegroundColor Cyan
        Send-Keys @('d', 'r', 'a')
        Start-Sleep -Seconds 2

        Write-Host '==> Enter' -ForegroundColor Cyan
        Send-Keys @('ret')

        if (-not (Wait-For -Text 'launch: asked for sold' -Seconds 60)) {
            $failures += 'the launcher never asked for the solid'
        }
        if (-not (Wait-For -Text 'compositor: started the three-dimensional drawing' -Seconds 60)) {
            $failures += 'the compositor never started it'
        }

        # Before the summary, because the summary is the last thing it does and
        # the window goes when it exits. A few seconds in is the middle of the
        # turn, which is the picture worth having.
        #
        # In a try, and that is not defensive habit: asking for a screendump
        # while the guest is filling a window pixel by pixel drops this monitor
        # connection about as often as not. The dump itself lands -- the file is
        # written -- and then the socket goes. The verdict is in the serial log
        # rather than down this socket, so losing the socket here must not lose
        # the run, and a test that threw away a result because it could not take
        # a picture of it would be a test measuring the wrong thing.
        if ($Shot) {
            Start-Sleep -Seconds 6
            $Ppm = Join-Path $BuildDir 'solid.ppm'
            try {
                Invoke-Screendump -Writer $writer -Path $Ppm
                Convert-PpmToPng -PpmPath $Ppm -PngPath $Shot | Out-Null
                Write-Host "    Screenshot: $Shot" -ForegroundColor DarkGray
            } catch {
                Write-Host "    the monitor went away taking the screenshot; carrying on" -ForegroundColor DarkYellow
                if (Test-Path $Ppm) {
                    try {
                        Convert-PpmToPng -PpmPath $Ppm -PngPath $Shot | Out-Null
                        Write-Host "    Screenshot: $Shot (written before it went)" -ForegroundColor DarkGray
                    } catch { }
                }
            }
        }

        # A hundred and eighty frames, each one every pixel of a window worked
        # out by the processor. It takes as long as it takes.
        #
        # Waited for by the summary's own words and not by "solid: ", which the
        # winding check says first -- a wait that matched the first line this
        # program logs would stop the machine before it had drawn anything, and
        # did.
        Write-Host '==> Waiting for it to finish drawing' -ForegroundColor Cyan
        if (-not (Wait-For -Text 'pixels by the processor' -Seconds $Timeout)) {
            $failures += 'it never said what it drew'
        }

        # Tidiness only, and it may well fail if the monitor has already gone.
        # The machine is killed in the `finally` below either way.
        try {
            $writer.WriteLine('quit')
            Start-Sleep -Milliseconds 800
        } catch { }
    } finally {
        $client.Close()
    }
} finally {
    if (-not $process.HasExited) {
        try { $process.Kill() } catch { }
    }
    $process.WaitForExit(5000) | Out-Null
    # Mine, so removing it is not the thing the protocol forbids. Left behind by
    # a run that is killed outright, which is why a stale one means ask rather
    # than act.
    if (Test-Path $LockFile) { Remove-Item $LockFile -Force }
}

$output = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''

foreach ($expected in @(
        'launch: asked for sold',
        'compositor: started the three-dimensional drawing, and gave it a surface',
        'solid: all 8 faces are wound so their front is the outside',
        'solid: geometry and rasteriser agreed on every face but'
    )) {
    if ($output.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "never reported: $expected"
    }
}

foreach ($bad in @('solid: FAILED', 'solid: PANIC', 'compositor: FAILED', 'KERNEL PANIC')) {
    if ($output.Contains($bad)) {
        $failures += "saw: $bad"
    } else {
        Write-Host "    ok   absent: $bad" -ForegroundColor DarkGray
    }
}

# What it drew, repeated here because it is the line worth reading and the
# numbers in it are what any document about this renderer should quote.
foreach ($line in ($output -split "`r?`n")) {
    if ($line -match 'solid: \d+ frames') {
        Write-Host ''
        Write-Host "    $($line.Trim())" -ForegroundColor Cyan
    }
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'The solid was drawn, and it was a solid.' -ForegroundColor Green
    exit 0
} else {
    foreach ($why in $failures) { Write-Host "    FAIL $why" -ForegroundColor Red }
    Write-Host "Log: $Log"
    exit 1
}
