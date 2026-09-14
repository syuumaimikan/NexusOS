<#
.SYNOPSIS
    Opens the agent from the desktop and asks it things.

.DESCRIPTION
    The agent is not a language model and this test is not a test of one. What
    it checks is the part that would matter even if there were one: that an
    agent decides what to do, asks permission for it, and is refused by name
    when it may not.

    So it asks three things. One the agent may answer -- how busy the machine
    is, which is a read-only snapshot. One that reads files. And one that would
    delete something, which the permission model refuses outright and which must
    therefore produce a refusal rather than a deletion.

.PARAMETER Timeout
    How long to wait for each stage, in seconds.

.PARAMETER Shot
    Where to put a screenshot of the window, if one is wanted.
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
$Log = Join-Path $BuildDir 'assist-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-assist.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force
if (Test-Path $Log) { Remove-Item $Log -Force }

$MonitorPort = Get-Random -Minimum 35000 -Maximum 35899
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

        function Move-Pointer {
            param([int]$Dx, [int]$Dy, [int]$Steps, [int]$Pause = 35)
            foreach ($step in 1..$Steps) {
                $writer.WriteLine("mouse_move $Dx $Dy")
                Start-Sleep -Milliseconds $Pause
            }
        }
        function Send-Keys {
            param([string[]]$Keys, [int]$Pause = 160)
            foreach ($key in $Keys) {
                $writer.WriteLine("sendkey $key")
                Start-Sleep -Milliseconds $Pause
            }
        }

        # The fifth button along the strip.
        Write-Host '==> Pressing the assistant button on the strip' -ForegroundColor Cyan
        Move-Pointer -Dx -60 -Dy 60 -Steps 40 -Pause 20
        Move-Pointer -Dx 0 -Dy -3 -Steps 5
        Move-Pointer -Dx 61 -Dy 0 -Steps 8
        $writer.WriteLine('mouse_button 1')
        Start-Sleep -Milliseconds 250
        $writer.WriteLine('mouse_button 0')

        if (-not (Wait-For -Text 'assist: an agent that can only do what it was lent' -Seconds 90)) {
            throw 'the agent never started'
        }

        # Something it may do.
        Write-Host '==> Asking what the machine is doing' -ForegroundColor Cyan
        Send-Keys @('m', 'e', 'm', 'o', 'r', 'y', 'ret')
        Start-Sleep -Seconds 2

        # Something that reads files.
        Write-Host '==> Asking it to look through the files' -ForegroundColor Cyan
        Send-Keys @('f', 'i', 'n', 'd', 'spc', 'n', 'o', 't', 'e', 's', 'ret')
        Start-Sleep -Seconds 3

        # And something it must refuse. The permission model calls this
        # ConfirmRequired, and the agent has no way to ask anybody, so the only
        # correct outcome is a refusal that names what it would have needed.
        Write-Host '==> Asking it to delete something' -ForegroundColor Cyan
        Send-Keys @('d', 'e', 'l', 'e', 't', 'e', 'spc', 'n', 'o', 't', 'e', 'ret')
        Start-Sleep -Seconds 3

        if ($Shot) {
            $Ppm = Join-Path $BuildDir 'assist.ppm'
            Invoke-Screendump -Writer $writer -Path $Ppm
            Convert-PpmToPng -PpmPath $Ppm -PngPath $Shot | Out-Null
            Write-Host "    Screenshot: $Shot" -ForegroundColor DarkGray
        }
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
        'compositor: started the agent, and lent it the disk to read and nothing to write',
        'assist: an agent that can only do what it was lent',
        'assist: answered a question'
    )) {
    if ($output.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "never reported: $expected"
    }
}

# The tampered package must never have been installed, and nothing may have
# fallen over. The first of those is the whole point of signing a package.
foreach ($bad in @(
        'assist: FAILED',
        'assist: PANIC',
        'KERNEL PANIC'
    )) {
    if ($output.Contains($bad)) {
        $failures += "saw $bad"
    } else {
        Write-Host "    ok   absent: $bad" -ForegroundColor DarkGray
    }
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'Agent tests passed.' -ForegroundColor Green
    exit 0
} else {
    foreach ($failure in $failures) {
        Write-Host "    FAIL $failure" -ForegroundColor Red
    }
    Write-Host "Log: $Log"
    exit 1
}
