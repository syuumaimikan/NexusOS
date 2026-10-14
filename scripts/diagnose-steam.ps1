<#
.SYNOPSIS
    Boot the actual Valve i386 bootstrap on an isolated NexusOS disk and collect failures.
.DESCRIPTION
    This is a diagnostic, not proof of a working Steam installation. No host
    installer/scripts are executed and no account credentials are supplied.
    Requires build.ps1 -WithGlibc32Runtime -SkipDiskImage first.
#>
[CmdletBinding()]
param([int]$Timeout = 180)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'stage.ps1')
. (Join-Path $PSScriptRoot 'qemu.ps1')
. (Join-Path $PSScriptRoot 'private-machine.ps1')
. (Join-Path $PSScriptRoot 'maintenance.ps1')
Invoke-NexusCleanup
$Repo = Split-Path -Parent $PSScriptRoot
$Run = Join-Path $Repo ('build/steam-diagnostic-' + [guid]::NewGuid().ToString('N'))
$Programs = Join-Path $Run 'programs'
$Esp = Join-Path $Run 'esp'
Register-NexusDisposableRun -Directory $Run
New-Item -ItemType Directory -Force $Programs | Out-Null
& python (Join-Path $Repo 'tools/nexus-runtime/fetch-steam.py')
if ($LASTEXITCODE) { throw 'Official Steam bootstrap fetch failed' }
& python (Join-Path $Repo 'tools/nexus-runtime/fetch-busybox.py')
if ($LASTEXITCODE) { throw 'Pinned i386 shell fetch failed' }
Copy-Item (Join-Path $Repo 'build/programs/*') $Programs -Recurse
foreach ($Required in @('g32ld.lx', 'gnu320.lx')) {
    if (-not (Test-Path (Join-Path $Programs $Required))) { throw 'Stage the i386 GNU runtime with build.ps1 -WithGlibc32Runtime first' }
}
$SteamRoot = Join-Path $Repo 'build/runtime/steam'
$BusyboxRoot = Join-Path $Repo 'build/runtime/busybox'
$Manifest = Join-Path $Programs 'linux.lst'
$ManifestText = [IO.File]::ReadAllText($Manifest)
foreach ($RequiredMapping in @(
    'BIN/G32LD.LX /lib/ld-linux.so.2',
    'BIN/GNU320.LX /lib/i386-linux-gnu/libc.so.6'
)) {
    if (-not [regex]::IsMatch($ManifestText, '(?m)^' + [regex]::Escape($RequiredMapping) + '\s*$')) {
        throw "The staged Linux runtime is missing '$RequiredMapping'; rebuild with -WithGlibc32Runtime -SkipDiskImage"
    }
}
$Entries = @(
    @('steam', 'steam32.lx', '/usr/lib/steam/ubuntu12_32/steam'),
    @('crashhandler.so', 'smcrash.lx', '/usr/lib/steam/ubuntu12_32/crashhandler.so'),
    @('libgcc_s.so.1', 'smgcc.lx', '/lib/i386-linux-gnu/libgcc_s.so.1'),
    @('libstdc++.so.6', 'smcpp.lx', '/lib/i386-linux-gnu/libstdc++.so.6'),
    @('copyright.txt', 'smnotice.txt', '/usr/share/steam-diagnostic/copyright'),
    @('manifest.json', 'smmeta.txt', '/usr/share/steam-diagnostic/manifest.json'),
    @((Join-Path $BusyboxRoot 'busybox'), 'busybox.lx', '/bin/sh'),
    @((Join-Path $BusyboxRoot 'copyright.txt'), 'bbnotice.txt', '/usr/share/doc/busybox-static/copyright')
)
$Text = [IO.File]::ReadAllText($Manifest).TrimEnd() + "`n"
foreach ($Entry in $Entries) {
    $Source = if ([IO.Path]::IsPathRooted($Entry[0])) { $Entry[0] } else { Join-Path $SteamRoot $Entry[0] }
    Copy-Item -LiteralPath $Source -Destination (Join-Path $Programs $Entry[1])
    $Text += "BIN/$($Entry[1].ToUpperInvariant()) $($Entry[2])`n"
}
[IO.File]::WriteAllText($Manifest, $Text, [Text.UTF8Encoding]::new($false))
Push-Location $Repo
try {
    & cargo kernel --offline
    if ($LASTEXITCODE) { throw 'kernel build failed' }
    & cargo build --offline -p nexus-init --bin nexus-steam-diagnostic --target targets/x86_64-nexus-user.json '-Zbuild-std=core,compiler_builtins,alloc' '-Zbuild-std-features=compiler-builtins-mem'
    if ($LASTEXITCODE) { throw 'Steam diagnostic init build failed' }
    Publish-Program -Elf (Join-Path $Repo 'target/x86_64-nexus-user/debug/nexus-steam-diagnostic') -ProgramDir $Programs -Name 'init.elf' | Out-Null
    Publish-Esp -BootEfi (Join-Path $Repo 'build/esp/EFI/BOOT/BOOTX64.EFI') -KernelElf (Join-Path $Repo 'target/x86_64-nexus/debug/nexus-kernel') -EspDir $Esp | Out-Null
    & (Join-Path $PSScriptRoot 'make-disk.ps1') -Output (Join-Path $Run 'nexus-disk.img') -ProgramDir $Programs -SizeMiB 512 -FatMiB 256
} finally { Pop-Location }
# The i386 GNU runtime plus Steam diagnostic payload exceeds 96 MiB. Give this
# disposable image enough FAT32 space for staged files.
function Get-NexusDiskSizes { return @{SizeMiB=512; FatMiB=256} }
$Qemu = (Get-Command qemu-system-x86_64).Source
$Share = Join-Path (Split-Path $Qemu -Parent) 'share'
$Code = Join-Path $Run 'code.fd'
$Vars = Join-Path $Run 'vars.fd'
Copy-Item (Join-Path $Share 'edk2-x86_64-code.fd') $Code
Copy-Item (Join-Path $Share 'edk2-i386-vars.fd') $Vars
$Serial = Join-Path $Run 'serial.log'
$SteamQemuArgs = Get-NexusQemuArgs -BuildDir $Run -EspDir $Esp -FirmwareCode $Code -FirmwareVars $Vars -SerialLog $Serial -MonitorPort (Get-FreeMonitorPort) -HostHttpPort (Get-FreeMonitorPort -From 35000 -To 36000) -Memory '2G' -Headless
$Process = Start-NexusQemu -Executable $Qemu -Arguments $SteamQemuArgs -ErrorLog (Join-Path $Run 'qemu-error.log')
try {
    $Deadline = [DateTime]::UtcNow.AddSeconds($Timeout)
    while ([DateTime]::UtcNow -lt $Deadline -and -not $Process.HasExited) {
        Start-Sleep -Milliseconds 500
        if (-not (Test-Path $Serial)) { continue }
        $Log = (Get-Content $Serial -Raw) -replace "`0", ''
        if ($Log.Contains('steam-diagnostic: bootstrap ending')) {
            Write-Host 'Diagnostic recorded. This does not verify Steam sign-in or hardware acceleration.'
            return
        }
        if ($Log -match 'steam-diagnostic: loader refused|steam-diagnostic: bootstrap did not complete|KERNEL PANIC|the system has been halted') {
            throw "Steam diagnostic stopped: $Serial"
        }
        if ($Log -match 'Failed to create thread \(error 0x') {
            throw "Steam bootstrap reached the still-unsupported i386 shared-thread path: $Serial"
        }
    }
    throw "Steam diagnostic did not finish: $Serial"
} finally {
    if (-not $Process.HasExited) { $Process.Kill(); $Process.WaitForExit(5000) | Out-Null }
    Write-Host "Evidence: $Serial"
}
