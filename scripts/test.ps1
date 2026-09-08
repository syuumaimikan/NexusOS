<#
.SYNOPSIS
    Runs the full NexusOS test suite.

.DESCRIPTION
    Four layers, cheapest first, so a failure surfaces as early as possible:

      1. formatting and lints
      2. host unit tests (UEFI layouts, ELF parsing, memory-map normalization,
         descriptor encoding)
      3. a boot test: build, boot in QEMU, and require the serial log to contain
         every marker a healthy boot produces
      4. an input test, which sends real keystrokes through QEMU's monitor and
         checks what the kernel made of them
      5. a boot from the disk image alone, which is the only run where the
         firmware reads the partition table and filesystem this project writes
      6. injection tests, which break the kernel one way per build -- a real
         CPU exception, a broken TLB shootdown -- and check that
         each is reported rather than resetting the machine

.PARAMETER SkipFaults
    Skip layer 5, which is the slowest because it boots QEMU three times.
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
        Invoke-Native 'cargo' @('+nightly', 'clippy', '-p', 'nexus-abi', '-p', 'nexus-boot', '-p', 'nexus-mm', '-p', 'nexus-user', '--lib', '--', '-D', 'warnings') 'clippy'
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
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'run.ps1'), '-Headless', '-Timeout', '40') 'boot'

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
        'exited through the system-call boundary',
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
        'disk verified',
        'EFI system partition',
        'mounted at sector',
        'filesystem verified',
        'loaded from BIN/INIT.ELF',
        'init: loaded from disk and running in ring 3',
        'init: clock, channel and handle checks all passed',
        "at a process's request",
        'hello: started because another program asked for me',
        'hello, from the program you asked for',
        'init: asked for a program, got a channel, and used it',
        'init: heard you',
        'early initialisation complete',
        'boot thread retiring',
        'display adopted',
        'display thread',
        'I/O APIC 0 version',
        'keyboard on IRQ 1',
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
    if (-not ($output -match '(\d+) address spaces created, (\d+) freed')) {
        throw 'the kernel never reported what became of the address spaces'
    }
    if ([int]$Matches[1] -ne [int]$Matches[2]) {
        throw "$($Matches[1]) address spaces were created but only $($Matches[2]) freed"
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
    # usefully running.
    if (-not ($output -match '(\d+) processes started, (\d+) ended')) {
        throw 'the kernel never reported what became of the processes'
    }
    if ([int]$Matches[1] -ne [int]$Matches[2]) {
        throw "$($Matches[1]) processes started but only $($Matches[2]) ended"
    }
    # Messages sent must all have been received. A send that quietly went
    # nowhere would still print the line above it.
    if (-not ($output -match '(\d+) channels, (\d+) messages sent, (\d+) received')) {
        throw 'the kernel never reported the channel traffic'
    }
    if ([int]$Matches[2] -lt 1) {
        throw 'no message was ever sent across a channel'
    }
    if ([int]$Matches[2] -ne [int]$Matches[3]) {
        throw "$($Matches[2]) messages were sent but $($Matches[3]) received"
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
    # The filesystem reader has to have walked a path and followed a chain, not
    # merely opened something in the root. The count of root entries is what
    # says the directory walk saw the whole directory.
    if (-not ($output -match 'filesystem verified: (\d+) partitions, (\d+) entries in the root')) {
        throw 'the kernel never reported reading its filesystem'
    }
    if ([int]$Matches[1] -lt 1 -or [int]$Matches[2] -lt 3) {
        throw "the filesystem reader saw $($Matches[1]) partitions and $($Matches[2]) root entries"
    }
    $banners = ([regex]::Matches($output, 'NexusOS bootloader')).Count
    if ($banners -gt 1) { throw "the machine reset ($banners boots seen)" }

    Write-Host "    $($markers.Count) boot markers present" -ForegroundColor DarkGray
}

Invoke-Step 'boot from the image' {
    Invoke-Native 'powershell' @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'test-image.ps1')) 'image boot'
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
