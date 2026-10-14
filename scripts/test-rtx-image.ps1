<#
.SYNOPSIS
    Boot the RTX diagnostic image through UEFI/xHCI USB, without virtio storage.
.DESCRIPTION
    Proves image boot and native USB firmware loading, not physical RTX rendering.
#>
[CmdletBinding()]
param([int]$Timeout = 120)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'maintenance.ps1')
. (Join-Path $PSScriptRoot 'qemu.ps1')
$Repo = Split-Path -Parent $PSScriptRoot
$Image = Join-Path $Repo 'build/rtx-5060-diagnostic.img'
if (-not (Test-Path -LiteralPath $Image)) { throw 'Run scripts/build-rtx-image.ps1 first' }
$Run = Join-Path $Repo ('build/rtx-bootstrap-' + [guid]::NewGuid().ToString('N'))
Register-NexusDisposableRun -Directory $Run
$Qemu = (Get-Command qemu-system-x86_64).Source
$Share = Join-Path (Split-Path -Parent $Qemu) 'share'
$Code = Join-Path $Run 'code.fd'
$Vars = Join-Path $Run 'vars.fd'
Copy-Item -LiteralPath (Join-Path $Share 'edk2-x86_64-code.fd') -Destination $Code
Copy-Item -LiteralPath (Join-Path $Share 'edk2-i386-vars.fd') -Destination $Vars
$Serial = Join-Path $Run 'usb-image-serial.log'
$Arguments = @('-machine','q35','-cpu','qemu64,+pdpe1gb,+rdrand,+rdseed','-smp','4','-m','1G',
    '-drive',"if=pflash,format=raw,unit=0,readonly=on,file=$Code",'-drive',"if=pflash,format=raw,unit=1,file=$Vars",
    '-serial',"file:$Serial",'-no-reboot','-display','none','-vga','none',
    '-device','virtio-gpu-pci,xres=1920,yres=1200','-device','qemu-xhci,id=nexusxhci',
    '-drive',"if=none,id=rtxmedium,format=raw,file=$Image",'-device','usb-storage,bus=nexusxhci.0,drive=rtxmedium,bootindex=1')
$Guest = Start-NexusQemu -Executable $Qemu -Arguments $Arguments -ErrorLog (Join-Path $Run 'qemu-error.log')
try {
    $Deadline = [DateTime]::UtcNow.AddSeconds($Timeout)
    while ([DateTime]::UtcNow -lt $Deadline -and -not $Guest.HasExited) {
        if (Test-Path -LiteralPath $Serial) {
            $Log = (Get-Content -LiteralPath $Serial -Raw) -replace "`0",''
            if ($Log -match '(?m)^ EXCEPTION [0-9]+:|KERNEL PANIC|NVIDIA bootstrap FAILED:') { throw "USB image guest failed: $Serial" }
            if ($Log.Contains('[test] NVIDIA bootstrap PASS:')) {
                if ($Log.Contains('[blk ] virtio')) { throw 'Unexpected virtio storage in USB image test' }
                Write-Host "PASS: UEFI image boot and real pinned bootstrap read over native USB; $Serial"
                return
            }
        }
        Start-Sleep -Milliseconds 250
    }
    throw "Diagnostic USB image did not reach the native bootstrap test: $Serial"
} finally {
    if (-not $Guest.HasExited) { $Guest.Kill(); $Guest.WaitForExit(5000) | Out-Null }
    $Guest.Dispose()
    Invoke-NexusCleanup
}
