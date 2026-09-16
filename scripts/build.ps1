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
# For `Get-NexusDiskSizes`: how big the disk is belongs in one place, with
# the rest of what the machine is.
. (Join-Path $PSScriptRoot 'qemu.ps1')

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

$GeminiElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-gemini"
$StagedGemini = Publish-Program -Elf $GeminiElf -ProgramDir $ProgramDir -Name 'gemini.elf'
$geminiSize = [math]::Round((Get-Item $StagedGemini).Length / 1KB, 1)

$ViewElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-view"
$StagedView = Publish-Program -Elf $ViewElf -ProgramDir $ProgramDir -Name 'view.elf'
$viewSize = [math]::Round((Get-Item $StagedView).Length / 1KB, 1)

$LaunchElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-launch"
$StagedLaunch = Publish-Program -Elf $LaunchElf -ProgramDir $ProgramDir -Name 'launch.elf'
$launchSize = [math]::Round((Get-Item $StagedLaunch).Length / 1KB, 1)

$EditElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-edit"
$StagedEdit = Publish-Program -Elf $EditElf -ProgramDir $ProgramDir -Name 'edit.elf'
$editSize = [math]::Round((Get-Item $StagedEdit).Length / 1KB, 1)

$FilesElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-files"
$StagedFiles = Publish-Program -Elf $FilesElf -ProgramDir $ProgramDir -Name 'files.elf'
$filesSize = [math]::Round((Get-Item $StagedFiles).Length / 1KB, 1)

# `ls`, which is a program now rather than a method on the shell. Small, and
# the first one on this machine that exists because a program can be given
# somewhere to write.
$LsElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-ls"
$StagedLs = Publish-Program -Elf $LsElf -ProgramDir $ProgramDir -Name 'ls.elf'
$lsSize = [math]::Round((Get-Item $StagedLs).Length / 1KB, 1)

# `count`, which reads its standard input to the end. The other half of a pipe:
# `ls | count` is one channel with each end handed to a different program.
$CountElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-count"
$StagedCount = Publish-Program -Elf $CountElf -ProgramDir $ProgramDir -Name 'count.elf'
$countSize = [math]::Round((Get-Item $StagedCount).Length / 1KB, 1)

# `cat`, `head` and `grep`: three programs from one crate, because what they
# share -- reading lines off a channel and writing lines back -- is most of what
# each of them is.
$textSizes = @()
foreach ($tool in @('cat', 'head', 'grep')) {
    $elf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-$tool"
    $staged = Publish-Program -Elf $elf -ProgramDir $ProgramDir -Name "$tool.elf"
    $textSizes += [math]::Round((Get-Item $staged).Length / 1KB, 1)
}

# `nex`, which runs a program written in the language this repository has. Not
# Python and not C, and named so nobody arrives expecting either.
$NexElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-nex"
$StagedNex = Publish-Program -Elf $NexElf -ProgramDir $ProgramDir -Name 'nex.elf'
$nexSize = [math]::Round((Get-Item $StagedNex).Length / 1KB, 1)

# The unpacker, which reads a downloaded archive and writes out what is in it.
# No window: the compositor starts it with handles and no surface, it says what
# it did, and it exits.
$UnpackElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-unpack"
$StagedUnpack = Publish-Program -Elf $UnpackElf -ProgramDir $ProgramDir -Name 'unpack.elf'
$unpackSize = [math]::Round((Get-Item $StagedUnpack).Length / 1KB, 1)

# The three-dimensional drawing, which is a window client like any other and
# needs nothing from the build beyond being on the disk. There is no graphics
# stack to link it against: every pixel it draws is worked out by the processor.
$SolidElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-solid"
$StagedSolid = Publish-Program -Elf $SolidElf -ProgramDir $ProgramDir -Name 'solid.elf'
$solidSize = [math]::Round((Get-Item $StagedSolid).Length / 1KB, 1)

# The network tool, which is a window client like any other and is lent the
# network and nothing else.
$NetoolElf = Join-Path $RepoRoot "target/x86_64-nexus-user/$Profile/nexus-netool"
$StagedNetool = Publish-Program -Elf $NetoolElf -ProgramDir $ProgramDir -Name 'netool.elf'
$netoolSize = [math]::Round((Get-Item $StagedNetool).Length / 1KB, 1)

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

