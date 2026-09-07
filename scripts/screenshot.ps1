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
        $writer.WriteLine("screendump $PpmPath")
        Start-Sleep -Seconds 3
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

if (-not (Test-Path $PpmPath)) { throw 'QEMU produced no screendump.' }

# Convert the binary PPM (P6) QEMU writes into a PNG that ordinary tools read.
Add-Type -AssemblyName System.Drawing
$bytes = [System.IO.File]::ReadAllBytes($PpmPath)

# Parse the P6 header: magic, width, height, maxval, each whitespace separated,
# with '#' comments permitted between tokens.
$pos = 0
$tokens = New-Object System.Collections.Generic.List[string]
while ($tokens.Count -lt 4 -and $pos -lt $bytes.Length) {
    # Skip whitespace.
    while ($pos -lt $bytes.Length -and [char]$bytes[$pos] -match '\s') { $pos++ }
    if ($pos -lt $bytes.Length -and [char]$bytes[$pos] -eq '#') {
        while ($pos -lt $bytes.Length -and $bytes[$pos] -ne 10) { $pos++ }
        continue
    }
    $start = $pos
    while ($pos -lt $bytes.Length -and -not ([char]$bytes[$pos] -match '\s')) { $pos++ }
    $tokens.Add([System.Text.Encoding]::ASCII.GetString($bytes, $start, $pos - $start))
}
$pos++  # single whitespace byte after maxval

if ($tokens[0] -ne 'P6') { throw "Unexpected screendump format: $($tokens[0])" }
$width = [int]$tokens[1]
$height = [int]$tokens[2]

$bitmap = New-Object System.Drawing.Bitmap($width, $height, [System.Drawing.Imaging.PixelFormat]::Format24bppRgb)
$rect = New-Object System.Drawing.Rectangle(0, 0, $width, $height)
$data = $bitmap.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::WriteOnly, $bitmap.PixelFormat)
try {
    # PPM is packed RGB rows; GDI+ wants BGR rows padded to a 4-byte stride.
    $row = New-Object byte[] $data.Stride
    for ($y = 0; $y -lt $height; $y++) {
        $src = $pos + $y * $width * 3
        for ($x = 0; $x -lt $width; $x++) {
            $i = $src + $x * 3
            $o = $x * 3
            $row[$o]     = $bytes[$i + 2]  # blue
            $row[$o + 1] = $bytes[$i + 1]  # green
            $row[$o + 2] = $bytes[$i]      # red
        }
        [System.Runtime.InteropServices.Marshal]::Copy($row, 0, [IntPtr]($data.Scan0.ToInt64() + $y * $data.Stride), $data.Stride)
    }
} finally {
    $bitmap.UnlockBits($data)
}

$bitmap.Save($Output, [System.Drawing.Imaging.ImageFormat]::Png)
$bitmap.Dispose()
Remove-Item $PpmPath -Force

Write-Host ''
Write-Host "Screenshot: $Output ($width x $height)" -ForegroundColor Green
if (Test-Path $SerialLog) {
    Write-Host "Serial log: $SerialLog"
}
