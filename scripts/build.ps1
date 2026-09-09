<#
.SYNOPSIS
    Builds NexusOS and stages a bootable EFI System Partition tree.

.DESCRIPTION
    Compiles the Nexus Bootloader for x86_64-unknown-uefi and the Nexus Kernel
    for the custom x86_64-nexus target, then lays them out under build/esp in
    the arrangement UEFI firmware expects:

        EFI/BOOT/BOOTX64.EFI   the bootloader, at the removable-media path
        nexus/kernel.elf       the kernel image the bootloader looks for

.PARAMETER Release
    Build with optimisations (cargo --release).

.PARAMETER Clean
    Remove build outputs before building.
#>
[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$Clean
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'stage.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'

if ($Clean) {
    Write-Host '==> Cleaning' -ForegroundColor Cyan
    if (Test-Path $BuildDir) { Remove-Item -Recurse -Force $BuildDir }
    Push-Location $RepoRoot
    try { cargo clean } finally { Pop-Location }
}

if ($Release) {
    $Profile = 'release'
    $ProfileArgs = @('--release')
} else {
    $Profile = 'debug'
    $ProfileArgs = @()
}

Push-Location $RepoRoot
try {
    Write-Host "==> Building nexus-boot (x86_64-unknown-uefi, $Profile)" -ForegroundColor Cyan
    # The `bootloader` and `kernel` aliases in .cargo/config.toml carry the
    # target and build-std flags each component needs.
    $bootArgs = @('+nightly', 'bootloader') + $ProfileArgs
    & cargo @bootArgs
    if ($LASTEXITCODE -ne 0) { throw "bootloader build failed (exit $LASTEXITCODE)" }

    Write-Host "==> Building nexus-kernel (x86_64-nexus, $Profile)" -ForegroundColor Cyan
    $kernelArgs = @('+nightly', 'kernel') + $ProfileArgs
    & cargo @kernelArgs
    if ($LASTEXITCODE -ne 0) { throw "kernel build failed (exit $LASTEXITCODE)" }
} finally {
    Pop-Location
}

$BootEfi = Join-Path $RepoRoot "target\x86_64-unknown-uefi\$Profile\nexus-boot.efi"
$KernelElf = Join-Path $RepoRoot "target\x86_64-nexus\$Profile\nexus-kernel"

Write-Host '==> Staging EFI System Partition tree' -ForegroundColor Cyan
$staged = Publish-Esp -BootEfi $BootEfi -KernelElf $KernelElf -EspDir $EspDir

$bootSize = $staged.BootSize
$kernelSize = $staged.KernelSize
$unstrippedSize = $staged.UnstrippedSize

# The programs the kernel loads from disk. Their own target -- the same
# custom-target machinery as the kernel, with the small code model, because they
# live in the low half of the address space rather than the top two gigabytes.
Push-Location $RepoRoot
try {
    Write-Host "==> Building user programs (x86_64-nexus-user, $Profile)" -ForegroundColor Cyan
    $userArgs = @('+nightly', 'user') + $ProfileArgs
    & cargo @userArgs
    if ($LASTEXITCODE -ne 0) { throw "user program build failed (exit $LASTEXITCODE)" }
} finally {
    Pop-Location
}

$ProgramDir = Join-Path $BuildDir 'programs'
$InitElf = Join-Path $RepoRoot "target\x86_64-nexus-user\$Profile\nexus-init"
$StagedInit = Publish-Program -Elf $InitElf -ProgramDir $ProgramDir -Name 'init.elf'
$initSize = [math]::Round((Get-Item $StagedInit).Length / 1KB, 1)

$HelloElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-hello"
$StagedHello = Publish-Program -Elf $HelloElf -ProgramDir $ProgramDir -Name 'hello.elf'
$helloSize = [math]::Round((Get-Item $StagedHello).Length / 1KB, 1)

$CompositorElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-compositor"
$StagedCompositor = Publish-Program -Elf $CompositorElf -ProgramDir $ProgramDir -Name 'comp.elf'
$compositorSize = [math]::Round((Get-Item $StagedCompositor).Length / 1KB, 1)

$ClientElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-client"
$StagedClient = Publish-Program -Elf $ClientElf -ProgramDir $ProgramDir -Name 'client.elf'
$clientSize = [math]::Round((Get-Item $StagedClient).Length / 1KB, 1)

$ShellElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-shell"
$StagedShell = Publish-Program -Elf $ShellElf -ProgramDir $ProgramDir -Name 'shell.elf'
$shellSize = [math]::Round((Get-Item $StagedShell).Length / 1KB, 1)

$IdleElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-idle"
$StagedIdle = Publish-Program -Elf $IdleElf -ProgramDir $ProgramDir -Name 'idle.elf'
$idleSize = [math]::Round((Get-Item $StagedIdle).Length / 1KB, 1)

# The disk the kernel drives. Data only, with nothing to boot from: a machine
# given two bootable disks leaves the firmware to choose between them, and it
# chose the one whose kernel was older. Made once and left alone, because the
# tests write to it and rebuilding would erase what a previous run proved.
#
# The bootable image is a different file, made by test-image.ps1, which is the
# only run that is about whether an image boots.
$DiskImage = Join-Path $BuildDir 'nexus-disk.img'
$stale = -not (Test-Path $DiskImage)
if (-not $stale) {
    $imageTime = (Get-Item $DiskImage).LastWriteTimeUtc
    $newest = Get-ChildItem -Path $ProgramDir -File |
        Sort-Object LastWriteTimeUtc -Descending |
        Select-Object -First 1
    if ($newest -and $newest.LastWriteTimeUtc -gt $imageTime) { $stale = $true }
}
if ($stale) {
    Write-Host '==> Making the disk image' -ForegroundColor Cyan
    & powershell -NoProfile -ExecutionPolicy Bypass `
        -File (Join-Path $PSScriptRoot 'make-disk.ps1') -Output $DiskImage -ProgramDir $ProgramDir
    if ($LASTEXITCODE -ne 0) { throw 'could not make the disk image' }
}

Write-Host ''
Write-Host 'NexusOS build complete' -ForegroundColor Green
Write-Host "  bootloader : $bootSize KiB  -> EFI\BOOT\BOOTX64.EFI"
Write-Host "  kernel     : $kernelSize KiB  -> nexus\kernel.elf  (from $unstrippedSize KiB with symbols)"
Write-Host "  init       : $initSize KiB  -> BIN\INIT.ELF on the disk"
Write-Host "  hello      : $helloSize KiB  -> BIN\HELLO.ELF on the disk"
Write-Host "  compositor : $compositorSize KiB  -> BIN\COMP.ELF on the disk"
Write-Host "  client     : $clientSize KiB  -> BIN\CLIENT.ELF on the disk"
Write-Host "  shell      : $shellSize KiB  -> BIN\SHELL.ELF on the disk"
Write-Host "  idle       : $idleSize KiB  -> BIN\IDLE.ELF on the disk"
Write-Host "  ESP tree   : $EspDir"
Write-Host ''
Write-Host 'Run it with: .\scripts\run.ps1'
