<#
.SYNOPSIS
    Breaks the kernel on purpose, one way per build, and checks it notices.

.DESCRIPTION
    Builds the kernel once per injection feature, boots each build in QEMU, and
    checks the serial log for the diagnostic that build should produce.

    This is the only honest way to test the things that only matter when
    something has gone wrong. An exception handler guards against a triple fault
    that silently reboots the machine, which is invisible unless a real fault is
    provoked. A self-test that looks for stale translations is worth no more
    than a comment unless it has been seen to fail when there are some.

    Each case asserts both that the expected output appeared and that the
    machine did not restart, which a reboot loop would reveal as a repeated
    banner.
#>
[CmdletBinding()]
param(
    [int]$Timeout = 180
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')

. (Join-Path $PSScriptRoot 'stage.ps1')

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
    },
    # Not a fault: this one checks that a *test* can fail. With the shootdown
    # reduced to a local invalidation, the other processors keep translations
    # the kernel has replaced, and the self-test that looks for exactly that
    # has to notice. If this case ever passes silently, the shootdown check in
    # the ordinary boot is decoration.
    @{
        Name = 'a shootdown that never leaves the processor'
        Feature = 'inject-no-shootdown'
        Processors = 4
        Expect = @(
            '[test] FAILED: processors',
            'kept a stale translation after the shootdown'
        )
    },
    # Also not a fault in the kernel: a fault the kernel is supposed to take.
    # The user program NexusOS starts at boot shows that ring 3 can be entered
    # and that the system-call boundary works. It cannot show that the boundary
    # keeps anything out -- a kernel that mapped itself readable from ring 3
    # would run it identically. This build reads kernel memory from ring 3, and
    # the protection is real only if that faults and says where it came from.
    @{
        Name = 'ring 3 reaching into kernel memory'
        Feature = 'inject-user-violation'
        Processors = 4
        Expect = @(
            'about to read kernel memory from ring 3',
            'EXCEPTION 14: page fault',
            'address    : 0xffffffff80000000',
            'origin   : user mode',
            'the system has been halted'
        )
        Reject = @(
            'FAILED: ring 3 read kernel memory'
        )
    },
    # And the same discipline for the address spaces. Two processes reporting
    # that nobody wrote into their memory means nothing unless the report can
    # come out the other way; this build hands them one page between them, which
    # is what having no separation would look like from inside a program.
    @{
        Name = 'two processes sharing one page'
        Feature = 'inject-shared-user-page'
        Processors = 4
        Expect = @(
            # The kernel side, which is a fact about the page tables and so is
            # the same every run.
            'FAILED: alpha and beta both map',
            'the same frame',
            # And the user side. Only one of the two has to notice: each
            # process sees corruption when the other's write lands between its
            # own write and its read, and which one that happens to is a race.
            # Requiring both to report would be requiring a coin to land twice.
            "FAILED: another process wrote into this one's memory"
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

    # Through the same staging the build script uses, so these runs boot the
    # same shape of image the build produces and differ only in the feature.
    Publish-Esp -BootEfi (Join-Path $RepoRoot 'target\x86_64-unknown-uefi\debug\nexus-boot.efi') `
        -KernelElf (Join-Path $RepoRoot 'target\x86_64-nexus\debug\nexus-kernel') `
        -EspDir $EspDir | Out-Null

    # A fresh variable store per case, so a previous run's boot entries cannot
    # change what the firmware does.
    $Vars = Join-Path $BuildDir "vars-$($case.Feature).fd"
    Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $Vars -Force
    $Log = Join-Path $BuildDir "fault-$($case.Feature).log"
    if (Test-Path $Log) { Remove-Item $Log -Force }

        $processors = if ($case.ContainsKey('Processors')) { $case.Processors } else { 1 }
    $QemuArgs = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
        -FirmwareCode $FirmwareCode -FirmwareVars $Vars -SerialLog $Log `
        -Processors $processors -Headless -StopOnFault

    # Stopped when the case has said what it had to say, rather than after a
    # fixed number of seconds. A fault that halts the machine says so in the
    # first second; one that has to wait for a user program to reach the thing
    # being injected can take a great deal longer on a busy host, and a fixed
    # wait turns that into a failure that has nothing to do with the fault.
    $process = Start-Process -FilePath $QemuExe.Source -ArgumentList $QemuArgs -PassThru -NoNewWindow
    for ($waited = 0; $waited -lt $Timeout; $waited++) {
        if ($process.WaitForExit(1000)) { break }
        if (Test-Path $Log) {
            $sofar = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''
            $all = $true
            foreach ($expected in $case.Expect) {
                if (-not $sofar.Contains($expected)) { $all = $false; break }
            }
            if ($all) { break }
        }
    }
    if (-not $process.HasExited) {
        try { $process.Kill() } catch { }
        $process.WaitForExit(5000) | Out-Null
    }

    if (-not (Test-Path $Log)) {
        Write-Host '    FAIL: no serial output' -ForegroundColor Red
        $failures++
        continue
    }

    $output = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''
    $caseFailed = $false

    foreach ($expected in $case.Expect) {
        if ($output.Contains($expected)) {
            Write-Host "    ok   $expected" -ForegroundColor DarkGray
        } else {
            Write-Host "    FAIL missing: $expected" -ForegroundColor Red
            $caseFailed = $true
        }
    }

    # Strings that must be absent. A case whose point is that something is
    # rejected cannot be checked by presence alone: the program says so itself
    # if it got through, and that line appearing is the failure.
    if ($case.ContainsKey('Reject')) {
        foreach ($forbidden in $case.Reject) {
            if ($output.Contains($forbidden)) {
                Write-Host "    FAIL present but must not be: $forbidden" -ForegroundColor Red
                $caseFailed = $true
            } else {
                Write-Host "    ok   absent: $forbidden" -ForegroundColor DarkGray
            }
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
Publish-EspKernel -KernelElf (Join-Path $RepoRoot 'target\x86_64-nexus\debug\nexus-kernel') `
    -EspDir $EspDir | Out-Null

Write-Host ''
if ($failures -eq 0) {
    Write-Host "All $($Cases.Count) injection tests passed." -ForegroundColor Green
    exit 0
} else {
    Write-Host "$failures of $($Cases.Count) injection tests failed." -ForegroundColor Red
    exit 1
}
