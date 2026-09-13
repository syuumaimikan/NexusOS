<#
.SYNOPSIS
    Boots NexusOS in QEMU.

.DESCRIPTION
    Launches QEMU with UEFI firmware and the staged EFI System Partition from
    scripts/build.ps1. Serial output is the kernel's primary diagnostic
    channel and is always captured to build/serial.log.

.PARAMETER Headless
    Run without a display and exit after -Timeout seconds. Used by the
    automated boot test.

.PARAMETER Timeout
    Seconds to run before shutting down, in headless mode. Default 20.

.PARAMETER Memory
    Guest RAM. Default 1G.

.PARAMETER Release
    Use the release build outputs.

.PARAMETER Gdb
    Start with a GDB stub on :1234 and wait for a debugger to attach.

.PARAMETER Until
    Stop as soon as this text appears in the serial log, instead of running for
    the whole timeout. -Timeout then means how long to wait for it.

    One monitor report is allowed to pass after the text appears, so that the
    figures the log carries describe the machine *after* whatever the marker
    was about. Stopping on the marker itself catches the system mid-tidy --
    processes that have said what they had to say but whose address spaces have
    not been reaped yet -- which reads as a leak rather than as a run that was
    cut short.

    How long a boot takes is a property of the host, not of the system under
    test: four emulated processors share one real one, and a debug build under
    dynamic translation runs at a fraction of wall-clock speed that changes with
    whatever else the machine is doing. Waiting for what the run is *for* keeps
    that out of the result.
#>
[CmdletBinding()]
param(
    [switch]$Headless,
    [int]$Timeout = 20,
    [string]$Until = '',
    # Batches of keys, separated by ';', each batch a space-separated list of
    # QEMU key names. One string rather than an array because these cross a
    # process boundary from `test.ps1`, and an array of values with spaces in
    # them does not survive that intact.
    [string]$Press = '',
    # One marker per batch, in the same order and with the same separator: each
    # batch is sent when its marker appears in the serial log.
    [string]$PressAfter = '',
    [string]$Memory = '1G',
    [switch]$Release,
    [switch]$Gdb
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'
$SerialLog = Join-Path $BuildDir 'serial.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw "No staged ESP at $EspDir. Run .\scripts\build.ps1 first."
}

# Locate QEMU and its bundled edk2 firmware.
$QemuExe = (Get-Command qemu-system-x86_64 -ErrorAction SilentlyContinue)
if ($null -eq $QemuExe) {
    throw 'qemu-system-x86_64 is not on PATH.'
}
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $QemuDir 'share\edk2-x86_64-code.fd'
$FirmwareVarsTemplate = Join-Path $QemuDir 'share\edk2-i386-vars.fd'

foreach ($firmware in @($FirmwareCode, $FirmwareVarsTemplate)) {
    if (-not (Test-Path $firmware)) { throw "UEFI firmware not found: $firmware" }
}

New-Item -ItemType Directory -Force -Path $BuildDir | Out-Null

# QEMU splits -drive options on commas and does not accept quoting inside them,
# so a firmware path containing spaces (the default "C:\Program Files\qemu")
# cannot be passed through. Stage both firmware blobs in the build directory,
# whose path we control.
$FirmwareCodeLocal = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCodeLocal)) {
    Copy-Item $FirmwareCode $FirmwareCodeLocal -Force
}

# The variable store must be writable, so it gets its own copy. It is kept
# across runs so that firmware boot entries persist.
$FirmwareVars = Join-Path $BuildDir 'edk2-vars.fd'
if (-not (Test-Path $FirmwareVars)) {
    Copy-Item $FirmwareVarsTemplate $FirmwareVars -Force
}

if (Test-Path $SerialLog) { Remove-Item $SerialLog -Force }

# A monitor, only when there are keys to send. The session on this machine
# ends when somebody ends it -- there is no other way out, by design -- so a
# headless run that wants to see the end of one has to press the key a person
# would.
$Batches = @($Press -split ';' | Where-Object { $_.Trim() })
$Markers = @($PressAfter -split ';' | Where-Object { $_.Trim() })
$MonitorPort = 0
if ($Batches.Count -gt 0) { $MonitorPort = Get-Random -Minimum 26000 -Maximum 28000 }
if ($Markers.Count -ne 0 -and $Markers.Count -ne $Batches.Count) {
    throw '-PressAfter must name one marker per -Press batch, or none at all'
}

