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

foreach ($artifact in @($BootEfi, $KernelElf)) {
    if (-not (Test-Path $artifact)) { throw "expected build output is missing: $artifact" }
}

Write-Host '==> Staging EFI System Partition tree' -ForegroundColor Cyan
New-Item -ItemType Directory -Force -Path (Join-Path $EspDir 'EFI\BOOT') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $EspDir 'nexus') | Out-Null

Copy-Item $BootEfi (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI') -Force

# Deploy a stripped kernel. Debug info is the overwhelming majority of the
# linked image and none of it is loadable, but the bootloader still has to read
# every byte off the ESP before it can parse the program headers -- which on a
# debug build means reading megabytes to load a hundred kilobytes. The
# unstripped image stays in target/ for debuggers and symbolisation.
$StagedKernel = Join-Path $EspDir 'nexus\kernel.elf'
$Sysroot = (& rustc +nightly --print sysroot).Trim()
$ObjCopy = Join-Path $Sysroot 'lib\rustlib\x86_64-pc-windows-msvc\bin\llvm-objcopy.exe'

if (Test-Path $ObjCopy) {
    & $ObjCopy --strip-debug $KernelElf $StagedKernel
    if ($LASTEXITCODE -ne 0) { throw "llvm-objcopy failed (exit $LASTEXITCODE)" }
} else {
    Write-Host "    llvm-objcopy not found; deploying an unstripped kernel" -ForegroundColor Yellow
    Copy-Item $KernelElf $StagedKernel -Force
}

$bootSize = [math]::Round((Get-Item $BootEfi).Length / 1KB, 1)
$kernelSize = [math]::Round((Get-Item $StagedKernel).Length / 1KB, 1)
$unstrippedSize = [math]::Round((Get-Item $KernelElf).Length / 1KB, 1)

Write-Host ''
Write-Host 'NexusOS build complete' -ForegroundColor Green
Write-Host "  bootloader : $bootSize KiB  -> EFI\BOOT\BOOTX64.EFI"
Write-Host "  kernel     : $kernelSize KiB  -> nexus\kernel.elf  (from $unstrippedSize KiB with symbols)"
Write-Host "  ESP tree   : $EspDir"
Write-Host ''
Write-Host 'Run it with: .\scripts\run.ps1'