# And a second one that asks for what a real libc asks for: anonymous memory,
# a scattered write, and the register that thread-local storage lives behind.
# The first proves the boundary exists; this proves it is wide enough to be
# worth having.
$RichProgram = Join-Path $ProgramDir 'rich.lx'
& $LinuxExe $RichProgram 'a linux program with memory of its own' rich
if ($LASTEXITCODE -ne 0) { throw 'could not emit the richer Linux program' }

# And a third, about files: it writes one, reads it back and checks the bytes,
# then lists a directory and reads its own auxiliary vector. Every step of it
# exits with its own number when what came back is wrong, so it is a test and
# not a demonstration.
$FilesProgram = Join-Path $ProgramDir 'files.lx'
& $LinuxExe $FilesProgram 'a linux program wrote a file and read it back' files
if ($LASTEXITCODE -ne 0) { throw 'could not emit the Linux file program' }
$filesLinuxSize = (Get-Item $FilesProgram).Length
$richSize = (Get-Item $RichProgram).Length

# And a fourth, which is a different kind of thing: a *dynamically linked*
# program and the interpreter its PT_INTERP names. Almost every real Linux
# program is one of these, and loading one is not loading a file -- it is
# loading two images into one address space, entering the second, and handing it
# the numbers it cannot work out for itself. The interpreter checks every one of
# those numbers and stops with its own status when one is wrong.
#
# It is not glibc's loader and does not claim to be: see the note at the top of
# tools/nexus-linux-example/src/dynamic.rs.
# A fifth, about threads: it makes one with `clone`, waits for it on a futex,
# and checks a value the new thread left in memory they share. A `clone` that
# returned a plausible number and made no thread would pass any check short of
# waiting for that thread to do something, so this waits.
$ThreadsProgram = Join-Path $ProgramDir 'thread.lx'
& $LinuxExe $ThreadsProgram 'two linux threads met at a futex' threads
if ($LASTEXITCODE -ne 0) { throw 'could not emit the threaded Linux program' }
$threadsSize = (Get-Item $ThreadsProgram).Length

# The programs built for Linux by the *compiler* rather than by hand.
#
# `rustc` can target `x86_64-unknown-linux-gnu` with `core` rebuilt from source,
# and `rust-lld` links a static ET_EXEC with no C runtime -- so a real Linux
# executable comes out of the toolchain this repository already uses, with no
# Linux toolchain and no network. See `.cargo/config.toml` for the flags.
#
# The hand-assembled fixtures above stay: they are the ones that prove the
# loader works on an image nobody could have shaped to suit, and one of them is
# a dynamic linker, which is not a thing a compiler will emit for you.
Write-Host '==> Building programs for Linux' -ForegroundColor Cyan
Push-Location $RepoRoot
try {
    & cargo guest --offline
    if ($LASTEXITCODE -ne 0) { throw 'could not build the Linux guest programs' }
} finally { Pop-Location }

# Stripped on the way in. These carry debug information -- the workspace asks
# for it and it is worth having when something goes wrong on this side -- and
# the guest reads the whole file into memory to load it, so two and a half
# megabytes of it per program is two and a half megabytes of the machine's
# memory spent on symbols nothing in the guest can read.
$GuestDir = Join-Path $RepoRoot 'target/x86_64-unknown-linux-gnu/release'
$GuestSysroot = (& rustc +nightly --print sysroot).Trim()
$GuestObjcopy = Join-Path $GuestSysroot 'lib/rustlib/x86_64-pc-windows-msvc/bin/llvm-objcopy.exe'
$GuestPrograms = @(
    @{ Name = 'posix'; File = 'guest-posix' },
    @{ Name = 'threads'; File = 'guest-threads' },
    @{ Name = 'signals'; File = 'guest-signals' },
    @{ Name = 'exec'; File = 'guest-exec' },
    @{ Name = 'execed'; File = 'guest-execed' },
    @{ Name = 'net'; File = 'guest-net' },
    @{ Name = 'wlsrv'; File = 'guest-wlserver' },
    @{ Name = 'wlcli'; File = 'guest-wlclient' }
)
$guestTotal = 0
foreach ($guest in $GuestPrograms) {
    $from = Join-Path $GuestDir $guest.File
    if (-not (Test-Path $from)) { throw "missing Linux guest program: $from" }
    $to = Join-Path $ProgramDir ('g' + $guest.Name + '.lx')
    if (Test-Path $GuestObjcopy) {
        & $GuestObjcopy --strip-all $from $to
        if ($LASTEXITCODE -ne 0) { throw "could not strip $($guest.File)" }
    } else {
        Copy-Item $from $to -Force
    }
    $guestTotal += (Get-Item $to).Length
}

