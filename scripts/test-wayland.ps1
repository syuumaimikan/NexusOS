<#
.SYNOPSIS
    Boots the machine, starts a Wayland compositor and a Wayland client -- both
    programs built for Linux -- and checks that a window is negotiated through
    `xdg-shell`, resized, typed at, and closed, with the client's picture
    reaching the screen at the size the compositor asked for.

.DESCRIPTION
    Two static Linux x86-64 executables, compiled from Rust for
    `x86_64-unknown-linux-gnu` and linked with no C runtime. Neither has heard
    of NexusOS.

    The compositor binds a Unix domain socket at `/tmp/wayland-0` and opens
    `/dev/nexus/display` for a window. The client is started with **no window of
    its own** -- a Wayland client's window is its `wl_surface`, which lives in
    the Wayland compositor -- connects to that name, asks the registry what is
    on offer, and binds `wl_compositor`, `wl_shm`, `xdg_wm_base`, `wl_seat` and
    `wl_output`.

    It then does what a real client does: creates a surface, wraps it in an
    `xdg_surface` and an `xdg_toplevel`, gives it a title, and commits *nothing*
    -- which is `xdg-shell`'s way of asking how big to be. Only after the
    configure comes back does it make a `memfd`, map it shared, draw, send the
    descriptor across with `SCM_RIGHTS`, damage the surface, ask for a frame
    callback and commit.

    What is then checked is the four things the acceptance gate names:

      * it presents frames        -- the picture reaches the screen
      * it resizes                -- the compositor asks for 240x150 and the
                                     client redraws at that size, which is
                                     visible from outside as a narrower band
      * it receives input         -- a key typed at the QEMU monitor reaches the
                                     client as an evdev code through `wl_seat`
      * it releases buffers       -- `wl_buffer.release` and the frame callback,
                                     twice each, checked by the client's own
                                     exit status

    The messages on that socket are the Wayland wire protocol: the object
    identifiers, opcodes, sizes and padding in `wayland.xml` and
    `xdg-shell.xml`.

    What this does NOT establish is that `libwayland` works here. There is no
    `libwayland` on this machine and no toolchain that could build one, so the
    client this was tested against is the one in this repository, written from
    the same protocol description. See the note at the top of
    `tools/nexus-guest/src/bin/wlserver.rs`.

    The picture check is from outside the machine: a screendump over the QEMU
    monitor, looked at for the three colours the *client* wrote, in the order it
    wrote them, and for the width of the band -- which is 240 only if the client
    redrew after being told to.

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
$Log = Join-Path $BuildDir 'wayland.log'

# A copy of the built machine that this run has to itself. `build/esp` and the
# disk image are shared, and a QEMU holding them is somebody else's build dying
# at `llvm-objcopy: permission denied` with nothing in the message about QEMU.
# Every test in this family shares one copy, because no two of them run at the
# same time and eight gigabytes each would be absurd. See
# `scripts/private-machine.ps1`.
$Machine = Get-PrivateMachine -BuildDir $BuildDir -Name 'linux-machine' -Fresh:$Fresh
$MachineDir = $Machine.BuildDir
$EspDir = $Machine.EspDir

# The three colours the *client* writes into its shared buffer, as 0x00RRGGBB.
# The middle one is drawn only in the frame after the resize, so finding it on
# the screen is finding the second frame and not the first.
$Top = @(0x33, 0x99, 0x66)
$Middle = @(0x22, 0x44, 0xEE)
$Bottom = @(0xCC, 0x55, 0x33)

# The two sizes the client draws at: the first one it is given, and the one it
# is asked to change to. What is on the screen at the end says which of the two
# frames survived, and so whether the resize was honoured or only announced.
$FirstWidth = 320
$ResizedWidth = 240

