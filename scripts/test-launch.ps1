<#
.SYNOPSIS
    Opens the launcher with F3, types a few letters, and checks that the thing
    it named started.

.DESCRIPTION
    The launcher is the one client whose messages this system reads rather than
    counts, so the thing worth checking is exactly that: four bytes from a
    window with no authority at all turn into a program the compositor started
    and lent things to.

    Typed rather than clicked. A launcher that could only be driven with a
    pointer would have missed the point of being a launcher.

.PARAMETER Timeout
    How long to wait for each stage, in seconds.

.PARAMETER Shot
    Where to put a screenshot of the launcher, if one is wanted.
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
$Log = Join-Path $BuildDir 'launch-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-launch.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $Log) { Remove-Item $Log -Force }

$MonitorPort = Get-Random -Minimum 33000 -Maximum 33999
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
            param([string[]]$Keys, [int]$Pause = 180)
            foreach ($key in $Keys) {
                $writer.WriteLine("sendkey $key")
                Start-Sleep -Milliseconds $Pause
            }
        }

        Write-Host '==> F3' -ForegroundColor Cyan
        Send-Keys @('f3')
        if (-not (Wait-For -Text 'launch: a window for starting things by name' -Seconds 60)) {
            $failures += 'the launcher never started'
        }

        # `tl` is not a substring of anything. It finds "Terminal" only if the
        # letters are matched in order, which is the whole reason the matching
        # is written the way it is.
        Write-Host '==> Typing "tl", which only a subsequence match finds' -ForegroundColor Cyan
        Send-Keys @('t', 'l')
        Start-Sleep -Seconds 2

        if ($Shot) {
            $Ppm = Join-Path $BuildDir 'launch.ppm'
            Invoke-Screendump -Writer $writer -Path $Ppm
            Convert-PpmToPng -PpmPath $Ppm -PngPath $Shot | Out-Null
            Write-Host "    Screenshot: $Shot" -ForegroundColor DarkGray
        }

        Write-Host '==> Enter' -ForegroundColor Cyan
        Send-Keys @('ret')

        if (-not (Wait-For -Text 'launch: asked for term' -Seconds 60)) {
            $failures += 'the launcher never asked for the terminal'
        }
        if (-not (Wait-For -Text 'compositor: started a terminal' -Seconds 60)) {
            $failures += 'the compositor never started a terminal'
        }

        # Opened a second time and closed with Escape, which is the other way
        # out and the one somebody uses when they change their mind.
        Write-Host '==> F3 again, then Escape' -ForegroundColor Cyan
        Send-Keys @('f3')
        Start-Sleep -Seconds 2
        Send-Keys @('esc')
        if (-not (Wait-For -Text 'launch: closed without starting anything' -Seconds 60)) {
            $failures += 'Escape did not close the launcher'
        }

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

$output = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''

foreach ($expected in @(
        'compositor: somebody asked for the launcher',
        'compositor: started the launcher, and lent it nothing at all',
        'launch: a window for starting things by name',
        'launch: asked for term',
        'compositor: started a terminal',
        'launch: closed without starting anything'
    )) {
    if ($output.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "never reported: $expected"
    }
}

foreach ($bad in @('launch: FAILED', 'launch: PANIC', 'KERNEL PANIC')) {
    if ($output.Contains($bad)) {
        $failures += "saw: $bad"
    } else {
        Write-Host "    ok   absent: $bad" -ForegroundColor DarkGray
    }
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'Launcher tests passed.' -ForegroundColor Green
    exit 0
} else {
    foreach ($why in $failures) { Write-Host "    FAIL $why" -ForegroundColor Red }
    Write-Host "Log: $Log"
    exit 1
}
