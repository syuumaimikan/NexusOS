<#
.SYNOPSIS
    Boots NexusOS twice on the same disk and checks that the second boot found
    what the first one wrote.

.DESCRIPTION
    Every other test runs one boot, and one boot cannot tell a filesystem from
    a very convincing pretence at one. Writing a file and reading it back in the
    same boot proves the code agrees with itself; it does not prove anything
    reached the disk, because a filesystem that kept everything in memory would
    pass exactly that test.

    So this one boots twice without rebuilding the image in between. The kernel
    keeps a count of boots in `/system/boots` and a line per boot in
    `/system/boot.log`. The first boot of a fresh disk has to format it and
    report boot 1; the second has to *mount* it -- not format it -- and report
    boot 2 with two lines in the log. The only place that 1 can have come from
    is the platter.

    The image is remade at the start, so the run always begins from an empty
    partition and the numbers mean the same thing every time.

.PARAMETER Timeout
    Seconds to let each guest run.
#>
[CmdletBinding()]
param(
    [int]$Timeout = 45
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw "No staged ESP at $EspDir. Run .\scripts\build.ps1 first."
}

# A fresh disk, so the first boot below really is the first boot. Remaking it
# here rather than trusting whatever the last run left behind is what makes the
# expected numbers 1 and 2 rather than "one more than last time".
#
# `-ProgramDir` and not `-SourceDir`, exactly as the build script does it. The
# difference is whether the image gets a copy of the EFI system partition, and
# an image that has one is an image the firmware can boot -- which would leave
# the machine with two bootable disks and let it choose. It chose wrong once
# already, and every test after that one booted a kernel that was not the one
# under test. This disk carries programs and NexusFS and nothing to start from.
$Disk = Get-NexusDiskImage -BuildDir $BuildDir
$ProgramDir = Join-Path $BuildDir 'programs'
Write-Host '==> Making a fresh disk' -ForegroundColor Cyan
& powershell -NoProfile -ExecutionPolicy Bypass `
    -File (Join-Path $PSScriptRoot 'make-disk.ps1') -Output $Disk -ProgramDir $ProgramDir | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'could not make the disk image' }

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source

$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}

# One boot, returning what it said on the serial line.
function Invoke-Boot {
    param([Parameter(Mandatory = $true)][int]$Number)

    $log = Join-Path $BuildDir "persistence-$Number.log"
    if (Test-Path $log) { Remove-Item $log -Force }

    $vars = Join-Path $BuildDir "vars-persistence-$Number.fd"
    Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $vars -Force

    $qemuArgs = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
        -FirmwareCode $FirmwareCode -FirmwareVars $vars -SerialLog $log -Headless

    Write-Host "==> Boot $Number" -ForegroundColor Cyan
    $process = Start-Process -FilePath $QemuExe.Source -ArgumentList $qemuArgs -PassThru -NoNewWindow
    try {
        for ($waited = 0; $waited -lt $Timeout; $waited++) {
            Start-Sleep -Seconds 1
            if ($process.HasExited) { break }
            if (Test-Path $log) {
                $sofar = (Get-Content $log -Raw -Encoding UTF8) -replace "`0", ''
                # Wait for what this test actually reads, not for the kernel
                # to finish starting. `init` does its filesystem work well
                # after the boot thread retires, and stopping at that marker
                # cut the guest off in the middle of it -- which reads as a
                # filesystem that lost a file rather than as a test that did
                # not wait.
                if ($sofar -and ($sofar -match 'init: (made a directory and a file|read what a previous boot wrote)')) {
                    break
                }
            }
        }
    } finally {
        if (-not $process.HasExited) {
            try { $process.Kill() } catch { }
        }
        $process.WaitForExit(5000) | Out-Null
    }

    if (-not (Test-Path $log)) { throw "boot $Number produced no serial output" }
    return (Get-Content $log -Raw -Encoding UTF8) -replace "`0", ''
}

