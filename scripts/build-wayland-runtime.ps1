<#
.SYNOPSIS
    Build a client against unmodified libwayland and its generated xdg-shell API.
.DESCRIPTION
    WSL runs only the upstream protocol scanner as a build tool. The resulting
    client runs in NexusOS, not in WSL. No WSL packages or settings are changed.
#>
[CmdletBinding()]
param([string]$Zig, [string]$WslDistribution = 'Ubuntu', [switch]$SkipPosixBuild)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$Repo = Split-Path -Parent $PSScriptRoot
if (-not $SkipPosixBuild) { & (Join-Path $PSScriptRoot 'build-posix-runtime.ps1') -Zig $Zig }
if (-not $Zig) { $Zig = Join-Path $Repo 'build/toolchains/zig-x86_64-windows-0.15.2/zig.exe' }
$OutputDir = Join-Path $Repo 'build/runtime/wayland'
& python (Join-Path $Repo 'tools/nexus-runtime/fetch-wayland.py') $OutputDir
if ($LASTEXITCODE) { throw 'Wayland package verification failed' }
Copy-Item (Join-Path $Repo 'build/runtime/ld-musl-x86_64.so.1') $OutputDir -Force
foreach ($Mode in @('client-header', 'private-code')) {
    $OutputName = if ($Mode -eq 'client-header') { 'xdg-shell-client-protocol.h' } else { 'xdg-shell-protocol.c' }
    & wsl --distribution $WslDistribution --cd $OutputDir --exec ./ld-musl-x86_64.so.1 --library-path . ./wayland-scanner $Mode xdg-shell.xml $OutputName
    if ($LASTEXITCODE) { throw "Upstream wayland-scanner failed: $Mode" }
}
& $Zig cc -target x86_64-linux-musl -mcpu=baseline -dynamic -O2 -I $OutputDir (Join-Path $Repo 'tools/nexus-runtime/wayland.c') (Join-Path $OutputDir 'xdg-shell-protocol.c') (Join-Path $OutputDir 'libwayland-client.so.0') -o (Join-Path $OutputDir 'wayland-client')
if ($LASTEXITCODE) { throw 'Real libwayland client build failed' }
Write-Host "Real libwayland client: $OutputDir"