# And one built for *i386*, which is a different world again: a different image
# format, a different system-call table, and `int 0x80` rather than `syscall`.
Write-Host '==> Building a program for i386 Linux' -ForegroundColor Cyan
Push-Location $RepoRoot
try {
    & cargo guest32 --offline
    if ($LASTEXITCODE -ne 0) { throw 'could not build the i386 guest program' }
} finally { Pop-Location }
$Guest32 = Join-Path $RepoRoot 'target/i686-unknown-linux-gnu/release/guest32'
if (-not (Test-Path $Guest32)) { throw "missing i386 guest program: $Guest32" }
$Guest32Staged = Join-Path $ProgramDir 'g32.lx'
if (Test-Path $GuestObjcopy) {
    & $GuestObjcopy --strip-all $Guest32 $Guest32Staged
    if ($LASTEXITCODE -ne 0) { throw 'could not strip the i386 guest program' }
} else {
    Copy-Item $Guest32 $Guest32Staged -Force
}
$guest32Size = (Get-Item $Guest32Staged).Length

# And one that draws. It opens `/dev/nexus/display`, asks how big its window
# is, maps the buffer shared with the compositor and fills it in two bands --
# which is what an X11 or Wayland client does with the connection and the object
# protocol taken away. The colours are specific so that a screenshot taken from
# outside the machine can be checked for them.
$DrawProgram = Join-Path $ProgramDir 'draw.lx'
& $LinuxExe $DrawProgram 'a linux program drew a window through the compositor' draw
if ($LASTEXITCODE -ne 0) { throw 'could not emit the drawing Linux program' }
$drawSize = (Get-Item $DrawProgram).Length

$DynamicInterp = Join-Path $ProgramDir 'ld.lx'
& $LinuxExe $DynamicInterp 'interpreter' interpreter
if ($LASTEXITCODE -ne 0) { throw 'could not emit the Linux interpreter' }
$DynamicProgram = Join-Path $ProgramDir 'dyn.lx'
& $LinuxExe $DynamicProgram 'a dynamically linked linux program ran through its interpreter' dynamic
if ($LASTEXITCODE -ne 0) { throw 'could not emit the dynamically linked Linux program' }
$dynamicSize = (Get-Item $DynamicProgram).Length

