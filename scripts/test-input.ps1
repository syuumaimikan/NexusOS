<#
.SYNOPSIS
    Verifies that NexusOS receives and acts on real keystrokes.

.DESCRIPTION
    Boots the system with QEMU's monitor attached, sends keys through it, and
    checks the serial log for what the kernel made of them.

    This is the only honest test of an input path. A unit test can check that a
    scancode table maps 0x1E to 'a'; it cannot check that the I/O APIC pin was
    programmed, that the interrupt arrived on the vector the IDT expects, that
    the handler drained the controller so the next interrupt can be raised, or
    that the decoded key reached something that acted on it. Every one of those
    has to be driven from outside.

.PARAMETER Timeout
    Seconds to let the guest run. Default 30.
#>
[CmdletBinding()]
param(
    [int]$Timeout = 30
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'
$SerialLog = Join-Path $BuildDir 'input-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw "No staged ESP at $EspDir. Run .\scripts\build.ps1 first."
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source

New-Item -ItemType Directory -Force -Path $BuildDir | Out-Null
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-input.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $SerialLog) { Remove-Item $SerialLog -Force }

$MonitorPort = Get-Random -Minimum 25000 -Maximum 25999

$QemuArgs = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
    -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars -SerialLog $SerialLog `
    -MonitorPort $MonitorPort -Headless

Write-Host "==> Booting NexusOS with a monitor on port $MonitorPort" -ForegroundColor Cyan
$process = Start-Process -FilePath $QemuExe.Source -ArgumentList $QemuArgs -PassThru -NoNewWindow

# The keys to send, and what the kernel should make of them. "nexus" spells out
# a word so that a mis-decoded scancode shows up as a wrong letter rather than
# as merely a different count.
# `tab` in the middle on purpose: it is the key the compositor keeps for itself,
# so what comes after it has to arrive somewhere different from what came
# before. That is what separates routing from broadcasting.
$Keys = @('n', 'e', 'x', 'tab', 'u', 's', 'f1')

try {
    # Let the system finish booting and start its input thread before typing.
    Write-Host '==> Waiting for the input thread' -ForegroundColor Cyan
    $ready = $false
    for ($waited = 0; $waited -lt $Timeout; $waited++) {
        Start-Sleep -Seconds 1
        if ($process.HasExited) { throw "QEMU exited early with code $($process.ExitCode)" }
        if (Test-Path $SerialLog) {
            $sofar = (Get-Content $SerialLog -Raw -Encoding UTF8) -replace "`0", ''
            if ($sofar.Contains('input thread')) { $ready = $true; break }
        }
    }
    if (-not $ready) { throw 'the input thread never started' }

    Write-Host "==> Sending keys: $($Keys -join ' ')" -ForegroundColor Cyan
    $client = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $MonitorPort)
    try {
        $writer = New-Object System.IO.StreamWriter($client.GetStream())
        $writer.AutoFlush = $true
        Start-Sleep -Milliseconds 500
        foreach ($key in $Keys) {
            $writer.WriteLine("sendkey $key")
            # Slower than a person types. The point is to check the path works,
            # not how fast it is, and spacing the keys keeps a dropped one from
            # being blamed on the queue.
            Start-Sleep -Milliseconds 300
        }
        # Wait for the monitor thread's next report, which is what carries the
        # keyboard figures and the decoded line into the serial log. It runs
        # every five seconds, so this has to outlast one full period.
        Start-Sleep -Seconds 7
        $writer.WriteLine('quit')
        Start-Sleep -Milliseconds 500
    } finally {
        $client.Close()
    }
} finally {
    if (-not $process.HasExited) {
        try { $process.Kill() } catch { }
    }
    $process.WaitForExit(5000) | Out-Null
}

if (-not (Test-Path $SerialLog)) { throw 'no serial output' }
$output = (Get-Content $SerialLog -Raw -Encoding UTF8) -replace "`0", ''

$failures = @()

# The interrupt path: the pin was programmed and something arrived on it.
if (-not $output.Contains('routed to vector')) {
    $failures += 'the keyboard was never routed to a vector'
}
if (-not ($output -match 'keyboard: (\d+) scancodes')) {
    $failures += 'no scancodes were received'
} else {
    $scancodes = [int]$Matches[1]
    # Seven keys, each a press and a release, so at least fourteen.
    if ($scancodes -lt 14) {
        $failures += "only $scancodes scancodes arrived; expected at least 14"
    } else {
        Write-Host "    ok   $scancodes scancodes received" -ForegroundColor DarkGray
    }
}

# The decode path, end to end: the letters have to come back as the word that
# was typed. A count of scancodes would pass with a completely wrong scancode
# table; the text will not.
# The tab in the middle is a space to the kernel's own panel, which is why the
# expected text has one: the kernel goes on acting on every key for the panel
# while the compositor routes a copy of the same keys to a client.
if ($output.Contains('line "nex us"')) {
    Write-Host '    ok   the typed letters decoded to "nex us"' -ForegroundColor DarkGray
} else {
    $failures += 'the typed letters did not decode to "nex us"'
}

# And the part that is new: a keystroke went into the kernel's keyboard driver,
# crossed a channel to the compositor, was routed to one client, and arrived.
$first = ([regex]::Matches($output, 'client 0: heard a key')).Count
$second = ([regex]::Matches($output, 'client 1: heard a key')).Count
if ($first -lt 1) {
    $failures += 'no key ever reached a client'
} else {
    Write-Host "    ok   $first keys reached the first client" -ForegroundColor DarkGray
}

# Tab is the key the compositor keeps for itself, so what was typed after it has
# to arrive somewhere different from what came before. Both clients hearing
# something is what says this is routing; either of them hearing *everything*
# would say it is broadcasting.
if (-not $output.Contains('compositor: focus moved to the second client')) {
    $failures += 'tab did not move the focus'
} elseif ($second -lt 1) {
    $failures += 'the focus moved but no key followed it'
} else {
    Write-Host "    ok   $second keys followed the focus to the second client" -ForegroundColor DarkGray
}

# And neither of them saw the other's keys. Three letters were typed before the
# tab and two after it, so a client that heard all five heard someone else's.
if ($first -gt 3 -or $second -gt 2) {
    $failures += "a client heard keys meant for the other ($first and $second of 3 and 2)"
} else {
    Write-Host '    ok   neither client heard the keys meant for the other' -ForegroundColor DarkGray
}

# F1 is a distinct key, and acting on it says the decoded key reached something
# that used it.
if ($output.Contains('F1: interface language is now ja-JP')) {
    Write-Host '    ok   F1 switched the interface language' -ForegroundColor DarkGray
} else {
    $failures += 'F1 did not switch the interface language'
}

if ($output.Contains('EXCEPTION')) { $failures += 'an exception was reported' }
if ($output.Contains('KERNEL PANIC')) { $failures += 'the kernel panicked' }

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'Input tests passed.' -ForegroundColor Green
    exit 0
} else {
    foreach ($failure in $failures) {
        Write-Host "    FAIL $failure" -ForegroundColor Red
    }
    Write-Host "Serial log: $SerialLog"
    exit 1
}
