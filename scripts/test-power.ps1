<#
.SYNOPSIS
    Presses the button that turns the machine off, and checks that it stops.

.DESCRIPTION
    This is the one test here whose pass condition is that QEMU *exits*. A
    kernel that writes the ACPI sleep register and carries on running has not
    shut the machine down, however healthy its log looks — and a log is all the
    other tests have to go on.

    So the check is the process: QEMU is started without `-no-reboot` suppressed
    and without being killed, the button is pressed, and the test waits for the
    process to go. If it is still there, the machine did not stop.

    Restart is checked the same way but the other way round: the machine must
    *not* exit, and must boot again, which shows up as a second copy of the
    lines a boot produces.

.PARAMETER Timeout
    How long to wait for each stage, in seconds.
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

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}

$failures = @()

<#
.SYNOPSIS
    Boot, press a button in the strip, and say what happened.

.PARAMETER Button
    Which of the power buttons: how many steps left of the right-hand edge.
#>
function Invoke-PowerRun {
    param(
        [string]$What,
        [int]$StepsLeft,
        [switch]$ExpectExit,
        [switch]$ThenWake,
        [string]$Picture
    )

    $log = Join-Path $BuildDir "power-$What.log"
    if (Test-Path $log) { Remove-Item $log -Force }
    $vars = Join-Path $BuildDir "vars-power-$What.fd"
    Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $vars -Force

    $port = Get-Random -Minimum 32000 -Maximum 32999
    $arguments = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
        -FirmwareCode $FirmwareCode -FirmwareVars $vars -SerialLog $log `
        -MonitorPort $port -Headless

    if (-not $ExpectExit) {
        # Every other test here runs with `-no-reboot`, which makes QEMU exit on
        # a processor reset instead of starting again -- exactly right when a
        # crash would otherwise loop forever.
        #
        # It is exactly wrong here. With it, a successful restart and a machine
        # that merely stopped look identical from the outside, and the thing
        # being tested is that it comes *back*. So this one run keeps the
        # reboot, and the pass condition is a second bootloader banner.
        $arguments = $arguments | Where-Object { $_ -ne '-no-reboot' }
    }

    Write-Host "==> $What : booting with a monitor on port $port" -ForegroundColor Cyan
    $process = Start-Process -FilePath $QemuExe.Source -ArgumentList $arguments -PassThru -NoNewWindow

    function Wait-For {
        param([string]$Text, [int]$Seconds, [int]$Nth = 1)
        for ($waited = 0; $waited -lt $Seconds; $waited++) {
            Start-Sleep -Seconds 1
            if ($process.HasExited) { return $false }
            if (Test-Path $log) {
                $sofar = (Get-Content $log -Raw -Encoding UTF8) -replace "`0", ''
                $seen = ([regex]::Matches($sofar, [regex]::Escape($Text))).Count
                if ($seen -ge $Nth) { return $true }
            }
        }
        return $false
    }

    try {
        if (-not (Wait-For -Text 'shell: took the strip' -Seconds $Timeout)) {
            throw "the desktop never appeared for $What"
        }

        $client = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $port)
        try {
            $writer = New-Object System.IO.StreamWriter($client.GetStream())
            $writer.AutoFlush = $true
            Start-Sleep -Milliseconds 800

            if ($Picture) {
                # Before anything moves. Sixty `mouse_move` commands leave the
                # compositor repainting, and a screendump taken in the middle
                # of that catches half a frame -- which looks exactly like a
                # drawing bug and is not one.
                Start-Sleep -Seconds 4
                $ppm = Join-Path $BuildDir 'power.ppm'
                Invoke-Screendump -Writer $writer -Path $ppm
                Convert-PpmToPng -PpmPath $ppm -PngPath $Picture | Out-Null
                Write-Host "    Screenshot: $Picture" -ForegroundColor DarkGray
            }

            # Into the bottom-right corner, where the pointer clamps, so nothing
            # here has to know where it started. Then up into the strip and left
            # by however many buttons.
            foreach ($step in 1..60) {
                $writer.WriteLine('mouse_move 60 60')
                Start-Sleep -Milliseconds 15
            }
            foreach ($step in 1..5) {
                $writer.WriteLine('mouse_move 0 -3')
                Start-Sleep -Milliseconds 25
            }
            # The buttons, right to left: log out, off, restart, sleep. Their
            # widths come from the words in them, so this steps by a generous
            # amount and relies on the buttons being adjacent.
            foreach ($step in 1..$StepsLeft) {
                foreach ($nudge in 1..6) {
                    $writer.WriteLine('mouse_move -12 0')
                    Start-Sleep -Milliseconds 20
                }
            }
            $writer.WriteLine('mouse_button 1')
            Start-Sleep -Milliseconds 250
            $writer.WriteLine('mouse_button 0')

            if ($ThenWake) {
                # The screen is out. Anything at all brings it back, and the
                # point of pressing a key rather than waiting is that waiting
                # would also pass on a compositor that never slept.
                if (-not (Wait-For -Text 'the desktop asked for the screen to go out' -Seconds 30)) {
                    $script:failures += "$What : the screen never went out"
                }
                Start-Sleep -Seconds 2
                $writer.WriteLine('sendkey spc')
                Start-Sleep -Milliseconds 500
                if (Wait-For -Text 'compositor: awake' -Seconds 30) {
                    Write-Host "    ok   $What : it woke on a key" -ForegroundColor DarkGray
                } else {
                    $script:failures += "$What : it never woke"
                }
            }
        } finally {
            $client.Close()
        }

        if ($ThenWake) {
            # Nothing else to wait for: the wake has already been checked.
        } elseif ($ExpectExit) {
            # The whole point. A machine that shut down is a process that is
            # gone; a log that says "shutting down" proves only that the kernel
            # intended to.
            Write-Host '    waiting for the machine to stop' -ForegroundColor DarkGray
            if ($process.WaitForExit(60000)) {
                Write-Host "    ok   $What : the machine stopped" -ForegroundColor DarkGray
            } else {
                $script:failures += "$What : the machine was still running a minute later"
            }
        } else {
            # Restart: the machine must come back, which is a second copy of a
            # line only a boot produces.
            Write-Host '    waiting for it to boot again' -ForegroundColor DarkGray
            if (Wait-For -Text 'NexusOS bootloader v' -Seconds 180 -Nth 2) {
                Write-Host "    ok   $What : the machine booted again" -ForegroundColor DarkGray
            } else {
                $script:failures += "$What : the machine never booted a second time"
            }
        }
    } finally {
        if (-not $process.HasExited) {
            try { $process.Kill() } catch { }
        }
        $process.WaitForExit(5000) | Out-Null
    }

    return (Get-Content $log -Raw -Encoding UTF8) -replace "`0", ''
}

