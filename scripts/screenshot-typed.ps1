<#
.SYNOPSIS
    Boots NexusOS, types into it, and captures the result.

.DESCRIPTION
    Like screenshot.ps1, but sends keystrokes through QEMU's monitor before
    capturing, so the picture shows the system reacting to input rather than
    merely running.

.PARAMETER Keys
    Keys to send, in QEMU `sendkey` names. Default types "nexus". Accepts either
    a PowerShell array or one comma-separated string.

.PARAMETER Output
    Path of the PNG to write.
#>
[CmdletBinding()]
param(
    [string[]]$Keys = @('n', 'e', 'x', 'u', 's'),
    [string]$Output
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')

# Split what the caller passed, whether it arrived as an array or as one string.
#
# `powershell -File screenshot-typed.ps1 -Keys n,e,x,u,s,f1` does not bind six
# elements: with -File the argument is a literal string, and the binder wraps it
# as a single-element array. The script then sent QEMU `sendkey n,e,x,u,s,f1`,
# which it rejected, and the capture showed a system that had received nothing —
# a failure that looked exactly like a broken input path and was not one.
$Keys = @($Keys | ForEach-Object { $_ -split '[,\s]+' } | Where-Object { $_ })
if ($Keys.Count -eq 0) { throw 'no keys to send' }

. (Join-Path $PSScriptRoot 'capture.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'
$SerialLog = Join-Path $BuildDir 'typed.log'
$PpmPath = Join-Path $BuildDir 'typed.ppm'
if (-not $Output) { $Output = Join-Path $BuildDir 'shot-typed.png' }

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source

New-Item -ItemType Directory -Force -Path $BuildDir | Out-Null
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-typed.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

foreach ($stale in @($SerialLog, $PpmPath, $Output)) {
    if (Test-Path $stale) { Remove-Item $stale -Force }
}

$MonitorPort = Get-Random -Minimum 26000 -Maximum 26999
$QemuArgs = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
    -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars -SerialLog $SerialLog `
    -MonitorPort $MonitorPort -Headless

Write-Host "==> Booting NexusOS (monitor on port $MonitorPort)" -ForegroundColor Cyan
$process = Start-Process -FilePath $QemuExe.Source -ArgumentList $QemuArgs -PassThru -NoNewWindow

try {
    # Wait for the input thread rather than a fixed delay: boot time varies with
    # how busy the host is, and typing before the thread exists loses the keys.
    $ready = $false
    for ($waited = 0; $waited -lt 40; $waited++) {
        Start-Sleep -Seconds 1
        if ($process.HasExited) { throw "QEMU exited early with code $($process.ExitCode)" }
        if (Test-Path $SerialLog) {
            $sofar = (Get-Content $SerialLog -Raw -Encoding UTF8) -replace "`0", ''
            if ($sofar.Contains('input thread')) { $ready = $true; break }
        }
    }
    if (-not $ready) { throw 'the input thread never started' }

    Write-Host "==> Typing: $($Keys -join ' ')" -ForegroundColor Cyan
    $client = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $MonitorPort)
    try {
        $writer = New-Object System.IO.StreamWriter($client.GetStream())
        $writer.AutoFlush = $true
        Start-Sleep -Milliseconds 500
        foreach ($key in $Keys) {
            $writer.WriteLine("sendkey $key")
            Start-Sleep -Milliseconds 250
        }
        # Wait for the guest to say it acted on the keys, rather than sleeping
        # for a plausible-looking interval. A capture taken on a guess shows
        # whatever the panel happened to hold, which for a while was a frame
        # from before the typing it was meant to demonstrate.
        Write-Host '==> Waiting for the guest to acknowledge the keys' -ForegroundColor Cyan
        $acted = $false
        for ($waited = 0; $waited -lt 20; $waited++) {
            Start-Sleep -Seconds 1
            if ($process.HasExited) { throw "QEMU exited early with code $($process.ExitCode)" }
            $sofar = (Get-Content $SerialLog -Raw -Encoding UTF8) -replace "`0", ''
            if ($sofar -match 'keyboard: \d+ scancodes, \d+ keys decoded,\s+(\d+) acted on') {
                if ([int]$Matches[1] -ge $Keys.Count) { $acted = $true; break }
            }
        }
        if (-not $acted) { throw 'the guest never reported acting on the keys' }
        # One repaint period, so the panel shows the result rather than the
        # frame that was on screen when the last key arrived.
        Start-Sleep -Seconds 1

        Write-Host '==> Capturing the display' -ForegroundColor Cyan
        Invoke-Screendump -Writer $writer -Path $PpmPath
        $writer.WriteLine('quit')
    } finally {
        $client.Close()
    }

    # Let QEMU shut down on its own after `quit`, so it closes the serial file
    # and the screendump properly. The kill in the finally block is the
    # fallback for a guest that will not go away, not the normal path.
    if (-not $process.WaitForExit(15000)) {
        Write-Warning 'QEMU did not exit after quit; killing it'
    }
} finally {
    if (-not $process.HasExited) {
        try { $process.Kill() } catch { }
    }
    $process.WaitForExit(5000) | Out-Null
}

if (-not (Test-Path $PpmPath)) { throw 'QEMU produced no screendump.' }

$size = Convert-PpmToPng -PpmPath $PpmPath -PngPath $Output
Remove-Item $PpmPath -Force

Write-Host ''
Write-Host "Screenshot: $Output ($($size.Width) x $($size.Height))" -ForegroundColor Green
