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
    Require release outputs staged by build.ps1 -Release.
.PARAMETER BuildDirectory
    Build directory containing the ESP and disk image. Defaults to build.

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
    [ValidateRange(1, 86400)][int]$Timeout = 20,
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
    [string]$BuildDirectory = 'build',
    [ValidateRange(0, 65535)][int]$HostHttpPort = 18080,
    [ValidateRange(0, 65535)][int]$MonitorPort = 0,
    [switch]$Release,
    [switch]$Gdb
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')
. (Join-Path $PSScriptRoot 'private-machine.ps1')
. (Join-Path $PSScriptRoot 'maintenance.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = if ([IO.Path]::IsPathRooted($BuildDirectory)) { $BuildDirectory } else { Join-Path $RepoRoot $BuildDirectory }
$EspDir = Join-Path $BuildDir 'esp'
$SerialLog = Join-Path $BuildDir 'serial.log'
$ErrorLog = Join-Path $BuildDir 'qemu-error.log'

if (($Until -or $Press -or $PressAfter) -and -not $Headless) {
    throw '-Until, -Press and -PressAfter require -Headless'
}
if ($Gdb -and $Headless -and $Until) { throw '-Gdb pauses execution and cannot be combined with -Until' }
Invoke-NexusCleanup

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    if ([IO.Path]::GetFullPath($BuildDir) -ne [IO.Path]::GetFullPath((Join-Path $RepoRoot 'build'))) {
        throw "No staged ESP at $EspDir. Stage this custom build directory first."
    }
    & (Join-Path $PSScriptRoot 'build.ps1') -Release:$Release -SkipDiskImage
}
if (-not (Test-Path -LiteralPath (Join-Path $EspDir 'nexus/kernel.elf'))) { throw "No staged kernel at $EspDir" }
if ($Release) {
    $metadata = Join-Path $BuildDir 'build.json'
    if (-not (Test-Path -LiteralPath $metadata) -or (Get-Content -LiteralPath $metadata -Raw | ConvertFrom-Json).Profile -ne 'release') {
        throw 'Release outputs are not staged. Run scripts/build.ps1 -Release first.'
    }
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

# Keep a writable variable store per machine. Windows argument quoting in
# Start-NexusQemu also permits build/firmware directories containing spaces.
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

# A monitor, only when there are keys to send. The session on this machine
# ends when somebody ends it -- there is no other way out, by design -- so a
# headless run that wants to see the end of one has to press the key a person
# would.
$Batches = @($Press -split ';' | Where-Object { $_.Trim() })
$Markers = @($PressAfter -split ';' | Where-Object { $_.Trim() })
if ($Batches.Count -gt 0 -and $MonitorPort -eq 0) { $MonitorPort = Get-FreeMonitorPort }
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

$RunLock = $null
$process = $null
try {
    # Acquire before clearing evidence or creating the disk. A second launch
    # against the same directory must leave the first one's files alone.
    try { $RunLock = [IO.File]::Open((Join-Path $BuildDir 'run.lock'), 'OpenOrCreate', 'ReadWrite', 'None') }
    catch { throw "This build directory is already running: $BuildDir" }
    $Disk = Get-NexusDiskImage -BuildDir $BuildDir
    if (-not (Test-Path -LiteralPath $Disk)) {
        $ProgramDir = Join-Path $BuildDir 'programs'
        if (-not (Test-Path -LiteralPath (Join-Path $ProgramDir 'init.elf'))) {
            throw "No disk or staged programs in $BuildDir. Run scripts/build.ps1 first."
        }
        $sizes = Get-NexusDiskSizes
        & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'make-disk.ps1') `
            -Output $Disk -ProgramDir $ProgramDir -SizeMiB $sizes.SizeMiB -FatMiB $sizes.FatMiB
        if ($LASTEXITCODE -ne 0) { throw 'Initial disk creation failed' }
    }
    # Check for a QEMU launched by another script before replacing its log.
    $diskCheck = [IO.File]::Open($Disk, 'Open', 'ReadWrite', 'None')
    $diskCheck.Dispose()
    if (Test-Path -LiteralPath $SerialLog) { Remove-Item -LiteralPath $SerialLog -Force }
    if ($HostHttpPort -eq 0) { $HostHttpPort = Get-FreeMonitorPort -From 18100 -To 19000 }
    Write-Host "==> Guest HTTP service: http://127.0.0.1:$HostHttpPort" -ForegroundColor DarkGray
    $QemuArgs = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
    -FirmwareCode $FirmwareCodeLocal -FirmwareVars $FirmwareVars -SerialLog $SerialLog `
    -Memory $Memory -MonitorPort $MonitorPort -HostHttpPort $HostHttpPort
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
    $process = Start-NexusQemu -Executable $QemuExe.Source -Arguments $QemuArgs -ErrorLog $ErrorLog
    $exited = $false
    $reached = $false
    $settled = $false
    if ($Until) {
        $seenAt = -1
        $reportsThen = 0
        # How far through the list of things to press we are. Each batch waits
        # for its own marker, so a machine that shows the wizard and then a
        # desktop can be answered at both points from one run.
        $pressing = 0
        $clock = [Diagnostics.Stopwatch]::StartNew()
        while ($clock.Elapsed.TotalSeconds -lt $Timeout) {
            $exited = $process.WaitForExit(500)
            $waited = [int]$clock.Elapsed.TotalSeconds
            if (-not (Test-Path $SerialLog)) { if ($exited) { break }; continue }
            $sofar = (Get-Content $SerialLog -Raw -Encoding UTF8) -replace "`0", ''
            if (-not $sofar) { if ($exited) { break }; continue }
            if ($sofar -match '(?m)^ EXCEPTION [0-9]+:|KERNEL PANIC|the system has been halted') {
                throw "Guest failed; see $SerialLog"
            }
            if ($exited) {
                if ($process.ExitCode -ne 0) { throw "QEMU failed (exit $($process.ExitCode)); see $ErrorLog" }
                if ($pressing -lt $Batches.Count) { throw 'QEMU exited before all key batches were sent' }
                $reached = $sofar.Contains($Until)
                break
            }
            $reports = ([regex]::Matches($sofar, '\[mon \] \d+s uptime')).Count

            # The keys somebody would press, once the machine has got far
            # enough to have somebody to press them at.
            if ($pressing -lt $Batches.Count) {
                $marker = if ($Markers.Count -gt 0) { $Markers[$pressing].Trim() } else { '' }
                if (-not $marker -or $sofar.Contains($marker)) {
                    $keys = $Batches[$pressing].Trim()
                    $pressing++
                    Write-Host "==> Pressing '$keys' after $waited s" -ForegroundColor Cyan
                    Send-Keys -Keys $keys -Port $MonitorPort
                }
                continue
            }

            if ($seenAt -lt 0) {
                if (-not $sofar.Contains($Until)) { continue }
                $seenAt = $waited
                $reached = $true
                $reportsThen = $reports
                Write-Host "==> Saw '$Until' after $waited s; waiting for one more report" -ForegroundColor Cyan
                continue
            }
            if ($reports -gt $reportsThen) {
                Write-Host "==> Settled after $($waited - $seenAt) s more; stopping QEMU" -ForegroundColor Cyan
                $settled = $true
                break
            }
            if ($exited) { break }
        }
        if (-not $reached) { throw "Did not reach '$Until' within $Timeout seconds (QEMU exited: $exited). See $SerialLog and $ErrorLog" }
        if (-not $settled -and -not $exited) { throw "Reached '$Until' but no subsequent monitor report before timeout. See $SerialLog" }
    } else {
        $exited = $process.WaitForExit($Timeout * 1000)
    }
    if (-not $exited -and -not $process.HasExited) {
        if (-not $Until) {
            Write-Host "==> Timeout after $Timeout s; stopping QEMU" -ForegroundColor Yellow
        }
        try { $process.Kill() } catch { }
        $process.WaitForExit(5000) | Out-Null
    } elseif ($exited -and $process.ExitCode -ne 0) {
        throw "QEMU failed (exit $($process.ExitCode)); see $ErrorLog"
    }
    if (-not (Test-Path -LiteralPath $SerialLog) -or (Get-Item -LiteralPath $SerialLog).Length -eq 0) {
        throw "QEMU produced no serial output; see $ErrorLog"
    }
    $finalLog = (Get-Content -LiteralPath $SerialLog -Raw) -replace "`0", ''
    if ($finalLog -match '(?m)^ EXCEPTION [0-9]+:|KERNEL PANIC|the system has been halted') { throw "Guest failed; see $SerialLog" }
} else {
    & $QemuExe.Source @QemuArgs
    if ($LASTEXITCODE -ne 0) { throw "QEMU failed (exit $LASTEXITCODE)" }
}
} finally {
    if ($null -ne $process -and -not $process.HasExited) {
        $process.Kill()
        $process.WaitForExit(5000) | Out-Null
    }
    if ($null -ne $RunLock) { $RunLock.Dispose() }
}

Write-Host ''
if (Test-Path $SerialLog) {
    Write-Host "==> Serial output ($SerialLog)" -ForegroundColor Cyan
    Get-Content $SerialLog
} else {
    Write-Host 'No serial output was produced.' -ForegroundColor Red
}
