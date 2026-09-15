<#
.SYNOPSIS
    Runs the full NexusOS test suite.

.DESCRIPTION
    Twenty-two stages, cheapest first, so a failure surfaces as early as possible.

    The first four run on this machine and take seconds:

      1. formatting
      2. lints, over every crate in the workspace -- which is checked, not
         assumed; see Assert-EveryCrateLinted
      3. host unit tests: every library, plus the drawing code
      4. the collaboration tool, driven as a binary rather than as a library

    The rest boot a real machine under QEMU and read its serial log. Nothing
    below is verified by having compiled:

      5. a boot test, with the filesystem's destructive checks on
      6. a boot from the disk image alone, the only run where the firmware
         reads the partition table and filesystem this project writes
      7. persistence: boot twice on one disk, and require the second boot to
         find what the first one wrote
      8. the network: DHCP, DNS and a fetch
      9. updates
     10. first-run setup, answered with the keys a person would press
     11. the terminal
     12. appearance: change the look and watch the desktop follow
     13. settings
     14. packages, including one whose signature does not hold
     15. pictures and a recording, decoded by this system's own decoders
     16. the agent
     17. browsing
     18. input, through QEMU's monitor and the same 8042 controller
     19. the wallpaper: put a picture and then a recording behind everything
     20. the launcher: open it with F3, type two letters that only a
         subsequence match finds, and require the thing named to start
     21. power: press the buttons that stop the machine, and require it to
         stop -- the one stage whose pass condition is that QEMU exits
     22. injection: break the kernel one way per build -- a real CPU exception,
         a broken TLB shootdown -- and require each to be reported rather than
         resetting the machine

.PARAMETER SkipFaults
    Skip stage 22, which is the slowest because it boots QEMU six times.
#>
[CmdletBinding()]
param(
    [switch]$SkipFaults
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$failures = @()

# Windows PowerShell turns a native program's stderr into error records, and
# under `$ErrorActionPreference = 'Stop'` those abort the script even when the
# program succeeded. cargo writes all of its progress to stderr, so every step
# below runs through here: output is printed, and success is judged by the exit
# code, which is the only thing that actually says whether the tool worked.
function Invoke-Native {
    param([string]$Exe, [string[]]$Arguments, [string]$What)
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $Exe @Arguments 2>&1 | ForEach-Object { Write-Host "    $_" }
        if ($LASTEXITCODE -ne 0) { throw "$What failed (exit $LASTEXITCODE)" }
    } finally {
        $ErrorActionPreference = $previous
    }
}

function Invoke-Step {
    param([string]$Name, [scriptblock]$Body)
    Write-Host ''
    Write-Host "==> $Name" -ForegroundColor Cyan
    try {
        & $Body
        Write-Host "    PASS $Name" -ForegroundColor Green
    } catch {
        Write-Host "    FAIL $Name : $_" -ForegroundColor Red
        $script:failures += $Name
    }
}

Invoke-Step 'formatting' {
    Push-Location $RepoRoot
    try {
        Invoke-Native 'cargo' @('+nightly', 'fmt', '--all', '--', '--check') 'cargo fmt'
    } finally { Pop-Location }
}

# Every crate in the workspace, sorted into the target it is built for. These
# lists are checked against `cargo metadata` below, which is the point of them:
# the bootloader -- the first code that runs on the machine -- went unlinted
# because nobody had put it on a list, and the whole of user space went unlinted
# for the same reason. A list maintained only by hand will eventually be short
# by one crate, and nothing will say so.
$HostLibraries = @(
    'nexus-abi', 'nexus-ai-core', 'nexus-config', 'nexus-crypto', 'nexus-dns', 'nexus-font',
    'nexus-html', 'nexus-http', 'nexus-i18n', 'nexus-image', 'nexus-ime', 'nexus-index',
    'nexus-inflate', 'nexus-json', 'nexus-look', 'nexus-machine', 'nexus-mm', 'nexus-net',
    'nexus-netclient', 'nexus-pkg', 'nexus-shellwords', 'nexus-time', 'nexus-tls', 'nexus-update',
    'nexus-user', 'nexus-window'
)

# Programs that run on the development machine rather than on Nexus: the package
# signer, and the example guest binary.
$HostTools = @('nexus-pack', 'nexus-collab', 'nexus-linux-example')

