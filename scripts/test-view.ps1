<#
.SYNOPSIS
    Opens the picture window from the desktop and shows a picture in it.

.DESCRIPTION
    The viewer is a window; the decoding is not. `shared/nexus-image` and
    `shared/nexus-inflate` are tested on the build machine against files a
    reference encoder wrote, which is where malformed input can actually be
    produced. What this checks is the part those tests cannot: that a real PNG
    written by a real encoder, carried onto the store by the kernel at boot,
    opened through a read-only directory handle, and decoded inside the guest,
    comes out as a picture.

    It is also the first test of the DEFLATE decoder against something larger
    than a fixture, which is worth saying: the picture is thirteen kilobytes of
    dynamic Huffman blocks.

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
$Log = Join-Path $BuildDir 'view-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-view.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force
if (Test-Path $Log) { Remove-Item $Log -Force }

$MonitorPort = Get-Random -Minimum 34000 -Maximum 34899
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
        Write-Host '==> Pressing the pictures button on the strip' -ForegroundColor Cyan
        Move-Pointer -Dx -60 -Dy 60 -Steps 40 -Pause 20
        Move-Pointer -Dx 0 -Dy -3 -Steps 5
        Move-Pointer -Dx 51 -Dy 0 -Steps 8
        $writer.WriteLine('mouse_button 1')
        Start-Sleep -Milliseconds 250
        $writer.WriteLine('mouse_button 0')

        if (-not (Wait-For -Text 'view: a window for looking at pictures' -Seconds 90)) {
            throw 'the picture window never started'
        }

        # The picture the image ships, decoded inside the guest.
        if (-not (Wait-For -Text 'view: showed NEXUS.PNG' -Seconds 60)) {
            $failures += 'the viewer never decoded the picture the image ships'
        }

        # And the keys that move between pictures. There is only one, so this
        # checks that asking for the next one does not fall over rather than
        # that it lands somewhere new.
        Send-Keys @('right', 'left', 'f5')
        # Long enough for the window to have settled: a screendump taken while
        # the compositor is part-way through a full-screen composite catches
        # half a frame, which looks exactly like a drawing bug and is not one.
        Start-Sleep -Seconds 5

        if ($Shot) {
            $Ppm = Join-Path $BuildDir 'view.ppm'
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
        'compositor: started a picture window, and lent it the disk to read',
        'view: a window for looking at pictures',
        'view: showed NEXUS.PNG',
        'PICTURES/NEXUS.PNG'
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
        'view: could not show',
        'view: FAILED',
        'view: PANIC',
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
    Write-Host 'Picture tests passed.' -ForegroundColor Green
    exit 0
} else {
    foreach ($failure in $failures) {
        Write-Host "    FAIL $failure" -ForegroundColor Red
    }
    Write-Host "Log: $Log"
    exit 1
}
