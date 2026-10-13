<#
.SYNOPSIS
    Cross-compile real musl programs from Windows with a pinned Zig toolchain.
#>
[CmdletBinding()]
param([string]$Zig, [string]$OutputDir)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$Repo = Split-Path -Parent $PSScriptRoot
if (-not $OutputDir) { $OutputDir = Join-Path $Repo 'build/runtime' }
New-Item -ItemType Directory -Force $OutputDir | Out-Null
if (-not $Zig) {
    $Cache = Join-Path $Repo 'build/toolchains'
    $Zig = Join-Path $Cache 'zig-x86_64-windows-0.15.2/zig.exe'
    if (-not (Test-Path -LiteralPath $Zig)) {
        New-Item -ItemType Directory -Force $Cache | Out-Null
        $Zip = Join-Path $Cache 'zig.zip'
        Invoke-WebRequest 'https://ziglang.org/download/0.15.2/zig-x86_64-windows-0.15.2.zip' -OutFile $Zip
        $Expected = '3a0ed1e8799a2f8ce2a6e6290a9ff22e6906f8227865911fb7ddedc3cc14cb0c'
        if ((Get-FileHash -LiteralPath $Zip -Algorithm SHA256).Hash.ToLowerInvariant() -ne $Expected) {
            throw 'Zig archive SHA256 does not match the official release'
        }
        Expand-Archive -LiteralPath $Zip -DestinationPath $Cache
    }
}
foreach ($Name in @('musl-static', 'musl-threads')) {
    $Arguments = @('cc', '-target', 'x86_64-linux-musl', '-mcpu=baseline', '-static', '-O2')
    if ($Name -eq 'musl-threads') { $Arguments += @('-DPROBE_THREADS', '-pthread') }
    $Arguments += @((Join-Path $Repo 'tools/nexus-runtime/probe.c'), '-o', (Join-Path $OutputDir $Name))
    & $Zig @Arguments
    if ($LASTEXITCODE -ne 0) { throw "Failed compiling $Name" }
}
Write-Host "Real musl executables: $OutputDir"
& $Zig cc -target x86_64-linux-musl -mcpu=baseline -static -O2 -pthread (Join-Path $Repo 'tools/nexus-runtime/exit-group-probe.c') -o (Join-Path $OutputDir 'musl-exit-group')
if ($LASTEXITCODE) { throw 'musl group-exit regression build failed' }
& $Zig cc -target x86_64-linux-musl -mcpu=baseline -dynamic -O2 -DPROBE_DYNAMIC (Join-Path $Repo 'tools/nexus-runtime/probe.c') -o (Join-Path $OutputDir 'musl-dynamic')
if ($LASTEXITCODE) { throw 'Dynamic musl probe build failed' }
& $Zig cc -target x86_64-linux-musl -mcpu=baseline -shared -dynamic -fPIC -O2 (Join-Path $Repo 'tools/nexus-runtime/library.c') -o (Join-Path $OutputDir 'libnexus-probe.so')
if ($LASTEXITCODE) { throw 'Shared library build failed' }
& python (Join-Path $Repo 'tools/nexus-runtime/fetch-musl.py') $OutputDir
if ($LASTEXITCODE) { throw 'Real musl loader download failed' }
