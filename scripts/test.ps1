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
      4. fault-handling tests, which provoke real CPU exceptions and check that
         each is reported rather than resetting the machine

.PARAMETER SkipFaults
    Skip layer 4, which is the slowest because it boots QEMU three times.
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
    $output = (Get-Content $log -Raw) -replace "`0", ''

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
        'early initialisation complete',
        '[idle] uptime'
    )
    $missing = @($markers | Where-Object { -not $output.Contains($_) })
    if ($missing.Count -gt 0) {
        throw "serial log is missing: $($missing -join ', ')"
    }

    # A boot must not produce an exception report or a second bootloader banner.
    if ($output.Contains('EXCEPTION')) { throw 'an unexpected exception was reported' }
    if ($output.Contains('KERNEL PANIC')) { throw 'the kernel panicked' }
    $banners = ([regex]::Matches($output, 'NexusOS bootloader')).Count
    if ($banners -gt 1) { throw "the machine reset ($banners boots seen)" }

    Write-Host "    $($markers.Count) boot markers present" -ForegroundColor DarkGray
}

if (-not $SkipFaults) {
    Invoke-Step 'fault handling' {
        Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'test-faults.ps1')) 'fault-handling tests'
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
