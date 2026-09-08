<#
.SYNOPSIS
    Boots NexusOS from the disk image alone and checks that it got there.

.DESCRIPTION
    Every other run hands QEMU a directory and lets it pretend to be a
    filesystem. That is the right trade for iteration and it means nothing else
    exercises the partition table, the boot sector, or the file allocation
    table this project writes -- the firmware never sees them, so a mistake in
    any of them is invisible until the day someone writes the image to a real
    disk.

    This run gives the machine nothing but the image. The firmware has to find
    the GPT, recognise the EFI system partition, read the FAT32 inside it, load
    the bootloader from `\EFI\BOOT\BOOTX64.EFI`, and the bootloader has to read
    the kernel out of the same filesystem. Anything wrong anywhere in that chain
    stops the boot.

.PARAMETER Timeout
    Seconds to let the guest run.
#>
[CmdletBinding()]
param(
    [int]$Timeout = 30
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'
$SerialLog = Join-Path $BuildDir 'image-boot.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw "No staged ESP at $EspDir. Run .\scripts\build.ps1 first."
}

# Built here rather than by the build script, and rebuilt whenever the staged
# tree is newer. This is the only run that boots it, so this is where it has to
# be current: an image holding yesterday's kernel would pass this test while
# saying nothing about today's.
$BootImage = Get-NexusBootImage -BuildDir $BuildDir
$stale = -not (Test-Path $BootImage)
if (-not $stale) {
    $imageTime = (Get-Item $BootImage).LastWriteTimeUtc
    $newest = Get-ChildItem -Path $EspDir -Recurse -File |
        Sort-Object LastWriteTimeUtc -Descending |
        Select-Object -First 1
    if ($newest -and $newest.LastWriteTimeUtc -gt $imageTime) { $stale = $true }
}
if ($stale) {
    Write-Host '==> Making the bootable image' -ForegroundColor Cyan
    & powershell -NoProfile -ExecutionPolicy Bypass `
        -File (Join-Path $PSScriptRoot 'make-disk.ps1') -Output $BootImage -SourceDir $EspDir
    if ($LASTEXITCODE -ne 0) { throw 'could not make the bootable image' }
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source

$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
# A fresh variable store, so a boot entry left by another run cannot be what
# makes this one work.
$FirmwareVars = Join-Path $BuildDir 'vars-image.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $SerialLog) { Remove-Item $SerialLog -Force }

$QemuArgs = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
    -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars -SerialLog $SerialLog `
    -Headless -BootFromImage

Write-Host '==> Booting from the disk image alone' -ForegroundColor Cyan
$process = Start-Process -FilePath $QemuExe.Source -ArgumentList $QemuArgs -PassThru -NoNewWindow

try {
    $ready = $false
    for ($waited = 0; $waited -lt $Timeout; $waited++) {
        Start-Sleep -Seconds 1
        if ($process.HasExited) { break }
        if (Test-Path $SerialLog) {
            $sofar = (Get-Content $SerialLog -Raw -Encoding UTF8) -replace "`0", ''
            if ($sofar.Contains('early initialisation complete')) { $ready = $true; break }
        }
    }
    if (-not $ready) { Write-Host '    (the guest did not report finishing bring-up)' -ForegroundColor DarkGray }
} finally {
    if (-not $process.HasExited) {
        try { $process.Kill() } catch { }
    }
    $process.WaitForExit(5000) | Out-Null
}

if (-not (Test-Path $SerialLog)) { throw 'no serial output' }
$output = (Get-Content $SerialLog -Raw -Encoding UTF8) -replace "`0", ''

$failures = @()

# The firmware got as far as the bootloader, which means it read the GPT, found
# the EFI system partition and understood the filesystem in it.
if ($output.Contains('NexusOS bootloader')) {
    Write-Host '    ok   the firmware found and loaded the bootloader' -ForegroundColor DarkGray
} else {
    $failures += 'the firmware never loaded the bootloader from the image'
}

# And the bootloader read the kernel out of the same filesystem.
if ($output -match 'read kernel image, (\d+) bytes') {
    Write-Host "    ok   the bootloader read $($Matches[1]) bytes of kernel" -ForegroundColor DarkGray
} else {
    $failures += 'the bootloader never read the kernel from the image'
}

if ($output.Contains('early initialisation complete')) {
    Write-Host '    ok   the kernel finished bring-up' -ForegroundColor DarkGray
} else {
    $failures += 'the kernel did not finish bring-up'
}

if ($output.Contains('KERNEL PANIC')) { $failures += 'the kernel panicked' }
if ($output.Contains('EXCEPTION')) { $failures += 'an exception was reported' }

$banners = ([regex]::Matches($output, 'NexusOS bootloader')).Count
if ($banners -gt 1) { $failures += "the machine reset ($banners boots seen)" }

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'Booted from the image.' -ForegroundColor Green
    exit 0
} else {
    foreach ($failure in $failures) {
        Write-Host "    FAIL $failure" -ForegroundColor Red
    }
    Write-Host "Serial log: $SerialLog"
    exit 1
}
