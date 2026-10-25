<#
.SYNOPSIS
    Execute real musl in NexusOS on a unique disposable disk.
.DESCRIPTION
    The Linux host/WSL is not used to run the guest. Only QEMU boots the custom
    kernel. Requires the normal OS build first. Keeps serial logs as evidence.
#>
[CmdletBinding()]
param([int]$Timeout = 180, [switch]$SkipRuntimeBuild, [switch]$WithWayland, [switch]$WithMesa, [switch]$WithGlibc, [switch]$WithVulkan, [switch]$WithGlibc32, [switch]$WithUIControls, [switch]$DeepFilesystemChecks)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($WithUIControls -and -not $WithWayland) { throw 'WithUIControls requires WithWayland' }
if ($WithGlibc32) { $WithGlibc = $true }
. (Join-Path $PSScriptRoot 'stage.ps1')
. (Join-Path $PSScriptRoot 'qemu.ps1')
. (Join-Path $PSScriptRoot 'runtime-stage.ps1')
. (Join-Path $PSScriptRoot 'capture.ps1')
. (Join-Path $PSScriptRoot 'private-machine.ps1')
. (Join-Path $PSScriptRoot 'maintenance.ps1')
Invoke-NexusCleanup
$Repo = Split-Path -Parent $PSScriptRoot
$Run = Join-Path $Repo ('build/runtime-test-' + [guid]::NewGuid().ToString('N'))
$Programs = Join-Path $Run 'programs'
$Esp = Join-Path $Run 'esp'
Register-NexusDisposableRun -Directory $Run
New-Item -ItemType Directory -Force $Programs | Out-Null
if (-not $SkipRuntimeBuild) { & (Join-Path $PSScriptRoot 'build-posix-runtime.ps1') }
if ($WithWayland -and -not $SkipRuntimeBuild) { & (Join-Path $PSScriptRoot 'build-wayland-runtime.ps1') -SkipPosixBuild }
if ($WithMesa -and -not $SkipRuntimeBuild) { & (Join-Path $PSScriptRoot 'build-mesa-runtime.ps1') -SkipPosixBuild -WithWayland:$WithWayland }
Copy-Item (Join-Path $Repo 'build/programs/*') $Programs -Recurse
if ($WithGlibc -and -not $SkipRuntimeBuild) { & (Join-Path $PSScriptRoot 'build-glibc-runtime.ps1') -WithI386:$WithGlibc32 }
if ($WithVulkan -and -not $SkipRuntimeBuild) { & (Join-Path $PSScriptRoot 'build-vulkan-runtime.ps1') }
Publish-PosixRuntime -ProgramDir $Programs -WithWayland:$WithWayland -WithMesa:$WithMesa -WithGlibc:$WithGlibc -WithVulkan:$WithVulkan -WithGlibc32:$WithGlibc32
Push-Location $Repo
try {
    $KernelFeatures = @()
    if ($DeepFilesystemChecks) { $KernelFeatures = @('--features', 'deep-selftest') }
    & cargo kernel --offline @KernelFeatures
    if ($LASTEXITCODE) { throw 'kernel build failed' }
    $ProbeFeatures = @()
    $FeatureNames = @()
    if ($WithMesa) { $FeatureNames += 'runtime-mesa-probe' }
    if ($WithGlibc) { $FeatureNames += 'runtime-glibc-probe' }
    if ($WithGlibc32) { $FeatureNames += 'runtime-glibc32-probe' }
    if ($WithVulkan) { $FeatureNames += 'runtime-vulkan-probe' }
    if ($FeatureNames.Count) { $ProbeFeatures = @('--features', ($FeatureNames -join ',')) }
    & cargo build --offline -p nexus-init --bin nexus-runtime-probe --target targets/x86_64-nexus-user.json '-Zbuild-std=core,compiler_builtins,alloc' '-Zbuild-std-features=compiler-builtins-mem' @ProbeFeatures
    if ($LASTEXITCODE) { throw 'probe build failed' }
    Publish-Program -Elf (Join-Path $Repo 'target/x86_64-nexus-user/debug/nexus-runtime-probe') -ProgramDir $Programs -Name 'init.elf' | Out-Null
    Publish-Esp -BootEfi (Join-Path $Repo 'build/esp/EFI/BOOT/BOOTX64.EFI') -KernelElf (Join-Path $Repo 'target/x86_64-nexus/debug/nexus-kernel') -EspDir $Esp | Out-Null
    # This focused runtime test requires only a small FAT partition and store.
    $RuntimeFatMiB = 40
    $RuntimeDiskMiB = 256
    if ($WithMesa -or $WithVulkan -or $WithGlibc32) { $RuntimeFatMiB = 96 }
    if ($WithGlibc32) {
        # The complete i386 runtime and its focused probes exceed the ordinary
        # FAT partition used by this runtime test.
        $RuntimeFatMiB = 192
        $RuntimeDiskMiB = 384
    }
    & (Join-Path $PSScriptRoot 'make-disk.ps1') -Output (Join-Path $Run 'nexus-disk.img') -ProgramDir $Programs -SizeMiB $RuntimeDiskMiB -FatMiB $RuntimeFatMiB
} finally { Pop-Location }
# Make the normal disk-size assertion describe this test's intentionally small disk.
function Get-NexusDiskSizes { return @{SizeMiB=$RuntimeDiskMiB; FatMiB=$RuntimeFatMiB} }
$Qemu = (Get-Command qemu-system-x86_64).Source
$Share = Join-Path (Split-Path $Qemu -Parent) 'share'
$Code = Join-Path $Run 'code.fd'
$Vars = Join-Path $Run 'vars.fd'
Copy-Item (Join-Path $Share 'edk2-x86_64-code.fd') $Code
Copy-Item (Join-Path $Share 'edk2-i386-vars.fd') $Vars
$Serial = Join-Path $Run 'serial.log'
$MonitorPort = Get-FreeMonitorPort
$QemuArgs = Get-NexusQemuArgs -BuildDir $Run -EspDir $Esp -FirmwareCode $Code -FirmwareVars $Vars -SerialLog $Serial -MonitorPort $MonitorPort -HostHttpPort (Get-FreeMonitorPort -From 35000 -To 36000) -Headless
$Process = Start-NexusQemu -Executable $Qemu -Arguments $QemuArgs -ErrorLog (Join-Path $Run 'qemu-error.log')
$Passed = $false
$Connection = $null
function Wait-RuntimeMarker([string]$Marker) {
    $MarkerDeadline = [DateTime]::UtcNow.AddSeconds($Timeout)
    while ([DateTime]::UtcNow -lt $MarkerDeadline -and -not $Process.HasExited) {
        if (Test-Path $Serial) {
            $MarkerLog = (Get-Content $Serial -Raw) -replace "`0", ''
            if ($MarkerLog -match 'libwayland-probe: FAIL|KERNEL PANIC|user fault') { throw "Wayland runtime failure: $Serial" }
            if ($MarkerLog.Contains($Marker)) { return }
        }
        Start-Sleep -Milliseconds 500
    }
    if ($null -ne $Connection) {
        Invoke-Screendump -Writer $Writer -Path (Join-Path $Run 'failure.ppm')
        Convert-PpmToPng -PpmPath (Join-Path $Run 'failure.ppm') -PngPath (Join-Path $Run 'failure.png')
    }
    throw "Missing '$Marker': $Serial"
}
try {
    $Deadline = [DateTime]::UtcNow.AddSeconds($Timeout)
    while ([DateTime]::UtcNow -lt $Deadline -and -not $Process.HasExited) {
        Start-Sleep -Milliseconds 500
        if (-not (Test-Path $Serial)) { continue }
        $Log = (Get-Content $Serial -Raw) -replace "`0", ''
        if ($Log -match 'runtime-probe: FAIL|runtime-probe: PANIC|KERNEL PANIC|\[test\] FAILED|the system has been halted') { throw "Runtime failure: $Serial" }
        if ($Log.Contains('runtime-probe: PASS all configured runtimes')) { $Passed = $true; break }
    }
    if (-not $Passed) { throw "Runtime test did not finish: $Serial" }
    if (-not $Log.Contains('[test] private address snapshot verified:')) {
        throw "Missing native private address-space snapshot evidence: $Serial"
    }
    Write-Host 'PASS: native private address snapshot, independent writes/unmaps, permissions/guards and failed-copy cleanup'
    $ExitGroupRuntimes = @('musl')
    if ($WithGlibc) { $ExitGroupRuntimes += 'glibc' }
    if ($WithGlibc32) { $ExitGroupRuntimes += 'glibc32' }
    foreach ($ExitRuntime in $ExitGroupRuntimes) {
        $ExitMarker = "runtime-probe: PASS $ExitRuntime group exit keeps status 42 with eight active siblings"
        if ([regex]::Matches($Log, [regex]::Escape($ExitMarker)).Count -ne 8) {
            throw "Missing eight group-exit regressions '$ExitRuntime': $Serial"
        }
    }
    if ($WithGlibc32 -and [regex]::Matches($Log, 'glibc32-probe: PASS i386 socketcall packed words stream IPC and checked peer buffers').Count -ne 3) {
        throw "Missing three i386 socketcall variants: $Serial"
    }
    if (-not $Log.Contains('runtime-probe: PASS native symlink type bytes authority and last-close identity')) {
        throw "Missing native symlink authority evidence: $Serial"
    }
    if ($WithGlibc32) {
        # Console writes can be shorter than a libc's buffered output. Join
        # the serial logger's chunk wrappers without losing application bytes.
        $GuestLog = [regex]::Replace($Log, '\r?\n\[linux\] \S+ wrote: ', '')
        foreach ($I386Marker in @('glibc32-probe: PASS get/set_thread_area slot allocation exhaustion and descriptor validation', 'glibc32-probe: PASS pthread create join and independent TLS', 'glibc32-probe: PASS futex unchanged wake zero-count shared aliases and deadlines', 'glibc32-probe: PASS 80 immediate thread exits and repeated joins', 'glibc32-probe: PASS four concurrent TLS owners across 4096 scheduler yields', 'glibc32-probe: PASS 256 concurrent pipe/poll round trips without lost notifications', 'glibc32-probe: PASS real dynamic loader dlopen symbol and library TLS')) {
            if (-not $GuestLog.Contains($I386Marker)) { throw "Missing i386 runtime evidence '$I386Marker': $Serial" }
        }
        Write-Host 'PASS: real i386 GNU libc loader, allocation, stat64/statx, pthread GS TLS, futex/join and dlopen in NexusOS'
    }
    $SemaphoreGuestLog = [regex]::Replace($Log, '\r?\n\[linux\] \S+ wrote: ', '')
    $SemaphoreRuntimes = @('musl-probe')
    if ($WithGlibc) { $SemaphoreRuntimes += 'glibc-probe' }
    if ($WithGlibc32) { $SemaphoreRuntimes += 'glibc32-probe' }
    foreach ($ProcRuntime in $SemaphoreRuntimes) {
        foreach ($ProcResult in @('executable identity live proc arguments/environment and shared cursor', 'real directory enumeration and executable inode', 'cwd parent traversal dirfd rename identity and removed directories', 'persistent symlinks follow lstat readlink loops dirfd and safe unlink', 'native urandom character metadata reads bounds dup flags and readiness')) {
            $ProcMarker = "${ProcRuntime}: PASS $ProcResult"
            if ([regex]::Matches($SemaphoreGuestLog, [regex]::Escape($ProcMarker)).Count -ne 3) {
                throw "Missing basic/thread/dynamic proc evidence '$ProcRuntime $ProcResult': $Serial"
            }
        }
        foreach ($RuntimeABIResult in @('flock shared/exclusive conflicts dup lifetime and proc descriptors', 'statfs and fstatfs filesystem identity and capacity', 'sigaltstack install query disable and minimum size')) {
            $RuntimeABIMarker = "${ProcRuntime}: PASS $RuntimeABIResult"
            if ([regex]::Matches($SemaphoreGuestLog, [regex]::Escape($RuntimeABIMarker)).Count -ne 3) {
                throw "Missing basic/thread/dynamic Linux ABI evidence '$ProcRuntime $RuntimeABIResult': $Serial"
            }
        }
    }
    if ($WithGlibc32 -and [regex]::Matches($SemaphoreGuestLog, 'glibc32-probe: PASS i386 sysinfo and robust-list ABI layouts').Count -ne 3) {
        throw "Missing three i386 sysinfo/robust-list variants: $Serial"
    }
    foreach ($ExecRuntime in @('musl-probe', $(if ($WithGlibc) { 'glibc-probe' }))) {
        if ($ExecRuntime -and -not $SemaphoreGuestLog.Contains("${ExecRuntime}: PASS relative execve retains cwd and actual executable identity")) {
            throw "Missing relative exec/cwd evidence '$ExecRuntime': $Serial"
        }
        if ($ExecRuntime -and -not $SemaphoreGuestLog.Contains("${ExecRuntime}: PASS execve refreshes image identity and inherited proc descriptor")) {
            throw "Missing execve/proc evidence '$ExecRuntime': $Serial"
        }
    }
    Write-Host 'PASS: genuine executable identity, live proc bytes, duplicated cursors and execve metadata'
    Write-Host 'PASS: real cwd/chdir/fchdir, relative dirfd operations, rename identity and relative execve'
    foreach ($SemaphoreRuntime in $SemaphoreRuntimes) {
        foreach ($SemaphoreMarker in @('semaphore metadata atomic rollback limits deadlines and stale IDs', 'semaphore negative/zero wait wake counters and removal', 'semaphore metadata racing 80 clones without lock inversion', 'eventfd counter semaphore limits buffer validation and shared flags', '128 concurrent eventfd/epoll round trips and blocking overflow wake')) {
            if (-not $SemaphoreGuestLog.Contains("${SemaphoreRuntime}: PASS $SemaphoreMarker")) {
                throw "Missing semaphore evidence '$SemaphoreRuntime $SemaphoreMarker': $Serial"
            }
        }
        $UndoMarker = 'shared and independent SEM_UNDO thread exit ownership'
        if ($SemaphoreRuntime -eq 'musl-probe') { $UndoMarker = 'shared SEM_UNDO thread exit ownership' }
        if (-not $SemaphoreGuestLog.Contains("${SemaphoreRuntime}: PASS $UndoMarker")) {
            throw "Missing undo evidence '$SemaphoreRuntime': $Serial"
        }
    }
    if ($WithGlibc32 -and -not $SemaphoreGuestLog.Contains('glibc32-probe: PASS old i386 ipc and direct semaphore/time64 ABI')) {
        throw "Missing legacy/direct semaphore ABI evidence: $Serial"
    }
    Write-Host 'PASS: real libc System V semaphore transactions, waits, removal, undo ownership and eventfd/epoll'
    if ($DeepFilesystemChecks) {
        foreach ($FilesystemMarker in @('NexusFS verified:', 'check verified:', 'journal verified:', 'symlinks verified:')) {
            if (-not $Log.Contains($FilesystemMarker)) { throw "Missing filesystem check '$FilesystemMarker': $Serial" }
        }
        Write-Host 'PASS: double-indirect files, remount/fsck, zero gaps, shrink/free and journal recovery'
    }
    Write-Host 'PASS: real musl allocation, files, clocks, pthread TLS/join and dynamic linking in NexusOS'
    if ($WithGlibc) { Write-Host 'PASS: real GNU libc allocation, files, clocks, pthread TLS/join and dlopen in NexusOS' }
    if ($WithVulkan) { Write-Host 'PASS: upstream Vulkan ICD compiled and executed SPIR-V with host-visible memory' }
    if ($WithMesa) { Write-Host 'PASS: real OSMesa softpipe OpenGL depth and GLSL rendered pixels' }
    if ($WithWayland) {
        Wait-RuntimeMarker 'setup: this machine has not been set up; asking'
        $Connection = [Net.Sockets.TcpClient]::new('127.0.0.1', $MonitorPort)
        $Writer = [IO.StreamWriter]::new($Connection.GetStream())
        $Writer.AutoFlush = $true
        foreach ($Key in @('ret','ret','n','e','x','u','s','ret','p','a','s','s','w','o','r','d','ret','p','a','s','s','w','o','r','d','ret','ret')) {
            $Writer.WriteLine("sendkey $Key")
            Start-Sleep -Milliseconds 250
        }
        Wait-RuntimeMarker 'shell: took the strip along the bottom of the screen'
        if ($WithUIControls) {
            $PointerX = 960
            $PointerY = 600
            function Move-DesktopPoint([int]$X, [int]$Y) {
                while ($PointerX -ne $X -or $PointerY -ne $Y) {
                    # Windows PowerShell 5.1 uses .NET Framework, which has no
                    # Math.Clamp. Keep the same bounded PS/2 movement there.
                    $Dx = [math]::Max(-60, [math]::Min(60, ($X - $PointerX)))
                    $Dy = [math]::Max(-60, [math]::Min(60, ($Y - $PointerY)))
                    $Writer.WriteLine("mouse_move $Dx $Dy")
                    $script:PointerX += $Dx
                    $script:PointerY += $Dy
                    Start-Sleep -Milliseconds 80
                }
            }
            function Click-DesktopPoint([int]$X, [int]$Y) {
                Move-DesktopPoint $X $Y
                $Writer.WriteLine('mouse_button 1')
                Start-Sleep -Milliseconds 150
                $Writer.WriteLine('mouse_button 0')
                Start-Sleep -Milliseconds 500
            }
            function Capture-Desktop([string]$Name) {
                Move-DesktopPoint 1400 500
                $DesktopPpm = Join-Path $Run "$Name.ppm"
                # Input delivery/rendering can lag the HMP writes during the
                # startup animation or resize. Wait for the actual pointer in
                # a captured frame rather than accepting an arbitrary delay.
                $PointerDeadline = [DateTime]::UtcNow.AddSeconds(20)
                do {
                    Invoke-Screendump -Writer $Writer -Path $DesktopPpm
                    & python (Join-Path $Repo 'tools/nexus-runtime/verify-desktop.py') --pointer $DesktopPpm
                    if ($LASTEXITCODE -eq 0) { break }
                    if ([DateTime]::UtcNow -ge $PointerDeadline) { throw "Pointer did not settle at the capture position: $DesktopPpm" }
                    Start-Sleep -Milliseconds 250
                } while ($true)
                Convert-PpmToPng -PpmPath $DesktopPpm -PngPath (Join-Path $Run "$Name.png")
            }
            Capture-Desktop 'desktop'
            Click-DesktopPoint 559 223
            Wait-RuntimeMarker 'compositor: decoration maximised the window'
            Capture-Desktop 'maximised'
            Click-DesktopPoint 65 31
            Wait-RuntimeMarker 'compositor: decoration restored the window'
            Capture-Desktop 'restored'
            Click-DesktopPoint 543 223
            Wait-RuntimeMarker 'compositor: decoration minimised the window'
            Capture-Desktop 'minimised'
            # The second stable window indicator restores the second window.
            Click-DesktopPoint 724 1190
            Start-Sleep -Milliseconds 500
            Capture-Desktop 'dock-restored'
            Click-DesktopPoint 527 223
            Wait-RuntimeMarker 'compositor: decoration requested closing'
            Wait-RuntimeMarker 'client: accepted decoration close'
            Capture-Desktop 'closed'
            & python (Join-Path $Repo 'tools/nexus-runtime/verify-desktop.py') $Run
            if ($LASTEXITCODE) { throw "Desktop control pixels failed: $Run" }
            Write-Host 'PASS: modern desktop decoration maximise/restore/minimise/Dock/close controls'
        }
        $Writer.WriteLine('sendkey f5')
        Wait-RuntimeMarker 'wlserver: listening on /tmp/wayland-0'
        $Writer.WriteLine('sendkey f6')
        Wait-RuntimeMarker 'libwayland-probe: committed 240x150'
        Wait-RuntimeMarker 'libwayland-probe: frame callback 2'
        Wait-RuntimeMarker 'libwayland-probe: keyboard focus'
        $Ppm = Join-Path $Run 'wayland.ppm'
        Invoke-Screendump -Writer $Writer -Path $Ppm
        $PixelOptions = @()
        if ($WithMesa) { $PixelOptions = @('--mesa') }
        & python (Join-Path $Repo 'tools/nexus-runtime/verify-wayland.py') $Ppm @PixelOptions
        if ($LASTEXITCODE) { throw "Wayland pixels failed: $Ppm" }
        Convert-PpmToPng -PpmPath $Ppm -PngPath (Join-Path $Run 'wayland.png')
        $Writer.WriteLine('sendkey a')
        Wait-RuntimeMarker 'libwayland-probe: PASS frames resize input buffer-release and close'
        Write-Host 'PASS: upstream libwayland frames, visible resize, keyboard input, buffer release and close'
    }
} finally {
    if ($null -ne $Connection) { $Connection.Close() }
    if (-not $Process.HasExited) { $Process.Kill(); $Process.WaitForExit(5000) | Out-Null }
    Write-Host "Evidence: $Serial"
}