# Where those two have to end up. A dynamically linked program names its
# interpreter by an absolute path, and that path is resolved in the Linux root
# -- `linux/` in the machine's own store -- which the build cannot write to,
# because it is inside a filesystem only the running machine mounts. So the
# build leaves a list, and the kernel installs from it on the first boot. See
# `install_linux_runtime`.
#
# The program is installed too, not only the interpreter, because a Linux
# program's `argv[0]` has to be a path it can open: that is how it finds its own
# file, and it is what the interpreter reads back through a file mapping.
$utf8NoBom = New-Object System.Text.UTF8Encoding $false
[System.IO.File]::WriteAllText((Join-Path $ProgramDir 'linux.lst'), @'
# What to put in the Linux root, and where. One line per file:
#   <path on the EFI partition>  <path a Linux program will see>
BIN/LD.LX   /lib/ld-nexus-x86-64.so.1
BIN/DYN.LX  /usr/bin/dyn
BIN/DRAW.LX /usr/bin/draw
BIN/GPOSIX.LX   /usr/bin/posix
BIN/GTHREADS.LX /usr/bin/threads
BIN/GSIGNALS.LX /usr/bin/signals
BIN/GEXEC.LX    /usr/bin/exec
BIN/GEXECED.LX  /usr/bin/execed
BIN/GNET.LX     /usr/bin/net
BIN/GWLSRV.LX   /usr/bin/wayland-server
BIN/GWLCLI.LX   /usr/bin/wayland-client
BIN/G32.LX      /usr/bin/thirty-two
'@, $utf8NoBom)

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

# A Debian package, travelling the same way and landing where a downloaded one
# would: the kernel copies `.DEB` out of the image's program directory into
# DOWNLOAD, the way it copies `.PNG` into PICTURES.
#
# Committed as a file rather than built here, and that is the point of it. Every
# other example on this disk is made by this repository, and a reader tested
# against a writer written from the same reading of a specification agrees with
# itself and with nothing else. This one came out of the reference tools, holds
# a genuine x86-64 executable, and that executable names the loader every
# program built against a C library asks for -- so unpacking it has something
# true to say about what this machine cannot run.
$DemoDeb = Join-Path $ProgramDir 'demo.deb'
Copy-Item (Join-Path $RepoRoot 'assets\demo.deb') $DemoDeb -Force
$debSize = (Get-Item $DemoDeb).Length

# The root certificate store, which is what makes `https://` mean anything.
#
# Built here rather than committed as a binary, because the interesting property
# is that it was filtered by the machine's own X.509 parser: a certificate that
# went in and did not come out is one this machine could never have used, and
# the tool says so rather than leaving it to be found later as a site that will
# not load. Staged beside the programs because the kernel copies `.NXR` out of
# the image onto the store, the same way it does `.NEX` and `.PNG`.
Write-Host '==> Building the root certificate store' -ForegroundColor Cyan
Push-Location $RepoRoot
try {
    & cargo build --offline -q -p nexus-roots
    if ($LASTEXITCODE -ne 0) { throw 'could not build nexus-roots' }
} finally { Pop-Location }

$RootsExe = Join-Path $RepoRoot 'target\debug\nexus-roots.exe'
$RootsPem = Join-Path $RepoRoot 'roots\mozilla-ca-bundle.pem'
$RootsOut = Join-Path $ProgramDir 'roots.nxr'
& $RootsExe $RootsPem --out $RootsOut
if ($LASTEXITCODE -ne 0) { throw 'could not build the root certificate store' }
$rootsSize = [math]::Round((Get-Item $RootsOut).Length / 1KB, 1)

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

# The image behind the USB stick. Made every build rather than kept, because it
# is a fixture and not a record: the kernel writes to its last sector to prove
# it can, and a fixture that accumulated previous runs' writes would be one
# whose content nobody could predict.
$UsbImage = Join-Path $BuildDir 'nexus-usb.img'
& powershell -NoProfile -ExecutionPolicy Bypass `
    -File (Join-Path $PSScriptRoot 'make-usb.ps1') -OutputFile $UsbImage | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'could not write the USB image' }
$usbSize = [math]::Round((Get-Item $UsbImage).Length / 1KB, 1)

# And a second stick, with a real filesystem on it. Built by the same script
# that builds this machine's own disk, because it is the same thing: a GPT with
# a FAT32 partition. What it proves is different -- that a filesystem can be
# read off a drive whose sectors arrive over four layers of USB -- and having
# two drives also means the driver has to cope with more than one device.
$UsbFiles = Join-Path $BuildDir 'usb-files'
New-Item -ItemType Directory -Force -Path $UsbFiles | Out-Null
[System.IO.File]::WriteAllText(
    (Join-Path $UsbFiles 'readme.txt'),
    "A file on a USB stick, read by NexusOS over xHCI.`n", $utf8)
[System.IO.File]::WriteAllText(
    (Join-Path $UsbFiles 'notes.txt'),
    "Second file, so the directory has to be walked.`n", $utf8)

$UsbFsImage = Join-Path $BuildDir 'nexus-usb-fs.img'
if (-not (Test-Path $UsbFsImage)) {
    & powershell -NoProfile -ExecutionPolicy Bypass `
        -File (Join-Path $PSScriptRoot 'make-disk.ps1') `
        -Output $UsbFsImage -SourceDir $UsbFiles -SizeMiB 64 | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'could not make the formatted USB image' }
}
$usbFsSize = [math]::Round((Get-Item $UsbFsImage).Length / 1MB, 0)

