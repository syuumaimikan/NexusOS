<#
.SYNOPSIS
    Build and boot the read-only AI service on a fresh, isolated QEMU disk.
.DESCRIPTION
    Uses the normal kernel, bootloader and desktop binaries. Only the isolated
    disk's INIT.ELF is replaced by nexus-ai-probe. Never writes the normal ESP,
    program staging directory, disk or serial log. No model/UI test is claimed.
#>
[CmdletBinding()]
param([int]$Timeout = 180)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'stage.ps1')
. (Join-Path $PSScriptRoot 'qemu.ps1')
$RepoRoot = Split-Path -Parent $PSScriptRoot
$RunDir = Join-Path $RepoRoot ('build/ai-' + [guid]::NewGuid().ToString('N'))
$Programs = Join-Path $RunDir 'programs'
$Esp = Join-Path $RunDir 'esp'
New-Item -ItemType Directory -Force $Programs | Out-Null

function Invoke-Cargo {
    param([string[]]$Arguments)
    & cargo @Arguments
    if ($LASTEXITCODE -ne 0) { throw "cargo $Arguments failed: $LASTEXITCODE" }
}
Push-Location $RepoRoot
try {
    Invoke-Cargo @('test', '--offline', '-p', 'nexus-ai-core')
    Invoke-Cargo @('bootloader', '--offline')
    Invoke-Cargo @('kernel', '--offline')
    Invoke-Cargo @('user', '--offline')
    Invoke-Cargo @('build', '--offline', '-p', 'nexus-ai', '--target', 'targets/x86_64-nexus-user.json',
        '-Zbuild-std=core,compiler_builtins,alloc', '-Zbuild-std-features=compiler-builtins-mem')
    Publish-Esp -BootEfi (Join-Path $RepoRoot 'target/x86_64-unknown-uefi/debug/nexus-boot.efi') `
        -KernelElf (Join-Path $RepoRoot 'target/x86_64-nexus/debug/nexus-kernel') -EspDir $Esp | Out-Null
    $Names = @{
        'nexus-ai-probe'='init.elf'; 'nexus-ai'='ai.elf'; 'nexus-compositor'='comp.elf';
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
    & powershell -NoProfile -File (Join-Path $PSScriptRoot 'make-disk.ps1') `
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
        if ($Log -match 'ai-probe: FAIL|nexus-ai: PANIC|KERNEL PANIC') { throw "Guest failure: $Serial" }
        $marker = 'ai-probe: PASS service IPC policy verification disconnect; kernel alive'
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
    Write-Host "AI evidence: $Serial"
}
if (-not $Passed) { throw "AI guest verification did not finish within $Timeout seconds: $Serial" }
Write-Host 'PASS: real service process, IPC, policy, observation, verification, teardown and live kernel'
