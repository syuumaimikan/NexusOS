<#
.SYNOPSIS
    Verify positional Linux reads and CPU 3D presentation on an isolated disk.
.DESCRIPTION
    Requires build/programs from build.ps1. Rebuilds the changed components,
    stages a fresh files.lx, and never writes the normal guest disk or ESP.
    This test does not establish Steam, Vulkan or Wayland compatibility.
#>
[CmdletBinding()]
param([int]$Timeout = 240)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'stage.ps1')
. (Join-Path $PSScriptRoot 'qemu.ps1')
. (Join-Path $PSScriptRoot 'capture.ps1')
. (Join-Path $PSScriptRoot 'private-machine.ps1')
. (Join-Path $PSScriptRoot 'maintenance.ps1')
Invoke-NexusCleanup
function Get-NexusDiskSizes { return @{SizeMiB=256; FatMiB=40} }
$Repo = Split-Path -Parent $PSScriptRoot
$Run = Join-Path $Repo ('build/linux-graphics-' + [guid]::NewGuid().ToString('N'))
$Programs = Join-Path $Run 'programs'
$Esp = Join-Path $Run 'esp'
Register-NexusDisposableRun -Directory $Run
New-Item -ItemType Directory -Force $Programs | Out-Null
Copy-Item (Join-Path $Repo 'build/programs/*') $Programs -Recurse
Push-Location $Repo
try {
    & cargo kernel --offline
    if ($LASTEXITCODE) { throw 'kernel build failed' }
    & cargo build --offline -p nexus-client --target targets/x86_64-nexus-user.json '-Zbuild-std=core,compiler_builtins,alloc' '-Zbuild-std-features=compiler-builtins-mem'
    if ($LASTEXITCODE) { throw 'client build failed' }
    & cargo build --offline -p nexus-linux-example
    if ($LASTEXITCODE) { throw 'Linux fixture build failed' }
    & (Join-Path $Repo 'target/debug/nexus-linux-example.exe') (Join-Path $Programs 'files.lx') 'a linux program wrote a file and read it back' files
    if ($LASTEXITCODE) { throw 'Linux fixture generation failed' }
    Publish-Program -Elf (Join-Path $Repo 'target/x86_64-nexus-user/debug/nexus-client') -ProgramDir $Programs -Name 'client.elf' | Out-Null
    Publish-Esp -BootEfi (Join-Path $Repo 'build/esp/EFI/BOOT/BOOTX64.EFI') -KernelElf (Join-Path $Repo 'target/x86_64-nexus/debug/nexus-kernel') -EspDir $Esp | Out-Null
    & (Join-Path $PSScriptRoot 'make-disk.ps1') -Output (Join-Path $Run 'nexus-disk.img') -ProgramDir $Programs -SizeMiB (Get-NexusDiskSizes).SizeMiB -FatMiB (Get-NexusDiskSizes).FatMiB
} finally { Pop-Location }
$Qemu = (Get-Command qemu-system-x86_64).Source
$Share = Join-Path (Split-Path $Qemu -Parent) 'share'
$Code = Join-Path $Run 'code.fd'
$Vars = Join-Path $Run 'vars.fd'
Copy-Item (Join-Path $Share 'edk2-x86_64-code.fd') $Code
Copy-Item (Join-Path $Share 'edk2-i386-vars.fd') $Vars
$Serial = Join-Path $Run 'serial.log'
$Port = Get-Random -Minimum 34000 -Maximum 35000
$QemuArgs = Get-NexusQemuArgs -BuildDir $Run -EspDir $Esp -FirmwareCode $Code -FirmwareVars $Vars -SerialLog $Serial -MonitorPort $Port -HostHttpPort (Get-FreeMonitorPort -From 35000 -To 36000) -Headless
$Process = Start-NexusQemu -Executable $Qemu -Arguments $QemuArgs -ErrorLog (Join-Path $Run 'qemu-error.log')
$Connection = $null
function Wait-Marker([string]$Marker) {
    $Deadline = [DateTime]::UtcNow.AddSeconds($Timeout)
    while ([DateTime]::UtcNow -lt $Deadline -and -not $Process.HasExited) {
        if (Test-Path $Serial) {
            $Log = (Get-Content $Serial -Raw) -replace "`0", ''
            if ($Log -match 'KERNEL PANIC|client: PANIC|a Linux program that wrote a file and read it back exited with status') {
                throw "Guest failure: $Serial"
            }
            if ($Log.Contains($Marker)) { return }
        }
        Start-Sleep -Milliseconds 500
    }
    throw "Missing '$Marker': $Serial"
}
try {
    Wait-Marker 'init: a Linux program that wrote a file and read it back ran and exited through the translation'
    Write-Host 'PASS: Linux pread64 bytes, cursor, EOF and invalid offset'
    Wait-Marker 'setup: this machine has not been set up; asking'
    $Connection = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $Port)
    $Writer = New-Object System.IO.StreamWriter($Connection.GetStream())
    $Writer.AutoFlush = $true
    # Complete first-run setup only on this disposable test disk.
    foreach ($Key in @('ret','ret','n','e','x','u','s','ret','p','a','s','s','w','o','r','d','ret','p','a','s','s','w','o','r','d','ret','ret')) {
        $Writer.WriteLine("sendkey $Key")
        Start-Sleep -Milliseconds 250
    }
    Wait-Marker 'client: software 3D cube rendered into compositor surface'
    Wait-Marker 'client: drew every frame into a surface it was given'
    $Ppm = Join-Path $Run 'desktop.ppm'
    Invoke-Screendump -Writer $Writer -Path $Ppm
    Convert-PpmToPng -PpmPath $Ppm -PngPath (Join-Path $Run 'desktop.png')
    Write-Host 'PASS: software 3D client submitted frames to the compositor'
} finally {
    if ($null -ne $Connection) { $Connection.Close() }
    if (-not $Process.HasExited) { $Process.Kill(); $Process.WaitForExit(5000) | Out-Null }
    Write-Host "Evidence: $Run"
}