# Programs that run on Nexus, built for the user target.
$Programs = @(
    'nexus-ai', 'nexus-assist', 'nexus-browser', 'nexus-client', 'nexus-compositor',
    'nexus-find', 'nexus-hello', 'nexus-idle', 'nexus-init', 'nexus-install', 'nexus-launch',
    'nexus-settings',
    'nexus-setup', 'nexus-shell', 'nexus-store', 'nexus-term', 'nexus-ui', 'nexus-updater',
    'nexus-view', 'nexus-wall'
)

# The two that stand alone, each with its own target.
$Bootloader = 'nexus-boot'
$Kernel = 'nexus-kernel'

<#
.SYNOPSIS
    Fail if any workspace crate is on none of the lists above, or on two.
#>
function Assert-EveryCrateLinted {
    $covered = @($HostLibraries) + @($HostTools) + @($Programs) + @($Bootloader) + @($Kernel)

    $twice = $covered | Group-Object | Where-Object { $_.Count -gt 1 } | ForEach-Object { $_.Name }
    if ($twice) {
        throw "listed more than once, so one of the lists is wrong: $($twice -join ', ')"
    }

    $metadata = & cargo metadata --no-deps --format-version 1 2>$null | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed' }
    $members = $metadata.packages | ForEach-Object { $_.name }

    $missing = $members | Where-Object { $covered -notcontains $_ }
    if ($missing) {
        throw ("in the workspace and on no lint list: {0}. " -f ($missing -join ', ')) +
        'Add each to $HostLibraries, $HostTools or $Programs in scripts/test.ps1.'
    }

    $ghosts = $covered | Where-Object { $members -notcontains $_ }
    if ($ghosts) {
        throw ("on a lint list and not in the workspace: {0}" -f ($ghosts -join ', '))
    }

    Write-Host "    every one of $($members.Count) crates is on a list" -ForegroundColor DarkGray
}

Invoke-Step 'clippy' {
    Push-Location $RepoRoot
    try {
        Assert-EveryCrateLinted

        # The libraries and their tests. `--tests` is not decoration: a lint
        # error in test code failed nothing here until a stray `--all-targets`
        # run turned two of them up, and test code is code -- this suite's own
        # reliability rests on it.
        $arguments = @('+nightly', 'clippy')
        foreach ($crate in $HostLibraries) { $arguments += @('-p', $crate) }
        $arguments += @('--lib', '--tests', '--', '-D', 'warnings')
        Invoke-Native 'cargo' $arguments 'clippy (libraries)'

        $arguments = @('+nightly', 'clippy')
        foreach ($crate in $HostTools) { $arguments += @('-p', $crate) }
        $arguments += @('--', '-D', 'warnings')
        Invoke-Native 'cargo' $arguments 'clippy (tools)'

        # The bootloader is a UEFI binary. It was on the library list with
        # `--lib`, which matched its small library and quietly checked none of
        # `main.rs` -- five findings, in the file that runs first.
        Invoke-Native 'cargo' @('+nightly', 'clippy', '-p', $Bootloader, '--',
            '-D', 'warnings') 'clippy (boot)'

        # The kernel, which needs its own target and core rebuilt for it, and so
        # was left out until it had accumulated a dozen findings nobody had seen.
        Invoke-Native 'cargo' @('+nightly', 'clippy', '-p', $Kernel,
            '--target', 'targets/x86_64-nexus.json',
            '-Zbuild-std=core,compiler_builtins,alloc',
            '-Zbuild-std-features=compiler-builtins-mem',
            '--', '-D', 'warnings') 'clippy (kernel)'

        # And every program a person actually uses. These had been linted by
        # hand as each was written -- the whole of user space turned up exactly
        # one finding when it was first checked together, which is the good news
        # -- but linting by hand is a habit, and a habit is one bad afternoon
        # from lapsing.
        $arguments = @('+nightly', 'clippy')
        foreach ($crate in $Programs) { $arguments += @('-p', $crate) }
        $arguments += @('--target', 'targets/x86_64-nexus-user.json',
            '-Zbuild-std=core,compiler_builtins,alloc',
            '-Zbuild-std-features=compiler-builtins-mem',
            '--', '-D', 'warnings')
        Invoke-Native 'cargo' $arguments 'clippy (programs)'
    } finally { Pop-Location }
}