# Send one batch of keys through QEMU's monitor, spaced out: what is being
# exercised is that the path works, not how fast it is, and a dropped key would
# otherwise be blamed on the queue.
function Send-Keys {
    param([string]$Keys, [int]$Port)
    $client = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $Port)
    try {
        $writer = New-Object System.IO.StreamWriter($client.GetStream())
        $writer.AutoFlush = $true
        foreach ($key in ($Keys -split ' ' | Where-Object { $_ })) {
            $writer.WriteLine("sendkey $key")
            Start-Sleep -Milliseconds 220
        }
    } finally {
        $client.Close()
    }
}

$QemuArgs = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
    -FirmwareCode $FirmwareCodeLocal -FirmwareVars $FirmwareVars -SerialLog $SerialLog `
    -Memory $Memory -MonitorPort $MonitorPort
$QemuArgs += @('-d', 'guest_errors')

if ($Headless) {
    $QemuArgs += @('-display', 'none')
} else {
    $QemuArgs += @('-display', 'gtk')
}

if ($Gdb) {
    $QemuArgs += @('-s', '-S')
    Write-Host 'GDB stub listening on localhost:1234; QEMU is paused.' -ForegroundColor Yellow
}

Write-Host "==> Booting NexusOS in QEMU (serial -> $SerialLog)" -ForegroundColor Cyan

if ($Headless) {
    $process = Start-Process -FilePath $QemuExe.Source -ArgumentList $QemuArgs -PassThru -NoNewWindow
    $exited = $false
    if ($Until) {
        $seenAt = -1
        $reportsThen = 0
        # How far through the list of things to press we are. Each batch waits
        # for its own marker, so a machine that shows the wizard and then a
        # desktop can be answered at both points from one run.
        $pressing = 0
        for ($waited = 0; $waited -lt $Timeout; $waited++) {
            if ($process.WaitForExit(1000)) { $exited = $true; break }
            if (-not (Test-Path $SerialLog)) { continue }
            $sofar = (Get-Content $SerialLog -Raw -Encoding UTF8) -replace "`0", ''
            $reports = ([regex]::Matches($sofar, '\[mon \] \d+s uptime')).Count

            # The keys somebody would press, once the machine has got far
            # enough to have somebody to press them at.
            if ($pressing -lt $Batches.Count) {
                $marker = if ($Markers.Count -gt 0) { $Markers[$pressing].Trim() } else { '' }
                if (-not $marker -or $sofar.Contains($marker)) {
                    $keys = $Batches[$pressing].Trim()
                    $pressing++
                    Write-Host "==> Pressing '$keys' after $waited s" -ForegroundColor Cyan
                    try {
                        Send-Keys -Keys $keys -Port $MonitorPort
                    } catch {
                        Write-Host "==> Could not reach the monitor: $_" -ForegroundColor Yellow
                    }
                }
                continue
            }

            if ($seenAt -lt 0) {
                if (-not $sofar.Contains($Until)) { continue }
                $seenAt = $waited
                $reportsThen = $reports
                Write-Host "==> Saw '$Until' after $waited s; waiting for one more report" -ForegroundColor Cyan
                continue
            }
            if ($reports -gt $reportsThen) {
                Write-Host "==> Settled after $($waited - $seenAt) s more; stopping QEMU" -ForegroundColor Cyan
                break
            }
        }
    } else {
        $exited = $process.WaitForExit($Timeout * 1000)
    }
    if (-not $exited -and -not $process.HasExited) {
        if (-not $Until) {
            Write-Host "==> Timeout after $Timeout s; stopping QEMU" -ForegroundColor Yellow
        }
        try { $process.Kill() } catch { }
        $process.WaitForExit(5000) | Out-Null
    }
} else {
    & $QemuExe.Source @QemuArgs
}

Write-Host ''
if (Test-Path $SerialLog) {
    Write-Host "==> Serial output ($SerialLog)" -ForegroundColor Cyan
    Get-Content $SerialLog
} else {
    Write-Host 'No serial output was produced.' -ForegroundColor Red
}
