<#
.SYNOPSIS
    Boots the machine and checks that the AC'97 card played the start-up chime.

.DESCRIPTION
    Nothing here can listen, so "it made a sound" has to be established some
    other way. The controller publishes where its DMA engine has got to -- which
    descriptor it is reading and how much of that buffer is left -- and those
    numbers change only because the engine is reading memory. `ac97::tone`
    watches them and returns false if they never moved, and `sound.rs` falls
    back to the speaker when it does.

    So the claim is carried by one line: the chime is played "through the sound
    card" or "on the speaker", and the first of those is only printed when the
    engine ran. A driver that set every register and forgot the run bit, or
    whose bus mastering was never enabled, prints the second.

    The machine is given `-audiodev none`, so the card is entirely real to the
    guest and the samples go nowhere on the host. A test that opened the
    developer's speakers on every boot would be a test nobody runs twice.

.PARAMETER Timeout
    How long to wait for the boot, in seconds.
#>
[CmdletBinding()]
param([int]$Timeout = 300)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'
$Log = Join-Path $BuildDir 'sound-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-sound.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $Log) { Remove-Item $Log -Force }

$QemuArgs = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
    -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars -SerialLog $Log -Headless

Write-Host '==> Booting NexusOS' -ForegroundColor Cyan
$process = Start-Process -FilePath $QemuExe.Source -ArgumentList $QemuArgs -PassThru -NoNewWindow
try {
    $done = $false
    for ($waited = 0; $waited -lt $Timeout; $waited++) {
        Start-Sleep -Seconds 1
        if ($process.HasExited) { break }
        if (Test-Path $Log) {
            $sofar = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''
            # Either outcome ends the wait; a failure is a result.
            if ($sofar -match 'played the start-up chime') {
                # And a little longer, so the monitor's first report -- which
                # carries the sample count -- has been written.
                Start-Sleep -Seconds 7
                $done = $true
                break
            }
        }
    }
    if (-not $done) { throw 'the machine never played its chime' }
} finally {
    if (-not $process.HasExited) {
        try { $process.Kill() } catch { }
    }
    $process.WaitForExit(5000) | Out-Null
}

$output = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''

$failures = @()
$checks = 0

foreach ($expected in @(
        # The card is on the bus and is the one this driver drives.
        '8086:2415',
        # The codec answered. A controller whose link is still in reset reports
        # itself present and every mixer write is silently lost.
        # Not the slot. Adding a device to the machine renumbers everything
        # after it on the bus, and this test broke the day a GPU was plugged in
        # in front of the sound card -- which is a fact about the argument list
        # and not about the driver. What matters is the card and the codec.
        "AC'97 at ",
        ': 48000 Hz, 2 channels, 128 KiB of buffer, codec ready',
        # And the engine ran. This is the check: the alternative ending of this
        # sentence is " on the speaker".
        'played the start-up chime through the sound card'
    )) {
    $checks++
    if ($output.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "never said '$expected'"
        Write-Host "    FAIL $expected" -ForegroundColor Red
    }
}

# How many samples the card was handed. Zero would mean the fallback above had
# been taken without saying so.
$checks++
$played = [regex]::Match($output, '(\d+) through the card \((\d+) samples\)')
if (-not $played.Success) {
    $failures += 'the monitor never reported what the card played'
    Write-Host '    FAIL the monitor never reported what the card played' -ForegroundColor Red
} else {
    $tones = [int]$played.Groups[1].Value
    $samples = [long]$played.Groups[2].Value
    # Three notes in the chime, and each is tens of thousands of samples at
    # 48 kHz in stereo -- 90 ms is 8640 frames, which is 17280 samples.
    if ($tones -ge 3 -and $samples -ge 17280) {
        Write-Host "    ok   $tones tones through the card, $samples samples" -ForegroundColor DarkGray
    } else {
        $failures += "the card played $tones tones and $samples samples"
        Write-Host "    FAIL only $tones tones and $samples samples" -ForegroundColor Red
    }
}

foreach ($bad in @(
        'KERNEL PANIC',
        # Each of these is a way the driver gives up, and each would otherwise
        # be invisible behind the speaker still working.
        'the AC''97 codec never reported itself ready',
        'no memory for the AC''97',
        'landed above four gigabytes',
        "no AC'97 card on this machine")) {
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
    Write-Host "Sound tests passed ($checks checks)." -ForegroundColor Green
    exit 0
} else {
    foreach ($why in $failures) { Write-Host "    FAIL $why" -ForegroundColor Red }
    Write-Host "$($failures.Count) of $checks checks failed. Log: $Log" -ForegroundColor Red
    exit 1
}
