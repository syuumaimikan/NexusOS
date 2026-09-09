<#
.SYNOPSIS
    Runs the full NexusOS test suite.

.DESCRIPTION
    Eight layers, cheapest first, so a failure surfaces as early as possible:

      1. formatting
      2. lints, over every crate including the kernel
      3. host unit tests (UEFI layouts, ELF parsing, memory-map normalization,
         descriptor encoding)
      4. a boot test: build, boot in QEMU, and require the serial log to contain
         every marker a healthy boot produces
      5. a boot from the disk image alone, which is the only run where the
         firmware reads the partition table and filesystem this project writes
      6. a persistence test, which boots twice on one disk and requires the
         second boot to find what the first one wrote
      7. an input test, which sends real keystrokes through QEMU's monitor and
         checks what the kernel made of them
      8. injection tests, which break the kernel one way per build -- a real
         CPU exception, a broken TLB shootdown -- and check that
         each is reported rather than resetting the machine

.PARAMETER SkipFaults
    Skip layer 8, which is the slowest because it boots QEMU six times.
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

Invoke-Step 'clippy' {
    Push-Location $RepoRoot
    try {
        Invoke-Native 'cargo' @('+nightly', 'clippy', '-p', 'nexus-abi', '-p', 'nexus-boot', '-p', 'nexus-mm', '-p', 'nexus-net', '-p', 'nexus-user', '--lib', '--', '-D', 'warnings') 'clippy'

        # And the kernel, which needs its own target and core rebuilt for it,
        # and so was left out until it had accumulated a dozen findings nobody
        # had seen. It is the largest crate in the tree; leaving the biggest
        # thing unlinted made the step read as passing when it covered a third
        # of the code.
        Invoke-Native 'cargo' @('+nightly', 'clippy', '-p', 'nexus-kernel',
            '--target', 'targets/x86_64-nexus.json',
            '-Zbuild-std=core,compiler_builtins,alloc',
            '-Zbuild-std-features=compiler-builtins-mem',
            '--', '-D', 'warnings') 'clippy (kernel)'
    } finally { Pop-Location }
}

Invoke-Step 'host unit tests' {
    Push-Location $RepoRoot
    try {
        Invoke-Native 'cargo' @('+nightly', 'test', '-p', 'nexus-abi', '-p', 'nexus-boot', '-p', 'nexus-mm', '-p', 'nexus-user', '--lib') 'unit tests'
    } finally { Pop-Location }
}

Invoke-Step 'boot test' {
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'build.ps1')) 'build'
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'run.ps1'), '-Headless', '-Timeout', '600',
        '-Until', 'compositor: composited every frame its clients drew') 'boot'

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
        'init: allocated, grew, and gave it all back',
        'client: laid out ',
        'lines, widest ',
        'loaded from BIN/SHELL.ELF',
        'shell: took the strip along the bottom of the screen',
        'shell: drew its strip in the language it was told',
        'rectangle at (',
        'client: drew every frame into a surface it was given',
        'compositor: composited every frame its clients drew',
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
    if ([int]$last.Groups[2].Value -ne [int]$last.Groups[3].Value) {
        throw "$($last.Groups[2].Value) messages were sent but $($last.Groups[3].Value) received"
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
    $statuses = [regex]::Matches($output, 'exited with status (\d+) through')
    if ($statuses.Count -lt 1) {
        throw 'no process reported the status it exited with'
    }
    foreach ($status in $statuses) {
        if ([int64]$status.Groups[1].Value -ne 0) {
            throw "a process exited with status $($status.Groups[1].Value)"
        }
    }
    Write-Host "    $($statuses.Count) processes exited with a status of zero" -ForegroundColor DarkGray

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

Invoke-Step 'input' {
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'test-input.ps1')) 'input tests'
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
