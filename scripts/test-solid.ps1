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
    [string]$Shot,
    # Take a new copy of the disk image even if one is already here. Wanted
    # after a build, and not wanted otherwise: the copy is eight gigabytes and
    # taking it means waiting for the shared image to be free.
    [switch]$Fresh
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

# This run gets its own copy of the disk, and that is not tidiness.
#
# There is one `build/nexus-disk.img` and three agents working in this checkout.
# Two QEMUs on one image do not fail loudly -- the second boots and hangs at
# `begin NexusFS` with no error at all -- and waiting for a gap between somebody
# else's test stages only means colliding with their next one. A copy costs
# eight gigabytes of disk and removes the whole problem.
#
# `Get-NexusQemuArgs` finds the disk under whatever `-BuildDir` it is given, so
# pointing that at a directory holding nothing but the copy is enough. The USB
# images are `Test-Path`-guarded there, so this machine simply has no USB
# sticks, which is no loss to a program that draws a triangle.
#
# Taken through `Open-ImageForReading`, which waits out the sharing violation
# rather than reading a half-written image: a copy taken while somebody's run is
# writing is torn, and a torn filesystem is exactly the confusing failure this
# exists to avoid.
$PrivateDir = Join-Path $BuildDir 'solid'
$SharedDisk = Join-Path $BuildDir 'nexus-disk.img'
$PrivateDisk = Join-Path $PrivateDir 'nexus-disk.img'
if (-not (Test-Path $PrivateDir)) { New-Item -ItemType Directory $PrivateDir | Out-Null }

# Copied when it is missing, or when `-Fresh` asks, and on no other condition.
#
# Comparing timestamps against the shared image was the obvious rule and it was
# wrong: that file is rewritten by every run anybody makes, so "newer than my
# copy" was true every single time and the copy was never reused. `-Fresh` after
# a rebuild is the honest way to say what is actually meant.
if ((-not (Test-Path $PrivateDisk)) -or $Fresh) {
    if (-not (Test-Path $SharedDisk)) { throw 'no disk image; run build.ps1 first' }
    Write-Host '==> Taking this run its own copy of the disk' -ForegroundColor Cyan
    $source = Open-ImageForReading -Image $SharedDisk
    try {
        $destination = [System.IO.File]::Create($PrivateDisk)
        try { $source.CopyTo($destination) } finally { $destination.Dispose() }
    } finally { $source.Dispose() }
    Write-Host "    $([math]::Round((Get-Item $PrivateDisk).Length / 1GB, 1)) GiB -> $PrivateDisk" -ForegroundColor DarkGray
}

# And its own copy of the EFI partition tree, for the same reason as the disk.
#
# `build/esp` is handed to QEMU as `fat:rw:` -- a live view of a host directory,
# written as well as read. Another agent's `build.ps1` rewrites that directory
# between their test stages, and a machine whose filesystem is being replaced
# underneath it is not a machine anything can be concluded from.
#
# Whether that is what kept stopping this run is **not established**: the serial
# log ends mid-sentence in the middle of a monitor report with no guest fault,
# which says the host process went away rather than the guest failing, and I
# have not identified what took it. Copying the tree removes one shared thing
# rather than proving it was the one. Cheap, and it makes this run independent
# of everybody else's, which is worth having either way.
$PrivateEsp = Join-Path $PrivateDir 'esp'
if ((-not (Test-Path $PrivateEsp)) -or $Fresh) {
    if (Test-Path $PrivateEsp) { Remove-Item $PrivateEsp -Recurse -Force }
    Write-Host '==> Taking this run its own copy of the EFI partition tree' -ForegroundColor Cyan
    Copy-Item $EspDir $PrivateEsp -Recurse -Force
}
$EspDir = $PrivateEsp

<#
.SYNOPSIS
    A host TCP port nothing is listening on, found by trying to listen on it.

