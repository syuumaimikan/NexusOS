<#
.SYNOPSIS
    Opens the picture window from the desktop, shows a picture, and plays a
    recording.

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

    The recording is the part that cannot be checked any other way. A host test
    can decode twenty-four frames; only this can say whether a machine decodes
    them *fast enough*, holding a file open, reading each frame through the
    filesystem, and handing each to the compositor on a clock. The viewer logs
    what it actually managed, and this requires it to have gone all the way
    round the loop.

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

        # The image ships the mark three times -- a recording, a JPEG and a PNG
        # -- and the viewer sorts by name, so NEXUS.AVI is what it opens with.
        if (-not (Wait-For -Text 'view: playing NEXUS.AVI' -Seconds 60)) {
            $failures += 'the viewer never opened the recording the image ships'
        }

        # And then all the way round it. This is the assertion that matters:
        # twenty-four frames read one at a time through an open handle and
        # decoded, which cannot happen if the container reader has the offsets
        # wrong, if a frame is read short, or if the clock never fires.
        #
        # Waited for rather than slept through. Two seconds of recording takes
        # two seconds only if the machine keeps up, and the whole point of the
        # line being waited for is that it says whether it did.
        if (-not (Wait-For -Text 'view: played 24 frames' -Seconds 90)) {
            $failures += 'the recording never played a whole time round'
        }

        # The space bar stops it where it is -- and a stopped frame is what a
        # screenshot can be taken of. A playing one would catch the compositor
        # part-way through a composite as often as not, which looks exactly
        # like a drawing bug and is not one.
        Write-Host '==> Pausing the recording' -ForegroundColor Cyan
        Send-Keys @('spc')
        Start-Sleep -Seconds 3

        if ($Shot) {
            $Ppm = Join-Path $BuildDir 'playing.ppm'
            Invoke-Screendump -Writer $writer -Path $Ppm
            $frozen = [System.IO.Path]::ChangeExtension($Shot, $null) + 'playing.png'
            Convert-PpmToPng -PpmPath $Ppm -PngPath $frozen | Out-Null
            Write-Host "    Screenshot: $frozen" -ForegroundColor DarkGray
        }

        # Then the two stills, which are the two picture decoders.
        Write-Host '==> Stepping to the stills' -ForegroundColor Cyan
        Send-Keys @('right')
        if (-not (Wait-For -Text 'view: showed NEXUS.JPG' -Seconds 60)) {
            $failures += 'the viewer never decoded the JPEG the image ships'
        }
        Send-Keys @('right')
        if (-not (Wait-For -Text 'view: showed NEXUS.PNG' -Seconds 60)) {
            $failures += 'the viewer never decoded the PNG the image ships'
        }
        Send-Keys @('left', 'f5')
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
        'view: playing NEXUS.AVI, 240x180, 24 frames at 12.0 a second',
        'view: showed NEXUS.JPG',
        'view: showed NEXUS.PNG',
        'PICTURES/NEXUS.PNG',
        'PICTURES/NEXUS.JPG',
        'PICTURES/NEXUS.AVI'
    )) {
    if ($output.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "never reported: $expected"
    }
}

# What the machine actually managed, said out loud. Not an assertion about a
# number -- QEMU on a loaded build machine is not a benchmark, and a threshold
# here would fail for reasons that have nothing to do with this code -- but a
# figure a person reading the output can see going the wrong way.
if ($output -match 'view: played (\d+) frames, decoding at ([\d.]+) a second, asked for ([\d.]+)') {
    Write-Host "    ok   decoded $($Matches[2]) frames a second, asked for $($Matches[3])" -ForegroundColor DarkGray
} else {
    $failures += 'the viewer never said what frame rate it managed'
}

# The tampered package must never have been installed, and nothing may have
# fallen over. The first of those is the whole point of signing a package.
foreach ($bad in @(
        'view: could not show',
        'view: could not play',
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
