<#
.SYNOPSIS
    Boots NexusOS in QEMU.

.DESCRIPTION
    Launches QEMU with UEFI firmware and the staged EFI System Partition from
    scripts/build.ps1. Serial output is the kernel's primary diagnostic
    channel and is always captured to build/serial.log.

.PARAMETER Headless
    Run without a display and exit after -Timeout seconds. Used by the
    automated boot test.

.PARAMETER Timeout
    Seconds to run before shutting down, in headless mode. Default 20.

.PARAMETER Memory
    Guest RAM. Default 1G.

.PARAMETER Release
    Use the release build outputs.

.PARAMETER Gdb
    Start with a GDB stub on :1234 and wait for a debugger to attach.
#>
[CmdletBinding()]
param(
    [switch]$Headless,
    [int]$Timeout = 20,
    [string]$Memory = '1G',
    [switch]$Release,
    [switch]$Gdb
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'
$SerialLog = Join-Path $BuildDir 'serial.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw "No staged ESP at $EspDir. Run .\scripts\build.ps1 first."
}

# Locate QEMU and its bundled edk2 firmware.
$QemuExe = (Get-Command qemu-system-x86_64 -ErrorAction SilentlyContinue)
if ($null -eq $QemuExe) {
    throw 'qemu-system-x86_64 is not on PATH.'
}
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $QemuDir 'share\edk2-x86_64-code.fd'
$FirmwareVarsTemplate = Join-Path $QemuDir 'share\edk2-i386-vars.fd'

foreach ($firmware in @($FirmwareCode, $FirmwareVarsTemplate)) {
    if (-not (Test-Path $firmware)) { throw "UEFI firmware not found: $firmware" }
}

New-Item -ItemType Directory -Force -Path $BuildDir | Out-Null

# QEMU splits -drive options on commas and does not accept quoting inside them,
# so a firmware path containing spaces (the default "C:\Program Files\qemu")
# cannot be passed through. Stage both firmware blobs in the build directory,
# whose path we control.
$FirmwareCodeLocal = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCodeLocal)) {
    Copy-Item $FirmwareCode $FirmwareCodeLocal -Force
}

# The variable store must be writable, so it gets its own copy. It is kept
# across runs so that firmware boot entries persist.
$FirmwareVars = Join-Path $BuildDir 'edk2-vars.fd'
if (-not (Test-Path $FirmwareVars)) {
    Copy-Item $FirmwareVarsTemplate $FirmwareVars -Force
}

if (Test-Path $SerialLog) { Remove-Item $SerialLog -Force }

$QemuArgs = @(
    '-machine', 'q35',
    '-cpu', 'qemu64,+pdpe1gb',
    '-smp', '4',
    '-m', $Memory,

    # UEFI firmware: read-only code plus a writable variable store.
    '-drive', "if=pflash,format=raw,unit=0,readonly=on,file=$FirmwareCodeLocal",
    '-drive', "if=pflash,format=raw,unit=1,file=$FirmwareVars",

    # Present build/esp as a FAT filesystem. QEMU's virtual FAT layer means
    # there is no image to rebuild between runs: edit, build, boot.
    '-drive', "format=raw,file=fat:rw:$EspDir",

    # Serial is the kernel console.
    '-serial', "file:$SerialLog",

    # A triple fault should stop the machine with a diagnosable state rather
    # than silently rebooting into another boot attempt.
    '-no-reboot',
    '-d', 'guest_errors',
    '-D', (Join-Path $BuildDir 'qemu.log')
)

if ($Headless) {
    $QemuArgs += @('-display', 'none')
} else {
    $QemuArgs += @('-display', 'gtk')
}

if ($Gdb) {
    $QemuArgs += @('-s', '-S')
    Write-Host 'GDB stub listening on localhost:1234; QEMU is paused.' -ForegroundColor Yellow
}

Write-Host "==> Booting NexusOS in QEMU (serial -> $SerialLog)" -ForegroundColor Cyan

if ($Headless) {
    $process = Start-Process -FilePath $QemuExe.Source -ArgumentList $QemuArgs -PassThru -NoNewWindow
    $exited = $process.WaitForExit($Timeout * 1000)
    if (-not $exited) {
        Write-Host "==> Timeout after $Timeout s; stopping QEMU" -ForegroundColor Yellow
        try { $process.Kill() } catch { }
        $process.WaitForExit(5000) | Out-Null
    }
} else {
    & $QemuExe.Source @QemuArgs
}

Write-Host ''
if (Test-Path $SerialLog) {
    Write-Host "==> Serial output ($SerialLog)" -ForegroundColor Cyan
    Get-Content $SerialLog
} else {
    Write-Host 'No serial output was produced.' -ForegroundColor Red
}