# How far off the measured width may be. This machine's own compositor draws a
# focus ring around a window, over the outermost column of the surface, so a
# 240-wide band measures 238 on the screen. The tolerance is for that and
# nothing else: it is far smaller than the eighty pixels between the two sizes,
# so a first frame still on the screen cannot pass as a second one.
$Slack = 4

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-wayland.fd'
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
    Wait-Marker 'shell: took the strip along the bottom of the screen'
    Write-Host '    ok   the desktop is up' -ForegroundColor DarkGray

    $connection = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $Port)
    $writer = New-Object System.IO.StreamWriter($connection.GetStream())
    $writer.AutoFlush = $true

    # F5 starts the Wayland compositor, F6 a client for it. Two keys because
    # they are two programs that find each other through a socket.
    $writer.WriteLine('sendkey f5')
    Wait-Marker 'wlserver: listening on /tmp/wayland-0'
    Write-Host '    ok   the Wayland compositor bound its socket' -ForegroundColor DarkGray

    $writer.WriteLine('sendkey f6')
    Wait-Marker 'wlclient: connected to the compositor'
    Write-Host '    ok   the client connected' -ForegroundColor DarkGray

    # ---- the handshake a real client does ---------------------------------
    Wait-Marker 'wlclient: the compositor offers a compositor, shm, xdg_wm_base, a seat and an output'
    Write-Host '    ok   the registry offers everything a window needs' -ForegroundColor DarkGray

    Wait-Marker 'wlserver: the window is called A window from a program built for Linux'
    Write-Host '    ok   the toplevel has a title' -ForegroundColor DarkGray

    Wait-Marker 'wlserver: a window asked to be mapped, and was told what size to be'
    Write-Host '    ok   a commit with no buffer was answered with a configure' -ForegroundColor DarkGray

    Wait-Marker 'wlclient: drew and committed a surface, 320x200'
    Wait-Marker 'wlserver: put a client buffer on the screen'
    Write-Host '    ok   the client drew at the size it was given' -ForegroundColor DarkGray

    Wait-Marker 'wlclient: a frame callback came back'
    Write-Host '    ok   the frame callback came back' -ForegroundColor DarkGray

    # ---- ping, and then a resize ------------------------------------------
    Wait-Marker 'wlserver: the client answered a ping'
    Write-Host '    ok   the client answered xdg_wm_base.ping' -ForegroundColor DarkGray

    Wait-Marker 'wlserver: asked the window to be 240x150'
    Wait-Marker 'wlclient: drew and committed a surface, 240x150'
    Wait-Marker 'wlserver: the client redrew at the size it was given'
    Write-Host '    ok   the window was resized and the client followed' -ForegroundColor DarkGray

    # ---- and a key --------------------------------------------------------
    #
    # Typed at the machine, not injected into either program. It goes keyboard
    # -> the kernel -> this machine's compositor -> the focused window, which is
    # the Wayland compositor -> `/dev/nexus/display` -> `wl_keyboard.key`.
    #
    # Which is also why the client is started with no window of its own: a
    # second window would have taken the focus, and the key would have gone to
    # a program that is not listening for one.
    Wait-Marker 'wlclient: this window has the keyboard'
    Write-Host '    ok   the seat put the keyboard on the surface' -ForegroundColor DarkGray

    $writer.WriteLine('sendkey a')
    Wait-Marker 'wlserver: forwarded a key to the client'
    Wait-Marker 'wlclient: a key reached the client, evdev code 30'
    Write-Host '    ok   a key typed at the machine reached the client' -ForegroundColor DarkGray

    # ---- and the close ----------------------------------------------------
    Wait-Marker 'wlserver: asked the window to close'
    Wait-Marker 'wlclient: the compositor asked the window to close'
    Wait-Marker 'wlclient: configured twice, drew twice, released twice, and was typed at'
    Write-Host '    ok   the client closed cleanly, having checked its own run' -ForegroundColor DarkGray

    Start-Sleep -Seconds 3

    $ppm = Join-Path $BuildDir 'wayland.ppm'
    if (Test-Path $ppm) { Remove-Item $ppm -Force }
    Invoke-Screendump -Writer $writer -Path $ppm
    Convert-PpmToPng -Ppm $ppm -Png (Join-Path $BuildDir 'wayland.png')

    # ---- the check from outside -------------------------------------------
    $bytes = [System.IO.File]::ReadAllBytes($ppm)
    $at = 0
    $fields = @()
    while ($fields.Count -lt 4 -and $at -lt $bytes.Length) {
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

    # For each band: which rows it is on, and the widest run of it on any row.
    # The run is the part that says the window was resized: the client's first
    # frame was 320 wide and its second is 240, and nothing but the client
    # redrawing makes the picture on the screen narrower.
    function Measure-Band([byte[]]$Colour) {
        $rows = @()
        $widest = 0
        for ($y = 0; $y -lt $height; $y++) {
            $row = $at + $y * $width * 3
            $run = 0
            $best = 0
            for ($x = 0; $x -lt $width; $x++) {
                $p = $row + $x * 3
                if ($bytes[$p] -eq $Colour[0] -and $bytes[$p + 1] -eq $Colour[1] `
                        -and $bytes[$p + 2] -eq $Colour[2]) {
                    $run++
                    if ($run -gt $best) { $best = $run }
                } else {
                    $run = 0
                }
            }
            if ($best -gt 0) {
                $rows += $y
                if ($best -gt $widest) { $widest = $best }
            }
        }
        [PSCustomObject]@{ Rows = $rows; Widest = $widest }
    }

    $bands = @{
        'upper'  = Measure-Band $Top
        'middle' = Measure-Band $Middle
        'lower'  = Measure-Band $Bottom
    }

    foreach ($name in 'upper', 'middle', 'lower') {
        if ($bands[$name].Rows.Count -eq 0) {
            $failures += "the client's $name band is not on the screen"
        } else {
            Write-Host ("    ok   its $name band is on the screen, on {0} rows, {1} wide" -f `
                    $bands[$name].Rows.Count, $bands[$name].Widest) -ForegroundColor DarkGray
        }
    }

    if ($bands['upper'].Rows.Count -gt 0 -and $bands['middle'].Rows.Count -gt 0 `
            -and $bands['lower'].Rows.Count -gt 0) {
        $upper = ($bands['upper'].Rows | Measure-Object -Minimum).Minimum
        $mid = ($bands['middle'].Rows | Measure-Object -Minimum).Minimum
        $low = ($bands['lower'].Rows | Measure-Object -Minimum).Minimum
        if ($upper -lt $mid -and $mid -lt $low) {
            Write-Host '    ok   the three bands are the way up the client drew them' `
                -ForegroundColor DarkGray
        } else {
            $failures += 'the bands are on the screen in the wrong order'
        }
    }

    # The decisive one. A band the width of the *first* frame would mean the
    # resize was announced and never honoured, and the picture on the screen is
    # the one from before it.
    $measured = $bands['upper'].Widest
    if ([Math]::Abs($measured - $ResizedWidth) -le $Slack) {
        Write-Host ("    ok   the picture on the screen is {0} wide, not {1}: it is the frame drawn after the resize" `
                -f $measured, $FirstWidth) -ForegroundColor DarkGray
    } else {
        $failures += ("the band on the screen is {0} wide; {1} was expected, and {2} would mean the frame from before the resize" `
                -f $measured, $ResizedWidth, $FirstWidth)
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
    Write-Host 'A Wayland window was negotiated, drawn, resized, typed at and closed.' `
        -ForegroundColor Green
    Write-Host 'This is the wire protocol, not libwayland: see the note in wlserver.rs.' `
        -ForegroundColor DarkGray
    Write-Host "Evidence: $(Join-Path $BuildDir 'wayland.png') and $Log" -ForegroundColor DarkGray
    exit 0
} else {
    foreach ($why in $failures) { Write-Host "    FAIL $why" -ForegroundColor Red }
    Write-Host "$($failures.Count) check(s) failed. Log: $Log" -ForegroundColor Red
    exit 1
}
