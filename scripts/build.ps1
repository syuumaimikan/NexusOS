<#
.SYNOPSIS
    Builds NexusOS and stages a bootable EFI System Partition tree.

.DESCRIPTION
    Compiles the Nexus Bootloader for x86_64-unknown-uefi and the Nexus Kernel
    for the custom x86_64-nexus target, then lays them out under build/esp in
    the arrangement UEFI firmware expects:

        EFI/BOOT/BOOTX64.EFI   the bootloader, at the removable-media path
        nexus/kernel.elf       the kernel image the bootloader looks for

.PARAMETER Release
    Build with optimisations (cargo --release).

.PARAMETER Clean
    Remove build outputs before building.
#>
[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$Clean,
    # Build the kernel with its destructive filesystem checks on.
    #
    # What the test suite uses. They write a thousand blocks across the store,
    # which is worth doing to a disk that exists for testing and is not worth
    # doing to somebody's machine every time it starts.
    [switch]$DeepSelfTest
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'stage.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'

if ($Clean) {
    Write-Host '==> Cleaning' -ForegroundColor Cyan
    if (Test-Path $BuildDir) { Remove-Item -Recurse -Force $BuildDir }
    Push-Location $RepoRoot
    try { cargo clean } finally { Pop-Location }
}

if ($Release) {
    $Profile = 'release'
    $ProfileArgs = @('--release')
} else {
    $Profile = 'debug'
    $ProfileArgs = @()
}

Push-Location $RepoRoot
try {
    Write-Host "==> Building nexus-boot (x86_64-unknown-uefi, $Profile)" -ForegroundColor Cyan
    # The `bootloader` and `kernel` aliases in .cargo/config.toml carry the
    # target and build-std flags each component needs.
    $bootArgs = @('+nightly', 'bootloader') + $ProfileArgs
    & cargo @bootArgs
    if ($LASTEXITCODE -ne 0) { throw "bootloader build failed (exit $LASTEXITCODE)" }

    Write-Host "==> Building nexus-kernel (x86_64-nexus, $Profile)" -ForegroundColor Cyan
    $kernelArgs = @('+nightly', 'kernel') + $ProfileArgs
    if ($DeepSelfTest) { $kernelArgs += @('--features', 'deep-selftest') }
    & cargo @kernelArgs
    if ($LASTEXITCODE -ne 0) { throw "kernel build failed (exit $LASTEXITCODE)" }
} finally {
    Pop-Location
}

$BootEfi = Join-Path $RepoRoot "target\x86_64-unknown-uefi\$Profile\nexus-boot.efi"
$KernelElf = Join-Path $RepoRoot "target\x86_64-nexus\$Profile\nexus-kernel"

Write-Host '==> Staging EFI System Partition tree' -ForegroundColor Cyan
$staged = Publish-Esp -BootEfi $BootEfi -KernelElf $KernelElf -EspDir $EspDir

$bootSize = $staged.BootSize
$kernelSize = $staged.KernelSize
$unstrippedSize = $staged.UnstrippedSize

# The programs the kernel loads from disk. Their own target -- the same
# custom-target machinery as the kernel, with the small code model, because they
# live in the low half of the address space rather than the top two gigabytes.
Push-Location $RepoRoot
try {
    Write-Host "==> Building user programs (x86_64-nexus-user, $Profile)" -ForegroundColor Cyan
    $userArgs = @('+nightly', 'user') + $ProfileArgs
    & cargo @userArgs
    if ($LASTEXITCODE -ne 0) { throw "user program build failed (exit $LASTEXITCODE)" }
} finally {
    Pop-Location
}

$ProgramDir = Join-Path $BuildDir 'programs'
$InitElf = Join-Path $RepoRoot "target\x86_64-nexus-user\$Profile\nexus-init"
$StagedInit = Publish-Program -Elf $InitElf -ProgramDir $ProgramDir -Name 'init.elf'
$initSize = [math]::Round((Get-Item $StagedInit).Length / 1KB, 1)

$HelloElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-hello"
$StagedHello = Publish-Program -Elf $HelloElf -ProgramDir $ProgramDir -Name 'hello.elf'
$helloSize = [math]::Round((Get-Item $StagedHello).Length / 1KB, 1)

$CompositorElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-compositor"
$StagedCompositor = Publish-Program -Elf $CompositorElf -ProgramDir $ProgramDir -Name 'comp.elf'
$compositorSize = [math]::Round((Get-Item $StagedCompositor).Length / 1KB, 1)

$ClientElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-client"
$StagedClient = Publish-Program -Elf $ClientElf -ProgramDir $ProgramDir -Name 'client.elf'
$clientSize = [math]::Round((Get-Item $StagedClient).Length / 1KB, 1)

$ShellElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-shell"
$StagedShell = Publish-Program -Elf $ShellElf -ProgramDir $ProgramDir -Name 'shell.elf'
$shellSize = [math]::Round((Get-Item $StagedShell).Length / 1KB, 1)

$SetupElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-setup"
$StagedSetup = Publish-Program -Elf $SetupElf -ProgramDir $ProgramDir -Name 'setup.elf'
$setupSize = [math]::Round((Get-Item $StagedSetup).Length / 1KB, 1)

$WallElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-wall"
$StagedWall = Publish-Program -Elf $WallElf -ProgramDir $ProgramDir -Name 'wall.elf'
$wallSize = [math]::Round((Get-Item $StagedWall).Length / 1KB, 1)

$SettingsElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-settings"
$StagedSettings = Publish-Program -Elf $SettingsElf -ProgramDir $ProgramDir -Name 'set.elf'
$settingsSize = [math]::Round((Get-Item $StagedSettings).Length / 1KB, 1)

$AssistElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-assist"
$StagedAssist = Publish-Program -Elf $AssistElf -ProgramDir $ProgramDir -Name 'assist.elf'
$assistSize = [math]::Round((Get-Item $StagedAssist).Length / 1KB, 1)

# GPT-6 Astra's read-only AI service, staged at their request so that a normal
# build carries it. Nothing starts it yet: it is a service a program would be
# handed a channel to, and no program is handed one.
$AiElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-ai"
$StagedAi = Publish-Program -Elf $AiElf -ProgramDir $ProgramDir -Name 'ai.elf'
$aiSize = [math]::Round((Get-Item $StagedAi).Length / 1KB, 1)

$ViewElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-view"
$StagedView = Publish-Program -Elf $ViewElf -ProgramDir $ProgramDir -Name 'view.elf'
$viewSize = [math]::Round((Get-Item $StagedView).Length / 1KB, 1)

$LaunchElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-launch"
$StagedLaunch = Publish-Program -Elf $LaunchElf -ProgramDir $ProgramDir -Name 'launch.elf'
$launchSize = [math]::Round((Get-Item $StagedLaunch).Length / 1KB, 1)

$StoreElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-store"
$StagedStore = Publish-Program -Elf $StoreElf -ProgramDir $ProgramDir -Name 'store.elf'
$storeSize = [math]::Round((Get-Item $StagedStore).Length / 1KB, 1)

$TermElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-term"
$StagedTerm = Publish-Program -Elf $TermElf -ProgramDir $ProgramDir -Name 'term.elf'
$termSize = [math]::Round((Get-Item $StagedTerm).Length / 1KB, 1)

$BrowserElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-browser"
$StagedBrowser = Publish-Program -Elf $BrowserElf -ProgramDir $ProgramDir -Name 'browse.elf'
$browserSize = [math]::Round((Get-Item $StagedBrowser).Length / 1KB, 1)

$UpdateElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-updater"
$StagedUpdate = Publish-Program -Elf $UpdateElf -ProgramDir $ProgramDir -Name 'updt.elf'
$updateSize = [math]::Round((Get-Item $StagedUpdate).Length / 1KB, 1)

$FindElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-find"
$StagedFind = Publish-Program -Elf $FindElf -ProgramDir $ProgramDir -Name 'find.elf'
$findSize = [math]::Round((Get-Item $StagedFind).Length / 1KB, 1)

$InstallElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-install"
$StagedInstall = Publish-Program -Elf $InstallElf -ProgramDir $ProgramDir -Name 'inst.elf'
$installSize = [math]::Round((Get-Item $StagedInstall).Length / 1KB, 1)

$IdleElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-idle"
$StagedIdle = Publish-Program -Elf $IdleElf -ProgramDir $ProgramDir -Name 'idle.elf'
$idleSize = [math]::Round((Get-Item $StagedIdle).Length / 1KB, 1)

# A program built for Linux, emitted by a tool that writes out every byte of the
# ELF with the field it belongs to named beside it. Nothing about it is
# NexusOS's: it is ET_EXEC, EM_X86_64, ELFOSABI_SYSV with no interpreter, and
# its machine code makes requests with Linux's own call numbers. Running it is
# the whole claim of the compatibility layer, and a binary that had been shaped
# to suit would prove nothing.
Write-Host '==> Emitting a Linux executable' -ForegroundColor Cyan
Push-Location $RepoRoot
try {
    & cargo build --offline -q -p nexus-linux-example
    if ($LASTEXITCODE -ne 0) { throw 'could not build nexus-linux-example' }
} finally { Pop-Location }

$LinuxExe = Join-Path $RepoRoot 'target\debug\nexus-linux-example.exe'
$LinuxProgram = Join-Path $ProgramDir 'hello.lx'
& $LinuxExe $LinuxProgram 'a program built for Linux, running on NexusOS'
if ($LASTEXITCODE -ne 0) { throw 'could not emit the Linux program' }
$linuxSize = (Get-Item $LinuxProgram).Length

# A package, built on this machine by the tool that makes them and read on the
# other side by the program that installs them. Both use `shared/nexus-pkg`, so
# the format has exactly one implementation: a packer with its own idea of the
# layout is a packer that agrees with the installer until somebody changes one
# of them.
Write-Host '==> Packing a package' -ForegroundColor Cyan
Push-Location $RepoRoot
try {
    & cargo build --offline -q -p nexus-pack
    if ($LASTEXITCODE -ne 0) { throw 'could not build nexus-pack' }
} finally { Pop-Location }

$PackExe = Join-Path $RepoRoot 'target\debug\nexus-pack.exe'
$PackageDir = Join-Path $BuildDir 'package'
New-Item -ItemType Directory -Force -Path $PackageDir | Out-Null

# What goes inside. Written here rather than committed, because the interesting
# part is the format and the install, not the contents -- and a file generated
# at build time cannot drift out of step with what the test expects to read.
#
# Written without a byte-order mark. `Set-Content -Encoding utf8` on Windows
# PowerShell puts one at the front, and a package carries exactly the bytes it
# was given -- so the file installed on the other side began with three bytes
# nobody expected and the program reading it back saw a string that did not
# start where it should. The package was right; the source of it was not.
$utf8 = New-Object System.Text.UTF8Encoding $false
$greeting = Join-Path $PackageDir 'hello.txt'
[System.IO.File]::WriteAllText($greeting, @'
installed from a package, verified twice: once before anything was written,
and once after it had been read back off the disk.
'@, $utf8)
$notes = Join-Path $PackageDir 'notes.txt'
[System.IO.File]::WriteAllText($notes, @'
A second file, so that the entry table has to be walked rather than guessed.
'@, $utf8)

# A picture, so the viewer has something to show on a machine nobody has put a
# file on yet. Staged beside the programs because the kernel copies `.PNG` out
# of the image's program directory onto the store, the same way it does `.NEX`.
$Picture = Join-Path $ProgramDir 'nexus.png'
& powershell -NoProfile -File (Join-Path $PSScriptRoot 'make-picture.ps1') -OutputFile $Picture | Out-Null
if ($LASTEXITCODE -ne 0) { throw "could not draw the picture (exit $LASTEXITCODE)" }
$pictureSize = [math]::Round((Get-Item $Picture).Length / 1KB, 1)

# And a recording, for the same reason and by the same route: `.AVI` is copied
# out of the image's program directory into PICTURES alongside the stills.
$Video = Join-Path $ProgramDir 'nexus.avi'
& powershell -NoProfile -File (Join-Path $PSScriptRoot 'make-video.ps1') -OutputFile $Video | Out-Null
if ($LASTEXITCODE -ne 0) { throw "could not draw the recording (exit $LASTEXITCODE)" }
$videoSize = [math]::Round((Get-Item $Video).Length / 1KB, 1)

$Package = Join-Path $ProgramDir 'demo.nex'
$SigningKey = Join-Path $RepoRoot 'keys\development.key'
& $PackExe $Package $SigningKey 'demo' '1.0.0' "demo/hello.txt=$greeting" "demo/notes.txt=$notes"
if ($LASTEXITCODE -ne 0) { throw 'could not pack the package' }
$packageSize = [math]::Round((Get-Item $Package).Length / 1KB, 1)

# And the same package one release on, which is what an update actually is: the
# same name, a higher version, and a file that says something new. The machine
# has both on its disk and has to work out that one of them supersedes the
# other -- and, on the boot after that, that the older one no longer does.
$NewNotes = Join-Path $PackageDir 'notes-1.1.txt'
[System.IO.File]::WriteAllText($NewNotes, @'
A second file, so that the entry table has to be walked rather than guessed.
And a third line, which is what changed in release 1.1.0.
'@, $utf8)
$Newer = Join-Path $ProgramDir 'demo11.nex'
& $PackExe $Newer $SigningKey 'demo' '1.1.0' "demo/hello.txt=$greeting" "demo/notes.txt=$NewNotes"
if ($LASTEXITCODE -ne 0) { throw 'could not pack the newer package' }
$newerSize = [math]::Round((Get-Item $Newer).Length / 1KB, 1)

# And a second package that is the first one with a byte changed after it was
# signed. Nothing about it is special: it is what a package looks like when
# somebody with no key alters it in transit, and the machine has to refuse it.
# A system that only ever sees packages that are correct is a system whose
# checking has never been exercised.
$Tampered = Join-Path $ProgramDir 'bad.nex'
$bytes = [System.IO.File]::ReadAllBytes($Package)
# The last byte, which is inside the payload rather than the header -- so what
# the machine notices is the content of a file, not a field it parses.
$bytes[$bytes.Length - 1] = $bytes[$bytes.Length - 1] -bxor 0x01
[System.IO.File]::WriteAllBytes($Tampered, $bytes)

# The disk the kernel drives. Data only, with nothing to boot from: a machine
# given two bootable disks leaves the firmware to choose between them, and it
# chose the one whose kernel was older. Made once and left alone, because the
# tests write to it and rebuilding would erase what a previous run proved.
#
# The bootable image is a different file, made by test-image.ps1, which is the
# only run that is about whether an image boots.
$DiskImage = Join-Path $BuildDir 'nexus-disk.img'
$stale = -not (Test-Path $DiskImage)
if (-not $stale) {
    $imageTime = (Get-Item $DiskImage).LastWriteTimeUtc
    $newest = Get-ChildItem -Path $ProgramDir -File |
        Sort-Object LastWriteTimeUtc -Descending |
        Select-Object -First 1
    if ($newest -and $newest.LastWriteTimeUtc -gt $imageTime) { $stale = $true }
}
if ($stale) {
    Write-Host '==> Making the disk image' -ForegroundColor Cyan
    & powershell -NoProfile -ExecutionPolicy Bypass `
        -File (Join-Path $PSScriptRoot 'make-disk.ps1') -Output $DiskImage -ProgramDir $ProgramDir
    if ($LASTEXITCODE -ne 0) { throw 'could not make the disk image' }
}

Write-Host ''
Write-Host 'NexusOS build complete' -ForegroundColor Green
Write-Host "  bootloader : $bootSize KiB  -> EFI\BOOT\BOOTX64.EFI"
Write-Host "  kernel     : $kernelSize KiB  -> nexus\kernel.elf  (from $unstrippedSize KiB with symbols)"
Write-Host "  init       : $initSize KiB  -> BIN\INIT.ELF on the disk"
Write-Host "  hello      : $helloSize KiB  -> BIN\HELLO.ELF on the disk"
Write-Host "  compositor : $compositorSize KiB  -> BIN\COMP.ELF on the disk"
Write-Host "  client     : $clientSize KiB  -> BIN\CLIENT.ELF on the disk"
Write-Host "  shell      : $shellSize KiB  -> BIN\SHELL.ELF on the disk"
Write-Host "  installer  : $installSize KiB  -> BIN\INST.ELF on the disk"
Write-Host "  finder     : $findSize KiB  -> BIN\FIND.ELF on the disk"
Write-Host "  setup      : $setupSize KiB  -> BIN\SETUP.ELF on the disk"
Write-Host "  updater    : $updateSize KiB  -> BIN\UPDT.ELF on the disk"
Write-Host "  browser    : $browserSize KiB  -> BIN\BROWSE.ELF on the disk"
Write-Host "  terminal   : $termSize KiB  -> BIN\TERM.ELF on the disk"
Write-Host "  wallpaper  : $wallSize KiB  -> BIN\WALL.ELF on the disk"
Write-Host "  settings   : $settingsSize KiB  -> BIN\SET.ELF on the disk"
Write-Host "  packages   : $storeSize KiB  -> BIN\STORE.ELF on the disk"
Write-Host "  viewer     : $viewSize KiB  -> BIN\VIEW.ELF on the disk"
Write-Host "  launcher   : $launchSize KiB  -> BIN\LAUNCH.ELF on the disk"
Write-Host "  assistant  : $assistSize KiB  -> BIN\ASSIST.ELF on the disk"
Write-Host "  ai service : $aiSize KiB  -> BIN\AI.ELF on the disk"
Write-Host "  picture    : $pictureSize KiB  -> PICTURES\NEXUS.PNG on the disk"
Write-Host "  recording  : $videoSize KiB  -> PICTURES\NEXUS.AVI on the disk"
Write-Host "  package    : $packageSize KiB  -> PKG\DEMO.NEX on the disk (signed)"
Write-Host "  update     : $newerSize KiB  -> PKG\DEMO11.NEX on the disk (demo 1.1.0, signed)"
Write-Host "  tampered   : one byte changed after signing -> PKG\BAD.NEX on the disk"
Write-Host "  linux      : $linuxSize bytes of static Linux ELF -> BIN\HELLO.LX on the disk"
Write-Host "  idle       : $idleSize KiB  -> BIN\IDLE.ELF on the disk"
Write-Host "  ESP tree   : $EspDir"
Write-Host ''
Write-Host 'Run it with: .\scripts\run.ps1'
