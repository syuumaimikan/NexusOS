<#
.SYNOPSIS
    Tests wallpaper switching and video wallpaper streaming on NexusOS.
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
$Log = Join-Path $BuildDir 'wall-switch-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-wall-switch.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $Log) { Remove-Item $Log -Force }

$MonitorPort = Get-Random -Minimum 44000 -Maximum 44999
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
            param([string[]]$Keys)
            foreach ($key in $Keys) {
                $writer.WriteLine("sendkey $key")
                Start-Sleep -Milliseconds 45
            }
        }

        function Send-Text {
            param([string]$Text)
            $keys = @()
            foreach ($character in $Text.ToCharArray()) {
                switch ($character) {
                    ' ' { $keys += 'spc' }
                    '.' { $keys += 'dot' }
                    '-' { $keys += 'minus' }
                    '_' { $keys += 'shift-minus' }
                    default { $keys += [string]$character }
                }
            }
            Send-Keys -Keys $keys
            Send-Keys @('ret')
        }

        Write-Host '==> Opening a terminal' -ForegroundColor Cyan
        Send-Keys @('f3')
        Start-Sleep -Milliseconds 500
        Send-Text 'term'
        if (-not (Wait-For -Text 'term: a terminal, with a shell in it' -Seconds 60)) {
            throw 'the terminal never started'
        }
        Start-Sleep -Seconds 2

        Write-Host '==> Querying wallpaper status (wallpaper)' -ForegroundColor Cyan
        Send-Text 'wallpaper'
        if (-not (Wait-For -Text 'term: ran wall' -Seconds 30)) {
            $failures += 'wallpaper status query failed'
        }

        Write-Host '==> Listing wallpapers and video backgrounds (wallpaper list)' -ForegroundColor Cyan
        Send-Text 'wallpaper list'
        Start-Sleep -Seconds 2

        Write-Host '==> Setting animated video wallpaper (wallpaper video rain.avi)' -ForegroundColor Cyan
        Send-Text 'wallpaper video rain.avi'
        if (-not (Wait-For -Text 'wall: playing rain.avi behind everything' -Seconds 45)) {
            $failures += 'failed to activate rain.avi video wallpaper'
        }
        if (-not (Wait-For -Text 'wall: decoding at' -Seconds 45)) {
            $failures += 'wallpaper engine failed to stream video frames'
        }

        Write-Host '==> Switching to static picture wallpaper (wallpaper set space.png)' -ForegroundColor Cyan
        Send-Text 'wallpaper set space.png'
        if (-not (Wait-For -Text 'wall: showing space.png behind everything' -Seconds 45)) {
            $failures += 'failed to switch to space.png picture wallpaper'
        }

        Write-Host '==> Switching to procedural animated style (wallpaper style stars)' -ForegroundColor Cyan
        Send-Text 'wallpaper style stars'
        if (-not (Wait-For -Text 'wall: the look changed to stars' -Seconds 45)) {
            $failures += 'failed to switch to stars style'
        }

        Write-Host '==> Clearing wallpaper to background pattern (wallpaper clear)' -ForegroundColor Cyan
        Send-Text 'wallpaper clear'
        Start-Sleep -Seconds 2

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

$output = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''

if ($output -match 'wall: decoding at ([\d.]+) frames a second') {
    Write-Host "    ok   decoded $($Matches[1]) frames a second behind everything" -ForegroundColor DarkGray
}

foreach ($check in @(
        'wall: playing rain.avi behind everything',
        'wall: showing space.png behind everything',
        'wall: the look changed to stars'
    )) {
    if ($output.Contains($check)) {
        Write-Host "    ok   $check" -ForegroundColor DarkGray
    } else {
        $failures += "missing expected output: $check"
    }
}

foreach ($bad in @(
        'wall: FAILED',
        'wall: PANIC',
        'term: PANIC',
        'KERNEL PANIC'
    )) {
    if ($output.Contains($bad)) {
        $failures += "found unexpected error: $bad"
    } else {
        Write-Host "    ok   absent: $bad" -ForegroundColor DarkGray
    }
}

if ($failures.Count -gt 0) {
    Write-Host ''
    Write-Host 'Wallpaper switching test failed:' -ForegroundColor Red
    foreach ($failure in $failures) {
        Write-Host "  - $failure" -ForegroundColor Red
    }
    exit 1
}

Write-Host ''
Write-Host 'Wallpaper switching and video wallpaper tests passed.' -ForegroundColor Green
exit 0

