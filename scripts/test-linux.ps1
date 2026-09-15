<#
.SYNOPSIS
    Boots the machine and checks that a program built for Linux can use files.

.DESCRIPTION
    Three programs run at every boot, started by `init` through the ordinary
    spawn service with `linux:` in front of the name. The third is the one this
    script is about: it creates a file, writes to it, closes it, opens it again,
    stats it, reads it back and compares the bytes; then it lists a directory,
    asks where it is, asks for randomness, and looks for `AT_RANDOM` in its own
    auxiliary vector.

    Every one of those steps exits with its own number when what came back is
    wrong, so "it exited 0" is a real claim and not a program that printed
    something. And the last check is outside the machine altogether: what the
    program wrote has to be findable in the host's copy of the disk.

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
$Log = Join-Path $BuildDir 'linux-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-linux.fd'
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
            # Either outcome ends the wait: a failure is a result, and sitting
            # here for the whole timeout to find out about one wastes five
            # minutes per run.
            if ($sofar -match 'a Linux program that wrote a file and read it back') {
                $done = $true
                break
            }
        }
    }
    if (-not $done) { throw 'the Linux programs never ran' }
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
        # All three, so a failure says which kind of call stopped working.
        'init: a Linux program ran and exited through the translation',
        'init: a Linux program that asked for memory ran and exited through the translation',
        'init: a Linux program that wrote a file and read it back ran and exited through the translation'
    )) {
    $checks++
    if ($output.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "never said '$expected'"
        Write-Host "    FAIL $expected" -ForegroundColor Red
    }
}

# An exit status is the program saying which step failed. Named here so a
# failure reads as the call rather than as a number.
$reasons = @{
    10 = 'mmap'
    11 = 'openat for writing'
    12 = 'the descriptor openat returned'
    13 = 'write'
    14 = 'close'
    15 = 'openat for reading'
    16 = 'fstat'
    17 = 'the size fstat reported'
    18 = 'read'
    19 = 'the bytes read back'
    20 = 'lseek to the end'
    21 = 'getcwd'
    22 = 'getrandom'
    23 = 'opening the root directory'
    24 = 'getdents64'
    25 = 'an empty root directory'
    26 = 'AT_RANDOM in the auxiliary vector'
}
$stopped = [regex]::Match(
    $output, 'a Linux program that wrote a file and read it back exited with status (\d+)')
if ($stopped.Success) {
    $status = [int]$stopped.Groups[1].Value
    $why = if ($reasons.ContainsKey($status)) { $reasons[$status] } else { 'something unlisted' }
    $failures += "the program stopped at: $why (status $status)"
    Write-Host "    FAIL it stopped at $why" -ForegroundColor Red
}

# And the check from outside: the file it wrote is in the host's image. A layer
# that accepted every byte and kept none passes everything above.
$checks++
$image = Join-Path $BuildDir 'nexus-disk.img'
$needle = [System.Text.Encoding]::ASCII.GetBytes('a linux program wrote a file and read it back')
$found = $false
$stream = [System.IO.File]::OpenRead($image)
try {
    # From the start of the NexusFS partition; there is no point reading the
    # quarter-gigabyte of FAT32 in front of it. The number is where the second
    # partition begins, which make-disk.ps1 puts straight after the first.
    $stream.Position = 526336L * 512
    $overlap = $needle.Length - 1
    $chunk = 8MB
    $buffer = New-Object byte[] ($chunk + $overlap)
    $held = 0
    while (-not $found) {
        $got = $stream.Read($buffer, $held, $chunk)
        if ($got -le 0) { break }
        $usable = $held + $got
        $last = $usable - $needle.Length
        $index = 0
        while ($index -le $last) {
            $index = [Array]::IndexOf($buffer, $needle[0], $index, $last - $index + 1)
            if ($index -lt 0) { break }
            $match = $true
            for ($offset = 1; $offset -lt $needle.Length; $offset++) {
                if ($buffer[$index + $offset] -ne $needle[$offset]) { $match = $false; break }
            }
            if ($match) { $found = $true; break }
            $index++
        }
        # Carry the tail forward, so a match lying across a chunk boundary is
        # still whole in the buffer next time round.
        $held = [math]::Min($overlap, $usable)
        [Array]::Copy($buffer, $usable - $held, $buffer, 0, $held)
    }
} finally {
    $stream.Close()
}
if ($found) {
    Write-Host "    ok   what it wrote is in the host's disk image" -ForegroundColor DarkGray
} else {
    $failures += "what the program wrote is not in $image"
    Write-Host "    FAIL what it wrote is not in the host's image" -ForegroundColor Red
}

foreach ($bad in @('KERNEL PANIC', 'is not translated yet')) {
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
    Write-Host "Linux translation tests passed ($checks checks)." -ForegroundColor Green
    exit 0
} else {
    foreach ($why in $failures) { Write-Host "    FAIL $why" -ForegroundColor Red }
    Write-Host "$($failures.Count) of $checks checks failed. Log: $Log" -ForegroundColor Red
    exit 1
}
