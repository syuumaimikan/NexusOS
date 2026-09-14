<#
.SYNOPSIS
    Changes how the machine looks, from inside it, and checks that it changed.

.DESCRIPTION
    One command typed at a prompt has to reach four programs: the terminal
    writes the settings file, the wallpaper notices and redraws, the desktop
    notices and re-colours its strip, and the compositor -- which is told the
    accent by the kernel rather than reading it -- keeps the colour it was given
    until the next session.

    Nothing tells any of them. They look, on the clock they already have. That
    is the property worth testing: a machine where changing a setting needs
    something to be restarted is a machine where settings are a restart.

    It also types Japanese, because the terminal's input method is the other
    thing a person changes with a keypress and the only way to see from outside
    that it converted anything is to have the shell echo back what it received.

.PARAMETER Timeout
    How long to wait for each stage, in seconds.
#>
[CmdletBinding()]
param(
    [int]$Timeout = 240
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'
$Log = Join-Path $BuildDir 'appearance-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-appearance.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force
if (Test-Path $Log) { Remove-Item $Log -Force }

$MonitorPort = Get-Random -Minimum 37000 -Maximum 38999
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

# Whether the wallpaper is drawing a style, however it came to be drawing it.
#
# Two lines can say so: the one it prints when it starts, and the one it prints
# when it notices a change. Waiting only for the second made this test depend on
# which style the machine was left on by whatever ran before it -- and it failed
# in both directions, once for each value: setting a style the machine is
# already on produces no change, and therefore no line.
function Wait-ForStyle {
    param([string]$Style, [int]$Seconds)
    $pattern = 'wall: (the look changed to|[0-9]+x[0-9]+ behind the windows,) ' + $Style
    for ($waited = 0; $waited -lt $Seconds; $waited++) {
        Start-Sleep -Seconds 1
        if ($process.HasExited) { return $false }
        if (Test-Path $Log) {
            $sofar = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''
            if ([regex]::IsMatch($sofar, $pattern)) { return $true }
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
            param([string[]]$Keys, [int]$Pause = 140)
            foreach ($key in $Keys) {
                $writer.WriteLine("sendkey $key")
                Start-Sleep -Milliseconds $Pause
            }
        }
        # A word, one letter per key, since QEMU names keys and not text.
        function Send-Text {
            param([string]$Text)
            $keys = @()
            foreach ($character in $Text.ToCharArray()) {
                switch ($character) {
                    ' ' { $keys += 'spc' }
                    '.' { $keys += 'dot' }
                    default { $keys += [string]$character }
                }
            }
            Send-Keys -Keys $keys
        }

        Write-Host '==> Opening a terminal' -ForegroundColor Cyan
        Move-Pointer -Dx -60 -Dy 60 -Steps 40 -Pause 20
        Move-Pointer -Dx 0 -Dy -3 -Steps 5
        Move-Pointer -Dx 20 -Dy 0 -Steps 8
        $writer.WriteLine('mouse_button 1')
        Start-Sleep -Milliseconds 250
        $writer.WriteLine('mouse_button 0')

        if (-not (Wait-For -Text 'term: a terminal, with a shell in it' -Seconds 90)) {
            throw 'the terminal never started'
        }

        Write-Host '==> Changing how the machine looks' -ForegroundColor Cyan
        Send-Text 'look'
        Send-Keys @('ret')

        # Put the machine in a known state first, because it remembers: this
        # test used to type `set look.style stars` on a machine the settings
        # test had already left on stars, and then waited for a change that had
        # no reason to happen. Setting it to something else and waiting for the
        # wallpaper to say so makes the second change real whatever came before.
        Send-Text 'set look.style gradient'
        Send-Keys @('ret')
        if (-not (Wait-ForStyle -Style 'gradient' -Seconds 60)) {
            $failures += 'the wallpaper never went back to a gradient'
        }

        Send-Text 'set look.style stars'
        Send-Keys @('ret')
        Send-Text 'set look.accent 40d090'
        Send-Keys @('ret')

        # The wallpaper looks every two seconds and the desktop on its minute
        # tick; both are waited for rather than slept through.
        if (-not (Wait-ForStyle -Style 'stars' -Seconds 60)) {
            $failures += 'the wallpaper never noticed the setting'
        }

        # The command word is typed *before* the input method is turned on,
        # because `echo` in kana is not a command -- which is correct behaviour
        # and was a mistake in this test before it was a feature of the shell.
        Write-Host '==> Typing Japanese' -ForegroundColor Cyan
        Send-Text 'echo '
        Send-Keys @('f2')
        Send-Text 'konnichiha'
        Send-Keys @('ret')

        # Waited for, not slept through. How long a keystroke takes to cross
        # the emulated 8042, the kernel's input thread, the compositor and a
        # shell that is drawing at the same time is a property of the host, and
        # a fixed wait turns a busy host into a failing test. This is the same
        # mistake `test-terminal.ps1` had, found the same way.
        if (-not (Wait-For -Text 'term: ran echo' -Seconds 60)) {
            Write-Host '    the shell never reported the command' -ForegroundColor Red
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
        # Whatever style the machine happened to start on. The size is the
        # claim worth checking here: the wallpaper is given the screen minus the
        # strip, and a wallpaper that drew the whole screen would cover it.
        'wall: 1920x1164 behind the windows,',
        'term: ran look',
        'term: ran set',
        # That the write actually landed, and not merely that the command ran.
        # The difference mattered: a suite run showed three `ran set` lines and
        # a wallpaper that never saw the style, and there was no way from the
        # log to tell a failed write from a reader that missed it.
        'term: set look.style to stars',
        'term: set look.accent to 40d090',
        'term: typing now makes ',
        'term: ran echo'
    )) {
    if ($output.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "never reported: $expected"
    }
}

foreach ($bad in @('wall: PANIC', 'term: PANIC', 'KERNEL PANIC', 'wall: FAILED')) {
    if ($output.Contains($bad)) { $failures += "saw $bad" }
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'Appearance tests passed.' -ForegroundColor Green
    exit 0
} else {
    foreach ($failure in $failures) {
        Write-Host "    FAIL $failure" -ForegroundColor Red
    }
    Write-Host "Log: $Log"
    exit 1
}
