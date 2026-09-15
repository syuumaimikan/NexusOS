<#
.SYNOPSIS
    Types `ls` in the terminal and checks that a separate program's output
    reaches the screen.

.DESCRIPTION
    `ls` used to be a method on the shell, because a program had nowhere to put
    text a person was meant to read. It is a program now -- `user/nexus-ls` --
    and this is the check that the whole path works:

      a key is typed -> the shell asks the spawn service for BIN/LS.ELF and
      attaches one end of a channel -> the kernel lends that end to the new
      process as its standard output -> the program writes names into it ->
      the shell reads the other end while the program is still running and
      draws each line

    Three things could go wrong and look identical from outside: the program
    not starting, the program starting and writing nowhere, and the shell
    quietly listing the directory itself. So this checks that the spawn
    happened, that the program was lent three things, that it exited, that the
    shell read bytes off the channel, and that the shell carried on afterwards
    -- which between them rule out all three.

    What it does not do is assert on pixels. That was tried: count the lit
    pixels in the window before and after. It does not work here, for two
    reasons worth writing down rather than rediscovering. The desktop's
    wallpaper is a *recording*, so the whole-screen count moves by tens of
    thousands of pixels a second on its own; and the terminal's window is
    placed by slot, so a fixed rectangle inside it is in the window on one run
    and on the desktop on the next. `-Shot` takes a picture for a person to
    look at, and the drawing itself is what `test-terminal.ps1` covers.

.PARAMETER Timeout
    How long to wait for each stage, in seconds.

.PARAMETER Shot
    Where to put a screenshot, if one is wanted.
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
$Log = Join-Path $BuildDir 'stdout-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-stdout.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $Log) { Remove-Item $Log -Force }

$MonitorPort = Get-Random -Minimum 38000 -Maximum 38999
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
$checks = 0

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

        function Send-Keys {
            param([string[]]$Keys, [int]$Pause = 150)
            foreach ($key in $Keys) {
                $writer.WriteLine("sendkey $key")
                Start-Sleep -Milliseconds $Pause
            }
        }
        function Send-Text {
            param([string]$Text)
            foreach ($letter in $Text.ToCharArray()) {
                # `|` is not a key. It is shift and the backslash key, and
                # QEMU's `sendkey` wants it said that way -- a bare `|` is
                # silently not sent, which reads as a shell that ignored the
                # pipe rather than as a test that never typed one.
                $key = switch ($letter) {
                    ' ' { 'spc' }
                    '.' { 'dot' }
                    '|' { 'shift-backslash' }
                    default { $letter }
                }
                Send-Keys @($key) -Pause 110
            }
        }

        Write-Host '==> Opening a terminal' -ForegroundColor Cyan
        Send-Keys @('f3')
        if (-not (Wait-For -Text 'launch: a window for starting things by name' -Seconds 60)) {
            throw 'the launcher never started'
        }
        Send-Text 'tl'
        Start-Sleep -Seconds 1
        Send-Keys @('ret')
        if (-not (Wait-For -Text 'compositor: started a terminal' -Seconds 60)) {
            throw 'the terminal never started'
        }
        Start-Sleep -Seconds 2

        Write-Host '==> ls' -ForegroundColor Cyan
        Send-Text 'ls'
        Send-Keys @('ret')
        if (-not (Wait-For -Text 'from BIN/LS.ELF at a process' -Seconds 60)) {
            $failures += 'the shell never started BIN/LS.ELF'
        }
        # Long, because the picture below is the only evidence a person can
        # read directly and a torn frame is worse than no frame.
        Start-Sleep -Seconds 6

        if ($Shot) {
            $Ppm = Join-Path $BuildDir 'stdout.ppm'
            Invoke-Screendump -Writer $writer -Path $Ppm
            Convert-PpmToPng -PpmPath $Ppm -PngPath $Shot | Out-Null
            Write-Host "    Screenshot: $Shot" -ForegroundColor DarkGray
        }

        # And a pipe, which is the point of all of the above: one channel,
        # each end handed to a different program, and neither knowing which.
        Write-Host '==> ls | count' -ForegroundColor Cyan
        Send-Text 'ls | count'
        Send-Keys @('ret')
        if (-not (Wait-For -Text 'from BIN/COUNT.ELF at a process' -Seconds 60)) {
            $failures += 'the shell never started BIN/COUNT.ELF'
        }
        Start-Sleep -Seconds 4

        # And a name that is not a command and not a program, so the fallback
        # is shown to have a floor: the shell has to say it does not know it
        # rather than sitting there waiting for something that never started.
        Write-Host '==> a name that is neither' -ForegroundColor Cyan
        Send-Text 'nosuchthing'
        Send-Keys @('ret')
        Start-Sleep -Seconds 3

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
        'compositor: started a terminal',
        'term: ran ls',
        # It really is a separate process, and it really was lent three things:
        # somewhere to write, somewhere to read, and the directory.
        'from BIN/LS.ELF at a process',
        'and lent it 3 things',
        # And it finished. A program still running when the shell drew its
        # output would be one whose output the shell had invented.
        'exited with status 0',
        # The shell went on afterwards, which is the check that the drain loop
        # ends rather than blocking for ever on a channel nobody will close.
        'term: ran nosuchthing',
        # And it really came from the program: bytes read off the channel, not
        # the size of the window's scrollback.
        'BIN/LS.ELF wrote ',
        # The pipe: both programs started, and the right-hand one wrote the
        # count of what the left-hand one sent it. A shell that had run them
        # separately would have counted nothing.
        'from BIN/COUNT.ELF at a process',
        'BIN/LS.ELF | BIN/COUNT.ELF wrote '
    )) {
    $checks++
    if ($output.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "never said '$expected'"
        Write-Host "    FAIL $expected" -ForegroundColor Red
    }
}

foreach ($bad in @('KERNEL PANIC', 'term: PANIC', 'ls: PANIC', 'ls: FAILED')) {
    $checks++
    if ($output.Contains($bad)) {
        $failures += "absent: $bad : it appeared"
        Write-Host "    FAIL absent: $bad" -ForegroundColor Red
    } else {
        Write-Host "    ok   absent: $bad" -ForegroundColor DarkGray
    }
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host "Standard output tests passed ($checks checks)." -ForegroundColor Green
    exit 0
} else {
    foreach ($why in $failures) { Write-Host "    FAIL $why" -ForegroundColor Red }
    Write-Host "$($failures.Count) of $checks checks failed. Log: $Log" -ForegroundColor Red
    exit 1
}
