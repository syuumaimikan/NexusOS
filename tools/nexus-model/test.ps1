<#
.SYNOPSIS
    Build and boot the local model worker on a fresh, isolated QEMU disk.
.DESCRIPTION
    Uses the normal kernel, bootloader and desktop binaries. Only the isolated
    disk's INIT.ELF is replaced by nexus-model-probe. Never writes the normal ESP,
    program staging directory, disk or serial log. Uses pinned pretrained weights embedded in MODEL.ELF; no UI test is claimed.
#>
[CmdletBinding()]
param([int]$Timeout = 240)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$Scripts = Join-Path (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)) 'scripts'
. (Join-Path $Scripts 'stage.ps1')
. (Join-Path $Scripts 'qemu.ps1')
$RepoRoot = Split-Path -Parent $Scripts
$RunDir = Join-Path $RepoRoot ('build/model-' + [guid]::NewGuid().ToString('N'))
$Programs = Join-Path $RunDir 'programs'
$Esp = Join-Path $RunDir 'esp'
New-Item -ItemType Directory -Force $Programs | Out-Null
foreach ($item in @(@('stories260K.bin','b0a507e7ad0f626624f17112325e66691f9076d622e1d3274d103d00299f2696'), @('tok512.bin','037cb335abb25d1fa9e8ecae30ed2a3a8ace9302862ebcdc05d51a6bbb10c312'))) {
    $path = Join-Path $RepoRoot ('build/models/' + $item[0])
    if (-not (Test-Path $path) -or (Get-FileHash $path -Algorithm SHA256).Hash -ne $item[1]) { throw 'Run python tools/nexus-model/fetch.py: pinned model assets missing or altered' }
}

function Invoke-Cargo {
    param([string[]]$Arguments)
    & cargo @Arguments
    if ($LASTEXITCODE -ne 0) { throw "cargo $Arguments failed: $LASTEXITCODE" }
}
Push-Location $RepoRoot
try {
    Invoke-Cargo @('test', '--offline', '-p', 'nexus-ai-core', '--features', 'model')
    Invoke-Cargo @('bootloader', '--offline')
    Invoke-Cargo @('kernel', '--offline')
    Invoke-Cargo @('user', '--offline')
    Invoke-Cargo @('build', '--offline', '-p', 'nexus-ai', '--features', 'model', '--target', 'targets/x86_64-nexus-user.json',
        '-Zbuild-std=core,compiler_builtins,alloc', '-Zbuild-std-features=compiler-builtins-mem')
    Publish-Esp -BootEfi (Join-Path $RepoRoot 'target/x86_64-unknown-uefi/debug/nexus-boot.efi') `
        -KernelElf (Join-Path $RepoRoot 'target/x86_64-nexus/debug/nexus-kernel') -EspDir $Esp | Out-Null
    $Names = @{
        'nexus-model-probe'='init.elf'; 'nexus-model'='model.elf'; 'nexus-compositor'='comp.elf';
        'nexus-shell'='shell.elf'; 'nexus-setup'='setup.elf'; 'nexus-wall'='wall.elf';
        'nexus-client'='client.elf'; 'nexus-hello'='hello.elf'; 'nexus-term'='term.elf';
        'nexus-settings'='set.elf'; 'nexus-store'='store.elf'; 'nexus-browser'='browse.elf';
        'nexus-idle'='idle.elf'; 'nexus-find'='find.elf'; 'nexus-updater'='updt.elf';
        'nexus-install'='inst.elf'
    }
    foreach ($name in $Names.Keys) {
        Publish-Program -Elf (Join-Path $RepoRoot "target/x86_64-nexus-user/debug/$name") `
            -ProgramDir $Programs -Name $Names[$name] | Out-Null
    }
    & powershell -NoProfile -File (Join-Path $Scripts 'make-disk.ps1') `
        -Output (Join-Path $RunDir 'nexus-disk.img') -ProgramDir $Programs
    if ($LASTEXITCODE -ne 0) { throw 'AI test disk creation failed' }
} finally { Pop-Location }

$Qemu = (Get-Command qemu-system-x86_64 -ErrorAction Stop).Source
$QemuDir = Split-Path -Parent $Qemu
$Code = Join-Path $RunDir 'code.fd'
$Vars = Join-Path $RunDir 'vars.fd'
Copy-Item (Join-Path $QemuDir 'share/edk2-x86_64-code.fd') $Code
Copy-Item (Join-Path $QemuDir 'share/edk2-i386-vars.fd') $Vars
$Serial = Join-Path $RunDir 'serial.log'
$Monitor = Get-Random -Minimum 29000 -Maximum 31000
$Args = Get-NexusQemuArgs -BuildDir $RunDir -EspDir $Esp -FirmwareCode $Code -FirmwareVars $Vars `
    -SerialLog $Serial -MonitorPort $Monitor -HostHttpPort (Get-Random -Minimum 31000 -Maximum 33000) -Headless
# ProcessStartInfo/Start-Process accept a single joined command line on Windows.
# Quote each argument so workspace paths containing spaces remain one argument.
$Quoted = @($Args | ForEach-Object { '"' + $_ + '"' })
$Process = Start-Process -FilePath $Qemu -ArgumentList $Quoted -PassThru -NoNewWindow
$Passed = $false
try {
    $Until = [DateTime]::UtcNow.AddSeconds($Timeout)
    while ([DateTime]::UtcNow -lt $Until -and -not $Process.HasExited) {
        Start-Sleep -Milliseconds 500
        if (-not (Test-Path $Serial)) { continue }
        $Log = (Get-Content $Serial -Raw) -replace "`0", ''
        if ($Log -match 'model-probe: FAIL|nexus-model: PANIC|KERNEL PANIC') { throw "Guest failure: $Serial" }
        $marker = 'model-probe: PASS pretrained inference IPC oracle cancellation limits; kernel alive'
        if ($Log.Contains($marker)) {
            # Require a subsequent OS monitor tick, proving the kernel continues.
            $tail = $Log.Substring($Log.IndexOf($marker) + $marker.Length)
            if ($tail -match '\[mon \]') { $Passed = $true; break }
        }
    }
} finally {
    if (-not $Process.HasExited) {
        try {
            $Client = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $Monitor)
            try {
                $Writer = New-Object System.IO.StreamWriter($Client.GetStream())
                $Writer.AutoFlush = $true
                $Writer.WriteLine('quit')
                $Process.WaitForExit(5000) | Out-Null
            } finally { $Client.Close() }
        } catch { Write-Warning "Monitor quit failed: $_" }
        if (-not $Process.HasExited) { $Process.Kill(); $Process.WaitForExit(5000) | Out-Null }
    }
    Write-Host "Model evidence: $Serial"
}
if (-not $Passed) { throw "Model guest verification did not finish within $Timeout seconds: $Serial" }
Write-Host 'PASS: pretrained local inference, upstream token oracle, IPC, cancellation, limits and live kernel'
