<#
.SYNOPSIS
    Boots a machine twice and checks that it updated itself once.

.DESCRIPTION
    The build puts three packages on the disk: `demo` 1.0.0, the same package at
    1.1.0, and a copy of the first with a byte changed after it was signed. What
    a machine should make of that is not obvious from any one of them, which is
    why all three are there:

      * the forged one must be refused, and named;
      * the older release must be recognised as superseded by the newer one on
        the same disk, rather than installed and then overwritten;
      * the newer one must be installed, and *recorded*.

    Then it boots the same disk again, and the machine must do nothing at all.
    That is the half that is easy to get wrong and impossible to see in one
    boot: an updater that reinstalls everything it finds looks exactly like one
    that works, until you watch it twice.

.PARAMETER Timeout
    How long to wait for each boot, in seconds.
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

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}

# A disk nothing has been installed onto. Made here rather than reused, because
# "has this machine already got the update" is a question about the disk and a
# previous run would have answered it.
$Disk = Join-Path $BuildDir 'nexus-update-disk.img'
$ProgramDir = Join-Path $BuildDir 'programs'
Write-Host '==> Making a disk with nothing installed on it' -ForegroundColor Cyan
& powershell -NoProfile -ExecutionPolicy Bypass `
    -File (Join-Path $PSScriptRoot 'make-disk.ps1') -Output $Disk -ProgramDir $ProgramDir `
    -SizeMiB (Get-NexusDiskSizes).SizeMiB -FatMiB (Get-NexusDiskSizes).FatMiB | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'could not make the disk' }

# One boot, returning what it said on the serial line. The standard disk path is
# what `Get-NexusQemuArgs` reads, so this one is swapped in by name and the
# original put back afterwards.
function Invoke-Boot {
    param([Parameter(Mandatory = $true)][int]$Number)

    $log = Join-Path $BuildDir "update-$Number.log"
    if (Test-Path $log) { Remove-Item $log -Force }
    $vars = Join-Path $BuildDir "vars-update-$Number.fd"
    Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $vars -Force

    $standard = Get-NexusDiskImage -BuildDir $BuildDir
    $saved = "$standard.update-saved"
    if (Test-Path $standard) { Move-Item $standard $saved -Force }
    Copy-Item $Disk $standard -Force

    $arguments = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
        -FirmwareCode $FirmwareCode -FirmwareVars $vars -SerialLog $log -Headless

    $process = Start-Process -FilePath $QemuExe.Source -ArgumentList $arguments -PassThru -NoNewWindow
    try {
        # Waited for rather than slept through: how long an emulated machine
        # takes to reach its updater is a property of the host, not of the code.
        $done = $false
        for ($waited = 0; $waited -lt $Timeout; $waited++) {
            Start-Sleep -Seconds 1
            if ($process.HasExited) { break }
            if (-not (Test-Path $log)) { continue }
            $sofar = (Get-Content $log -Raw -Encoding UTF8) -replace "`0", ''
            if ($sofar.Contains('init: the machine checked itself for updates')) {
                $done = $true
                break
            }
        }
        if (-not $done) { throw "boot $Number never finished checking for updates" }
        # And let the filesystem finish with what it wrote.
        Start-Sleep -Seconds 4
    } finally {
        if (-not $process.HasExited) {
            try { $process.Kill() } catch { }
        }
        $process.WaitForExit(5000) | Out-Null
        # What the run left on the disk is what the next boot has to read.
        Copy-Item $standard $Disk -Force
        if (Test-Path $saved) { Move-Item $saved $standard -Force }
    }

    return (Get-Content $log -Raw -Encoding UTF8) -replace "`0", ''
}

$failures = @()

Write-Host '==> Boot 1: a machine with nothing installed' -ForegroundColor Cyan
$first = Invoke-Boot -Number 1

foreach ($expected in @(
        'update: refused PKG/BAD.NEX',
        'update: demo 1.0.0 is superseded by another file on this disk',
        'update: demo 1.1.0 is new',
        'update: demo is now at 1.1.0',
        'update: installed 1 update(s)'
    )) {
    if ($first.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "boot 1 never reported: $expected"
    }
}
# The older release must not have been installed on the way to the newer one.
if ($first.Contains('update: demo is now at 1.0.0')) {
    $failures += 'the machine installed the release it had already superseded'
}

Write-Host '==> Boot 2: the same disk, already up to date' -ForegroundColor Cyan
$second = Invoke-Boot -Number 2

foreach ($expected in @(
        'update: 1 package(s) on record',
        'update: demo 1.1.0 is already current',
        'update: nothing to do'
    )) {
    if ($second.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "boot 2 never reported: $expected"
    }
}
if ($second -match 'update: installed [1-9]') {
    $failures += 'the machine installed something it already had'
}
# And the forged package has to be refused every time, not remembered as
# refused. A machine that stopped checking would pass the line above.
if (-not $second.Contains('update: refused PKG/BAD.NEX')) {
    $failures += 'the second boot did not check the forged package again'
}

foreach ($log in @($first, $second)) {
    if ($log.Contains('KERNEL PANIC')) { $failures += 'the kernel panicked' }
    if ($log.Contains('update: FAILED')) { $failures += 'the updater reported a failure' }
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'Update tests passed.' -ForegroundColor Green
    exit 0
} else {
    foreach ($failure in $failures) {
        Write-Host "    FAIL $failure" -ForegroundColor Red
    }
    exit 1
}
