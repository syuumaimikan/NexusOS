<# Cross-build probes against the GNU ABI, then use the unmodified Debian libc. #>
[CmdletBinding()]
param([switch]$WithI386)
$ErrorActionPreference = 'Stop'
$Repo = Split-Path -Parent $PSScriptRoot
$Runtime = Join-Path $Repo 'build/runtime/glibc'
$Zig = Join-Path $Repo 'build/toolchains/zig-x86_64-windows-0.15.2/zig.exe'
& python (Join-Path $Repo 'tools/nexus-runtime/fetch-glibc.py') $Runtime
if ($LASTEXITCODE) { throw 'glibc fetch failed' }
foreach ($Name in @('glibc-basic', 'glibc-threads', 'glibc-dynamic')) {
    $Arguments = @('cc', '-target', 'x86_64-linux-gnu.2.36', '-mcpu=baseline', '-dynamic', '-O2')
    if ($Name -eq 'glibc-threads') { $Arguments += @('-DPROBE_THREADS', '-pthread') }
    if ($Name -eq 'glibc-dynamic') { $Arguments += @('-DPROBE_DYNAMIC', '-DPROBE_GLIBC_LIBRARY') }
    $Arguments += @((Join-Path $Repo 'tools/nexus-runtime/probe.c'), '-o', (Join-Path $Runtime $Name))
    & $Zig @Arguments
    if ($LASTEXITCODE) { throw "Failed compiling $Name" }
}
& $Zig cc -target x86_64-linux-gnu.2.36 -mcpu=baseline -dynamic -shared -fPIC -O2 (Join-Path $Repo 'tools/nexus-runtime/library.c') -o (Join-Path $Runtime 'libnexus-glibc-probe.so')
if ($LASTEXITCODE) { throw 'GNU ABI shared library compilation failed' }
Write-Host "GNU ABI probes and real glibc: $Runtime"
& $Zig cc -target x86_64-linux-gnu.2.36 -mcpu=baseline -dynamic -O2 -pthread (Join-Path $Repo 'tools/nexus-runtime/exit-group-probe.c') -o (Join-Path $Runtime 'glibc-exit-group')
if ($LASTEXITCODE) { throw 'GNU group-exit regression build failed' }
if ($WithI386) {
    & $Zig cc -target x86-linux-gnu.2.36 -mcpu=baseline -dynamic -O2 -pthread (Join-Path $Repo 'tools/nexus-runtime/exit-group-probe.c') -o (Join-Path $Runtime 'i386/glibc-exit-group')
    if ($LASTEXITCODE) { throw 'i386 group-exit regression build failed' }
    foreach ($Name in @('glibc-basic', 'glibc-dynamic', 'glibc-threads')) {
        $Arguments = @('cc', '-target', 'x86-linux-gnu.2.36', '-mcpu=baseline', '-dynamic', '-O2')
        if ($Name -eq 'glibc-threads') { $Arguments += @('-DPROBE_THREADS', '-pthread') }
        if ($Name -eq 'glibc-dynamic') { $Arguments += @('-DPROBE_DYNAMIC', '-DPROBE_GLIBC_LIBRARY') }
        $Arguments += @((Join-Path $Repo 'tools/nexus-runtime/probe.c'), '-o', (Join-Path $Runtime "i386/$Name"))
        & $Zig @Arguments
        if ($LASTEXITCODE) { throw "Failed compiling i386 $Name" }
    }
    & $Zig cc -target x86-linux-gnu.2.36 -mcpu=baseline -dynamic -shared -fPIC -O2 (Join-Path $Repo 'tools/nexus-runtime/library.c') -o (Join-Path $Runtime 'i386/libnexus-glibc-probe.so')
    if ($LASTEXITCODE) { throw 'i386 GNU ABI shared library compilation failed' }
}
