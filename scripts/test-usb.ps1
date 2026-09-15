<#
.SYNOPSIS
    Types `usb` into the shell and checks that a program can reach the drive.

.DESCRIPTION
    The kernel reading a USB drive and a *program* reading one are different
    claims. The kernel's half is checked at boot, in the serial log; this checks
    the other half, by driving the machine the way a person would:

      the desktop is clicked -> a terminal opens and is lent the removable-drive
      channel -> `usb` lists the drives -> `usb 1` lists what is on the one with
      a filesystem -> `usb 1 HELLO.TXT` reads a file off it

    Every one of those crosses the whole stack: a request on a channel, the
    kernel's service thread, a FAT32 mount, and four layers of USB underneath
    that.

.PARAMETER Timeout
    How long to wait for each stage, in seconds.

.PARAMETER Shot
    Where to put a screenshot, if one is wanted.
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
$Log = Join-Path $BuildDir 'usb-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-usb.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $Log) { Remove-Item $Log -Force }

$MonitorPort = Get-Random -Minimum 36000 -Maximum 36999
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
$checks = 0

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
            param([string[]]$Keys, [int]$Pause = 150)
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
                    default { $letter }
                }
                Send-Keys @($key) -Pause 110
            }
        }

        Write-Host '==> Opening a terminal' -ForegroundColor Cyan
        Send-Keys @('f3')
        if (-not (Wait-For -Text 'launch: a window for starting things by name' -Seconds 60)) {
            throw 'the launcher never started'
        }
        Send-Text 'tl'
        Start-Sleep -Seconds 1
        Send-Keys @('ret')
        if (-not (Wait-For -Text 'compositor: started a terminal' -Seconds 60)) {
            throw 'the terminal never started'
        }
        Start-Sleep -Seconds 2

        # `usb` with nothing: what drives are there.
        Write-Host '==> usb' -ForegroundColor Cyan
        Send-Text 'usb'
        Send-Keys @('ret')
        Start-Sleep -Seconds 3

        # `usb 1`: what is on the one with a filesystem.
        Write-Host '==> usb 1' -ForegroundColor Cyan
        Send-Text 'usb 1'
        Send-Keys @('ret')
        Start-Sleep -Seconds 3

        # `usb 1 hello.txt`: read a file off it.
        Write-Host '==> usb 1 hello.txt' -ForegroundColor Cyan
        Send-Text 'usb 1 hello.txt'
        Send-Keys @('ret')
        if (-not (Wait-For -Text 'term: usb 1 hello.txt is 35 bytes' -Seconds 60)) {
            $failures += 'the shell never read a file off the drive'
        }
        Start-Sleep -Seconds 2

        if ($Shot) {
            $Ppm = Join-Path $BuildDir 'usb.ppm'
            Invoke-Screendump -Writer $writer -Path $Ppm
            Convert-PpmToPng -PpmPath $Ppm -PngPath $Shot | Out-Null
            Write-Host "    Screenshot: $Shot" -ForegroundColor DarkGray
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
        # The kernel's half, so a failure higher up can be told from a failure
        # lower down.
        'QEMU QEMU HARDDISK: 2048 blocks of 512 bytes',
        'drive 1 mounted: "NEXUSOS"',
        # And the program's half, which is what this test is for.
        'term: usb 1 hello.txt is 35 bytes',
        'removable',
        # It was lent the channel rather than helping itself to it.
        'compositor: started a terminal'
    )) {
    $checks++
    if ($output.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "never said '$expected'"
        Write-Host "    FAIL $expected" -ForegroundColor Red
    }
}

foreach ($bad in @('KERNEL PANIC', 'term: PANIC', '[rem ] FAILED')) {
    $checks++
    if ($output.Contains($bad)) {
        $failures += "absent: $bad : it appeared"
        Write-Host "    FAIL absent: $bad" -ForegroundColor Red
    } else {
        Write-Host "    ok   absent: $bad" -ForegroundColor DarkGray
    }
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host "USB tests passed ($checks checks)." -ForegroundColor Green
    exit 0
} else {
    foreach ($why in $failures) { Write-Host "    FAIL $why" -ForegroundColor Red }
    Write-Host "$($failures.Count) of $checks checks failed. Log: $Log" -ForegroundColor Red
    exit 1
}
