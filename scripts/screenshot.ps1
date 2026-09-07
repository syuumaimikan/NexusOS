<#
.SYNOPSIS
    Boots NexusOS in QEMU and captures a screenshot of the guest display.

.DESCRIPTION
    Starts QEMU headless with its human monitor on a local TCP port, waits for
    the guest to reach a steady state, then issues `screendump` and converts the
    resulting PPM to PNG.

    This is how the graphical side of the boot is verified: the serial log
    proves the kernel ran, and the screenshot proves what it actually painted.

.PARAMETER Delay
    Seconds to let the guest run before capturing. Default 12.

.PARAMETER Output
    Path of the PNG to write. Default build/screenshot.png.
#>
[CmdletBinding()]
param(
    [int]$Delay = 12,
    [string]$Output
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'capture.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'
$SerialLog = Join-Path $BuildDir 'serial.log'
$PpmPath = Join-Path $BuildDir 'screen.ppm'

if (-not $Output) { $Output = Join-Path $BuildDir 'screenshot.png' }

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw "No staged ESP at $EspDir. Run .\scripts\build.ps1 first."
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source

New-Item -ItemType Directory -Force -Path $BuildDir | Out-Null
$FirmwareCodeLocal = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCodeLocal)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCodeLocal -Force
}
$FirmwareVars = Join-Path $BuildDir 'edk2-vars.fd'
if (-not (Test-Path $FirmwareVars)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force
}

foreach ($stale in @($SerialLog, $PpmPath, $Output)) {
    if (Test-Path $stale) { Remove-Item $stale -Force }
}

# Pick a free monitor port so concurrent runs do not collide.
$MonitorPort = Get-Random -Minimum 24000 -Maximum 24999

$QemuArgs = @(
    '-machine', 'q35',
    '-cpu', 'qemu64,+pdpe1gb',
    '-smp', '4',
    '-m', '1G',
    '-drive', "if=pflash,format=raw,unit=0,readonly=on,file=$FirmwareCodeLocal",
    '-drive', "if=pflash,format=raw,unit=1,file=$FirmwareVars",
    '-drive', "format=raw,file=fat:rw:$EspDir",
    '-serial', "file:$SerialLog",
    '-monitor', "tcp:127.0.0.1:$MonitorPort,server,nowait",
    '-display', 'none',
    '-no-reboot'
)

Write-Host "==> Booting NexusOS (monitor on port $MonitorPort)" -ForegroundColor Cyan
$process = Start-Process -FilePath $QemuExe.Source -ArgumentList $QemuArgs -PassThru -NoNewWindow

try {
    Write-Host "==> Letting the guest run for $Delay s" -ForegroundColor Cyan
    Start-Sleep -Seconds $Delay

    if ($process.HasExited) { throw "QEMU exited early with code $($process.ExitCode)" }

    Write-Host '==> Capturing the display' -ForegroundColor Cyan
    $client = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $MonitorPort)
    try {
        $stream = $client.GetStream()
        $writer = New-Object System.IO.StreamWriter($stream)
        $writer.AutoFlush = $true
        # The monitor greets us first; give it a moment, then issue the dump.
        Start-Sleep -Milliseconds 500
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
if (Test-Path $SerialLog) {
    Write-Host "Serial log: $SerialLog"
}
