<#
.SYNOPSIS
    Runs the full NexusOS test suite.

.DESCRIPTION
    Four layers, cheapest first, so a failure surfaces as early as possible:

      1. formatting and lints
      2. host unit tests (UEFI layouts, ELF parsing, memory-map normalization,
         descriptor encoding)
      3. a boot test: build, boot in QEMU, and require the serial log to contain
         every marker a healthy boot produces
      4. an input test, which sends real keystrokes through QEMU's monitor and
         checks what the kernel made of them
      5. injection tests, which break the kernel one way per build -- a real
         CPU exception, a broken TLB shootdown -- and check that
         each is reported rather than resetting the machine

.PARAMETER SkipFaults
    Skip layer 5, which is the slowest because it boots QEMU three times.
#>
[CmdletBinding()]
param(
    [switch]$SkipFaults
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$failures = @()

# Windows PowerShell turns a native program's stderr into error records, and
# under `$ErrorActionPreference = 'Stop'` those abort the script even when the
# program succeeded. cargo writes all of its progress to stderr, so every step
# below runs through here: output is printed, and success is judged by the exit
# code, which is the only thing that actually says whether the tool worked.
function Invoke-Native {
    param([string]$Exe, [string[]]$Arguments, [string]$What)
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $Exe @Arguments 2>&1 | ForEach-Object { Write-Host "    $_" }
        if ($LASTEXITCODE -ne 0) { throw "$What failed (exit $LASTEXITCODE)" }
    } finally {
        $ErrorActionPreference = $previous
    }
}

function Invoke-Step {
    param([string]$Name, [scriptblock]$Body)
    Write-Host ''
    Write-Host "==> $Name" -ForegroundColor Cyan
    try {
        & $Body
        Write-Host "    PASS $Name" -ForegroundColor Green
    } catch {
        Write-Host "    FAIL $Name : $_" -ForegroundColor Red
        $script:failures += $Name
    }
}

Invoke-Step 'formatting' {
    Push-Location $RepoRoot
    try {
        Invoke-Native 'cargo' @('+nightly', 'fmt', '--all', '--', '--check') 'cargo fmt'
    } finally { Pop-Location }
}

Invoke-Step 'clippy' {
    Push-Location $RepoRoot
    try {
        Invoke-Native 'cargo' @('+nightly', 'clippy', '-p', 'nexus-abi', '-p', 'nexus-boot', '-p', 'nexus-mm', '--lib', '--', '-D', 'warnings') 'clippy'
    } finally { Pop-Location }
}

Invoke-Step 'host unit tests' {
    Push-Location $RepoRoot
    try {
        Invoke-Native 'cargo' @('+nightly', 'test', '-p', 'nexus-abi', '-p', 'nexus-boot', '-p', 'nexus-mm', '--lib') 'unit tests'
    } finally { Pop-Location }
}

Invoke-Step 'boot test' {
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'build.ps1')) 'build'
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'run.ps1'), '-Headless', '-Timeout', '40') 'boot'

    $log = Join-Path $BuildDir 'serial.log'
    if (-not (Test-Path $log)) { throw 'no serial output' }
    $output = (Get-Content $log -Raw -Encoding UTF8) -replace "`0", ''

    # Every marker a healthy boot must produce, in the order it produces them.
    $markers = @(
        'NexusOS bootloader',
        'exited boot services',
        'Nexus Kernel v',
        'handoff revision 1',
        'physical memory regions',
        'GDT, TSS and IDT installed',
        'PIT running at 1000 Hz',
        'breakpoint at',
        'timer is live',
        'frame allocator:',
        'frame allocator verified',
        'identity map torn down',
        'heap verified',
        'ACPI 2.0, root table XSDT',
        '4 processors (4 enabled)',
        'local APIC 0 version',
        'ticking at 1000 Hz on vector 48',
        'legacy PIC masked, PIT stopped, LINT0 disconnected',
        '3 of 3 additional processors started',
        '4 processors online',
        'scheduler started',
        'joined the scheduler as thread',
        'threads ran to completion',
        'work was spread across',
        'preemption verified',
        'TLB shootdown verified',
        'TLB shootdowns broadcast',
        'syscall entry at',
        'hello from ring 3',
        'callee-saved registers survived',
        'thread exited through the system-call boundary',
        'interrupts taken from ring 3',
        'early initialisation complete',
        'boot thread retiring',
        'display adopted',
        'display thread',
        'I/O APIC 0 version',
        'keyboard on IRQ 1',
        'input thread',
        'glyphs available, including CJK',
        'interface language en-US, 2 available',
        '[mon ]'
    )
    $missing = @($markers | Where-Object { -not $output.Contains($_) })
    if ($missing.Count -gt 0) {
        throw "serial log is missing: $($missing -join ', ')"
    }

    # A boot must not produce an exception report or a second bootloader banner.
    if ($output.Contains('EXCEPTION')) { throw 'an unexpected exception was reported' }
    if ($output.Contains('KERNEL PANIC')) { throw 'the kernel panicked' }
    # Any self-test that judged itself says so; a marker being present only
    # means the line was printed, not that what it reported was right.
    if ($output.Contains('[test] FAILED')) {
        $failed = ([regex]::Matches($output, '\[test\] FAILED[^
]*') | ForEach-Object { $_.Value }) -join '; '
        throw "a kernel self-test failed: $failed"
    }
    # Every processor the firmware reported must be scheduling, not just alive.
    if (-not ($output -match 'work was spread across (\d+) of (\d+) processors')) {
        throw 'the workers never reported which processors they ran on'
    }
    if ([int]$Matches[2] -gt 1 -and [int]$Matches[1] -lt 2) {
        throw "work ran on only $($Matches[1]) of $($Matches[2]) processors"
    }
    # Ring 3 has to have actually been ring 3. System calls alone would not say
    # so -- `syscall` is legal from ring 0 -- but an interrupt whose saved code
    # selector has privilege 3 could only have come from user mode.
    if (-not ($output -match '(\d+) interrupts taken from ring 3')) {
        throw 'the kernel never reported interrupts taken from ring 3'
    }
    if ([int]$Matches[1] -lt 1) {
        throw 'no interrupt ever arrived from ring 3, so nothing ran at user privilege'
    }
    # A user program that reported a failure across the boundary is a failure.
    if ($output -match '\[user\][^
]*FAILED[^
]*') {
        throw "the user program reported: $($Matches[0])"
    }
    $banners = ([regex]::Matches($output, 'NexusOS bootloader')).Count
    if ($banners -gt 1) { throw "the machine reset ($banners boots seen)" }

    Write-Host "    $($markers.Count) boot markers present" -ForegroundColor DarkGray
}

Invoke-Step 'input' {
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'test-input.ps1')) 'input tests'
}

if (-not $SkipFaults) {
    Invoke-Step 'injection' {
        Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'test-faults.ps1')) 'injection tests'
    }
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'All NexusOS tests passed.' -ForegroundColor Green
    exit 0
} else {
    Write-Host "Failed: $($failures -join ', ')" -ForegroundColor Red
    exit 1
}
