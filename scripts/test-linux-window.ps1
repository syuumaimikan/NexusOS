<#
.SYNOPSIS
    Boots the machine, opens a window for a program built for Linux, and checks
    the pixels that program wrote are on the screen.

.DESCRIPTION
    The program is `draw.lx`: a static Linux x86-64 executable that opens
    `/dev/nexus/display`, asks how large its window is, maps the buffer
    `MAP_SHARED`, fills it in two bands and tells the compositor which rectangle
    changed. It has never heard of NexusOS. Nothing about it was shaped to suit
    this machine -- it makes its requests with Linux's own call numbers and the
    `syscall` instruction, and the compatibility layer turns them into the same
    operations every Nexus window program uses.

    The check that matters is the last one and it is outside the machine: a
    screendump is taken over the QEMU monitor and the two colours are looked for
    in it, in the right order, top to bottom. A display device that accepted
    every request and showed nothing would pass every check inside the program.

.PARAMETER Timeout
    How long to wait for each stage, in seconds.
#>
[CmdletBinding()]
param([int]$Timeout = 300, [switch]$Fresh)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')
. (Join-Path $PSScriptRoot 'private-machine.ps1')
. (Join-Path $PSScriptRoot 'capture.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$Log = Join-Path $BuildDir 'linux-window.log'

# A copy of the built machine that this run has to itself. `build/esp` and the
# disk image are shared, and a QEMU holding them is somebody else's build dying
# at `llvm-objcopy: permission denied` with nothing in the message about QEMU.
# Every test in this family shares one copy, because no two of them run at the
# same time and eight gigabytes each would be absurd. See
# `scripts/private-machine.ps1`.
$Machine = Get-PrivateMachine -BuildDir $BuildDir -Name 'linux-machine' -Fresh:$Fresh
$MachineDir = $Machine.BuildDir
$EspDir = $Machine.EspDir

# The two colours the program writes, as it packs them: 0x00RRGGBB.
$Background = @(0x1E, 0x5A, 0xA8)
$Band = @(0xE8, 0xB2, 0x3C)

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-linux-window.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $Log) { Remove-Item $Log -Force }

# Asked for rather than guessed: see `Get-FreeMonitorPort`.
$Port = Get-FreeMonitorPort
$QemuArgs = Get-NexusQemuArgs -BuildDir $MachineDir -EspDir $EspDir `
    -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars -SerialLog $Log `
    -MonitorPort $Port -Headless

Write-Host '==> Booting NexusOS' -ForegroundColor Cyan
$process = Start-Process -FilePath $QemuExe.Source -ArgumentList $QemuArgs -PassThru -NoNewWindow
$connection = $null
$failures = @()

function Wait-Marker([string]$Marker) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Timeout)
    while ([DateTime]::UtcNow -lt $deadline -and -not $process.HasExited) {
        if (Test-Path $Log) {
            $sofar = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''
            if ($sofar -match 'KERNEL PANIC') { throw "the kernel panicked; see $Log" }
            if ($sofar.Contains($Marker)) { return }
        }
        Start-Sleep -Milliseconds 500
    }
    throw "the machine never said '$Marker'; see $Log"
}

