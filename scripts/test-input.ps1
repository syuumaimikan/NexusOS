<#
.SYNOPSIS
    Verifies that NexusOS receives and acts on real keystrokes.

.DESCRIPTION
    Boots the system with QEMU's monitor attached, sends keys through it, and
    checks the serial log for what the kernel made of them.

    This is the only honest test of an input path. A unit test can check that a
    scancode table maps 0x1E to 'a'; it cannot check that the I/O APIC pin was
    programmed, that the interrupt arrived on the vector the IDT expects, that
    the handler drained the controller so the next interrupt can be raised, or
    that the decoded key reached something that acted on it. Every one of those
    has to be driven from outside.

.PARAMETER Timeout
    Seconds to let the guest run. Default 30.
#>
[CmdletBinding()]
param(
    [int]$Timeout = 120
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'
$SerialLog = Join-Path $BuildDir 'input-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw "No staged ESP at $EspDir. Run .\scripts\build.ps1 first."
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source

New-Item -ItemType Directory -Force -Path $BuildDir | Out-Null
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-input.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $SerialLog) { Remove-Item $SerialLog -Force }

$MonitorPort = Get-Random -Minimum 25000 -Maximum 25999

$QemuArgs = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
    -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars -SerialLog $SerialLog `
    -MonitorPort $MonitorPort -Headless

Write-Host "==> Booting NexusOS with a monitor on port $MonitorPort" -ForegroundColor Cyan
$process = Start-Process -FilePath $QemuExe.Source -ArgumentList $QemuArgs -PassThru -NoNewWindow

# The keys to send, and what the kernel should make of them. "nexus" spells out
# a word so that a mis-decoded scancode shows up as a wrong letter rather than
# as merely a different count.
# `tab` in the middle on purpose: it is the key the compositor keeps for itself,
# so what comes after it has to arrive somewhere different from what came
# before. That is what separates routing from broadcasting.
$Keys = @('n', 'e', 'x', 'tab', 'u', 's', 'f1')

try {
    # Let the system finish booting before typing. Not merely the input thread:
    # there has to be something for the keys and the clicks to *reach*, and the
    # desktop is the last thing to appear -- the compositor starts its windows
    # first and gives it the strip afterwards. Waiting for a marker rather than
    # for a number of seconds is what keeps this from being a test that passes
    # on a fast host and fails on a busy one; four emulated processors on one
    # real one do not run at any fixed fraction of wall-clock time.
    Write-Host '==> Waiting for the desktop' -ForegroundColor Cyan
    $ready = $false
    for ($waited = 0; $waited -lt $Timeout; $waited++) {
        Start-Sleep -Seconds 1
        if ($process.HasExited) { throw "QEMU exited early with code $($process.ExitCode)" }
        if (Test-Path $SerialLog) {
            $sofar = (Get-Content $SerialLog -Raw -Encoding UTF8) -replace "`0", ''
            if ($sofar.Contains('input thread') -and
                $sofar.Contains('shell: took the strip')) { $ready = $true; break }
        }
    }
    if (-not $ready) { throw 'the desktop never appeared' }

    Write-Host "==> Sending keys: $($Keys -join ' ')" -ForegroundColor Cyan
    $client = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $MonitorPort)
    try {
        $writer = New-Object System.IO.StreamWriter($client.GetStream())
        $writer.AutoFlush = $true
        Start-Sleep -Milliseconds 500
        foreach ($key in $Keys) {
            $writer.WriteLine("sendkey $key")
            # Slower than a person types. The point is to check the path works,
            # not how fast it is, and spacing the keys keeps a dropped one from
            # being blamed on the queue.
            Start-Sleep -Milliseconds 300
        }

        # And the pointer. `mouse_move` is relative, which is what a PS/2 mouse
        # reports and the only thing it can report -- so this is the same shape
        # of input the hardware produces, not a position injected past the
        # driver.
        #
        # Nothing below tracks where the pointer is. It drives it into a corner
        # first, where the compositor clamps it, and moves a known distance from
        # there -- which is the only way to be sure of a position on a machine
        # whose every packet is a delta and some of which may be coalesced.
        #
        # The geometry it aims at is the compositor's: the whole display, two
        # tiles side by side with a four-pixel gap, a fourteen-pixel title bar
        # along the top of each, and a twenty-four-pixel strip along the bottom
        # that belongs to the desktop.

        # Park at the top left, then into the body of the first tile.
        Write-Host '==> Moving the pointer and clicking' -ForegroundColor Cyan
        function Move-Pointer {
            param([int]$Dx, [int]$Dy, [int]$Steps, [int]$Pause = 40)
            foreach ($step in 1..$Steps) {
                $writer.WriteLine("mouse_move $Dx $Dy")
                Start-Sleep -Milliseconds $Pause
            }
        }
        # Far enough to reach the corner from anywhere on a 1920x1200 display.
        function Reset-Pointer {
            param([int]$Dx, [int]$Dy)
            Move-Pointer -Dx $Dx -Dy $Dy -Steps 40 -Pause 25
        }

        Reset-Pointer -Dx -60 -Dy -60
        Move-Pointer -Dx 60 -Dy 0 -Steps 5     # x about 300
        Move-Pointer -Dx 0 -Dy 80 -Steps 5     # y about 400, inside the tile
        $writer.WriteLine('mouse_button 1')
        Start-Sleep -Milliseconds 200
        $writer.WriteLine('mouse_button 0')
        Start-Sleep -Milliseconds 200

        # Resize it first, while its corner is where the layout put it. The
        # first tile starts four pixels in from the left and top and is half the
        # display wide, so its grip -- the last twelve pixels of it -- is a
        # little under a thousand across and a little above the strip. Driving
        # into the bottom left corner and coming back up and right lands there,
        # without this script having to know where the pointer was.
        Write-Host '==> Resizing a window by its corner' -ForegroundColor Cyan
        Reset-Pointer -Dx -60 -Dy 60
        # Aimed at the middle of the grip rather than its edge, and in small
        # steps: under load the guest can miss a packet, and a movement made of
        # one large delta loses all of it where one made of twenty loses a
        # twentieth.
        Move-Pointer -Dx 0 -Dy -11 -Steps 4    # y about 1155, inside the grip
        Move-Pointer -Dx 50 -Dy 0 -Steps 19    # x about 950, likewise
        Start-Sleep -Milliseconds 150
        $writer.WriteLine('mouse_button 1')
        Start-Sleep -Milliseconds 200
        Move-Pointer -Dx -20 -Dy -16 -Steps 20
        $writer.WriteLine('mouse_button 0')
        Start-Sleep -Milliseconds 400

        # And carry a window. The title bar is the only part that can be taken
        # hold of, and it is fourteen pixels of the top of the tile -- so the
        # pointer goes to the top of the display, where it clamps, and comes
        # back down by ten.
        #
        # Negative is upwards here. QEMU's monitor takes screen coordinates and
        # its PS/2 emulation flips the sign on the way to the guest, because a
        # mouse reports Y increasing upwards and a screen has it increasing
        # downwards. Both flips are real and they are in different places.
        Write-Host '==> Carrying a window by its title bar' -ForegroundColor Cyan
        Reset-Pointer -Dx 0 -Dy -60
        Move-Pointer -Dx 0 -Dy 5 -Steps 2      # y about 10, inside the bar
        $writer.WriteLine('mouse_button 1')
        Start-Sleep -Milliseconds 200
        # Down and to the right until both the window and the pointer run into
        # the far corner. Both clamp there, which is the point: afterwards the
        # window is somewhere this script knows without having tracked it.
        Move-Pointer -Dx 40 -Dy 40 -Steps 45 -Pause 25
        $writer.WriteLine('mouse_button 0')
        Start-Sleep -Milliseconds 300

        # And put a window away. The second tile was never moved, so its title
        # bar is still along the top of the right-hand half of the display.
        Write-Host '==> Minimising a window and restoring it' -ForegroundColor Cyan
        Reset-Pointer -Dx -60 -Dy -60
        Move-Pointer -Dx 60 -Dy 0 -Steps 20    # x about 1200, the second tile
        Move-Pointer -Dx 0 -Dy 5 -Steps 2      # y about 10, its title bar
        # The right button, which is the one that puts a window away.
        $writer.WriteLine('mouse_button 2')
        Start-Sleep -Milliseconds 250
        $writer.WriteLine('mouse_button 0')
        Start-Sleep -Milliseconds 300

        # Its tab is in the strip along the very bottom, which belongs to the
        # desktop: the compositor no longer knows what a tab is. It reports
        # where the press landed inside the strip, the desktop decides that was
        # a tab, and it asks for the window back. Three processes for one click,
        # and the point is that the middle one is replaceable.
        #
        # Bottom left, where the pointer clamps, then up into the tabs' own
        # height and right past the button that starts a program.
        Write-Host '==> Bringing a window back from the desktop' -ForegroundColor Cyan
        Reset-Pointer -Dx -60 -Dy 60
        # Up into the middle of the tabs rather than just inside their bottom
        # edge, in several small steps rather than one. A single delta that the
        # guest misses under load leaves the pointer below the tabs entirely,
        # where the click proves the button arrived and nothing about what it
        # landed on -- which is exactly what it did.
        Move-Pointer -Dx 0 -Dy -3 -Steps 5     # y about 1184: the middle of a tab

        # Where the second tab is, read out of the machine rather than guessed.
        # The desktop says where its tabs are whenever that changes, and it
        # changes every time a button is added to the left of them -- which is
        # how this test came to click the first tab while believing it was
        # clicking the second.
        $tabs = [regex]::Match(
            ((Get-Content $SerialLog -Raw -Encoding UTF8) -replace "`0", ''),
            'shell: \d+ tabs start at (\d+) and are (\d+) wide, (\d+) apart')
        if (-not $tabs.Success) { throw 'the desktop never said where its tabs are' }
        $centre = [int]$tabs.Groups[1].Value + [int]$tabs.Groups[3].Value +
            [int]([int]$tabs.Groups[2].Value / 2)
        Write-Host "    the second tab is centred at x=$centre" -ForegroundColor DarkGray
        # In steps of thirty, then whatever is left over, because one large
        # delta is one the guest can miss under load.
        Move-Pointer -Dx 30 -Dy 0 -Steps ([int]($centre / 30))
        Move-Pointer -Dx ($centre % 30) -Dy 0 -Steps 1
        $writer.WriteLine('mouse_button 1')
        Start-Sleep -Milliseconds 250
        $writer.WriteLine('mouse_button 0')
        Start-Sleep -Milliseconds 400

        # And back left onto the button that starts a program. Nothing on this
        # machine could do that before: every process that has ever run was
        # started at boot or by another program deciding to. This is a person
        # pressing something.
        Write-Host '==> Starting a program from the desktop' -ForegroundColor Cyan
        Move-Pointer -Dx -60 -Dy 0 -Steps ([int]($centre / 60) + 2)   # back to the left edge
        Move-Pointer -Dx 30 -Dy 0 -Steps 1     # x about 30: the launcher
        $writer.WriteLine('mouse_button 1')
        Start-Sleep -Milliseconds 300
        $writer.WriteLine('mouse_button 0')

        # Starting a program means reading an ELF off the disk and building an
        # address space for it, which is not instant on an emulated machine.
        # Waited for rather than slept through, for the same reason as the
        # report below: how long it takes is a property of the host.
        for ($waited = 0; $waited -lt 60; $waited++) {
            Start-Sleep -Seconds 1
            if ($process.HasExited) { break }
            if (Test-Path $SerialLog) {
                $sofar = (Get-Content $SerialLog -Raw -Encoding UTF8) -replace "`0", ''
                if ($sofar.Contains('started a window because someone pressed')) { break }
            }
        }
        # Wait for the monitor thread's next report, which is what carries the
        # keyboard figures and the decoded line into the serial log. It runs
        # every five seconds of *guest* time, which is not wall-clock time: this
        # machine emulates four processors on one, and there is a compositor, a
        # desktop and three clients on it by the end.
        #
        # So this waits for the report itself rather than for a number of
        # seconds. A fixed sleep here is a test that passes on an idle host and
        # fails on a busy one, which is the worst kind: it fails for a reason
        # that has nothing to do with the system under test.
        Write-Host '==> Waiting for the report that carries the figures' -ForegroundColor Cyan
        $reported = $false
        for ($waited = 0; $waited -lt 90; $waited++) {
            Start-Sleep -Seconds 1
            if ($process.HasExited) { break }
            if (Test-Path $SerialLog) {
                $sofar = (Get-Content $SerialLog -Raw -Encoding UTF8) -replace "`0", ''
                $seen = [regex]::Matches($sofar, 'keyboard: (\d+) scancodes')
                $felt = [regex]::Matches($sofar, 'pointer (\d+) packets')
                # Both, because the keys were sent long before the pointer was
                # moved: a report carrying fourteen scancodes and no packets is
                # one written while the mouse was still being dragged.
                if ($seen.Count -gt 0 -and $felt.Count -gt 0 -and
                    [int]$seen[$seen.Count - 1].Groups[1].Value -ge 14 -and
                    [int]$felt[$felt.Count - 1].Groups[1].Value -ge 5) {
                    $reported = $true
                    break
                }
            }
        }
        if (-not $reported) {
            Write-Host '    the monitor never reported the keyboard' -ForegroundColor Yellow
        }
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

if (-not (Test-Path $SerialLog)) { throw 'no serial output' }
$output = (Get-Content $SerialLog -Raw -Encoding UTF8) -replace "`0", ''

$failures = @()

# The interrupt path: the pin was programmed and something arrived on it.
if (-not $output.Contains('routed to vector')) {
    $failures += 'the keyboard was never routed to a vector'
}
# The *last* report, not the first: the monitor writes one every five seconds
# and the earliest of them was written before anything had been typed. Reading
# that one is how a run that sent fourteen scancodes comes to report four.
$reports = [regex]::Matches($output, 'keyboard: (\d+) scancodes')
if ($reports.Count -eq 0) {
    $failures += 'no scancodes were received'
} else {
    $scancodes = [int]$reports[$reports.Count - 1].Groups[1].Value
    # Seven keys, each a press and a release, so at least fourteen.
    if ($scancodes -lt 14) {
        $failures += "only $scancodes scancodes arrived; expected at least 14"
    } else {
        Write-Host "    ok   $scancodes scancodes received" -ForegroundColor DarkGray
    }
}

# The decode path, end to end: the letters have to come back as the word that
# was typed. A count of scancodes would pass with a completely wrong scancode
# table; the text will not.
# The tab in the middle is a space to the kernel's own panel, which is why the
# expected text has one: the kernel goes on acting on every key for the panel
# while the compositor routes a copy of the same keys to a client.
if ($output.Contains('line "nex us"')) {
    Write-Host '    ok   the typed letters decoded to "nex us"' -ForegroundColor DarkGray
} else {
    $failures += 'the typed letters did not decode to "nex us"'
}

# And the part that is new: a keystroke went into the kernel's keyboard driver,
# crossed a channel to the compositor, was routed to one client, and arrived.
$first = ([regex]::Matches($output, 'client 0: heard a key')).Count
$second = ([regex]::Matches($output, 'client 1: heard a key')).Count
if ($first -lt 1) {
    $failures += 'no key ever reached a client'
} else {
    Write-Host "    ok   $first keys reached the first client" -ForegroundColor DarkGray
}

# Tab is the key the compositor keeps for itself, so what was typed after it has
# to arrive somewhere different from what came before. Both clients hearing
# something is what says this is routing; either of them hearing *everything*
# would say it is broadcasting.
if (-not $output.Contains('compositor: focus moved to the second client')) {
    $failures += 'tab did not move the focus'
} elseif ($second -lt 1) {
    $failures += 'the focus moved but no key followed it'
} else {
    Write-Host "    ok   $second keys followed the focus to the second client" -ForegroundColor DarkGray
}

# And neither of them saw the other's keys. Three letters were typed before the
# tab and two after it, so a client that heard all five heard someone else's.
if ($first -gt 3 -or $second -gt 2) {
    $failures += "a client heard keys meant for the other ($first and $second of 3 and 2)"
} else {
    Write-Host '    ok   neither client heard the keys meant for the other' -ForegroundColor DarkGray
}

# The pointer. Packets rather than bytes, because three bytes that never became
# a packet is a driver that is reading the stream and not understanding it.
$pointers = [regex]::Matches(
    $output,
    'pointer (\d+) packets from (\d+) bytes,\s+(\d+) resynchronised, (\d+) overflowed')
if ($pointers.Count -eq 0) {
    $failures += 'the kernel never reported what the pointer did'
} else {
    $last = $pointers[$pointers.Count - 1]
    $packets = [int]$last.Groups[1].Value
    $desynchronised = [int]$last.Groups[3].Value
    if ($packets -lt 5) {
        $failures += "only $packets pointer packets arrived"
    } else {
        Write-Host "    ok   $packets pointer packets decoded from $($last.Groups[2].Value) bytes" -ForegroundColor DarkGray
    }
    # A packet stream that had to resynchronise is one where a byte went to the
    # wrong driver: the keyboard and the mouse share a controller, and only one
    # bit says whose byte is waiting.
    if ($desynchronised -gt 0) {
        $failures += "$desynchronised pointer packets had to resynchronise, so bytes are going astray"
    }
}

# And the click. The keyboard's tab left the second client focused, so a click
# landing in the first one has something to change -- which is what says the
# compositor turned a relative movement into a position and worked out what was
# under it.
if ($output.Contains('the pointer gave focus to the first client')) {
    Write-Host '    ok   clicking a tile moved the focus to it' -ForegroundColor DarkGray
} else {
    $failures += 'clicking a tile did not move the focus'
}

# And the drag. A window that moved is the whole of what a title bar is for, and
# it is the one thing here the client is never told about: it draws into a
# surface and has no idea where that surface ends up, so it cannot have moved
# itself.
if ($output.Contains('compositor: carried a window by its title bar')) {
    Write-Host '    ok   a window was carried by its title bar' -ForegroundColor DarkGray
} else {
    $failures += 'the pointer never carried a window'
}

# And the resize. A window's surface is tightly packed, so its size is its
# shape: changing one means replacing the other, and the client has to take the
# new one and keep drawing. Both halves are checked, because a compositor that
# resized a window whose client never noticed would be compositing from a
# surface nobody was drawing into.
if ($output.Contains('compositor: resized a window by its corner')) {
    Write-Host '    ok   a window was resized by its corner' -ForegroundColor DarkGray
} else {
    $failures += 'the pointer never resized a window'
}
if ($output.Contains('client: took a new surface and kept drawing')) {
    Write-Host '    ok   the client took the new surface and went on drawing' -ForegroundColor DarkGray
} else {
    $failures += 'the client never took the surface it was given'
}

# And minimising. The tab is the whole reason this is a feature rather than a
# trap: a window that can be put away and not brought back has been destroyed
# with extra steps, and its client would go on drawing frames nobody would see.
# So both halves are required.
if ($output.Contains('compositor: put a window away, leaving its tab')) {
    Write-Host '    ok   a window was put away' -ForegroundColor DarkGray
} else {
    $failures += 'no window was ever put away'
}
# Bringing it back is now three processes rather than one. The compositor knows
# only that a button went down at a point inside the strip; the desktop decides
# that point was a tab and asks for the window; the compositor checks the slot
# it named and does it. Every step is required, because a chain that works with
# a step missing is a chain where that step is decoration.
if ($output.Contains('compositor: a press in the strip went to the desktop')) {
    Write-Host '    ok   a press in the strip was passed to the desktop' -ForegroundColor DarkGray
} else {
    $failures += 'a press in the strip never reached the desktop'
}
if ($output.Contains('shell: turned a click in the strip into a command')) {
    Write-Host '    ok   the desktop turned it into a command' -ForegroundColor DarkGray
} else {
    $failures += 'the desktop never turned a click into a command'
}
if ($output.Contains('compositor: brought a window back because the desktop asked')) {
    Write-Host '    ok   and the window came back' -ForegroundColor DarkGray
} else {
    $failures += 'a window that was put away could not be brought back'
}

# And starting one. Everything that has ever run on this machine was started at
# boot or by a program that decided to; this is a person pressing a button, and
# the whole path from the press to a process in ring 3 is new.
if ($output.Contains('shell: asked for a program to be started')) {
    Write-Host '    ok   the desktop asked for a program' -ForegroundColor DarkGray
} else {
    $failures += 'pressing the desktop never asked for a program'
}
if ($output.Contains('compositor: started a window because someone pressed the desktop')) {
    Write-Host '    ok   and a window was started for it' -ForegroundColor DarkGray
} else {
    $failures += 'pressing the desktop started no window'
}

# The desktop draws words, so it has to be told which language they are in. It
# is told by the kernel, through the compositor, because the kernel is where F1
# is acted on -- and a desktop counting keys of its own would agree with the
# kernel's panel only until it missed one.
if ($output.Contains('shell: drew its strip in the language it was told')) {
    Write-Host '    ok   the desktop drew its strip in the language it was told' -ForegroundColor DarkGray
} else {
    $failures += 'the desktop was never told what language to draw in'
}

# F1 is a distinct key, and acting on it says the decoded key reached something
# that used it.
if ($output.Contains('F1: interface language is now ja-JP')) {
    Write-Host '    ok   F1 switched the interface language' -ForegroundColor DarkGray
} else {
    $failures += 'F1 did not switch the interface language'
}

if ($output.Contains('EXCEPTION')) { $failures += 'an exception was reported' }
if ($output.Contains('KERNEL PANIC')) { $failures += 'the kernel panicked' }

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'Input tests passed.' -ForegroundColor Green
    exit 0
} else {
    foreach ($failure in $failures) {
        Write-Host "    FAIL $failure" -ForegroundColor Red
    }
    Write-Host "Serial log: $SerialLog"
    exit 1
}
