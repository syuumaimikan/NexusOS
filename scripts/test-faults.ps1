<#
.SYNOPSIS
    Verifies that the Nexus Kernel reports CPU exceptions instead of resetting.

.DESCRIPTION
    Builds the kernel once per fault-injection feature, boots each build in
    QEMU, and checks the serial log for the expected diagnostic.

    This is the only way to test an exception handler honestly: the failure it
    guards against — a triple fault that silently reboots the machine — is
    invisible unless a real fault is provoked and the output inspected.

    Each case asserts both that the right report appeared and that the machine
    did not restart, which a reboot loop would reveal as a repeated banner.
#>
[CmdletBinding()]
param(
    [int]$Timeout = 25
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'

# Each case: the cargo feature to build with, and the strings its serial log
# must contain for the test to pass.
$Cases = @(
    @{
        Name = 'page fault'
        Feature = 'inject-page-fault'
        Expect = @(
            'EXCEPTION 14: page fault',
            'address    : 0xffffa00000000000',
            'cause    : page not present',
            'access   : write',
            'origin   : kernel mode',
            'the system has been halted'
        )
    },
    @{
        Name = 'stack overflow into the guard page'
        Feature = 'inject-stack-overflow'
        Expect = @(
            'EXCEPTION 8: double fault',
            'running on the double-fault IST stack',
            'the system has been halted'
        )
    },
    @{
        Name = 'divide error'
        Feature = 'inject-divide-error'
        Expect = @(
            'EXCEPTION 0: divide error',
            'the system has been halted'
        )
    }
)

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source

New-Item -ItemType Directory -Force -Path $BuildDir | Out-Null
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}

$failures = 0

foreach ($case in $Cases) {
    Write-Host ''
    Write-Host "==> $($case.Name)  [--features $($case.Feature)]" -ForegroundColor Cyan

    Push-Location $RepoRoot
    try {
        & cargo +nightly kernel --features $case.Feature | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "kernel build failed for $($case.Feature)" }
    } finally {
        Pop-Location
    }

    New-Item -ItemType Directory -Force -Path (Join-Path $EspDir 'EFI\BOOT') | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $EspDir 'nexus') | Out-Null
    Copy-Item (Join-Path $RepoRoot 'target\x86_64-unknown-uefi\debug\nexus-boot.efi') `
        (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI') -Force
    Copy-Item (Join-Path $RepoRoot 'target\x86_64-nexus\debug\nexus-kernel') `
        (Join-Path $EspDir 'nexus\kernel.elf') -Force

    # A fresh variable store per case, so a previous run's boot entries cannot
    # change what the firmware does.
    $Vars = Join-Path $BuildDir "vars-$($case.Feature).fd"
    Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $Vars -Force
    $Log = Join-Path $BuildDir "fault-$($case.Feature).log"
    if (Test-Path $Log) { Remove-Item $Log -Force }

    $QemuArgs = @(
        '-machine', 'q35',
        '-cpu', 'qemu64',
        '-smp', '1',
        '-m', '1G',
        '-drive', "if=pflash,format=raw,unit=0,readonly=on,file=$FirmwareCode",
        '-drive', "if=pflash,format=raw,unit=1,file=$Vars",
        '-drive', "format=raw,file=fat:rw:$EspDir",
        '-serial', "file:$Log",
        '-display', 'none',
        # Without this a triple fault would reboot and look like a hang rather
        # than the specific failure it is.
        '-no-reboot',
        '-no-shutdown'
    )

    $process = Start-Process -FilePath $QemuExe.Source -ArgumentList $QemuArgs -PassThru -NoNewWindow
    if (-not $process.WaitForExit($Timeout * 1000)) {
        try { $process.Kill() } catch { }
        $process.WaitForExit(5000) | Out-Null
    }

    if (-not (Test-Path $Log)) {
        Write-Host '    FAIL: no serial output' -ForegroundColor Red
        $failures++
        continue
    }

    $output = (Get-Content $Log -Raw) -replace "`0", ''
    $caseFailed = $false

    foreach ($expected in $case.Expect) {
        if ($output.Contains($expected)) {
            Write-Host "    ok   $expected" -ForegroundColor DarkGray
        } else {
            Write-Host "    FAIL missing: $expected" -ForegroundColor Red
            $caseFailed = $true
        }
    }

    # A triple fault restarts the machine, which shows up as the bootloader
    # banner appearing more than once.
    $banners = ([regex]::Matches($output, 'NexusOS bootloader')).Count
    if ($banners -gt 1) {
        Write-Host "    FAIL the machine reset ($banners boots seen)" -ForegroundColor Red
        $caseFailed = $true
    } else {
        Write-Host '    ok   the machine did not reset' -ForegroundColor DarkGray
    }

    if ($caseFailed) { $failures++ } else {
        Write-Host "    PASS $($case.Name)" -ForegroundColor Green
    }
}

# Leave the tree holding an ordinary kernel, not a fault-injecting one.
Write-Host ''
Write-Host '==> Restoring the default kernel build' -ForegroundColor Cyan
Push-Location $RepoRoot
try { & cargo +nightly kernel | Out-Null } finally { Pop-Location }
Copy-Item (Join-Path $RepoRoot 'target\x86_64-nexus\debug\nexus-kernel') `
    (Join-Path $EspDir 'nexus\kernel.elf') -Force

Write-Host ''
if ($failures -eq 0) {
    Write-Host "All $($Cases.Count) fault-handling tests passed." -ForegroundColor Green
    exit 0
} else {
    Write-Host "$failures of $($Cases.Count) fault-handling tests failed." -ForegroundColor Red
    exit 1
}