# The disk the kernel drives. Data only, with nothing to boot from: a machine
# given two bootable disks leaves the firmware to choose between them, and it
# chose the one whose kernel was older. Made once and left alone, because the
# tests write to it and rebuilding would erase what a previous run proved.
#
# The bootable image is a different file, made by test-image.ps1, which is the
# only run that is about whether an image boots.
#
# Eight gigabytes of it, a quarter of a gigabyte as FAT32 for programs and the
# rest as NexusFS. It was sixty-four megabytes, which was enough for a machine
# whose whole software collection was three megabytes of its own programs and
# is not enough for one meant to hold software brought in from outside. The
# image is written sparsely-ish -- labelled sectors all the way down, at about a
# gigabyte a second -- so the size costs eight seconds and eight gigabytes of
# disk, and nothing at boot.
$DiskImage = Join-Path $BuildDir 'nexus-disk.img'
$sizes = Get-NexusDiskSizes
$DiskMiB = $sizes.SizeMiB
$DiskFatMiB = $sizes.FatMiB
$stale = -not (Test-Path $DiskImage)
if (-not $stale) {
    # An image of the wrong size is stale whatever its timestamp says. Without
    # this, changing the size above would leave every existing checkout running
    # on the old disk and wondering why it had not grown.
    if ((Get-Item $DiskImage).Length -ne [long]$DiskMiB * 1MB) { $stale = $true }
}
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
        -File (Join-Path $PSScriptRoot 'make-disk.ps1') -Output $DiskImage `
        -ProgramDir $ProgramDir -SizeMiB $DiskMiB -FatMiB $DiskFatMiB
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
Write-Host "  editor     : $editSize KiB  -> BIN\EDIT.ELF on the disk"
Write-Host "  files      : $filesSize KiB  -> BIN\FILES.ELF on the disk"
Write-Host "  ls         : $lsSize KiB  -> BIN\LS.ELF on the disk"
Write-Host "  count      : $countSize KiB  -> BIN\COUNT.ELF on the disk"
Write-Host "  cat/head/grep: $($textSizes -join '/') KiB  -> BIN\{CAT,HEAD,GREP}.ELF on the disk"
Write-Host "  nex        : $nexSize KiB  -> BIN\NEX.ELF on the disk"
Write-Host "  assistant  : $assistSize KiB  -> BIN\ASSIST.ELF on the disk"
Write-Host "  ai service : $aiSize KiB  -> BIN\AI.ELF on the disk"
Write-Host "  gemini agt : $geminiSize KiB  -> BIN\GEMINI.ELF on the disk"
Write-Host "  unpack     : $unpackSize KiB  -> BIN\UNPACK.ELF on the disk"
Write-Host "  solid      : $solidSize KiB  -> BIN\SOLID.ELF on the disk"
Write-Host "  netool     : $netoolSize KiB  -> BIN\NETOOL.ELF on the disk"
Write-Host "  download   : $debSize bytes of Debian package -> DOWNLOAD\DEMO.DEB on the disk"
Write-Host "  picture    : $pictureSize KiB  -> PICTURES\NEXUS.PNG on the disk"
Write-Host "  recording  : $videoSize KiB  -> PICTURES\NEXUS.AVI on the disk"
Write-Host "  root store : $rootsSize KiB  -> SYSTEM\ROOTS.NXR on the disk"
Write-Host "  usb stick  : $usbSize KiB, every sector numbered  -> plugged into qemu-xhci"
Write-Host "  usb stick 2: $usbFsSize MiB, GPT with FAT32 and two files  -> plugged in beside it"
Write-Host "  package    : $packageSize KiB  -> PKG\DEMO.NEX on the disk (signed)"
Write-Host "  update     : $newerSize KiB  -> PKG\DEMO11.NEX on the disk (demo 1.1.0, signed)"
Write-Host "  tampered   : one byte changed after signing -> PKG\BAD.NEX on the disk"
Write-Host "  linux      : $linuxSize bytes of static Linux ELF -> BIN\HELLO.LX on the disk"
Write-Host "  linux+     : $richSize bytes, mmap and writev -> BIN\RICH.LX on the disk"
Write-Host "  linux files: $filesLinuxSize bytes, opens and reads a file -> BIN\FILES.LX on the disk"
Write-Host "  linux thrd : $threadsSize bytes, clone and futex -> BIN\THREAD.LX on the disk"
Write-Host "  linux dyn  : $dynamicSize bytes, ET_DYN with PT_INTERP -> installed at /usr/bin/dyn"
Write-Host "  linux draw : $drawSize bytes, a window through the compositor -> /usr/bin/draw"
Write-Host ("  linux built: {0:N0} bytes of compiled Linux programs -> /usr/bin/ in the Linux root" -f $guestTotal)
Write-Host ("  linux i386 : {0:N0} bytes, ET_EXEC/EM_386 -> /usr/bin/thirty-two" -f $guest32Size)
Write-Host "  idle       : $idleSize KiB  -> BIN\IDLE.ELF on the disk"
Write-Host "  ESP tree   : $EspDir"
Write-Host ''
Write-Host 'Run it with: .\scripts\run.ps1'