Invoke-Step 'host unit tests' {
    Push-Location $RepoRoot
    try {
        # Every library, rather than the subset that happened to be listed.
        # nexus-index, nexus-net, nexus-pkg and nexus-ai-core between them had
        # sixty tests this suite had never run, all of them passing -- which is
        # the worst way for that to be true, because nobody would have noticed
        # when they stopped.
        #
        # nexus-ui is a program rather than a library, but its tests run on the
        # host and it is the drawing code every window goes through, so it runs
        # here too.
        $arguments = @('+nightly', 'test')
        foreach ($crate in $HostLibraries) { $arguments += @('-p', $crate) }
        $arguments += @('-p', $Bootloader, '-p', 'nexus-ui', '--lib')
        Invoke-Native 'cargo' $arguments 'unit tests'
    } finally { Pop-Location }
}

Invoke-Step 'the collaboration tool' {
    # Its own tests cover the library; this drives the real binary through a
    # session and checks what it refuses to do. Here rather than at the end
    # because it needs no machine and takes two seconds, and the suite is
    # ordered cheapest first.
    Invoke-Native 'powershell' @('-NoProfile', '-File',
        (Join-Path $PSScriptRoot 'test-collab.ps1')) 'collaboration tool tests'
}

Invoke-Step 'boot test' {
    # With the filesystem's destructive checks on. This is the one place they
    # belong: a disk that exists to be tested, in a run somebody is watching.
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'build.ps1'), '-DeepSelfTest') 'build'
    # One boot, on the disk the build just made, with somebody at the keyboard.
    #
    # A fresh disk is a machine nobody has set up, and that is the right thing
    # for a fresh disk to be: the wizard comes up and waits. So this answers it
    # -- the same keys a person would press, through the same 8042 controller --
    # and then, once the desktop has been up and its clients have finished,
    # presses the key that ends the session.
    #
    # Both are needed and for different reasons. Without the first there is no
    # desktop to test. Without the second the machine is still running when the
    # timeout comes, because nothing else ends a session: a desktop whose last
    # window closes stays a desktop, and every check below about processes
    # having ended would be reading a machine mid-session.
    #
    # It has to be one boot rather than two, because half the markers below are
    # about a machine's *first* boot -- the store being seeded from the image,
    # `init` making its directory rather than finding it.
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'run.ps1'), '-Headless', '-Timeout', '600',
        # The second marker is the *latest* of the things this test looks for,
        # which is a client finishing its frames. Ending the session on an
        # earlier one leaves whatever comes after it unreported -- and every
        # marker below has to have happened by the time the session ends,
        # because the session ending is what stops the machine.
        '-PressAfter', 'setup: this machine has not been set up;client: drew every frame into a surface it was given',
        '-Press', 'ret ret n e x u s ret p a s s w o r d ret p a s s w o r d ret ret;f10',
        '-Until', 'compositor: the session ended') 'boot'

    $log = Join-Path $BuildDir 'serial.log'
    if (-not (Test-Path $log)) { throw 'no serial output' }
    $output = (Get-Content $log -Raw -Encoding UTF8) -replace "`0", ''

    # Every marker a healthy boot must produce, in the order it produces them.
    $markers = @(
        'NexusOS bootloader',
        'exited boot services',
        'Nexus Kernel v',
        'handoff revision 1',
        'physical memory regions',
        'GDT, TSS and IDT installed',
        'PIT running at 1000 Hz',
        'breakpoint at',
        'timer is live',
        'frame allocator:',
        'frame allocator verified',
        'identity map torn down',
        'heap verified',
        'ACPI 2.0, root table XSDT',
        '4 processors (4 enabled)',
        'local APIC 0 version',
        'ticking at 1000 Hz on vector 48',
        'legacy PIC masked, PIT stopped, LINT0 disconnected',
        '3 of 3 additional processors started',
        '4 processors online',
        'scheduler started',
        'joined the scheduler as thread',
        'threads ran to completion',
        'work was spread across',
        'preemption verified',
        'TLB shootdown verified',
        'TLB shootdowns broadcast',
        'syscall entry at',
        'hello from ring 3',
        'callee-saved registers survived',
        'through the system-call boundary',
        'interrupts taken from ring 3',
        'kernel address space rooted at',
        'process alpha kept its own memory',
        'process beta kept its own memory',
        'address spaces created',
        'threads waiting for a key',
        'IPC verified',
        'hello across a channel',
        'channel round trip, bad handle and closed peer all behaved',
        'messages sent',
        'processes started',
        'starts holding handle',
        'here is a channel of my own',
        'a request from the client, on the channel it passed over',
        'an answer from the server, on the channel it was handed',
        'server: the passed channel closed',
        'devices',
        'virtio disk at',
        'requests now block instead of spinning',
        'block cache',
        'disk verified',
        'EFI system partition',
        'mounted at sector',
        'filesystem verified',
        'structural checks passed',
        'NexusFS verified',
        'check verified',
        'journal verified',
        'finished an operation the last boot did not',
        'the boot log has',
        'starts holding directory handle',
        'init: made a directory and a file',
        'init: appended to a file and changed four bytes in the middle',
        'init: a name was taken away and the open file went on reading',
        'process lifetime verified',
        'wait set verified',
        'one wait covered a channel and a process, and reported both',
        'exited with status 0',
        'the program it asked for finished, and said it worked',
        'idle: waiting for something that will never arrive',
        'stopped because it was asked to',
        'init: stopped a program that was waiting forever',
        'idle: spinning, and asking the kernel for nothing at all',
        'init: stopped a program that was asking the kernel for nothing',
        'loaded from BIN/INIT.ELF',
        'init: loaded from disk and running in ring 3',
        'init: clock, channel and handle checks all passed',
        "at a process's request",
        'hello: started because another program asked for me',
        'hello, from the program you asked for',
        'init: asked for a program, got a channel, and used it',
        'init: heard you',
        'hello: read the shared page and wrote back into it',
        'init: the other process wrote into memory we both map',
        'placed on the store from the image',
        'install: demo 1.0.0, 2 files, signed by the key this machine trusts',
        "install: refused PKG/BAD.NEX: the package's signature is not from a trusted key",
        'init: a package altered after signing was refused',
        'loaded from BIN/HELLO.LX',
        '[linux] spawned wrote: a program built for Linux, running on NexusOS',
        'exited with status 0 through the Linux boundary',
        'init: a Linux program ran and exited through the translation',
        'find: given one directory with read transfer',
        'find: asked for more authority than it holds, and was refused',
        'find: tried to write where it was reading, and was refused',
        'find: indexed 2 files it was able to read',
        'is closest to hello.txt',
        'init: something read a directory it was lent and answered',
        'install: the package is on the filesystem',
        'init: the installer finished, and said it worked',
        'init: read a file that arrived inside a package',
        'init: allocated, grew, and gave it all back',
        'client: laid out ',
        'lines, widest ',
        'loaded from BIN/SHELL.ELF',
        'shell: took the strip along the bottom of the screen',
        'shell: drew its strip in the language it was told',
        'rectangle at (',
        'client: drew every frame into a surface it was given',
        'compositor: composited every frame its clients drew',
        'update: demo 1.1.0 is new',
        'update: demo is now at 1.1.0',
        'init: the machine checked itself for updates',
        'desktop: 0 update(s) waiting, 1 installed this boot',
        'snd ] played the start-up chime',
        'wall: ',
        'frames of wallpaper from a program it does not read',
        'compositor: the session ended',
        'seconds since 1970',
        'desktop: welcome, nexus',
        'repaints covered',
        'composites for',
        'shared pages made',
        'early initialisation complete',
        'boot thread retiring',
        'display adopted',
        'display thread',
        'I/O APIC 0 version',
        'virtio card at',
        'network thread',
        'address 10.0.2.15/24 from 10.0.2.2',
        'gateway 10.0.2.2, DNS 10.0.2.3',
        'ping 10.0.2.2: reply in',
        'listening on TCP port 80',
        'keyboard on IRQ 1',
        'PS/2 mouse reporting on IRQ 12',
        'mouse on IRQ 12',
        'input thread',
        'glyphs available, including CJK',
        'interface language en-US, 2 available',
        '[mon ]'
    )
    $missing = @($markers | Where-Object { -not $output.Contains($_) })
    if ($missing.Count -gt 0) {
        throw "serial log is missing: $($missing -join ', ')"
    }

    # A boot must not produce an exception report or a second bootloader banner.
    if ($output.Contains('EXCEPTION')) { throw 'an unexpected exception was reported' }
    if ($output.Contains('KERNEL PANIC')) { throw 'the kernel panicked' }
    # Any self-test that judged itself says so; a marker being present only
    # means the line was printed, not that what it reported was right.
    if ($output.Contains('[test] FAILED')) {
        $failed = ([regex]::Matches($output, '\[test\] FAILED[^
]*') | ForEach-Object { $_.Value }) -join '; '
        throw "a kernel self-test failed: $failed"
    }
    # Every processor the firmware reported must be scheduling, not just alive.
    if (-not ($output -match 'work was spread across (\d+) of (\d+) processors')) {
        throw 'the workers never reported which processors they ran on'
    }
    if ([int]$Matches[2] -gt 1 -and [int]$Matches[1] -lt 2) {
        throw "work ran on only $($Matches[1]) of $($Matches[2]) processors"
    }
    # Ring 3 has to have actually been ring 3. System calls alone would not say
    # so -- `syscall` is legal from ring 0 -- but an interrupt whose saved code
    # selector has privilege 3 could only have come from user mode.
    if (-not ($output -match '(\d+) interrupts taken from ring 3')) {
        throw 'the kernel never reported interrupts taken from ring 3'
    }
    if ([int]$Matches[1] -lt 1) {
        throw 'no interrupt ever arrived from ring 3, so nothing ran at user privilege'
    }
    # A user program that reported a failure across the boundary is a failure.
    if ($output -match '\[user\][^\r\n]*FAILED[^\r\n]*') {
        throw "the user program reported: $($Matches[0])"
    }
    # Two processes must map the same user address to different frames. This is
    # the kernel's own view of the page tables, so unlike what the programs
    # report it cannot come out right by a lucky interleaving.
    if (-not ($output -match 'alpha and beta both map [^,]+, to (\S+) and (\S+)')) {
        throw "the kernel never compared the two processes' mappings"
    }
    if ($Matches[1] -eq $Matches[2]) {
        throw "alpha and beta share the frame at $($Matches[1])"
    }
    # Every address space a process was given has to come back when it exits.
    #
    # The *last* monitor line, not the first. The monitor reports every five
    # seconds, and the earliest report is taken while processes are still
    # running, so a program that has not exited yet reads as a leak. What is
    # being asserted is that nothing is outstanding once the system has settled.
    $spaces = [regex]::Matches($output, '(\d+) address spaces created, (\d+) freed')
    if ($spaces.Count -lt 1) {
        throw 'the kernel never reported what became of the address spaces'
    }
    $last = $spaces[$spaces.Count - 1]
    if ([int]$last.Groups[1].Value -ne [int]$last.Groups[2].Value) {
        throw "$($last.Groups[1].Value) address spaces were created but only $($last.Groups[2].Value) freed"
    }
    # The input thread must be blocked, not sleeping. It is the difference
    # between a thread that costs nothing between keystrokes and one that wakes
    # fifty times a second to find nothing, and only the state says which.
    if (-not ($output -match '(\d+) blocked\)')) {
        throw 'the kernel never reported how many threads are blocked'
    }
    if ([int]$Matches[1] -lt 1) {
        throw 'no thread is blocked, so the input thread is still polling'
    }
    if (-not ($output -match '(\d+) threads waiting for a key')) {
        throw 'the kernel never reported who is waiting for a key'
    }
    if ([int]$Matches[1] -lt 1) {
        throw 'nothing is waiting for a key'
    }
    # Every process that started has to have ended, and its address space with
    # it. A process that leaked would look exactly like one that is still
    # usefully running -- which is also why this reads the last report and not
    # the first: early on, one that is still running is one that is still
    # running.
    $processes = [regex]::Matches($output, '(\d+) processes started, (\d+) ended')
    if ($processes.Count -lt 1) {
        throw 'the kernel never reported what became of the processes'
    }
    $last = $processes[$processes.Count - 1]
    if ([int]$last.Groups[1].Value -ne [int]$last.Groups[2].Value) {
        throw "$($last.Groups[1].Value) processes started but only $($last.Groups[2].Value) ended"
    }
    # Messages sent must all have been received. A send that quietly went
    # nowhere would still print the line above it.
    #
    # The last report, not the first: a message in flight when the monitor ran
    # is a message in flight and not a message lost.
    $traffic = [regex]::Matches($output, '(\d+) channels, (\d+) messages sent, (\d+) received')
    if ($traffic.Count -lt 1) {
        throw 'the kernel never reported the channel traffic'
    }
    $last = $traffic[$traffic.Count - 1]
    if ([int]$last.Groups[2].Value -lt 1) {
        throw 'no message was ever sent across a channel'
    }
    # Received may lag sent, and a small gap is not a lost message. A queue can
    # legitimately be destroyed with something still in it: the compositor sends
    # the desktop a final list of windows and then closes the channel, and the
    # desktop is gone before it reads one. What would be wrong is *more*
    # received than sent, or a gap that keeps growing -- so the gap is bounded
    # rather than required to be zero.
    $sent = [int]$last.Groups[2].Value
    $received = [int]$last.Groups[3].Value
    if ($received -gt $sent) {
        throw "$received messages were received but only $sent sent"
    }
    if ($sent - $received -gt 4) {
        throw "$sent messages were sent but only $received received"
    }
    # The conversation has to have gone the way a conversation goes. Both lines
    # being present says two processes logged something; the order says the
    # request reached the server before the answer reached the client, which is
    # the only reading that is a round trip rather than two monologues.
    $handed = $output.IndexOf('here is a channel of my own')
    $request = $output.IndexOf('a request from the client, on the channel it passed over')
    $answer = $output.IndexOf('an answer from the server, on the channel it was handed')
    $closed = $output.IndexOf('server: the passed channel closed')
    if ($handed -lt 0 -or $request -lt $handed) {
        throw 'the server was not handed a channel before it was used'
    }
    if ($request -lt 0 -or $answer -lt 0 -or $closed -lt 0) {
        throw 'the client and server did not complete their exchange'
    }
    if ($answer -lt $request) {
        throw 'the server answered before the request arrived'
    }
    if ($closed -lt $answer) {
        throw 'the server saw the channel close before it had answered'
    }
    # A program asked for another program and then talked to it. The order is
    # what says the second was started because the first asked, rather than
    # because the kernel decided to start both.
    $asked = $output.IndexOf("at a process's request")
    $started = $output.IndexOf('hello: started because another program asked for me')
    $spoke = $output.IndexOf('hello, from the program you asked for')
    $used = $output.IndexOf('init: asked for a program, got a channel, and used it')
    if ($asked -lt 0 -or $started -lt 0 -or $spoke -lt 0 -or $used -lt 0) {
        throw 'a program did not manage to have another one started'
    }
    if ($used -lt $spoke) {
        throw 'the asking program finished before the started one spoke'
    }
    # Shared memory has to be given back. It is the one kind of frame an address
    # space deliberately does not free, so a leak here would be silent.
    #
    # And the last report again, for the same reason: memory still mapped by a
    # process that has not exited yet is memory in use, not memory leaked.
    $shared = [regex]::Matches($output, '(\d+) shared pages made, (\d+) released')
    if ($shared.Count -lt 1) {
        throw 'the kernel never reported what became of the shared memory'
    }
    $last = $shared[$shared.Count - 1]
    if ([int]$last.Groups[1].Value -lt 1) {
        throw 'no shared memory was ever created'
    }
    if ([int]$last.Groups[1].Value -ne [int]$last.Groups[2].Value) {
        throw "$($last.Groups[1].Value) shared pages were made but only $($last.Groups[2].Value) released"
    }
    # The disk has to be the one the build made, and the driver has to have
    # used it. A driver that found the device and never moved a sector would
    # print the line above and nothing else would notice.
    if (-not ($output -match 'disk (\d+) sectors, (\d+) read, (\d+) written')) {
        throw 'the kernel never reported what it did with the disk'
    }
    if ([int]$Matches[1] -ne 131072) {
        throw "the disk reports $($Matches[1]) sectors, expected 131072"
    }
    if ([int]$Matches[2] -lt 1 -or [int]$Matches[3] -lt 1) {
        throw 'the disk was found but never read or written'
    }
    # And it did so by taking its interrupt rather than by spinning. A driver
    # that fell back is still correct, which is why the fallback exists -- but
    # on this machine the interrupt works, so a boot that spins is a boot where
    # something stopped working.
    if (-not ($output -match 'disk \d+ sectors, (\d+) read, (\d+) written,\s+(\d+) interrupts, requests (block|spin)')) {
        throw 'the kernel never reported how the disk waits'
    }
    if ($Matches[4] -ne 'block') {
        throw 'the disk fell back to spinning for completions'
    }
    if ([int]$Matches[3] -lt 1) {
        throw 'the disk never raised an interrupt'
    }
    Write-Host "    the disk raised $($Matches[3]) interrupts and never spun for one" -ForegroundColor DarkGray

    # And the cache under the filesystem is actually catching the repeated
    # reads it exists for. A cache that missed everything would still be
    # correct, and would be a cache in name only.
    $cache = [regex]::Matches($output, 'block cache (\d+)% of (\d+) reads served from memory')
    if ($cache.Count -lt 1) {
        throw 'the kernel never reported its block cache'
    }
    $last = $cache[$cache.Count - 1]
    if ([int]$last.Groups[2].Value -lt 50) {
        throw "the block cache saw only $($last.Groups[2].Value) reads, which proves nothing"
    }
    if ([int]$last.Groups[1].Value -lt 80) {
        throw "the block cache served only $($last.Groups[1].Value)% of reads from memory"
    }
    Write-Host "    the block cache served $($last.Groups[1].Value)% of $($last.Groups[2].Value) reads from memory" -ForegroundColor DarkGray
    # The filesystem reader has to have walked a path and followed a chain, not
    # merely opened something in the root. The count of root entries is what
    # says the directory walk saw the whole directory.
    if (-not ($output -match 'filesystem verified: (\d+) partitions, (\d+) entries in the root')) {
        throw 'the kernel never reported reading its filesystem'
    }
    if ([int]$Matches[1] -lt 1 -or [int]$Matches[2] -lt 3) {
        throw "the filesystem reader saw $($Matches[1]) partitions and $($Matches[2]) root entries"
    }
    # NexusFS has to have been made or found, and to have come through its own
    # exercise with the free-block count where it started. A write path that
    # allocated a block and forgot it would pass every other check here.
    if (-not ($output -match 'NexusFS (made|mounted) at sector (\d+): (\d+) MiB')) {
        throw 'the kernel never reported its own filesystem'
    }
    if ([int]$Matches[3] -lt 1) {
        throw "NexusFS came up with $($Matches[3]) MiB in it"
    }
    if (-not ($output -match 'NexusFS verified: made, written, read, emptied; (\d+) blocks, (\d+) free, nothing leaked')) {
        throw 'NexusFS did not come through its own checks'
    }
    if ([int]$Matches[2] -ge [int]$Matches[1]) {
        throw "NexusFS says $($Matches[2]) of $($Matches[1]) blocks are free, which leaves nowhere for its own metadata"
    }

    # Every process that ended has to have ended with a status somebody could
    # have acted on. The failure this guards against is a program exiting with
    # whatever was left in a register: it looks like a working system until a
    # parent believes a garbage number means failure.
    #
    # One process is expected to exit non-zero: the installer, when it is handed
    # the package that was altered after signing. Refusing it *is* the test, so
    # the check is that exactly one process said so and it said 1 -- which is
    # what this system's installer means by "refused", as against 2 for "broke".
    # A run where nothing refused, or where two things did, is a run where
    # something happened that nobody arranged.
    $statuses = [regex]::Matches($output, 'exited with status (\d+) through')
    if ($statuses.Count -lt 1) {
        throw 'no process reported the status it exited with'
    }
    $refusals = 0
    foreach ($status in $statuses) {
        $value = [int64]$status.Groups[1].Value
        if ($value -eq 0) { continue }
        if ($value -eq 1) { $refusals++; continue }
        throw "a process exited with status $value"
    }
    if ($refusals -ne 1) {
        throw "expected exactly one refusal, saw $refusals"
    }
    Write-Host "    $($statuses.Count) processes ended: $($statuses.Count - 1) with zero, one refusing a forged package" -ForegroundColor DarkGray

    $banners = ([regex]::Matches($output, 'NexusOS bootloader')).Count
    if ($banners -gt 1) { throw "the machine reset ($banners boots seen)" }

    Write-Host "    $($markers.Count) boot markers present" -ForegroundColor DarkGray
}

