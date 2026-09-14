<#
.SYNOPSIS
    Opens a terminal from the desktop and types commands into it.

.DESCRIPTION
    The path a person takes: press the button, get a window with a prompt, type
    something, and have the machine do it. Every step of that crosses a boundary
    -- the desktop asks the compositor, the compositor starts the program and
    lends it the filesystem, the kernel routes the keys, the shell reads them
    and asks the filesystem -- and none of those can be checked from inside any
    one of them.

    What it types makes a file and reads it back, because that is the shortest
    command that proves the handle it was lent actually reaches a disk.

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
$Log = Join-Path $BuildDir 'terminal-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-terminal.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force
if (Test-Path $Log) { Remove-Item $Log -Force }

$MonitorPort = Get-Random -Minimum 33000 -Maximum 34999
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
            param([string[]]$Keys, [int]$Pause = 150)
            foreach ($key in $Keys) {
                $writer.WriteLine("sendkey $key")
                Start-Sleep -Milliseconds $Pause
            }
        }

        # The button that opens a terminal sits past the launcher and the one
        # that opens a page. Driven into the corner first, where the pointer
        # clamps, so nothing here has to know where it was.
        Write-Host '==> Pressing the button that opens a terminal' -ForegroundColor Cyan
        Move-Pointer -Dx -60 -Dy 60 -Steps 40 -Pause 20
        Move-Pointer -Dx 0 -Dy -3 -Steps 5
        Move-Pointer -Dx 20 -Dy 0 -Steps 8      # x about 160: past Open and Web
        $writer.WriteLine('mouse_button 1')
        Start-Sleep -Milliseconds 250
        $writer.WriteLine('mouse_button 0')

        if (-not (Wait-For -Text 'term: a terminal, with a shell in it' -Seconds 90)) {
            throw 'the terminal never started'
        }

        # And type at it. A file written and read back is the shortest command
        # that proves the directory handle reaches a real disk.
        Write-Host '==> Typing at the prompt' -ForegroundColor Cyan
        Send-Keys @('h', 'e', 'l', 'p', 'ret')
        Send-Keys @('l', 's', 'ret')
        Send-Keys @('w', 'r', 'i', 't', 'e', 'spc', 'n', 'o', 't', 'e', 'spc', 'h', 'i', 'ret')
        Send-Keys @('c', 'a', 't', 'spc', 'n', 'o', 't', 'e', 'ret')
        Send-Keys @('u', 'p', 't', 'i', 'm', 'e', 'ret')
        Send-Keys @('b', 'e', 'e', 'p', 'ret')
        # And what the machine is doing, which comes from the kernel over a
        # channel rather than from anything this shell knows.
        Send-Keys @('s', 'y', 's', 'ret')
        # And Japanese. The command word is typed *before* the input method is
        # turned on, because `echo` in kana is not a command -- which is correct
        # behaviour and was a mistake in this test before it was a feature of
        # the shell.
        Send-Keys @('e', 'c', 'h', 'o', 'spc')
        Send-Keys @('f2')
        Send-Keys @('k', 'o', 'n', 'n', 'i', 'c', 'h', 'i', 'h', 'a', 'ret')
        Send-Keys @('f2')
        Send-Keys @('f2')
        Send-Keys @('n', 'o', 'p', 'e', 'ret')

        # Waited for, not slept through.
        #
        # This was three seconds of sleep and it failed about half the time,
        # which cost an afternoon: the keys are queued by the emulated 8042,
        # read by the kernel's input thread, forwarded by the compositor and
        # acted on by a shell that is drawing at the same time, and how long all
        # of that takes is a property of the host rather than of the guest. A
        # fixed wait turns a slow host into a failing test, and every marker
        # this file checks for is one the machine says as soon as it happens.
        if (-not (Wait-For -Text 'term: ran nope' -Seconds 60)) {
            Write-Host '    the shell never reported the last command' -ForegroundColor Red
        }

        # Stopped rather than killed. A machine that is shot loses whatever its
        # serial line had not got to the file yet, and what it loses is the last
        # thing that happened -- which is always the thing being tested.
        if ($Shot) {
            $Ppm = Join-Path $BuildDir 'terminal.ppm'
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
        'compositor: started a terminal, and lent it the filesystem',
        'term: a terminal, with a shell in it',
        'term: ran help',
        'term: ran ls',
        'term: ran write',
        'term: ran cat',
        'term: ran uptime',
        'term: ran beep',
        'term: ran sys',
        'term: typing now makes ',
        'term: ran echo',
        'term: ran nope',
        'snd ] played the start-up chime'
    )) {
    if ($output.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "never reported: $expected"
    }
}

if ($output.Contains('term: PANIC')) { $failures += 'the terminal panicked' }
if ($output.Contains('term: FAILED')) { $failures += 'the terminal reported a failure' }
if ($output.Contains('KERNEL PANIC')) { $failures += 'the kernel panicked' }

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'Terminal tests passed.' -ForegroundColor Green
    exit 0
} else {
    foreach ($failure in $failures) {
        Write-Host "    FAIL $failure" -ForegroundColor Red
    }
    Write-Host "Log: $Log"
    exit 1
}