.DESCRIPTION
    Picking at random and hoping is what this script did, and the range it
    picked from is the range every other test script here picks from. When the
    pick collides, QEMU cannot bind its monitor and the machine is gone before
    it boots -- and what is left behind is a serial log holding nothing but the
    firmware's clear-screen codes, which reads exactly like a machine that
    failed for no reason.

    Asking the operating system is not much more code than hoping. It is still
    a race -- something can take the port between the test and QEMU's bind --
    but it turns a collision from likely into unlikely, and a retry covers the
    rest.
#>
function Get-FreePort {
    param([int]$From, [int]$To)
    for ($attempt = 0; $attempt -lt 40; $attempt++) {
        $candidate = Get-Random -Minimum $From -Maximum $To
        $listener = $null
        try {
            $listener = New-Object System.Net.Sockets.TcpListener(
                [System.Net.IPAddress]::Loopback, $candidate)
            $listener.Start()
            return $candidate
        } catch {
            continue
        } finally {
            if ($listener) { $listener.Stop() }
        }
    }
    throw "could not find a free port between $From and $To"
}

$MonitorPort = Get-FreePort -From 33000 -To 33999
# And a host port of its own for the guest's HTTP forward. `Get-NexusQemuArgs`
# defaults it to 18080 and its own comment says why it should not be left there;
# this script left it there anyway, and with somebody else's machine already on
# 18080 QEMU refuses the forwarding rule and exits before it boots:
#
#   Could not set up host forwarding rule 'tcp:127.0.0.1:18080-:80'
#
# on standard error, and nothing at all in the serial log. That is why this run
# kept reporting a desktop that never appeared.
$HttpPort = Get-FreePort -From 18100 -To 18999
$QemuArgs = Get-NexusQemuArgs -BuildDir $PrivateDir -EspDir $EspDir `
    -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars -SerialLog $Log `
    -MonitorPort $MonitorPort -HostHttpPort $HttpPort -Headless

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
# Still taken, and still only a warning, although this run no longer touches the
# shared image: it says who is on the host, and the copy above is read from the
# shared image, which is the one moment this does contend.
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
# QEMU's own complaints go to a file rather than to a console nobody is reading.
# Its refusals -- a host port already taken, an image it cannot open -- are on
# standard error and *not* in the serial log, so a run that dies for one of them
# looks from the log exactly like a guest that stopped for no reason. That cost
# several runs before the port clash showed itself by luck.
$QemuErrors = Join-Path $BuildDir 'solid-qemu.err'
if (Test-Path $QemuErrors) { Remove-Item $QemuErrors -Force }
$process = Start-Process -FilePath $QemuExe.Source -ArgumentList $QemuArgs -PassThru -NoNewWindow `
    -RedirectStandardError $QemuErrors

function Wait-For {
    param([string]$Text, [int]$Seconds)
    for ($waited = 0; $waited -lt $Seconds; $waited++) {
        Start-Sleep -Seconds 1
        # Read the log first and ask whether the machine is still there second.
        # A machine that has just stopped may have written the thing being
        # waited for on its way out, and answering "no" to a question the log
        # already answers sends the reader to look at the wrong thing --
        # `configure-disk.ps1` carries the same note for the same reason.
        $gone = $process.HasExited
        if (Test-Path $Log) {
            $sofar = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''
            if ($sofar.Contains($Text)) { return $true }
        }
        if ($gone) {
            Write-Host "    the machine stopped while waiting for: $Text" -ForegroundColor DarkYellow
            return $false
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

# Whatever QEMU said on its way out, said here, because the serial log will not
# contain it.
if ((Test-Path $QemuErrors) -and (Get-Item $QemuErrors).Length -gt 0) {
    Write-Host ''
    Write-Host '    qemu said:' -ForegroundColor DarkYellow
    foreach ($line in (Get-Content $QemuErrors)) {
        if ($line.Trim()) { Write-Host "      $line" -ForegroundColor DarkYellow }
    }
    $failures += 'qemu wrote to standard error'
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
