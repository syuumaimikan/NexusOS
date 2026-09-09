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

$InstallElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-install"
$StagedInstall = Publish-Program -Elf $InstallElf -ProgramDir $ProgramDir -Name 'inst.elf'
$installSize = [math]::Round((Get-Item $StagedInstall).Length / 1KB, 1)

$IdleElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-idle"
$StagedIdle = Publish-Program -Elf $IdleElf -ProgramDir $ProgramDir -Name 'idle.elf'
$idleSize = [math]::Round((Get-Item $StagedIdle).Length / 1KB, 1)

# A package, built on this machine by the tool that makes them and read on the
# other side by the program that installs them. Both use `shared/nexus-pkg`, so
# the format has exactly one implementation: a packer with its own idea of the
# layout is a packer that agrees with the installer until somebody changes one
# of them.
Write-Host '==> Packing a package' -ForegroundColor Cyan
Push-Location $RepoRoot
try {
    & cargo build --offline -q -p nexus-pack
    if ($LASTEXITCODE -ne 0) { throw 'could not build nexus-pack' }
} finally { Pop-Location }

$PackExe = Join-Path $RepoRoot 'target\debug\nexus-pack.exe'
$PackageDir = Join-Path $BuildDir 'package'
New-Item -ItemType Directory -Force -Path $PackageDir | Out-Null

# What goes inside. Written here rather than committed, because the interesting
# part is the format and the install, not the contents -- and a file generated
# at build time cannot drift out of step with what the test expects to read.
#
# Written without a byte-order mark. `Set-Content -Encoding utf8` on Windows
# PowerShell puts one at the front, and a package carries exactly the bytes it
# was given -- so the file installed on the other side began with three bytes
# nobody expected and the program reading it back saw a string that did not
# start where it should. The package was right; the source of it was not.
$utf8 = New-Object System.Text.UTF8Encoding $false
$greeting = Join-Path $PackageDir 'hello.txt'
[System.IO.File]::WriteAllText($greeting, @'
installed from a package, verified twice: once before anything was written,
and once after it had been read back off the disk.
'@, $utf8)
$notes = Join-Path $PackageDir 'notes.txt'
[System.IO.File]::WriteAllText($notes, @'
A second file, so that the entry table has to be walked rather than guessed.
'@, $utf8)

$Package = Join-Path $ProgramDir 'demo.nex'
$SigningKey = Join-Path $RepoRoot 'keys\development.key'
& $PackExe $Package $SigningKey 'demo' '1.0.0' "demo/hello.txt=$greeting" "demo/notes.txt=$notes"
if ($LASTEXITCODE -ne 0) { throw 'could not pack the package' }
$packageSize = [math]::Round((Get-Item $Package).Length / 1KB, 1)

# And a second package that is the first one with a byte changed after it was
# signed. Nothing about it is special: it is what a package looks like when
# somebody with no key alters it in transit, and the machine has to refuse it.
# A system that only ever sees packages that are correct is a system whose
# checking has never been exercised.
$Tampered = Join-Path $ProgramDir 'bad.nex'
$bytes = [System.IO.File]::ReadAllBytes($Package)
# The last byte, which is inside the payload rather than the header -- so what
# the machine notices is the content of a file, not a field it parses.
$bytes[$bytes.Length - 1] = $bytes[$bytes.Length - 1] -bxor 0x01
[System.IO.File]::WriteAllBytes($Tampered, $bytes)

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
Write-Host "  installer  : $installSize KiB  -> BIN\INST.ELF on the disk"
Write-Host "  package    : $packageSize KiB  -> PKG\DEMO.NEX on the disk (signed)"
Write-Host "  tampered   : one byte changed after signing -> PKG\BAD.NEX on the disk"
Write-Host "  idle       : $idleSize KiB  -> BIN\IDLE.ELF on the disk"
Write-Host "  ESP tree   : $EspDir"
Write-Host ''
Write-Host 'Run it with: .\scripts\run.ps1'