$failures = @()

# What each boot has to say for itself: whether it made the filesystem or found
# one, which boot it thinks this is, and how many lines the log has.
function Read-Report {
    param(
        [Parameter(Mandatory = $true)][string]$Output,
        [Parameter(Mandatory = $true)][int]$Number
    )

    if ($Output.Contains('KERNEL PANIC')) { throw "boot $Number panicked" }
    if ($Output.Contains('[test] FAILED')) {
        $failed = ([regex]::Matches($Output, '\[test\] FAILED[^\r\n]*') |
            ForEach-Object { $_.Value }) -join '; '
        throw "boot ${Number}: $failed"
    }
    if (-not ($Output -match 'NexusFS (made|mounted) at sector \d+: .*boot (\d+)')) {
        throw "boot $Number never reported the state of its filesystem"
    }
    $state = $Matches[1]
    $boots = [int]$Matches[2]

    if (-not ($Output -match 'the boot log has (\d+) lines, ending "boot (\d+) at')) {
        throw "boot $Number never reported its boot log"
    }
    $lines = [int]$Matches[1]
    $lastLog = [int]$Matches[2]

    # And what the user program made of it. The kernel writing its own file
    # proves the filesystem persists; this proves a *program* can, which is the
    # part that goes through the system-call boundary and the handle table.
    $userState = ''
    if ($Output -match 'init: made a directory and a file') { $userState = 'made' }
    elseif ($Output -match 'init: read what a previous boot wrote') { $userState = 'read' }
    else { throw "boot $Number : init never reported what it did with the filesystem" }

    return [pscustomobject]@{
        State     = $state
        Boots     = $boots
        Lines     = $lines
        LastLog   = $lastLog
        UserState = $userState
    }
}

$first = Read-Report -Output (Invoke-Boot -Number 1) -Number 1
if ($first.State -ne 'made') {
    $failures += "the first boot of an empty partition $($first.State) a filesystem instead of making one"
} else {
    Write-Host '    ok   the first boot found no filesystem and made one' -ForegroundColor DarkGray
}
if ($first.Boots -ne 1) { $failures += "the first boot called itself boot $($first.Boots)" }
if ($first.Lines -ne 1) { $failures += "the first boot's log has $($first.Lines) lines" }
if ($first.UserState -ne 'made') {
    $failures += "on the first boot init found a file it should have had to create"
}

$second = Read-Report -Output (Invoke-Boot -Number 2) -Number 2
if ($second.State -ne 'mounted') {
    $failures += "the second boot $($second.State) the filesystem instead of mounting the one already there"
} else {
    Write-Host '    ok   the second boot mounted the filesystem the first one made' -ForegroundColor DarkGray
}
if ($second.Boots -ne 2) {
    $failures += "the second boot called itself boot $($second.Boots), so the count did not survive"
} else {
    Write-Host '    ok   the count of boots came off the disk as 1 and went back as 2' -ForegroundColor DarkGray
}
if ($second.Lines -ne 2) {
    $failures += "the second boot's log has $($second.Lines) lines, so the first boot's line was lost"
} else {
    Write-Host "    ok   the log kept both lines, the last saying boot $($second.LastLog)" -ForegroundColor DarkGray
}
if ($second.LastLog -ne 2) {
    $failures += "the log's last line says boot $($second.LastLog)"
}
if ($second.UserState -ne 'read') {
    $failures += 'the second boot did not find the file the user program wrote on the first'
} else {
    Write-Host '    ok   a user program read back the file it wrote on the previous boot' -ForegroundColor DarkGray
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'The filesystem outlived the machine.' -ForegroundColor Green
    exit 0
} else {
    foreach ($failure in $failures) {
        Write-Host "    FAIL $failure" -ForegroundColor Red
    }
    Write-Host "Serial logs: $BuildDir\persistence-1.log, $BuildDir\persistence-2.log"
    exit 1
}
