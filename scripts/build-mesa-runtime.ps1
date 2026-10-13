[CmdletBinding()]
param([switch]$SkipPosixBuild, [switch]$WithWayland)
$ErrorActionPreference = 'Stop'
$Repo = Split-Path -Parent $PSScriptRoot
if (-not $SkipPosixBuild) { & (Join-Path $PSScriptRoot 'build-posix-runtime.ps1') }
& python (Join-Path $Repo 'tools/nexus-runtime/build-mesa.py')
if ($LASTEXITCODE) { throw 'OSMesa cross-build failed' }
$Zig = Join-Path $Repo 'build/toolchains/zig-x86_64-windows-0.15.2/zig.exe'
$Runtime = Join-Path $Repo 'build/runtime'
$Library = Join-Path $Repo 'build/mesa/out/src/gallium/targets/osmesa/libOSMesa.so.8.0.0'
if (-not (Test-Path $Library)) { throw "OSMesa library missing: $Library" }
& 'C:/Program Files/LLVM/bin/llvm-strip.exe' --strip-unneeded -o (Join-Path $Runtime 'libOSMesa.so.8') $Library
if ($LASTEXITCODE) { throw 'OSMesa stripping failed' }
& $Zig cc -target x86_64-linux-musl -mcpu=baseline -dynamic -O2 -I (Join-Path $Repo 'build/mesa/mesa-23.3.6/include') (Join-Path $Repo 'tools/nexus-runtime/mesa.c') (Join-Path $Runtime 'libOSMesa.so.8') -o (Join-Path $Runtime 'mesa-probe')
if ($LASTEXITCODE) { throw 'OSMesa verification program build failed' }
if ($WithWayland) {
    $Wayland = Join-Path $Runtime 'wayland'
    if (-not (Test-Path (Join-Path $Wayland 'xdg-shell-protocol.c'))) { throw 'Run build-wayland-runtime.ps1 first' }
    & $Zig cc -target x86_64-linux-musl -mcpu=baseline -dynamic -O2 -DPROBE_MESA_LINKED -I $Wayland -I (Join-Path $Repo 'build/mesa/mesa-23.3.6/include') (Join-Path $Repo 'tools/nexus-runtime/wayland.c') (Join-Path $Wayland 'xdg-shell-protocol.c') (Join-Path $Wayland 'libwayland-client.so.0') (Join-Path $Runtime 'libOSMesa.so.8') -o (Join-Path $Wayland 'mesa-wayland-client')
    if ($LASTEXITCODE) { throw 'OSMesa/Wayland desktop client build failed' }
}
Write-Host "OSMesa and real OpenGL probe: $Runtime"
