<#
.SYNOPSIS
    Verify pinned RTX 5060 bootstrap layouts and unpublished DMA on NexusOS.
.DESCRIPTION
    Real production firmware is read from a disposable FAT32 image. Tests
    copying, zero padding, reclamation, allocation rollback and corrupt files.
    QEMU has no RTX GPU: this never proves firmware boot or GPU acceleration.
#>
[CmdletBinding()]
param([int]$Timeout = 240, [switch]$WithGspFirmware)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'maintenance.ps1')
. (Join-Path $PSScriptRoot 'stage.ps1')
. (Join-Path $PSScriptRoot 'rtx-stage.ps1')
. (Join-Path $PSScriptRoot 'qemu.ps1')
. (Join-Path $PSScriptRoot 'private-machine.ps1')
$Repo = Split-Path -Parent $PSScriptRoot
$Run = Join-Path $Repo ('build/rtx-bootstrap-' + [guid]::NewGuid().ToString('N'))
Register-NexusDisposableRun -Directory $Run
$Programs = Join-Path $Run 'programs'
Copy-Item -LiteralPath (Join-Path $Repo 'build/programs') -Destination $Programs -Recurse
Copy-Item -LiteralPath (Join-Path $Repo 'build/esp') -Destination (Join-Path $Run 'esp') -Recurse
Publish-NvidiaBootstrap -ProgramDir $Programs
if ($WithGspFirmware) { Publish-NvidiaGsp -ProgramDir $Programs }
Push-Location $Repo
try {
    & cargo test --offline -p nexus-nvidia
    if ($LASTEXITCODE -ne 0) { throw 'NVIDIA host parser tests failed' }
    $Features = if ($WithGspFirmware) { 'nvidia-gsp-test' } else { 'nvidia-bootstrap-test' }
    & cargo kernel --offline --features $Features
    if ($LASTEXITCODE -ne 0) { throw 'Native bootstrap test kernel failed' }
    Publish-EspKernel -KernelElf (Join-Path $Repo 'target/x86_64-nexus/debug/nexus-kernel') -EspDir (Join-Path $Run 'esp') | Out-Null
} finally { Pop-Location }
$Disk = Join-Path $Run 'nexus-disk.img'
$TestSize = if ($WithGspFirmware) { 384 } else { 256 }
$TestFat = if ($WithGspFirmware) { 192 } else { 96 }
& (Join-Path $PSScriptRoot 'make-disk.ps1') -Output $Disk -ProgramDir $Programs -SizeMiB $TestSize -FatMiB $TestFat
# This test deliberately uses a small disk. Keep the production run.ps1 size
# guard intact; describe the test geometry only in this script's QEMU builder.
function Get-NexusDiskSizes { return @{ SizeMiB = $TestSize; FatMiB = $TestFat } }
$Qemu = (Get-Command qemu-system-x86_64).Source
$Share = Join-Path (Split-Path -Parent $Qemu) 'share'
$Code = Join-Path $Run 'code.fd'
$Vars = Join-Path $Run 'vars.fd'
Copy-Item -LiteralPath (Join-Path $Share 'edk2-x86_64-code.fd') -Destination $Code
Copy-Item -LiteralPath (Join-Path $Share 'edk2-i386-vars.fd') -Destination $Vars
function Boot-Case([string]$Name,[string]$Marker) {
    $Serial = Join-Path $Run "$Name-serial.log"
    $QemuArgs = Get-NexusQemuArgs -BuildDir $Run -EspDir (Join-Path $Run 'esp') -FirmwareCode $Code -FirmwareVars $Vars `
        -SerialLog $Serial -HostHttpPort (Get-FreeMonitorPort -From 18100 -To 19000) -Headless
    $Guest = Start-NexusQemu -Executable $Qemu -Arguments $QemuArgs -ErrorLog (Join-Path $Run "$Name-qemu-error.log")
    try {
        $Deadline = [DateTime]::UtcNow.AddSeconds($Timeout)
        while ([DateTime]::UtcNow -lt $Deadline -and -not $Guest.HasExited) {
            if (Test-Path -LiteralPath $Serial) {
                $Log = (Get-Content -LiteralPath $Serial -Raw) -replace "`0", ''
                if ($Log -match '(?m)^ EXCEPTION [0-9]+:|KERNEL PANIC|the system has been halted') { throw "Guest failed: $Serial" }
                if ($Log.Contains($Marker)) { return $Log }
                if ($Name -eq 'production' -and $Log -match 'NVIDIA (bootstrap|GSP preparation) FAILED:') { throw "Firmware/DMA failure: $Serial" }
            }
            Start-Sleep -Milliseconds 250
        }
        throw "Missing $Name evidence; see $Serial"
    } finally {
        if (-not $Guest.HasExited) { $Guest.Kill(); $Guest.WaitForExit(5000) | Out-Null }
        $Guest.Dispose()
    }
}
try {
    $GoodMarker = if ($WithGspFirmware) { '[test] NVIDIA GSP preparation PASS:' } else { '[test] NVIDIA bootstrap PASS:' }
    $Good = Boot-Case 'production' $GoodMarker
    if ($Good.Contains('[test] NVIDIA bootstrap FAILED:')) { throw 'Production bootstrap was rejected' }
    Write-Host 'PASS: native production bootstrap parsing, pinned hashes, contiguous copies, zero padding, reclamation and partial-allocation rollback'
    if ($WithGspFirmware) {
        if (-not $Good.Contains('[test] NVIDIA bootstrap PASS:')) { throw 'Bootstrap check missing before GSP check' }
        if (-not $Good.Contains('[nv  ] GB206 boot ABI ready:') -or -not $Good.Contains('native boot ABI, RPC queues, seven logs, DMA permissions')) { throw 'Native GSP argument/queue check missing' }
        if (-not $Good.Contains('[nv  ] FSP COT single-packet=868 bytes staged;')) { throw 'Native COT wire packet missing' }
        Write-Host 'PASS: native real GSP-RM/ucodes, radix, initial boot ABI, RPC queues, seven logs, DMA permissions and frame rollback'
        $GspFile = Join-Path $Programs 'NVFW/GSPRM.BIN'
        $GspStream = [IO.File]::Open($GspFile,'Open','ReadWrite','None')
        try {
            $GspStream.Position = $GspStream.Length - 1
            $Byte = $GspStream.ReadByte()
            $GspStream.Position = $GspStream.Length - 1
            $GspStream.WriteByte($Byte -bxor 1)
        } finally { $GspStream.Dispose() }
        & (Join-Path $PSScriptRoot 'make-disk.ps1') -Output $Disk -ProgramDir $Programs -SizeMiB $TestSize -FatMiB $TestFat -UpdatePrograms
        $Rejected = Boot-Case 'corrupt-gsp' '[test] NVIDIA GSP preparation FAILED: Image(ReleaseMismatch)'
        if ($Rejected.Contains('[test] NVIDIA GSP preparation PASS:') -or $Rejected.Contains('[test] NVIDIA GSP preparation FAILED: leaked frames')) { throw 'Corrupt GSP accepted or leaked memory' }
        Write-Host 'PASS: native modified GSP-RM refused and all partially prepared frames reclaimed'
        Copy-Item -LiteralPath (Join-Path $Repo 'build/nvidia/gsp-bundle/GSPRM.BIN') -Destination $GspFile -Force
    }
    $BootFile = Join-Path $Programs 'NVFW/GSPBOOT.BIN'
    $Data = [IO.File]::ReadAllBytes($BootFile)
    $Data[$Data.Length-1] = $Data[$Data.Length-1] -bxor 1
    [IO.File]::WriteAllBytes($BootFile,$Data)
    & (Join-Path $PSScriptRoot 'make-disk.ps1') -Output $Disk -ProgramDir $Programs -SizeMiB $TestSize -FatMiB $TestFat -UpdatePrograms
    $Bad = Boot-Case 'corrupt' '[test] NVIDIA bootstrap FAILED: Image(ReleaseMismatch)'
    if ($Bad.Contains('[test] NVIDIA bootstrap PASS:')) { throw 'Corrupt bootstrap passed' }
    Write-Host "PASS: native kernel refused modified production firmware; no GPU submitted; $Run"
} finally {
    # Keep the default ESP on the normal kernel; the test kernel stays only in
    # this owned run. No persistent disk or firmware file is replaced.
    Push-Location $Repo
    try {
        & cargo kernel --offline
        if ($LASTEXITCODE -ne 0) { throw 'Normal kernel restoration failed' }
        Publish-EspKernel -KernelElf (Join-Path $Repo 'target/x86_64-nexus/debug/nexus-kernel') -EspDir (Join-Path $Repo 'build/esp') | Out-Null
    } finally { Pop-Location }
    Invoke-NexusCleanup
}
