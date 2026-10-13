[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
$Repo = Split-Path -Parent $PSScriptRoot
& python (Join-Path $Repo 'tools/nexus-runtime/build-swiftshader.py')
if ($LASTEXITCODE) { throw 'SwiftShader build failed' }
$Zig = Join-Path $Repo 'build/toolchains/zig-x86_64-windows-0.15.2/zig.exe'
& $Zig cc -target x86_64-linux-musl -dynamic -O2 -I (Join-Path $Repo 'build/swiftshader/source/include') (Join-Path $Repo 'tools/nexus-runtime/vulkan.c') -o (Join-Path $Repo 'build/runtime/vulkan-probe')
if ($LASTEXITCODE) { throw 'Vulkan probe compilation failed' }