# ---------------------------------------------------------------------------

$off = Invoke-PowerRun -What 'shutdown' -StepsLeft 1 -ExpectExit

foreach ($expected in @(
        'shell: somebody pressed the button that turns the machine off',
        'compositor: the machine is being turned off',
        '[pwr ] shutting down'
    )) {
    if ($off.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "shutdown never reported: $expected"
    }
}
# It must not have needed the fallbacks. Reaching them means the ACPI path did
# not work, which on this machine would be a defect rather than a difference.
foreach ($bad in @(
        '[pwr ] ACPI shutdown did not take',
        '[pwr ] this machine will not turn itself off',
        'KERNEL PANIC'
    )) {
    if ($off.Contains($bad)) {
        $failures += "shutdown fell back: $bad"
    } else {
        Write-Host "    ok   absent: $bad" -ForegroundColor DarkGray
    }
}

$sleep = Invoke-PowerRun -What 'sleep' -StepsLeft 3 -ThenWake -Picture $Shot

foreach ($expected in @(
        'shell: somebody pressed the button that puts the screen out',
        'compositor: the desktop asked for the screen to go out',
        'compositor: awake'
    )) {
    if ($sleep.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "sleep never reported: $expected"
    }
}
if ($sleep.Contains('KERNEL PANIC')) { $failures += 'the kernel panicked during sleep' }

$restart = Invoke-PowerRun -What 'restart' -StepsLeft 2

foreach ($expected in @(
        'shell: somebody pressed the button that restarts the machine',
        'compositor: the machine is being restarted',
        '[pwr ] restarting'
    )) {
    if ($restart.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "restart never reported: $expected"
    }
}
if ($restart.Contains('KERNEL PANIC')) { $failures += 'the kernel panicked during restart' }

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'Power tests passed.' -ForegroundColor Green
    exit 0
} else {
    foreach ($why in $failures) { Write-Host "    FAIL $why" -ForegroundColor Red }
    exit 1
}