Invoke-Step 'boot from the image' {
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'test-image.ps1')) 'image boot'
}

Invoke-Step 'persistence' {
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'test-persistence.ps1')) 'persistence tests'
}

Invoke-Step 'network' {
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'test-network.ps1')) 'network tests'
}

Invoke-Step 'updates' {
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'test-update.ps1')) 'update tests'
}

Invoke-Step 'first-run setup' {
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'test-setup.ps1')) 'setup tests'
}

Invoke-Step 'the terminal' {
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'configure-disk.ps1')) 'first-run setup'
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'test-terminal.ps1')) 'terminal tests'
}

Invoke-Step 'appearance' {
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'configure-disk.ps1')) 'first-run setup'
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'test-appearance.ps1')) 'appearance tests'
}

Invoke-Step 'settings' {
    # The same properties the appearance stage covers, reached from the strip
    # instead of a prompt -- and one the prompt cannot reach at all: a value the
    # window has to refuse rather than write.
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'configure-disk.ps1')) 'first-run setup'
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'test-settings.ps1')) 'settings tests'
}

Invoke-Step 'packages' {
    # The window over the installing machinery, and the two answers a package
    # manager has to get right: a signature that does not hold is refused, and
    # one that does is installed and recorded.
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'configure-disk.ps1')) 'first-run setup'
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'test-store.ps1')) 'package tests'
}

