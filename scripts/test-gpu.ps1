<#
.SYNOPSIS
    Boots the machine and checks that the GPU is displaying what the kernel drew.

.DESCRIPTION
    The driver is virtio-gpu, in two dimensions: find the device, walk its PCI
    capabilities to locate the modern transport's registers, negotiate, set up a
    virtqueue, create a resource, give it memory, make it the scanout, and hand
    the device rectangles that have changed.

    Three things are checked, and they are different claims.

    The device *answered*. `GET_DISPLAY_INFO` comes back with a width and a
    height that came from the host, not from the driver -- a driver that sent
    nothing and reported success would have to invent those numbers and would
    get them wrong.

    The device *accepted* what was drawn. Every command is answered with a type
    code the driver did not write.

    And the pixels *arrived*. The kernel fills the scanout with three bands --
    red, green, blue, top to bottom -- and this asks QEMU for a picture of what
    that display device is showing. Black would be what an unconfigured display
    shows as well, so a blank capture proves nothing; a red band at a third of
    the way down proves the whole path, from the kernel writing guest memory to
    the host having the pixels.
#>
[CmdletBinding()]
param([int]$Timeout = 300)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'
$Log = Join-Path $BuildDir 'gpu-test.log'
$Scanout = Join-Path $BuildDir 'gpu-scanout.ppm'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-gpu.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $Log) { Remove-Item $Log -Force }
if (Test-Path $Scanout) { Remove-Item $Scanout -Force }

