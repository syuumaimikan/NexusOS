<#
.SYNOPSIS
    Boots the machine and captures the screen at a moment of your choosing.

.DESCRIPTION
    `screenshot.ps1` waits a fixed number of seconds, which is the right thing
    for a picture of a settled desktop and the wrong thing for anything that
    happens while the machine is starting: the boot logo is on screen for as
    long as bring-up takes, and how long that is depends on the host.

    So this waits for a *marker* on the serial line and captures as soon as it
    appears. The logo is up before the line that says the scheduler started, and
    gone by the time the display thread has drawn its first panel.

.PARAMETER Until
    The serial marker to capture at.

.PARAMETER After
    Seconds to wait after the marker, for something that is drawn just after it
    is announced.

.PARAMETER Output
    Where to put the PNG.
#>
[CmdletBinding()]
param(
    [string]$Until = 'scheduler started',
    [double]$After = 0.2,
    [string]$Output
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')
. (Join-Path $PSScriptRoot 'capture.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'
$Log = Join-Path $BuildDir 'shot.log'
$Ppm = Join-Path $BuildDir 'shot.ppm'
if (-not $Output) { $Output = Join-Path $BuildDir 'shot.png' }

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-shot.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force
if (Test-Path $Log) { Remove-Item $Log -Force }

$MonitorPort = Get-Random -Minimum 35000 -Maximum 36999
$QemuArgs = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
    -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars -SerialLog $Log `
    -MonitorPort $MonitorPort -Headless

Write-Host "==> Booting, and capturing at '$Until'" -ForegroundColor Cyan
$process = Start-Process -FilePath $QemuExe.Source -ArgumentList $QemuArgs -PassThru -NoNewWindow
try {
    $seen = $false
    # Polled finely, because what is being caught may be on screen for less
    # than a second and a one-second poll would miss it about half the time.
    for ($waited = 0; $waited -lt 1200; $waited++) {
        Start-Sleep -Milliseconds 100
        if ($process.HasExited) { break }
        if (-not (Test-Path $Log)) { continue }
        $sofar = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''
        if ($sofar.Contains($Until)) { $seen = $true; break }
    }
    if (-not $seen) { throw "never saw '$Until' on the serial line" }
    Start-Sleep -Milliseconds ([int]($After * 1000))

    $client = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $MonitorPort)
    try {
        $writer = New-Object System.IO.StreamWriter($client.GetStream())
        $writer.AutoFlush = $true
        Invoke-Screendump -Writer $writer -Path $Ppm
        Convert-PpmToPng -PpmPath $Ppm -PngPath $Output | Out-Null
    } finally {
        $client.Close()
    }
} finally {
    if (-not $process.HasExited) {
        try { $process.Kill() } catch { }
    }
    $process.WaitForExit(5000) | Out-Null
}

Write-Host "Screenshot: $Output"