Invoke-Step 'pictures' {
    # A real PNG, written by a reference encoder, carried onto the store by the
    # kernel at boot, and decoded inside the guest by this system's own DEFLATE.
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'configure-disk.ps1')) 'first-run setup'
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'test-view.ps1')) 'picture tests'
}

Invoke-Step 'the agent' {
    # Not a test of a language model; there is not one. What it checks is that
    # an agent decides what to do, asks permission, and is refused by name when
    # it may not -- including the question whose only correct outcome is a
    # refusal rather than a deletion.
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'configure-disk.ps1')) 'first-run setup'
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'test-assist.ps1')) 'agent tests'
}

Invoke-Step 'browsing' {
    # Needs a machine somebody has set up, because there is no desktop to press
    # until the wizard has been answered.
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'configure-disk.ps1')) 'first-run setup'
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'test-browser.ps1')) 'browser tests'
}

Invoke-Step 'input' {
    # The persistence stage above makes a fresh disk, which is a machine nobody
    # has set up -- and this one needs a desktop to send keys and clicks at. So
    # it is set up first. On a disk that is already configured this notices and
    # does nothing.
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'configure-disk.ps1')) 'first-run setup'
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'test-input.ps1')) 'input tests'
}

Invoke-Step 'the wallpaper' {
    Invoke-Native 'powershell' @('-NoProfile', '-File',
        (Join-Path $PSScriptRoot 'configure-disk.ps1')) 'first-run setup'
    Invoke-Native 'powershell' @('-NoProfile', '-File',
        (Join-Path $PSScriptRoot 'test-wallpaper.ps1')) 'wallpaper tests'
}

