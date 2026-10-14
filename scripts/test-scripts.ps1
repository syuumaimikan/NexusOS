<#
.SYNOPSIS
    Regression checks for program updates and bounded cleanup; no guest boots.
#>
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'maintenance.ps1')
$repo = Split-Path -Parent $PSScriptRoot
$fixture = Join-Path $repo ('build/script-check-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $fixture | Out-Null
function Require([bool]$Ok, [string]$Message) { if (-not $Ok) { throw $Message } }
function Region-Hash([string]$Image, [long]$Start, [int]$Count) {
    $stream = [IO.File]::OpenRead($Image)
    try {
        $bytes = New-Object byte[] $Count
        $stream.Position = $Start
        $done = 0
        while ($done -lt $Count) {
            $got = $stream.Read($bytes, $done, $Count - $done)
            if ($got -eq 0) { throw 'Truncated test image' }
            $done += $got
        }
        $sha = [Security.Cryptography.SHA256]::Create()
        try { return [BitConverter]::ToString($sha.ComputeHash($bytes)) } finally { $sha.Dispose() }
    } finally { $stream.Dispose() }
}
$busy = $null
try {
    . (Join-Path $PSScriptRoot 'qemu.ps1')
    $exitProcess = Start-NexusQemu -Executable (Get-Command powershell).Source `
        -Arguments @('-NoProfile', '-Command', 'Start-Sleep -Milliseconds 250; exit 0')
    try {
        Require ($exitProcess.WaitForExit(10000)) 'Process helper did not observe normal exit'
        Require ($null -ne $exitProcess.ExitCode -and $exitProcess.ExitCode -eq 0) 'Normal exit code was lost'
    } finally { $exitProcess.Dispose() }
    $failedProcess = Start-NexusQemu -Executable (Get-Command powershell).Source `
        -Arguments @('-NoProfile', '-Command', 'Start-Sleep -Milliseconds 250; exit 7')
    try {
        Require ($failedProcess.WaitForExit(10000)) 'Process helper did not observe failed exit'
        Require ($failedProcess.ExitCode -eq 7) 'Failure exit code was lost'
    } finally { $failedProcess.Dispose() }
    Write-Host 'PASS: Windows process helper retains normal and failed exit codes'
    $programs = Join-Path $fixture 'programs'
    New-Item -ItemType Directory -Path $programs | Out-Null
    Set-Content -LiteralPath (Join-Path $programs 'init.elf') -Value 'first program'
    New-Item -ItemType Directory -Path (Join-Path $programs 'NVFW') | Out-Null
    Set-Content -LiteralPath (Join-Path $programs 'NVFW/FMC.BIN') -Value 'nested firmware payload'
    $disk = Join-Path $fixture 'nexus-disk.img'
    & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'make-disk.ps1') -Output $disk -ProgramDir $programs -SizeMiB 96 -FatMiB 40
    Require ($LASTEXITCODE -eq 0) 'Initial test image failed'
    $imageBytes = [IO.File]::ReadAllBytes($disk)
    $imageText = [Text.Encoding]::ASCII.GetString($imageBytes)
    Require ($imageText.Contains('NVFW       ') -and $imageText.Contains('FMC     BIN') -and $imageText.Contains('nested firmware payload')) 'Nested program resources were omitted'
    $imageBytes = $null
    $imageText = $null
    $dataStart = 41MB
    $stream = [IO.File]::Open($disk, 'Open', 'ReadWrite', 'None')
    try {
        $stream.Position = $dataStart + 4096
        $sentinel = [Text.Encoding]::UTF8.GetBytes('persistent settings must survive program replacement')
        $stream.Write($sentinel, 0, $sentinel.Length)
    } finally { $stream.Dispose() }
    $protected = Region-Hash $disk $dataStart (55MB)
    $partition = Region-Hash $disk 1MB (40MB)
    $header = Region-Hash $disk 0 1MB
    Set-Content -LiteralPath (Join-Path $programs 'init.elf') -Value 'second program with different bytes'
    & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'make-disk.ps1') -Output $disk -ProgramDir $programs -SizeMiB 96 -FatMiB 40 -UpdatePrograms
    Require ($LASTEXITCODE -eq 0) 'Program refresh failed'
    Require ((Region-Hash $disk $dataStart (55MB)) -eq $protected) 'Program update changed NexusFS or backup GPT'
    Require ((Region-Hash $disk 0 1MB) -eq $header) 'Program update changed primary GPT'
    Require ((Region-Hash $disk 1MB (40MB)) -ne $partition) 'Program bytes did not change'
    $beforeRejected = Region-Hash $disk 0 (96MB)
    $ErrorActionPreference = 'Continue'
    try {
        & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'make-disk.ps1') -Output $disk -ProgramDir $programs -SizeMiB 96 -FatMiB 48 -UpdatePrograms 2> (Join-Path $fixture 'expected-error.log')
    } finally { $ErrorActionPreference = 'Stop' }
    Require ($LASTEXITCODE -ne 0) 'Different geometry was accepted'
    Require ((Region-Hash $disk 0 (96MB)) -eq $beforeRejected) 'Rejected geometry changed the disk'
    Write-Host 'PASS: FAT refresh changes programs, preserves all NexusFS/GPT bytes, rejects changed geometry'

    $cleanupRoot = Join-Path $fixture 'cleanup'
    New-Item -ItemType Directory -Path $cleanupRoot | Out-Null
    $runs = @()
    foreach ($index in 0..2) {
        $run = Join-Path $cleanupRoot ('runtime-test-' + [guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path (Join-Path $run 'programs') -Force | Out-Null
        Set-Content -LiteralPath (Join-Path $run 'programs/init.elf') -Value 'generated'
        Set-Content -LiteralPath (Join-Path $run 'nexus-disk.img') -Value 'generated'
        Set-Content -LiteralPath (Join-Path $run 'serial.log') -Value 'evidence'
        Set-Content -LiteralPath (Join-Path $run 'screenshot.png') -Value 'evidence'
        Set-Content -LiteralPath (Join-Path $run 'notes.txt') -Value 'retain unknown files'
        @{ Version = 1; OwnerPid = $(if ($index -eq 1) { $PID } else { 2147483647 }) } |
            ConvertTo-Json | Set-Content -LiteralPath (Join-Path $run '.nexus-disposable.json')
        $runs += $run
    }
    $busy = [IO.File]::Open((Join-Path $runs[2] 'nexus-disk.img'), 'Open', 'ReadWrite', 'None')
    $emptyRun = Join-Path $cleanupRoot ('runtime-test-' + [guid]::NewGuid().ToString('N'))
    $rtxRun = Join-Path $cleanupRoot ('rtx-bootstrap-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path (Join-Path $rtxRun 'programs') -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $rtxRun 'serial.log') -Value 'retain firmware diagnostics'
    Set-Content -LiteralPath (Join-Path $rtxRun 'programs/FMC.BIN') -Value 'generated'
    @{ Version = 1; OwnerPid = 2147483647 } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $rtxRun '.nexus-disposable.json')
    New-Item -ItemType Directory -Path $emptyRun | Out-Null
    @{ Version = 1; OwnerPid = 2147483647 } | ConvertTo-Json |
        Set-Content -LiteralPath (Join-Path $emptyRun '.nexus-disposable.json')
    Invoke-NexusCleanup -RootDirectory $cleanupRoot -Keep 0 -WhatIf
    Require (Test-Path -LiteralPath (Join-Path $runs[0] 'nexus-disk.img')) 'WhatIf removed a file'
    Invoke-NexusCleanup -RootDirectory $cleanupRoot -Keep 0
    Require (-not (Test-Path -LiteralPath $emptyRun)) 'Empty disposable run record was retained'
    Require (-not (Test-Path -LiteralPath (Join-Path $rtxRun 'programs'))) 'RTX disposable payload was retained'
    Require (Test-Path -LiteralPath (Join-Path $rtxRun 'serial.log')) 'RTX evidence was deleted'
    Require (-not (Test-Path -LiteralPath (Join-Path $runs[0] 'programs'))) 'Old generated programs were retained'
    foreach ($name in @('serial.log', 'screenshot.png', 'notes.txt')) {
        Require (Test-Path -LiteralPath (Join-Path $runs[0] $name)) "Cleanup deleted $name"
    }
    foreach ($index in @(1, 2)) { Require (Test-Path -LiteralPath (Join-Path $runs[$index] 'programs')) 'Cleanup changed an active or locked run' }
    $rejected = $false
    try { Assert-NexusGeneratedPath -Root $cleanupRoot -Path (Join-Path $cleanupRoot '../outside') | Out-Null } catch { $rejected = $true }
    Require $rejected 'Outside-root deletion path accepted'
    $junction = Join-Path $cleanupRoot 'junction'
    New-Item -ItemType Junction -Path $junction -Target $programs | Out-Null
    try {
        $rejected = $false
        try { Assert-NexusGeneratedPath -Root $cleanupRoot -Path (Join-Path $junction 'init.elf') | Out-Null } catch { $rejected = $true }
        Require $rejected 'Path through junction accepted'
        $rejected = $false
        try { Assert-NexusGeneratedPath -Root $fixture -Path $cleanupRoot | Out-Null } catch { $rejected = $true }
        Require $rejected 'Generated tree containing junction accepted'
    } finally { [IO.Directory]::Delete($junction) }
    Write-Host 'PASS: cleanup preserves evidence, active/locked runs and unknown files; WhatIf and containment work'
} finally {
    if ($null -ne $busy) { $busy.Dispose() }
    $safe = Assert-NexusGeneratedPath -Path $fixture -Root (Join-Path $repo 'build')
    Remove-Item -LiteralPath $safe -Recurse -Force
}
exit 0
