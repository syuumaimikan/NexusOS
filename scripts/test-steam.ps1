<#
.SYNOPSIS
    Tests the signed-out Steam integration preview and its fail-closed controls.
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
$Log = Join-Path $BuildDir 'steam-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-steam.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $Log) { Remove-Item $Log -Force }

$MonitorPort = Get-Random -Minimum 44000 -Maximum 44999
$QemuArgs = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
    -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars -SerialLog $Log `
    -MonitorPort $MonitorPort -Headless -EspReadOnly
$QemuArgs += @('-snapshot')

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
            param([string[]]$Keys, [int]$Pause = 140)
            foreach ($key in $Keys) {
                $writer.WriteLine("sendkey $key")
                Start-Sleep -Milliseconds $Pause
            }
        }

        function Send-Text {
            param([string]$Text)
            foreach ($letter in $Text.ToCharArray()) {
                $key = switch ($letter) {
                    ' ' { 'spc' }
                    '.' { 'dot' }
                    '/' { 'slash' }
                    '-' { 'minus' }
                    default { $letter }
                }
                Send-Keys @($key) -Pause 105
            }
        }

        Write-Host '==> Launching the signed-out Steam integration preview (F3 -> steam)' -ForegroundColor Cyan
        Send-Keys @('f3')
        if (-not (Wait-For -Text 'launch: a window for starting things by name' -Seconds 60)) {
            throw 'the launcher never started'
        }
        Start-Sleep -Milliseconds 800
        Send-Text 'steam'
        Start-Sleep -Seconds 1
        Send-Keys @('ret')

        if (-not (Wait-For -Text 'steam: window opened with compositor, running UI loop' -Seconds 60)) {
            $failures += 'Steam preview did not enter its UI loop'
        }

        if ($Shot) {
            if (-not (Wait-For -Text 'steam: window opened with compositor, running UI loop' -Seconds 30)) {
                $failures += 'Steam preview window never entered its UI loop for capture'
            } else {
                $shotDirectory = Split-Path -Parent $Shot
                if ($shotDirectory -and -not (Test-Path $shotDirectory)) {
                    New-Item -ItemType Directory -Force -Path $shotDirectory | Out-Null
                }
                $Ppm = [System.IO.Path]::ChangeExtension($Shot, '.ppm')
                Invoke-Screendump -Writer $writer -Path $Ppm
                Convert-PpmToPng -PpmPath $Ppm -PngPath $Shot | Out-Null
                Write-Host "    Screenshot: $Shot" -ForegroundColor DarkGray
            }
        }

        Write-Host '==> Verifying unauthenticated controls are denied' -ForegroundColor Cyan
        Start-Sleep -Seconds 2
        # These inputs must not unlock achievements or launch games while signed out.
        Send-Keys @('2')
        Start-Sleep -Milliseconds 500
        Send-Keys @('u')
        Start-Sleep -Milliseconds 500

        # The preview must not launch a game without a genuine authenticated session.
        Write-Host '==> Attempting a blocked game launch' -ForegroundColor Cyan
        Send-Keys @('p')
        Start-Sleep -Seconds 2

        $writer.WriteLine('quit')
        Start-Sleep -Milliseconds 800
    } finally {
        $client.Close()
    }
} finally {
    if (-not $process.HasExited) {
        try { $process.Kill() } catch { }
    }
    $process.WaitForExit(5000) | Out-Null
}

$output = if (Test-Path $Log) { (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", '' } else { '' }

foreach ($expected in @(
        'steam: Steam integration preview starting',
        'steam: window opened with compositor, running UI loop',
        'steam: signed out; no authenticated Valve session',
        'steam: blocked achievement update; no authenticated Valve session',
        'steam: blocked game launch; no authenticated Valve session'
    )) {
    if ($output.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "never reported: $expected"
    }
}

if ($output.Contains('KERNEL PANIC')) { $failures += 'the kernel panicked' }
if ($output.Contains('steam: PANIC')) { $failures += 'the steam client panicked' }

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'Steam signed-out preview tests passed.' -ForegroundColor Green
    exit 0
} else {
    foreach ($failure in $failures) {
        Write-Host "    FAIL $failure" -ForegroundColor Red
    }
    Write-Host "Log: $Log"
    exit 1
}