Invoke-Step 'the launcher' {
    # Needs a desktop to press F3 at.
    Invoke-Native 'powershell' @('-NoProfile', '-File',
        (Join-Path $PSScriptRoot 'configure-disk.ps1')) 'first-run setup'
    Invoke-Native 'powershell' @('-NoProfile', '-File',
        (Join-Path $PSScriptRoot 'test-launch.ps1')) 'launcher tests'
}

Invoke-Step 'power' {
    # After everything that needs a running machine, because this is the one
    # stage that deliberately stops one. Outside the `-SkipFaults` guard: it is
    # not a fault-injection test, and somebody skipping the slow six-boot stage
    # should not also lose the check that the machine can be turned off.
    Invoke-Native 'powershell' @('-NoProfile', '-File',
        (Join-Path $PSScriptRoot 'configure-disk.ps1')) 'first-run setup'
    Invoke-Native 'powershell' @('-NoProfile', '-File',
        (Join-Path $PSScriptRoot 'test-power.ps1')) 'power tests'
}

if (-not $SkipFaults) {
    Invoke-Step 'injection' {
        Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'test-faults.ps1')) 'injection tests'
    }
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'All NexusOS tests passed.' -ForegroundColor Green
    exit 0
} else {
    Write-Host "Failed: $($failures -join ', ')" -ForegroundColor Red
    exit 1
}
