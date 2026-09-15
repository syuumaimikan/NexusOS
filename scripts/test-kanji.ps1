<#
.SYNOPSIS
    Types romaji in the terminal, presses space, and checks that kanji came out.

.NOTES
    This file is UTF-8 *with* a byte order mark, and every other script here is
    without one. That is deliberate and it is not a style choice: Windows
    PowerShell 5.1 reads a script with no mark as the system's ANSI code page,
    so the Japanese in the strings below arrives as mojibake and the quoting
    falls apart -- the parser reports a missing brace two hundred lines from
    anything that is wrong. The other scripts have non-ASCII only in comments,
    where being mangled does not stop them parsing.

.DESCRIPTION
    Romaji to kana is a function of the letters and has been here for a long
    time. Kana to kanji is not a function of anything: `かんじ` is `漢字` or
    `感じ` or `幹事`, and only meaning decides -- which means a dictionary, and
    a way to choose between what it offers.

    This drives the whole path the way a person does:

      F2 switches the script -> `kanji` becomes かんじ as the letters arrive ->
      space converts it to 漢字 and opens the choice -> space again moves to
      感じ -> escape puts かんじ back -> space converts again and return commits

    Every one of those steps says what it did on the log, because what the
    window shows is the candidate itself in the line and there is no other way
    for anything outside the machine to see it happened.
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
$Log = Join-Path $BuildDir 'kanji-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-kanji.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $Log) { Remove-Item $Log -Force }

$MonitorPort = Get-Random -Minimum 39000 -Maximum 39999
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
                $key = switch ($letter) {
                    ' ' { 'spc' }
                    '.' { 'dot' }
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

        # `echo ` first, in letters, so the line runs afterwards and the kanji
        # ends up somewhere a person would see it. The space here is a space:
        # there is no kana behind it, and a space with no kana behind it is what
        # somebody separating two English words means by it.
        Write-Host '==> Typing romaji and converting it' -ForegroundColor Cyan
        Send-Text 'echo '
        Send-Keys @('f2')
        Send-Text 'kanji'
        Start-Sleep -Milliseconds 500

        # The first space converts.
        Send-Keys @('spc')
        if (-not (Wait-For -Text 'term: converted かんじ to 漢字' -Seconds 30)) {
            $failures += 'space did not convert the kana'
        }

        # The second moves to the next candidate.
        Send-Keys @('spc')
        Start-Sleep -Milliseconds 700

        # And escape puts the kana back.
        Send-Keys @('esc')
        Start-Sleep -Milliseconds 700

        # Then convert again and commit it, so the shell runs `echo 漢字`.
        Send-Keys @('spc')
        Start-Sleep -Milliseconds 700
        Send-Keys @('ret')
        if (-not (Wait-For -Text 'term: ran echo' -Seconds 30)) {
            $failures += 'the shell never ran the line the kanji was on'
        }
        Start-Sleep -Seconds 2

        if ($Shot) {
            $Ppm = Join-Path $BuildDir 'kanji.ppm'
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
        'compositor: started a terminal',
        # The conversion itself, with the reading and what it became. Both, so
        # that a converter which produced *a* kanji for everything would fail.
        'term: converted かんじ to 漢字',
        # Cycling. 感じ is the second entry for this reading in the dictionary,
        # which is a fact about the table and is what makes this checkable.
        'term: kanji candidate 2 of',
        'is 感じ',
        # And escaping, which has to put back exactly what was typed.
        'term: went back to the kana',
        'term: ran echo'
    )) {
    $checks++
    if ($output.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "never said '$expected'"
        Write-Host "    FAIL $expected" -ForegroundColor Red
    }
}

foreach ($bad in @('KERNEL PANIC', 'term: PANIC')) {
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
    Write-Host "Kanji conversion tests passed ($checks checks)." -ForegroundColor Green
    exit 0
} else {
    foreach ($why in $failures) { Write-Host "    FAIL $why" -ForegroundColor Red }
    Write-Host "$($failures.Count) of $checks checks failed. Log: $Log" -ForegroundColor Red
    exit 1
}
