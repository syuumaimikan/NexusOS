<#
.SYNOPSIS
    Boots the machine and checks that a program built for Linux can use files.

.DESCRIPTION
    Four programs run at every boot, started by `init` through the ordinary
    spawn service with `linux:` in front of the name.

    The third is about files: it creates one, writes to it, closes it, opens it
    again, stats it, reads it back and compares the bytes; then it lists a
    directory, asks where it is, asks for randomness, and looks for `AT_RANDOM`
    in its own auxiliary vector.

    The fourth is about being *dynamically linked*, which is what almost every
    real Linux program is: it is ET_DYN, it has a PT_INTERP, and starting it
    means the kernel loads two images into one address space, enters the
    interpreter instead of the program, and hands it the numbers it cannot work
    out for itself. The interpreter checks each of those, exercises the memory
    calls a real loader makes -- MAP_FIXED, a file mapping, mprotect -- and only
    then jumps to the program, which refuses to run if it did not.

    Every one of those steps exits with its own number when what came back is
    wrong, so "it exited 0" is a real claim and not a program that printed
    something. And the last check is outside the machine altogether: what the
    program wrote has to be findable in the host's copy of the disk.

.PARAMETER Timeout
    How long to wait for the boot, in seconds.
#>
[CmdletBinding()]
param([int]$Timeout = 300, [switch]$Fresh)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')
. (Join-Path $PSScriptRoot 'private-machine.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$Log = Join-Path $BuildDir 'linux-test.log'

# A copy of the built machine that this run has to itself. `build/esp` and the
# disk image are shared, and a QEMU holding them is somebody else's build dying
# at `llvm-objcopy: permission denied` with nothing in the message about QEMU.
# Every test in this family shares one copy, because no two of them run at the
# same time and eight gigabytes each would be absurd. See
# `scripts/private-machine.ps1`.
$Machine = Get-PrivateMachine -BuildDir $BuildDir -Name 'linux-machine' -Fresh:$Fresh
$MachineDir = $Machine.BuildDir
$EspDir = $Machine.EspDir

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-linux.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $Log) { Remove-Item $Log -Force }

