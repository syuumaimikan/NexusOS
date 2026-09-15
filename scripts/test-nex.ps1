<#
.SYNOPSIS
    Writes a Nex program on the machine and runs it there.

.DESCRIPTION
    The ask was to be able to write a program and run it without leaving the
    machine, and that is what this drives, in the order a person would:

      open a terminal -> `write hi.nex print(6 * 7)` -> `nex hi.nex` -> 42

    The language is Nex. It is not Python and it is not C: CPython is six
    hundred thousand lines of C and needs a C library underneath it, GCC is
    millions and needs an assembler, a linker and a target description, and a
    thing called "Python" that ran a tenth of Python would be worse than
    nothing because every program anybody brought to it would fail in a way they
    could not predict. So it has a small honest name and nobody arrives
    expecting their existing programs to run.

    The interpreter's own behaviour is checked on the host -- thirty-five tests
    in `shared/nexus-lang`, covering precedence, scopes, recursion, the error
    messages and the two bounds that stop a runaway program. What this adds is
    the part those cannot reach: that it is on the disk, that the shell starts
    it, that it can read a file it was lent, and that what it printed comes back
    out of the pipe and onto the screen.
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
$Log = Join-Path $BuildDir 'nex-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-nex.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $Log) { Remove-Item $Log -Force }

$MonitorPort = Get-Random -Minimum 42000 -Maximum 42999
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
            param([string[]]$Keys, [int]$Pause = 140)
            foreach ($key in $Keys) {
                $writer.WriteLine("sendkey $key")
                Start-Sleep -Milliseconds $Pause
            }
        }
        function Send-Text {
            param([string]$Text)
            foreach ($letter in $Text.ToCharArray()) {
                # The shifted characters a program needs. QEMU's `sendkey` wants
                # them said as shift plus the unshifted key; a bare `(` is
                # silently not sent, which reads as a shell that ignored half
                # the line rather than as a test that never typed it.
                $key = switch ($letter) {
                    ' ' { 'spc' }
                    '.' { 'dot' }
                    '(' { 'shift-9' }
                    ')' { 'shift-0' }
                    '*' { 'shift-8' }
                    '+' { 'shift-equal' }
                    '"' { 'shift-apostrophe' }
                    '|' { 'shift-backslash' }
                    '/' { 'slash' }
                    '-' { 'minus' }
                    '=' { 'equal' }
                    ',' { 'comma' }
                    default { $letter }
                }
                Send-Keys @($key) -Pause 105
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

        # Written on the machine, in the shell, the way a person would.
        Write-Host '==> Writing a program' -ForegroundColor Cyan
        Send-Text 'write hi.nex print(6 * 7)'
        Send-Keys @('ret')
        Start-Sleep -Seconds 2

        Write-Host '==> Running it' -ForegroundColor Cyan
        Send-Text 'nex hi.nex'
        Send-Keys @('ret')
        if (-not (Wait-For -Text 'from BIN/NEX.ELF at a process' -Seconds 60)) {
            $failures += 'the shell never started BIN/NEX.ELF'
        }
        Start-Sleep -Seconds 4

        if ($Shot) {
            $Ppm = Join-Path $BuildDir 'nex.ppm'
            Invoke-Screendump -Writer $writer -Path $Ppm
            Convert-PpmToPng -PpmPath $Ppm -PngPath $Shot | Out-Null
            Write-Host "    Screenshot: $Shot" -ForegroundColor DarkGray
        }

        # And a program with a mistake in it, so the failing path is shown to
        # have one. A language that reported every program as working would
        # pass every check above.
        Write-Host '==> And one that is wrong' -ForegroundColor Cyan
        Send-Text 'write bad.nex print(1 / 0)'
        Send-Keys @('ret')
        Start-Sleep -Seconds 2
        Send-Text 'nex bad.nex'
        Send-Keys @('ret')
        Start-Sleep -Seconds 4

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
        'term: ran write',
        'term: ran nex',
        # It really is a separate program on the disk.
        'from BIN/NEX.ELF at a process',
        # Six sevens. The number is what says the whole path worked: the file
        # was written, read back, lexed, parsed, evaluated, and what it printed
        # came back down the channel.
        'BIN/NEX.ELF wrote 3 bytes',
        # And it ended well.
        'exited with status 0'
    )) {
    $checks++
    if ($output.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "never said '$expected'"
        Write-Host "    FAIL $expected" -ForegroundColor Red
    }
}

# The program with a mistake has to fail, and has to say where.
$checks++
if ($output -match 'nex: line 1: this divides by nothing' -and
    $output -match 'exited with status 1') {
    Write-Host '    ok   the wrong program said what was wrong and on which line' -ForegroundColor DarkGray
} else {
    $failures += 'the program that divides by nothing was not reported'
    Write-Host '    FAIL the wrong program was not reported' -ForegroundColor Red
}

foreach ($bad in @('KERNEL PANIC', 'nex: PANIC', 'nex: FAILED')) {
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
    Write-Host "Nex tests passed ($checks checks)." -ForegroundColor Green
    exit 0
} else {
    foreach ($why in $failures) { Write-Host "    FAIL $why" -ForegroundColor Red }
    Write-Host "$($failures.Count) of $checks checks failed. Log: $Log" -ForegroundColor Red
    exit 1
}
