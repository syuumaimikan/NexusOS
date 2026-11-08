<#
.SYNOPSIS
    Tests the creative and media utilities: Synthesizer & Music Studio, Paint Studio,
    and Archive Manager on NexusOS.
#>
[CmdletBinding()]
param(
    [int]$Timeout = 240
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'
$Log = Join-Path $BuildDir 'creative-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-creative.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $Log) { Remove-Item $Log -Force }

$MonitorPort = Get-Random -Minimum 46000 -Maximum 46999
$QemuArgs = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
    -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars -SerialLog $Log `
    -MonitorPort $MonitorPort -Headless

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

        # 1. Test Synthesizer & Music Studio
        Write-Host '==> Launching Music Studio from launcher (F3 -> musi)' -ForegroundColor Cyan
        Send-Keys @('f3')
        if (-not (Wait-For -Text 'launch: a window for starting things by name' -Seconds 60)) {
            throw 'the launcher never started'
        }
        Start-Sleep -Milliseconds 800
        Send-Text 'musi'
        Start-Sleep -Milliseconds 500
        Send-Keys @('ret')

        if (-not (Wait-For -Text 'compositor: started the synthesizer and music studio' -Seconds 60)) {
            $failures += 'compositor did not start music'
        } else {
            Write-Host '    ok compositor started music' -ForegroundColor Green
        }
        Start-Sleep -Seconds 2

        # Play piano notes (Z, C, G) and toggle melody
        Send-Keys @('z', 'c', 'g')
        Start-Sleep -Milliseconds 400
        Send-Keys @('2') # Bach Minuet
        Start-Sleep -Milliseconds 500
        Send-Keys @('spc') # Pause
        Start-Sleep -Milliseconds 300

        # 2. Test Paint & Pixel Art Studio
        Write-Host '==> Launching Paint Studio from launcher (F3 -> pain)' -ForegroundColor Cyan
        Send-Keys @('f3')
        Start-Sleep -Milliseconds 800
        Send-Text 'pain'
        Start-Sleep -Milliseconds 500
        Send-Keys @('ret')

        if (-not (Wait-For -Text 'compositor: started the paint and pixel art studio' -Seconds 60)) {
            $failures += 'compositor did not start paint'
        } else {
            Write-Host '    ok compositor started paint' -ForegroundColor Green
        }
        Start-Sleep -Seconds 2

        # Draw a line / circle and export BMP
        Send-Keys @('down', 'right', 'ret')
        Start-Sleep -Milliseconds 300
        Send-Keys @('c') # Circle tool
        Start-Sleep -Milliseconds 300
        Send-Keys @('ret', 'right', 'right', 'down', 'down', 'ret')
        Start-Sleep -Milliseconds 500
        Send-Keys @('s') # Save BMP
        Start-Sleep -Seconds 1

        # 3. Test Archive Manager
        Write-Host '==> Launching Archive Manager from launcher (F3 -> arch)' -ForegroundColor Cyan
        Send-Keys @('f3')
        Start-Sleep -Milliseconds 800
        Send-Text 'arch'
        Start-Sleep -Milliseconds 500
        Send-Keys @('ret')

        if (-not (Wait-For -Text 'compositor: started the archive and package manager' -Seconds 60)) {
            $failures += 'compositor did not start archive'
        } else {
            Write-Host '    ok compositor started archive' -ForegroundColor Green
        }
        Start-Sleep -Seconds 2
        Send-Keys @('down', 'down', 'up')
        Start-Sleep -Milliseconds 400
        Send-Keys @('v') # Verify
        Start-Sleep -Seconds 1

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

$content = if (Test-Path $Log) { (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", '' } else { '' }

if ($content.Contains('KERNEL PANIC')) {
    $failures += 'kernel panic observed'
}
if ($content.Contains('music: PANIC')) {
    $failures += 'music studio crashed'
}
if ($content.Contains('paint: PANIC')) {
    $failures += 'paint studio crashed'
}
if ($content.Contains('archive: PANIC')) {
    $failures += 'archive manager crashed'
}

if ($failures.Count -gt 0) {
    Write-Host "`nTest FAILED with $($failures.Count) error(s):" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}

Write-Host "`nAll Music Studio, Paint Studio, and Archive Manager tests PASSED!" -ForegroundColor Green
exit 0

