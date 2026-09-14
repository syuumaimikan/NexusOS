<#
.SYNOPSIS
    Opens the settings window from the desktop and changes the machine with it.

.DESCRIPTION
    The same property the appearance test checks, reached the way a person would
    reach it: a button on the strip rather than a command typed at a prompt.

    Four programs are involved and none of them is told anything. The desktop
    asks the compositor for a window; the compositor lends the settings window
    the one directory it is allowed to write; the settings window rewrites a
    line of a text file; and the wallpaper, which is looking at that file on a
    clock of its own, notices and redraws. Nothing is restarted and nothing
    subscribes to anything.

    It also types a colour into a field, which is the other kind of row and the
    one with a mistake available: a value that is not a colour has to be refused
    by the window rather than written to the file for somebody else to trip on.

.PARAMETER Timeout
    How long to wait for each stage, in seconds.
#>
[CmdletBinding()]
param(
    [int]$Timeout = 240,
    # Capture the window before the machine is stopped. A picture is the only
    # way to check the part no log can say anything about: whether the thing is
    # laid out so a person can read it.
    [string]$Shot
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')
. (Join-Path $PSScriptRoot 'capture.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'
$Log = Join-Path $BuildDir 'settings-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-settings.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force
if (Test-Path $Log) { Remove-Item $Log -Force }

$MonitorPort = Get-Random -Minimum 39000 -Maximum 39899
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

# The same, for a line whose interesting part is not known in advance. Returns
# the first capture group, or $null.
#
# Which style the window lands on depends on which one the machine was already
# using, and the machine remembers between runs -- so a test that waited for the
# word "stars" would pass once and fail on the second run against the same disk.
# What is actually being checked is that the window wrote *a* style and that the
# wallpaper picked up *that* one.
function Wait-ForMatch {
    param([string]$Pattern, [int]$Seconds)
    for ($waited = 0; $waited -lt $Seconds; $waited++) {
        Start-Sleep -Seconds 1
        if ($process.HasExited) { return $null }
        if (Test-Path $Log) {
            $sofar = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''
            $found = [regex]::Match($sofar, $Pattern)
            if ($found.Success) { return $found.Groups[1].Value }
        }
    }
    return $null
}

$failures = @()
$style = $null
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
            param([string[]]$Keys, [int]$Pause = 140)
            foreach ($key in $Keys) {
                $writer.WriteLine("sendkey $key")
                Start-Sleep -Milliseconds $Pause
            }
        }

        # Into the bottom-left corner, then up into the strip and right along it
        # to the fourth button. Relative moves, because that is all the monitor
        # offers: the corner is the only position that can be reached without
        # knowing where the pointer started.
        Write-Host '==> Pressing the settings button on the strip' -ForegroundColor Cyan
        Move-Pointer -Dx -60 -Dy 60 -Steps 40 -Pause 20
        Move-Pointer -Dx 0 -Dy -3 -Steps 5
        Move-Pointer -Dx 31 -Dy 0 -Steps 8
        $writer.WriteLine('mouse_button 1')
        Start-Sleep -Milliseconds 250
        $writer.WriteLine('mouse_button 0')

        if (-not (Wait-For -Text 'settings: a window for what this machine is' -Seconds 90)) {
            throw 'the settings window never started'
        }

        # The first row is the background style. Right moves it along the list,
        # and the window writes the file the moment it does.
        Write-Host '==> Choosing a different background' -ForegroundColor Cyan
        Send-Keys @('right')
        $style = Wait-ForMatch -Pattern 'settings: look\.style is now (\w+)' -Seconds 30
        if (-not $style) {
            $failures += 'the settings window never wrote the style'
        } else {
            Write-Host "    the window chose $style" -ForegroundColor DarkGray
            if (-not (Wait-For -Text "wall: the look changed to $style" -Seconds 60)) {
                $failures += "the wallpaper never noticed the style $style"
            }
        }

        # Down to the accent, which is typed rather than chosen. Backspace six
        # times first, because editing starts from what is already there.
        Write-Host '==> Typing a colour' -ForegroundColor Cyan
        Send-Keys @('down', 'down', 'down')
        Send-Keys @('backspace', 'backspace', 'backspace', 'backspace', 'backspace', 'backspace')

        # Something that is not a colour, first. The window has to refuse it:
        # a settings file with `zz` in its accent is a settings file every
        # program that reads it has to be careful about.
        Send-Keys @('z', 'z', 'ret')
        Start-Sleep -Seconds 1

        Send-Keys @('backspace', 'backspace')
        Send-Keys @('4', '0', 'd', '0', '9', '0', 'ret')
        if (-not (Wait-For -Text 'settings: look.accent is now 40d090' -Seconds 30)) {
            $failures += 'the settings window never wrote the accent'
        }
        Start-Sleep -Seconds 2

        if ($Shot) {
            $Ppm = Join-Path $BuildDir 'settings.ppm'
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

$wanted = @(
    'compositor: started the settings, and lent them the settings',
    'settings: a window for what this machine is',
    'settings: look.accent is now 40d090'
)
if ($style) {
    $wanted += "settings: look.style is now $style"
    $wanted += "wall: the look changed to $style"
}
foreach ($expected in $wanted) {
    if ($output.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "never reported: $expected"
    }
}

# What must not have happened: the thing that is not a colour must not have
# reached the file.
foreach ($bad in @(
        'settings: look.accent is now zz',
        'settings: PANIC',
        'wall: PANIC',
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
    Write-Host 'Settings tests passed.' -ForegroundColor Green
    exit 0
} else {
    foreach ($failure in $failures) {
        Write-Host "    FAIL $failure" -ForegroundColor Red
    }
    Write-Host "Log: $Log"
    exit 1
}
