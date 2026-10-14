<#
.SYNOPSIS
    Test native AMD-Vi DMA with QEMU's explicit DMA test PCI device.
.DESCRIPTION
    Uses the persistent disk through a temporary snapshot. No persistent
    sectors/settings or physical IOMMU registers on Windows are modified.
#>
[CmdletBinding()]
param([int]$Timeout=120)
$ErrorActionPreference='Stop'
. (Join-Path $PSScriptRoot 'maintenance.ps1')
. (Join-Path $PSScriptRoot 'stage.ps1')
. (Join-Path $PSScriptRoot 'qemu.ps1')
$Repo=Split-Path -Parent $PSScriptRoot
$Run=Join-Path $Repo ('build/rtx-bootstrap-'+[guid]::NewGuid().ToString('N'))
Register-NexusDisposableRun -Directory $Run
Copy-Item -LiteralPath (Join-Path $Repo 'build/esp') -Destination (Join-Path $Run 'esp') -Recurse
$Qemu=(Get-Command qemu-system-x86_64).Source
$Share=Join-Path (Split-Path -Parent $Qemu) 'share'
$Code=Join-Path $Run 'code.fd'; $Vars=Join-Path $Run 'vars.fd'
Copy-Item -LiteralPath (Join-Path $Share 'edk2-x86_64-code.fd') -Destination $Code
Copy-Item -LiteralPath (Join-Path $Share 'edk2-i386-vars.fd') -Destination $Vars
$Serial=Join-Path $Run 'amdvi-serial.log'
$Guest=$null
Push-Location $Repo
try {
    & cargo kernel --offline --features amdvi-dma-test
    if ($LASTEXITCODE -ne 0) {throw 'AMD-Vi test kernel build failed'}
    Publish-EspKernel -KernelElf (Join-Path $Repo 'target/x86_64-nexus/debug/nexus-kernel') -EspDir (Join-Path $Run 'esp') | Out-Null
    $Arguments=Get-NexusQemuArgs -BuildDir (Join-Path $Repo 'build') -EspDir (Join-Path $Run 'esp') -FirmwareCode $Code -FirmwareVars $Vars -SerialLog $Serial -HostHttpPort 0 -Headless
    $Arguments=@($Arguments | ForEach-Object {
        if ($_ -like 'if=none,id=nexusdisk,*') {$_+',snapshot=on'} else {$_}
    })+@('-device','amd-iommu,intremap=off,dma-remap=on','-device','edu,dma_mask=0xffffffffffffffff')
    $Guest=Start-NexusQemu -Executable $Qemu -Arguments $Arguments -ErrorLog (Join-Path $Run 'qemu-error.log')
    $Deadline=[DateTime]::UtcNow.AddSeconds($Timeout)
    while ([DateTime]::UtcNow -lt $Deadline -and -not $Guest.HasExited) {
        if (Test-Path -LiteralPath $Serial) {
            $Log=(Get-Content -LiteralPath $Serial -Raw) -replace "`0",''
            if ($Log -match '(?m)^ EXCEPTION [0-9]+:|KERNEL PANIC|AMD-Vi DMA FAILED:') {throw "AMD-Vi guest failed: $Serial"}
            if ($Log.Contains('[test] AMD-Vi DMA PASS:')) {
                Write-Host "PASS: native translated DMA and denied address, recovery and reclamation; $Serial"
                return
            }
        }
        Start-Sleep -Milliseconds 250
    }
    throw "AMD-Vi guest did not complete: $Serial"
} finally {
    if ($Guest) {
        if (-not $Guest.HasExited) {$Guest.Kill(); $Guest.WaitForExit(5000) | Out-Null}
        $Guest.Dispose()
    }
    try {
        & cargo kernel --offline
        if ($LASTEXITCODE -ne 0) {throw 'Default kernel restoration failed'}
        Publish-EspKernel -KernelElf (Join-Path $Repo 'target/x86_64-nexus/debug/nexus-kernel') -EspDir (Join-Path $Repo 'build/esp') | Out-Null
    } finally {Pop-Location; Invoke-NexusCleanup}
}
