<#
.SYNOPSIS
    Puts a picture behind everything, then a recording, and checks both.

.DESCRIPTION
    The wallpaper is a program like any other: it is lent the settings
    directory and the disk read-only, and it decides what to draw. What this
    checks is that a name in `look.picture` reaches the screen -- through the
    settings file, a re-read on a clock, an open through a lent directory, this
    system's own decoder, and a composite.

    The recording is the more interesting half. There are no per-client damage
    rectangles here, so a full-screen wallpaper frame costs a composite of the
    whole display; it is capped at four a second and the wallpaper says what it
    actually managed. This requires that line rather than a frame rate, because
    a threshold would fail for reasons that have nothing to do with this code.

.PARAMETER Timeout
    How long to wait for each stage, in seconds.

.PARAMETER Shot
    Where to put a screenshot of the picture wallpaper, if one is wanted.
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
$Log = Join-Path $BuildDir 'wallpaper-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-wallpaper.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $Log) { Remove-Item $Log -Force }

$MonitorPort = Get-Random -Minimum 34000 -Maximum 34999
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

        function Send-Keys {
            param([string[]]$Keys, [int]$Pause = 130)
            foreach ($key in $Keys) {
                $writer.WriteLine("sendkey $key")
                Start-Sleep -Milliseconds $Pause
            }
        }

        # A word, one letter per key, since QEMU names keys and not text.
        # Lowercase only: `sendkey n` is `n`, and the names on this store are
        # uppercase -- which the wallpaper handles, and which is exactly the
        # thing worth testing rather than working around here.
        function Invoke-Command {
            param([string[]]$Words)
            foreach ($word in $Words) {
                $keys = @()
                foreach ($letter in $word.ToCharArray()) {
                    switch ($letter) {
                        ' ' { $keys += 'spc' }
                        '.' { $keys += 'dot' }
                        default { $keys += [string]$letter }
                    }
                }
                Send-Keys $keys
            }
            Send-Keys @('ret')
        }

        # The terminal, from the strip. The launcher would do as well and is
        # tested on its own; this takes the path that has been working longest,
        # because what is being tested here is the wallpaper.
        Write-Host '==> Opening a terminal' -ForegroundColor Cyan
        foreach ($step in 1..40) { $writer.WriteLine('mouse_move -60 60'); Start-Sleep -Milliseconds 20 }
        foreach ($step in 1..5) { $writer.WriteLine('mouse_move 0 -3'); Start-Sleep -Milliseconds 35 }
        foreach ($step in 1..8) { $writer.WriteLine('mouse_move 20 0'); Start-Sleep -Milliseconds 35 }
        $writer.WriteLine('mouse_button 1')
        Start-Sleep -Milliseconds 250
        $writer.WriteLine('mouse_button 0')
        if (-not (Wait-For -Text 'term: a terminal, with a shell in it' -Seconds 90)) {
            throw 'the terminal never started'
        }
        Start-Sleep -Seconds 2

        Write-Host '==> Setting a picture as the wallpaper' -ForegroundColor Cyan
        Invoke-Command @('set look.picture nexus.jpg')
        if (-not (Wait-For -Text 'wall: showing nexus.jpg behind everything' -Seconds 60)) {
            $failures += 'the picture never became the wallpaper'
        }
        Start-Sleep -Seconds 3

        if ($Shot) {
            $Ppm = Join-Path $BuildDir 'wallpaper.ppm'
            Invoke-Screendump -Writer $writer -Path $Ppm
            Convert-PpmToPng -PpmPath $Ppm -PngPath $Shot | Out-Null
            Write-Host "    Screenshot: $Shot" -ForegroundColor DarkGray
        }

        Write-Host '==> And then a recording' -ForegroundColor Cyan
        Invoke-Command @('set look.picture nexus.avi')
        if (-not (Wait-For -Text 'wall: playing nexus.avi behind everything' -Seconds 60)) {
            $failures += 'the recording never became the wallpaper'
        }
        # Long enough for twenty frames at four a second, which is what the
        # wallpaper waits for before it says what it managed.
        if (-not (Wait-For -Text 'wall: decoding at' -Seconds 60)) {
            $failures += 'the wallpaper never said what frame rate it managed'
        }

        Write-Host '==> And back to a pattern' -ForegroundColor Cyan
        Invoke-Command @('set look.picture none.png')
        if (-not (Wait-For -Text 'wall: none.png will not open' -Seconds 60)) {
            $failures += 'a name that does not exist was not reported'
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

if ($output -match 'wall: decoding at ([\d.]+) frames a second') {
    Write-Host "    ok   decoded $($Matches[1]) frames a second behind everything" -ForegroundColor DarkGray
}

foreach ($bad in @('wall: FAILED', 'wall: PANIC', 'KERNEL PANIC')) {
    if ($output.Contains($bad)) {
        $failures += "saw: $bad"
    } else {
        Write-Host "    ok   absent: $bad" -ForegroundColor DarkGray
    }
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'Wallpaper tests passed.' -ForegroundColor Green
    exit 0
} else {
    foreach ($why in $failures) { Write-Host "    FAIL $why" -ForegroundColor Red }
    Write-Host "Log: $Log"
    exit 1
}
