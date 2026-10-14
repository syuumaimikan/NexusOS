<#
.SYNOPSIS
    Create a bootable UEFI image for native RTX 5060 bootstrap diagnostics.
.DESCRIPTION
    Contains the Nexus kernel, production bootstrap files and their license.
    The kernel tests unpublished DMA; GSP boot and rendering are not implemented.
    Writes only an image file under build, never a physical USB/disk or Windows.
.PARAMETER WithGspFirmware
    Include pinned GSP-RM/microcode and test native scatter/radix preparation.
    Requires Python 3.14 and downloads the matching package into the cache once.
#>
[CmdletBinding()]
param([switch]$WithGspFirmware)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'maintenance.ps1')
. (Join-Path $PSScriptRoot 'rtx-stage.ps1')
. (Join-Path $PSScriptRoot 'stage.ps1')
$Repo = Split-Path -Parent $PSScriptRoot
$Run = Join-Path $Repo ('build/rtx-bootstrap-' + [guid]::NewGuid().ToString('N'))
Register-NexusDisposableRun -Directory $Run
$Esp = Join-Path $Run 'esp'
New-Item -ItemType Directory -Path (Join-Path $Esp 'EFI/BOOT') -Force | Out-Null
Copy-Item -LiteralPath (Join-Path $Repo 'build/esp/EFI/BOOT/BOOTX64.EFI') -Destination (Join-Path $Esp 'EFI/BOOT/BOOTX64.EFI')
Publish-NvidiaBootstrap -ProgramDir (Join-Path $Esp 'BIN')
if ($WithGspFirmware) { Publish-NvidiaGsp -ProgramDir (Join-Path $Esp 'BIN') }
$Image = Join-Path $Repo 'build/rtx-5060-diagnostic.img'
Push-Location $Repo
try {
    $Features = if ($WithGspFirmware) { 'nvidia-gsp-test' } else { 'nvidia-bootstrap-test' }
    & cargo kernel --offline --features $Features
    if ($LASTEXITCODE -ne 0) { throw 'RTX diagnostic kernel build failed' }
    Publish-EspKernel -KernelElf (Join-Path $Repo 'target/x86_64-nexus/debug/nexus-kernel') -EspDir $Esp | Out-Null
    $Size = if ($WithGspFirmware) { 256 } else { 128 }
    $Fat = if ($WithGspFirmware) { 128 } else { 40 }
    & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'make-disk.ps1') `
        -Output $Image -SourceDir $Esp -SizeMiB $Size -FatMiB $Fat
    if ($LASTEXITCODE -ne 0) { throw 'RTX diagnostic image creation failed' }
    $Hash = (Get-FileHash -LiteralPath $Image -Algorithm SHA256).Hash.ToLowerInvariant()
    "$Hash  rtx-5060-diagnostic.img" | Set-Content -LiteralPath (Join-Path $Repo 'build/rtx-5060-diagnostic.sha256')
    Write-Host "Created diagnostic UEFI image: $Image"
} finally {
    try {
        & cargo kernel --offline
        if ($LASTEXITCODE -ne 0) { throw 'Normal kernel restoration failed' }
        Publish-EspKernel -KernelElf (Join-Path $Repo 'target/x86_64-nexus/debug/nexus-kernel') -EspDir (Join-Path $Repo 'build/esp') | Out-Null
    } finally {
        Pop-Location
        Invoke-NexusCleanup
    }
}