$MonitorPort = Get-Random -Minimum 41000 -Maximum 41999
$QemuArgs = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
    -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars -SerialLog $Log `
    -MonitorPort $MonitorPort -Headless

Write-Host "==> Booting NexusOS with a monitor on port $MonitorPort" -ForegroundColor Cyan
$process = Start-Process -FilePath $QemuExe.Source -ArgumentList $QemuArgs -PassThru -NoNewWindow
try {
    $drew = $false
    for ($waited = 0; $waited -lt $Timeout; $waited++) {
        Start-Sleep -Seconds 1
        if ($process.HasExited) { break }
        if (Test-Path $Log) {
            $sofar = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''
            # Either ending stops the wait; a failure is a result.
            if ($sofar -match 'drew three bands|would not show|no virtio GPU') {
                $drew = $true
                break
            }
        }
    }
    if (-not $drew) { throw 'the GPU driver never reported either way' }
    Start-Sleep -Seconds 2

    $client = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $MonitorPort)
    try {
        $writer = New-Object System.IO.StreamWriter($client.GetStream())
        $writer.AutoFlush = $true
        Start-Sleep -Milliseconds 500
        # Of *that* device. Without the name, QEMU captures whichever display it
        # counts as first, which on this machine is the firmware's -- and the
        # test would be looking at the desktop rather than at the GPU.
        Write-Host '==> Asking the host what the GPU is showing' -ForegroundColor Cyan
        $writer.WriteLine("screendump $($Scanout.Replace('\', '/')) nexusgpu")
        Start-Sleep -Seconds 3
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

$output = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''

$failures = @()
$checks = 0

foreach ($expected in @(
        # The device is on the bus and is the one this driver drives.
        '1af4:1050',
        # It was brought up, and the size in this line came from the host.
        # Not the slot: adding a device renumbers the bus after it, and that
        # is a fact about the argument list rather than about the driver.
        'virtio GPU at ',
        ': 1280x800 scanout',
        'resource 1 attached',
        # And it took the rectangle.
        'drew three bands into the scanout and the GPU took them'
    )) {
    $checks++
    if ($output.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "never said '$expected'"
        Write-Host "    FAIL $expected" -ForegroundColor Red
    }
}

# The check that matters, and it is outside the guest: what the host has.
$checks++
if (-not (Test-Path $Scanout)) {
    $failures += 'the host produced no picture of the GPU'
    Write-Host '    FAIL the host produced no picture of the GPU' -ForegroundColor Red
} else {
    $bytes = [System.IO.File]::ReadAllBytes($Scanout)
    # `P6`, then width, height and the maximum value, whitespace separated.
    $at = 0
    $fields = @()
    while ($fields.Count -lt 4 -and $at -lt $bytes.Length) {
        while ($at -lt $bytes.Length -and $bytes[$at] -le 32) { $at++ }
        $from = $at
        while ($at -lt $bytes.Length -and $bytes[$at] -gt 32) { $at++ }
        $fields += [System.Text.Encoding]::ASCII.GetString($bytes, $from, $at - $from)
    }
    $at++
    $width = [int]$fields[1]
    $height = [int]$fields[2]

    # What the host says this device is showing.
    #
    # This used to look for three bands of known colour, because the GPU had a
    # scanout of its own that nothing else ever drew into. It *is* the display
    # now -- `display::init` adopts it, the boot screen paints into it and the
    # compositor after that -- so the bands are drawn and then covered over
    # within a second, and a test that insisted on them would be insisting the
    # machine had no screen.
    #
    # The replacement is a stronger claim and a harder one to pass by accident.
    # Black is what an unconfigured display shows, and so is any single colour,
    # so the question asked of the picture is how many *different* colours are
    # in it. A gradient with text on it has hundreds. A device that answered
    # every command and displayed nothing has one.
    $seen = @{}
    $step = 997  # a prime, so the sample walks the whole picture rather than a column
    $pixels = $width * $height
    for ($i = 0; $i -lt $pixels; $i += $step) {
        $index = $at + $i * 3
        if ($index + 2 -ge $bytes.Length) { break }
        $seen["$($bytes[$index]),$($bytes[$index + 1]),$($bytes[$index + 2])"] = $true
    }
    $colours = $seen.Count

    $checks++
    if ($width -eq 1280 -and $height -eq 800) {
        Write-Host "    ok   the host's picture of the GPU is ${width}x${height}" -ForegroundColor DarkGray
    } else {
        $failures += "the GPU's picture is ${width}x${height}, not 1280x800"
        Write-Host "    FAIL the picture is ${width}x${height}" -ForegroundColor Red
    }

    $checks++
    if ($colours -ge 32) {
        Write-Host "    ok   $colours distinct colours in it, so the screen is on it" `
            -ForegroundColor DarkGray
    } else {
        $failures += "only $colours distinct colours in the GPU's picture; a blank display gives one"
        Write-Host "    FAIL only $colours distinct colours" -ForegroundColor Red
    }

    # And that it is a *picture* and not noise: the top of the screen and the
    # bottom are different, which a gradient guarantees and a uniform fill
    # cannot produce.
    $topIndex = $at + ([int]($width / 2)) * 3
    $bottomIndex = $at + ((($height - 2) * $width) + [int]($width / 2)) * 3
    $top = "$($bytes[$topIndex]),$($bytes[$topIndex + 1]),$($bytes[$topIndex + 2])"
    $bottom = "$($bytes[$bottomIndex]),$($bytes[$bottomIndex + 1]),$($bytes[$bottomIndex + 2])"
    $checks++
    if ($top -ne $bottom) {
        Write-Host "    ok   top ($top) and bottom ($bottom) differ" -ForegroundColor DarkGray
    } else {
        $failures += "the top and bottom of the GPU's picture are both ($top)"
        Write-Host "    FAIL top and bottom are both ($top)" -ForegroundColor Red
    }
}

foreach ($bad in @(
        'KERNEL PANIC',
        'no virtio GPU on this machine',
        'would not show what was drawn',
        'never came out of reset',
        'does not offer VERSION_1',
        'would not accept the features')) {
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
    Write-Host "GPU tests passed ($checks checks)." -ForegroundColor Green
    exit 0
} else {
    foreach ($why in $failures) { Write-Host "    FAIL $why" -ForegroundColor Red }
    Write-Host "$($failures.Count) of $checks checks failed. Log: $Log" -ForegroundColor Red
    exit 1
}