try {
    # The desktop is up, which means the compositor has the display and has
    # started its boot windows. The strip is the last thing to appear, so it is
    # the marker: pressing a key before it would be pressing one at a machine
    # that is not yet listening.
    Wait-Marker 'shell: took the strip along the bottom of the screen'
    Write-Host '    ok   the desktop is up' -ForegroundColor DarkGray

    $connection = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $Port)
    $writer = New-Object System.IO.StreamWriter($connection.GetStream())
    $writer.AutoFlush = $true

    # F4 asks the compositor for a window for a program built for Linux. A key
    # rather than a launcher entry: see `LINUX_KEY` in the compositor.
    $writer.WriteLine('sendkey f4')

    # The kernel says so when the device is opened, and the line carries the
    # size -- which is the compositor's answer and not the program's guess.
    Wait-Marker '[linux] spawned opened /dev/nexus/display'
    Write-Host '    ok   the Linux program opened the display device' -ForegroundColor DarkGray
    Wait-Marker 'compositor: started a program built for Linux'
    Write-Host '    ok   the compositor gave it a surface' -ForegroundColor DarkGray

    # Long enough for several frames, so what is captured is a window that has
    # been drawing rather than one that has drawn once.
    Start-Sleep -Seconds 3

    $ppm = Join-Path $BuildDir 'linux-window.ppm'
    if (Test-Path $ppm) { Remove-Item $ppm -Force }
    Invoke-Screendump -Writer $writer -Path $ppm
    Convert-PpmToPng -Ppm $ppm -Png (Join-Path $BuildDir 'linux-window.png')

    # ---- the check from outside -------------------------------------------
    #
    # Read the screendump and look for the two colours. Not "is this colour
    # anywhere": both, with every pixel of the band below every pixel of the
    # background it sits under, because a window filled entirely with one of
    # them would satisfy a looser check and is not what the program drew.
    $bytes = [System.IO.File]::ReadAllBytes($ppm)
    # A binary PPM header: "P6\n<width> <height>\n<max>\n". Walked rather than
    # assumed, because the writer is free to put comments in it.
    $at = 0
    $fields = @()
    while ($fields.Count -lt 4 -and $at -lt $bytes.Length) {
        # Skip whitespace.
        while ($at -lt $bytes.Length -and $bytes[$at] -in 32, 9, 10, 13) { $at++ }
        if ($at -lt $bytes.Length -and $bytes[$at] -eq 35) {
            while ($at -lt $bytes.Length -and $bytes[$at] -ne 10) { $at++ }
            continue
        }
        $start = $at
        while ($at -lt $bytes.Length -and $bytes[$at] -notin 32, 9, 10, 13) { $at++ }
        $fields += [System.Text.Encoding]::ASCII.GetString($bytes, $start, $at - $start)
    }
    $at++
    if ($fields[0] -ne 'P6') { throw "the screendump is not a binary PPM: $ppm" }
    $width = [int]$fields[1]
    $height = [int]$fields[2]
    Write-Host "    ok   screendump is ${width}x${height}" -ForegroundColor DarkGray

    $backgroundRows = @()
    $bandRows = @()
    for ($y = 0; $y -lt $height; $y++) {
        $row = $at + $y * $width * 3
        for ($x = 0; $x -lt $width; $x++) {
            $p = $row + $x * 3
            if ($bytes[$p] -eq $Background[0] -and $bytes[$p + 1] -eq $Background[1] `
                    -and $bytes[$p + 2] -eq $Background[2]) {
                $backgroundRows += $y
                break
            }
        }
        for ($x = 0; $x -lt $width; $x++) {
            $p = $row + $x * 3
            if ($bytes[$p] -eq $Band[0] -and $bytes[$p + 1] -eq $Band[1] `
                    -and $bytes[$p + 2] -eq $Band[2]) {
                $bandRows += $y
                break
            }
        }
    }

    if ($backgroundRows.Count -eq 0) {
        $failures += 'the colour the Linux program filled its window with is not on the screen'
    } else {
        Write-Host ("    ok   its background is on the screen, on {0} rows" -f $backgroundRows.Count) `
            -ForegroundColor DarkGray
    }
    if ($bandRows.Count -eq 0) {
        $failures += 'the band the Linux program drew is not on the screen'
    } else {
        Write-Host ("    ok   its band is on the screen, on {0} rows" -f $bandRows.Count) `
            -ForegroundColor DarkGray
    }
    if ($backgroundRows.Count -gt 0 -and $bandRows.Count -gt 0) {
        # The band starts a third of the way down, so its first row is below the
        # first row of background. A window filled with either colour alone, or
        # one drawn upside down, fails here.
        if (($backgroundRows | Measure-Object -Minimum).Minimum -lt
            ($bandRows | Measure-Object -Minimum).Minimum) {
            Write-Host '    ok   the band is below the background, as it was drawn' `
                -ForegroundColor DarkGray
        } else {
            $failures += 'the two colours are on the screen in the wrong order'
        }
    }
} finally {
    if ($null -ne $connection) { $connection.Close() }
    if (-not $process.HasExited) {
        try { $process.Kill() } catch { }
    }
    $process.WaitForExit(5000) | Out-Null
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'A program built for Linux drew a window and it reached the screen.' -ForegroundColor Green
    Write-Host "Evidence: $(Join-Path $BuildDir 'linux-window.png') and $Log" -ForegroundColor DarkGray
    exit 0
} else {
    foreach ($why in $failures) { Write-Host "    FAIL $why" -ForegroundColor Red }
    Write-Host "$($failures.Count) check(s) failed. Log: $Log" -ForegroundColor Red
    exit 1
}