$QemuArgs = Get-NexusQemuArgs -BuildDir $MachineDir -EspDir $EspDir `
    -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars -SerialLog $Log -Headless

Write-Host '==> Booting NexusOS' -ForegroundColor Cyan
$process = Start-Process -FilePath $QemuExe.Source -ArgumentList $QemuArgs -PassThru -NoNewWindow
try {
    $done = $false
    for ($waited = 0; $waited -lt $Timeout; $waited++) {
        Start-Sleep -Seconds 1
        if ($process.HasExited) { break }
        if (Test-Path $Log) {
            $sofar = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''
            # Either outcome ends the wait: a failure is a result, and sitting
            # here for the whole timeout to find out about one wastes five
            # minutes per run.
            # The *last* of the four, not the third: waiting on an earlier one
            # ends the boot before the dynamically linked program has run, and
            # what that looks like is a check that says the line never appeared.
            if ($sofar -match 'a dynamically linked Linux program') {
                $done = $true
                break
            }
        }
    }
    if (-not $done) { throw 'the Linux programs never ran' }
} finally {
    if (-not $process.HasExited) {
        try { $process.Kill() } catch { }
    }
    $process.WaitForExit(5000) | Out-Null
}

$output = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''

$failures = @()
$checks = 0

foreach ($expected in @(
        # All three, so a failure says which kind of call stopped working.
        'init: a Linux program ran and exited through the translation',
        'init: a Linux program that asked for memory ran and exited through the translation',
        'init: a Linux program that wrote a file and read it back ran and exited through the translation',
        # The fourth is a different kind of program rather than a wider one:
        # ET_DYN with a PT_INTERP, which is what almost every real Linux program
        # is.
        #
        # The two lines below are the loader saying it did the two things that
        # make it dynamic, and they are checked as well as the exit status
        # because the program would still exit zero if the kernel had put both
        # images in the same place. The addresses are the ones the loader
        # chooses for an interpreter and for a movable executable, and they have
        # to be different -- the interpreter checks that from inside, and this
        # checks it from outside.
        #
        # Not the install line: that appears on the first boot with a fresh disk
        # and not on the ones after it, because a file that is already installed
        # is left alone.
        # The first program here a *compiler* produced rather than an
        # assembler: Rust, built for x86_64-unknown-linux-gnu, linked static
        # with no C runtime. It is also the one that found two things nothing
        # else could have -- that the machine had never enabled SSE, and that a
        # process entry point is not reached the way a function is.
        'init: a Linux program that used descriptors, a pipe and poll ran and exited through the translation',
        '[fpu ] x87, MMX and SSE enabled for ring 3',
        # And one built for i386: a different image format, a different
        # system-call table, and `int 0x80` through a code segment that makes
        # the processor decode thirty-two bit instructions.
        'init: a Linux program built for i386 ran and exited through the translation',
        # A program that became another one. `execve` with no `fork` in front
        # of it, which is what a bootstrapper is -- and the pipe it opened was
        # still open on the other side, which is what makes it useful.
        'init: a Linux program that became another one ran and exited through the translation',
        # A signal raised, handled on the program's own stack, and returned
        # from. The program checks a stack array that was live across it, which
        # is the only thing that notices a frame written over the red zone.
        'init: a Linux program that handled a signal ran and exited through the translation',
        # A server and a client over a Unix domain socket, with a descriptor
        # handed across it. The shape every desktop protocol on Linux has.
        'init: a Linux server and client over a socket ran and exited through the translation',
        'init: a Linux program with two threads ran and exited through the translation',
        'needs /lib/ld-nexus-x86-64.so.1: 794 bytes loaded at 0x7f0000000000',
        'loaded from /usr/bin/dyn: 441 bytes, entry 0x7f0000000078, 1 segments at 0x555555554000',
        'init: a dynamically linked Linux program ran and exited through the translation'
    )) {
    $checks++
    if ($output.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "never said '$expected'"
        Write-Host "    FAIL $expected" -ForegroundColor Red
    }
}

# An exit status is the program saying which step failed. Named here so a
# failure reads as the call rather than as a number.
$reasons = @{
    10 = 'mmap'
    11 = 'openat for writing'
    12 = 'the descriptor openat returned'
    13 = 'write'
    14 = 'close'
    15 = 'openat for reading'
    16 = 'fstat'
    17 = 'the size fstat reported'
    18 = 'read'
    19 = 'the bytes read back'
    20 = 'lseek to the end'
    21 = 'getcwd'
    22 = 'getrandom'
    23 = 'opening the root directory'
    24 = 'getdents64'
    25 = 'an empty root directory'
    26 = 'AT_RANDOM in the auxiliary vector'
    27 = 'pread64 at offset zero'
    28 = 'the bytes pread64 returned'
    29 = 'pread64 preserving the descriptor cursor'
    30 = 'pread64 at EOF'
    31 = 'pread64 rejecting a negative offset'
}

# The dynamically linked pair has its own numbers. The forties are the
# interpreter checking what the kernel handed it, and the memory calls a real
# loader makes; the sixties are the program checking that the interpreter ran at
# all. See tools/nexus-linux-example/src/dynamic.rs.
$dynamicReasons = @{
    40 = 'the auxiliary vector ended before an entry the interpreter must have'
    41 = 'AT_BASE is not where the interpreter actually is'
    42 = 'AT_ENTRY is zero, or names the interpreter instead of the program'
    43 = 'AT_PHDR is zero'
    44 = 'mmap of an anonymous page'
    45 = 'what was written into that page did not read back'
    46 = 'munmap'
    47 = 'MAP_FIXED did not return the address it was given'
    48 = 'the fixed page did not hold what was written into it'
    49 = 'mprotect to read-only'
    50 = 'the page lost its contents when it was protected'
    51 = 'openat of the program own file, named by argv[0]'
    52 = 'mmap of that file'
    53 = 'the file mapping does not begin with an ELF signature'
    60 = 'the interpreter did not run: PT_INTERP was ignored'
    61 = 'the auxiliary vector ended before AT_ENTRY'
    62 = 'AT_ENTRY is not where the program code actually is'
    70 = 'mmap for the page the two threads share'
    71 = 'mmap for the new thread stack'
    72 = 'clone'
    73 = 'the parent gave up waiting: the new thread never woke it'
    74 = 'the new thread ran but left the wrong value: the page is not shared'
    75 = 'the new thread could not make a system call of its own'
    100 = 'dup'
    101 = 'dup gave back a descriptor that is already in use'
    102 = 'writing through a duplicate did not reach the same place'
    103 = 'dup2 did not return the descriptor it was told to use'
    104 = 'close of a duplicate'
    105 = 'pipe2'
    106 = 'the two descriptors a pipe returned are the same'
    107 = 'writing into the pipe'
    108 = 'reading out of it'
    109 = 'what came out is not what went in'
    110 = 'a read after the write end closed did not report end of file'
    111 = 'poll said an empty pipe was readable'
    112 = 'poll said a pipe with something in it was not readable'
    113 = 'poll did not report the write end as writable'
    114 = 'closing the write end did not show up as a hangup'
    115 = 'epoll_create1'
    116 = 'epoll_ctl'
    117 = 'epoll_wait did not report the descriptor that is ready'
    118 = 'epoll_wait handed back the wrong cookie'
    119 = 'reading the byte epoll promised'
    99  = 'the program panicked'
    140 = 'rt_sigaction'
    141 = 'the handler did not run'
    142 = 'the handler ran with the wrong signal number'
    143 = 'a value live across the signal was changed'
    144 = 'the call the signal interrupted returned the wrong thing'
    145 = 'rt_sigprocmask'
    146 = 'a blocked signal was delivered anyway'
    147 = 'a signal was not delivered after being unblocked'
    148 = 'an ignored signal ran a handler'
    149 = 'the handler ran twice for one signal'
    240 = 'the 32-bit write did not report what it took'
    241 = 'the 32-bit getpid'
    242 = 'mmap2'
    243 = 'what was written into the mapped page did not read back'
    244 = 'the 32-bit munmap'
    245 = 'the auxiliary vector is not thirty-two bit words'
    246 = 'AT_ENTRY is not where the 32-bit code actually is'
    160 = 'pipe2 before the execve'
    161 = 'writing into that pipe'
    162 = 'execve returned, which means it failed'
    170 = 'the new program got the wrong number of arguments'
    171 = 'an argument is not the one that was passed'
    172 = 'the descriptor the previous program left open is not readable'
    173 = 'what came out of it is not what went in'
    180 = 'socketpair'
    181 = 'what went into one side of a socket pair did not come out of the other'
    182 = 'socket'
    183 = 'bind'
    184 = 'listen'
    185 = 'the server thread could not be started'
    186 = 'connect'
    187 = 'sending the request'
    188 = 'reading the reply'
    189 = 'the reply is not what the server was asked for'
    190 = 'pipe2 for the descriptor to pass'
    191 = 'sendmsg with a descriptor on it'
    192 = 'the server never reported what it read through the passed descriptor'
    193 = 'the server read the wrong thing through the passed descriptor'
    194 = 'the server never accepted the connection'
    195 = 'poll did not report the listening socket as ready'
}
$stopped32 = [regex]::Match(
    $output, 'a Linux program built for i386 exited with status (\d+)')
if ($stopped32.Success) {
    $status = [int]$stopped32.Groups[1].Value
    $why = if ($dynamicReasons.ContainsKey($status)) {
        $dynamicReasons[$status]
    } else {
        'something unlisted'
    }
    $failures += "the i386 program stopped at: $why (status $status)"
    Write-Host "    FAIL the i386 program stopped at $why" -ForegroundColor Red
}
$stoppedExec = [regex]::Match(
    $output, 'a Linux program that became another one exited with status (\d+)')
if ($stoppedExec.Success) {
    $status = [int]$stoppedExec.Groups[1].Value
    $why = if ($dynamicReasons.ContainsKey($status)) {
        $dynamicReasons[$status]
    } else {
        'something unlisted'
    }
    $failures += "the execve program stopped at: $why (status $status)"
    Write-Host "    FAIL the execve program stopped at $why" -ForegroundColor Red
}
$stoppedSignals = [regex]::Match(
    $output, 'a Linux program that handled a signal exited with status (\d+)')
if ($stoppedSignals.Success) {
    $status = [int]$stoppedSignals.Groups[1].Value
    $why = if ($dynamicReasons.ContainsKey($status)) {
        $dynamicReasons[$status]
    } else {
        'something unlisted'
    }
    $failures += "the signal program stopped at: $why (status $status)"
    Write-Host "    FAIL the signal program stopped at $why" -ForegroundColor Red
}
$stoppedNet = [regex]::Match(
    $output, 'a Linux server and client over a socket exited with status (\d+)')
if ($stoppedNet.Success) {
    $status = [int]$stoppedNet.Groups[1].Value
    $why = if ($dynamicReasons.ContainsKey($status)) {
        $dynamicReasons[$status]
    } else {
        'something unlisted'
    }
    $failures += "the socket program stopped at: $why (status $status)"
    Write-Host "    FAIL the socket program stopped at $why" -ForegroundColor Red
}
$stoppedPosix = [regex]::Match(
    $output, 'a Linux program that used descriptors, a pipe and poll exited with status (\d+)')
if ($stoppedPosix.Success) {
    $status = [int]$stoppedPosix.Groups[1].Value
    $why = if ($dynamicReasons.ContainsKey($status)) {
        $dynamicReasons[$status]
    } else {
        'something unlisted'
    }
    $failures += "the descriptor program stopped at: $why (status $status)"
    Write-Host "    FAIL the descriptor program stopped at $why" -ForegroundColor Red
}
$stoppedThreads = [regex]::Match(
    $output, 'a Linux program with two threads exited with status (\d+)')
if ($stoppedThreads.Success) {
    $status = [int]$stoppedThreads.Groups[1].Value
    $why = if ($dynamicReasons.ContainsKey($status)) {
        $dynamicReasons[$status]
    } else {
        'something unlisted'
    }
    $failures += "the threaded program stopped at: $why (status $status)"
    Write-Host "    FAIL the threaded program stopped at $why" -ForegroundColor Red
}
$stoppedDynamic = [regex]::Match(
    $output, 'a dynamically linked Linux program exited with status (\d+)')
if ($stoppedDynamic.Success) {
    $status = [int]$stoppedDynamic.Groups[1].Value
    $why = if ($dynamicReasons.ContainsKey($status)) {
        $dynamicReasons[$status]
    } else {
        'something unlisted'
    }
    $failures += "the dynamically linked program stopped at: $why (status $status)"
    Write-Host "    FAIL the dynamic program stopped at $why" -ForegroundColor Red
}
$stopped = [regex]::Match(
    $output, 'a Linux program that wrote a file and read it back exited with status (\d+)')
if ($stopped.Success) {
    $status = [int]$stopped.Groups[1].Value
    $why = if ($reasons.ContainsKey($status)) { $reasons[$status] } else { 'something unlisted' }
    $failures += "the program stopped at: $why (status $status)"
    Write-Host "    FAIL it stopped at $why" -ForegroundColor Red
}

# And the check from outside: the file it wrote is in the host's image. A layer
# that accepted every byte and kept none passes everything above.
$checks++
$image = Join-Path $BuildDir 'nexus-disk.img'
$needle = [System.Text.Encoding]::ASCII.GetBytes('a linux program wrote a file and read it back')
$found = $false
$stream = [System.IO.File]::OpenRead($image)
try {
    # From the start of the NexusFS partition; there is no point reading the
    # quarter-gigabyte of FAT32 in front of it. The number is where the second
    # partition begins, which make-disk.ps1 puts straight after the first.
    $stream.Position = 526336L * 512
    $overlap = $needle.Length - 1
    $chunk = 8MB
    $buffer = New-Object byte[] ($chunk + $overlap)
    $held = 0
    while (-not $found) {
        $got = $stream.Read($buffer, $held, $chunk)
        if ($got -le 0) { break }
        $usable = $held + $got
        $last = $usable - $needle.Length
        $index = 0
        while ($index -le $last) {
            $index = [Array]::IndexOf($buffer, $needle[0], $index, $last - $index + 1)
            if ($index -lt 0) { break }
            $match = $true
            for ($offset = 1; $offset -lt $needle.Length; $offset++) {
                if ($buffer[$index + $offset] -ne $needle[$offset]) { $match = $false; break }
            }
            if ($match) { $found = $true; break }
            $index++
        }
        # Carry the tail forward, so a match lying across a chunk boundary is
        # still whole in the buffer next time round.
        $held = [math]::Min($overlap, $usable)
        [Array]::Copy($buffer, $usable - $held, $buffer, 0, $held)
    }
} finally {
    $stream.Close()
}
if ($found) {
    Write-Host "    ok   what it wrote is in the host's disk image" -ForegroundColor DarkGray
} else {
    $failures += "what the program wrote is not in $image"
    Write-Host "    FAIL what it wrote is not in the host's image" -ForegroundColor Red
}

foreach ($bad in @('KERNEL PANIC', 'is not translated yet')) {
    $checks++
    if ($output.Contains($bad)) {
        $failures += "absent: $bad : it appeared"
        Write-Host "    FAIL absent: $bad" -ForegroundColor Red
    } else {
        Write-Host "    ok   absent: $bad" -ForegroundColor DarkGray
    }
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host "Linux translation tests passed ($checks checks)." -ForegroundColor Green
    exit 0
} else {
    foreach ($why in $failures) { Write-Host "    FAIL $why" -ForegroundColor Red }
    Write-Host "$($failures.Count) of $checks checks failed. Log: $Log" -ForegroundColor Red
    exit 1
}
