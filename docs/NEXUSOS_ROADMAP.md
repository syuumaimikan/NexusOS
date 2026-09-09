# NexusOS Roadmap

The ordering principle throughout: **each phase must actually run before the
next one starts.** A phase is complete when it boots in QEMU, its serial output
proves the behaviour, and its tests pass — not when the code compiles.

Status: ✅ complete · 🚧 in progress · ⬜ not started

---

## Phase 0 — Repository audit ✅

Empty repository confirmed; toolchain, firmware and emulator verified. See
[NEXUSOS_ARCHITECTURE_AUDIT.md](NEXUSOS_ARCHITECTURE_AUDIT.md).

## Phase 1 — Boot ✅

UEFI → Nexus Bootloader → Nexus Kernel, with serial output and a painted
framebuffer.

Delivered: hand-written UEFI bindings, GOP mode selection, ESP file loading,
ELF64 loader, 4-level page tables (identity, direct, per-segment kernel, guarded
stack), memory-map normalization, `ExitBootServices`, higher-half kernel entry,
serial console, panic handler, QEMU build/run/screenshot harness, 16 unit tests.

## Phase 2 — CPU and interrupts ✅

Without this, every fault is a silent triple fault. Highest priority.

- GDT with kernel and user code/data descriptors
- TSS, with an IST stack for double faults so a stack overflow is still
  reportable
- IDT and handlers for all 32 architectural exceptions
- Decoded page-fault reporting (address, cause, faulting instruction)
- Legacy PIC masked; local APIC brought up
- APIC timer calibrated against the PIT
- IRQ routing, `end_of_interrupt`, interrupt-safe spinlocks

**Done when:** a deliberate fault prints a decoded diagnostic instead of
resetting, and a periodic timer interrupt ticks.

Delivered: GDT with descriptors ordered for `syscall`/`sysret`; TSS with IST
stacks for double fault, NMI and machine check; all 256 IDT vectors populated,
with decoded page-fault and selector-error reports; the 8259 remapped above the
exception vectors and masked; a 1000 Hz PIT tick; `IrqSpinLock`. Three
fault-injection builds prove the exception path by taking real faults.

The local APIC is deferred to after Phase 4: its registers sit above RAM, so
mapping them needs a virtual memory manager. The PIT reaches the same place
through port I/O alone.

Also delivered since: the I/O APIC. Its redirection entries are programmed from
the MADT, including the interrupt source overrides, so devices other than the
timer can raise interrupts — which is what made a keyboard possible at all.

## Phase 3 — Physical memory ✅

- Frame allocator over the handoff memory map (buddy allocator)
- Frame accounting and statistics
- Reserved-range handling for the kernel image, loader data and the framebuffer

**Done when:** the kernel allocates and frees frames under a stress test that
verifies no frame is ever handed out twice.

Delivered: a buddy allocator in the `nexus-mm` crate, with free-list links
stored inside the free blocks and coalescing driven by one parity bit per buddy
pair. 21 tests including a 20000-step randomised workload. On hardware: 1017 MiB
across 260513 frames from a 33 KiB bitmap.

## Phase 4 — Virtual memory ✅

- Kernel-owned page tables replacing the loader's, and teardown of the identity
  map
- `map`/`unmap`/`protect`, TLB shootdown
- Kernel heap and a global allocator, unlocking `alloc` in the kernel
- Guard pages, PAT setup for write-combining framebuffer access

**Done when:** the kernel runs on its own tables with `Vec` and `Box` available.

Delivered: `map`/`unmap`/`translate` over the bootloader's tables, teardown of
the identity map, and a 16 MiB kernel heap behind a `GlobalAlloc`. The heap
allocator lives in `nexus-mm` with 13 tests. The on-hardware test cross-checks a
heap address against the direct map through `translate`, which is what actually
proves the mappings are right.

Still outstanding from this phase: PAT setup for write-combining framebuffer
access, and TLB shootdown, which cannot be written before there is a second
core to shoot down.

## Phase 5 — Processes, threads, scheduling 🚧

- Process and thread objects, address spaces, TLS
- Context switching, preemption from the APIC timer
- Priorities, sleep/wake, per-CPU run queues
- TLB shootdown across processors
- SMP bring-up via the MADT, per-CPU data
- Ring 3 transition, `syscall`/`sysret`
- A separate address space per process

Delivered: kernel threads with their own guarded stacks, context switching,
preemption from the timer, strict priority with round-robin inside each level,
sleep and wake, thread exit and reaping, and an idle thread per processor.

**Done when:** a thread that never yields can still be taken off the processor.
Verified: a sleeping ticker woke on schedule five times while a non-yielding
thread at equal priority completed twelve million iterations.

Also delivered since: ACPI table parsing (RSDP, XSDT, MADT) and the local APIC
timer, calibrated against the PIT and now driving the scheduling tick with the
8259 and PIT shut down behind it.

SMP bring-up is also done: every processor the MADT reports is started, through
a real-mode trampoline and INIT/SIPI, and each runs on its own local APIC timer.

And every one of them now schedules. Which thread is running, which to fall back
to, and whether a preemption is due are per-processor, held in the `GS`-based
per-CPU area and readable without a lock; the thread table and the run queues
stay shared behind one lock. A thread completes its departure from a processor
only once the *next* thread there has run, which is what keeps a second core
from switching onto a stack whose pointer has not been saved. The boot self-test
records which processors ran its workers and fails if the answer is only one.

TLB shootdown came with it, because it had to. `invlpg` invalidates on the
processor that runs it and nowhere else, so unmapping a page while other cores
hold the translation is a silent read or write to memory that has been handed to
someone else. Each processor now has a shootdown mailbox that is both
interrupted and polled: the interrupt reaches a core running normally, and the
polling — from every spin loop — reaches one spinning for a lock with interrupts
masked, which is the case that otherwise deadlocks, since reaping a thread
unmaps its stack while holding the scheduler lock.

Ring 3 is open. A user program runs at user privilege, in pages of its own that
it cannot write and on a stack it cannot execute, and the only way back into the
kernel is `syscall`. The entry stub is the interesting part: `syscall` does not
switch stacks, so the processor arrives in ring 0 still standing on the user's,
and `swapgs` plus the per-CPU area is how it gets off without destroying a
register belonging to the caller. Every interrupt entry does the same, because
user code can zero `GS.base` with one instruction and the next timer tick would
otherwise read per-CPU state through a null pointer.

Five system calls, each one exercised from ring 3 by the program the kernel
starts at boot, because a call that has never been made from user mode is a
function with an unusual name. The boot test fails unless an interrupt was taken
from ring 3 — system calls alone would not prove user privilege, since `syscall`
is legal from ring 0 — and an injection build reads kernel memory from ring 3 to
show the boundary keeps something out rather than merely being crossable.

Address spaces followed. Each process has a page-table root of its own; the
kernel's upper half is shared by copying the top-level entries, so a kernel
mapping made later appears in every space at once, and the entries are all
created up front so that they never change afterwards. The scheduler loads the
incoming thread's root on every switch, skipping the write when it is already
right — writing `cr3` discards every non-global translation, so doing it
needlessly would throw away a working set.

The proof is on both sides of the boundary. The kernel compares the two
processes' page tables and reports the different frames one address maps to,
which is a fact rather than an interleaving; the two programs write their own
identifier to that address and read it back two hundred times, which would
collide within a few rounds if the page were shared. An injection build gives
them one page between them, so the second check is known to be able to fail.

Finding that took a scheduler bug with it. A newly created thread started with
its interrupt flag already set, so it could be preempted in the handful of
instructions between being switched to and releasing the thread it displaced —
and the displaced thread was then left ready, on no run queue, never to run
again. About one boot in a dozen lost a thread that way. New threads now start
with interrupts masked and enable them once the hand-off is done, the hand-off
slot asserts that it is not being overwritten, and the monitor checks every five
seconds that no thread is ready and unqueued.

Still outstanding: per-processor run queues, thread affinity, and a process
object worth the name — there is no parent, no exit status, and no way to create
one from inside the system.

## Input 🚧

Not a phase of its own either: the system had to become interactive before the
language could be *chosen* rather than cycled on a timer.

Delivered: the I/O APIC routing the keyboard's line to a vector, a PS/2 driver
reading scancode set 1, decoding to keys, and an input thread that acts on them
— typing edits a line on screen, F1 switches the interface language.

Verified from outside: `scripts/test-input.ps1` types through QEMU's monitor and
checks the guest reports back the word that was typed and the language change.
Nothing internal can prove an input path; only driving it from outside can.

The input thread no longer polls: it blocks on a wait queue that the keyboard
interrupt wakes. At ten seconds of uptime with the same keystrokes, that took
the system from 1766 context switches to 688.

Outstanding: there is no focus, no delivery to a process, and no mouse. Keys are
acted on by the kernel itself, which is where a window server will take over.

## Phase 6 — Handles, IPC, system calls 🚧

- Handle table with per-handle rights, the root of the capability model
- Channels, shared memory, events, semaphores, mutexes
- The Nexus system-call ABI
- Zero-copy message passing

Delivered: wait queues, a handle table with per-handle rights, message
channels, and ten system calls.

A handle is an index into a table that belongs to one process, and holding it is
the authority — there is no way to name a channel a process was not handed, so
there is no ambient check to forget. The rights beside it say what may be done,
so the same object can be given to one process to read and another to write.
Both compatibility layers will be tables above this one rather than a second
idea of authority inside the kernel.

A channel is two endpoints, each with an inbox; writing to one appends to the
*other* and wakes whoever waits there. Messages are whole, because a byte stream
pushes framing into every user of it. An endpoint holds its peer weakly, which
is what makes "the other end has gone" something the kernel states rather than
something a caller times out on.

Both halves are exercised from both sides. A kernel self-test starts a thread on
a channel and waits for the *scheduler* to report it blocked before sending
anything — a receiver that polled would never appear in that count — then checks
the bytes that came out. A user program creates a channel from ring 3, writes,
reads it back, is refused a handle it was never given, and is told the peer has
closed after closing it.

Two processes talk. The kernel creates a channel and gives one end to each
before either starts, so neither can name the other and neither needs to — the
handle is the introduction and the authority at once. The client asks once and
blocks for the answer; the server answers whatever arrives until the channel
closes, which is how it learns the client has gone. Nothing polls and nothing
times out. The boot test checks the order as well as the presence: a request
that arrived after its answer would be two monologues rather than a round trip.

And a handle can be carried in a message, which is what makes the whole thing a
capability system rather than a pair of pipes. The client makes a second channel
and sends one end of it down the first; everything after that happens somewhere
the kernel never arranged. Handles move rather than copy — they leave the
sender's table at the moment the message is built, so there is no instant where
both processes hold one — and a send that fails after taking them puts them
back, because losing authority to a full queue would be a leak the caller could
not have avoided. Passing one on needs a right of its own, so a process can be
given something it may use and may not delegate.

Shared memory is the second kind of handle. A channel copies its message twice,
once out of the sender and once into the receiver, which is right for a request
and wrong for a framebuffer; this is the other arrangement. The frames exist
once, and a process holding the handle maps them wherever suits it — the two
programs that demonstrate it deliberately choose different addresses, because
sharing memory does not mean agreeing on where it goes. The handle crosses the
channel; the contents never do.

It needed one bit in a page-table entry. An address space frees every frame it
maps when it is dropped, and a shared frame is mapped in more than one, so
without a way to say "not mine" the second space to go would free a frame the
first had already returned. Bits 9 to 11 of an entry are ignored by the
processor and available to the operating system, which is what makes saying it
possible; the object that owns the frames frees them when its last handle does,
and the boot test fails if the numbers do not match.

And the first pixels a process put on the screen. `paint` is a program on the
disk that is handed a rectangle and a handle to the framebuffer -- the same
shared-memory mechanism two programs use to talk, pointed at memory the firmware
chose instead of at pages the allocator made. It maps it at an address of its
own choosing, is told the stride rather than guessing it, checks the rectangle
against the screen rather than trusting what it was sent, and fills it.

That is the shape a compositor has: a process holding a handle to the display,
not a thing inside the kernel. This is not one. It has no windows and no
clients, and the kernel still draws its own chrome -- but it now repaints
*around* the rectangle it gave away, which it did not before, and the first
version of this looked exactly like a program that had failed to draw.

Two things came out of measuring it. Mapping a nine-megabyte framebuffer meant
two and a half thousand rounds of interrupting every processor to shoot down a
translation for a page that had never been present -- and the architecture does
not permit a processor to have cached one, because there was nothing to cache.
New mappings now invalidate locally and say nothing to anyone else; changing or
removing a present mapping still broadcasts. And the painter's completion
message had nobody reading it, which the boot test caught by counting messages
sent against messages received.

Outstanding: events and semaphores. A process waits on a channel or on nothing.

## Continuous integration ✅

Not a phase, and it should have come earlier. Two jobs: one that needs only a
toolchain -- formatting, lints, the host unit tests, and that all four targets
build -- and one that needs QEMU and boots the thing.

The second is the one that matters. Every claim in this project is verified by
running rather than by compiling, and those runs are the ones that cannot be
reproduced from a build log. A hosted runner has no emulation acceleration, so
boots take seconds instead of milliseconds; the timeouts allow for it, and the
one measurement that would otherwise be sensitive to a slow machine -- the APIC
calibration -- measures against the PIT rather than assuming a frequency, so a
slower host gives a smaller number and not a wrong one. A failing run keeps the
serial logs, because a red cross is not a diagnosis.

## Phase 7 — Storage and NexusFS ✅

- virtio-blk, then NVMe and AHCI
- Block cache, VFS layer
- NexusFS: copy-on-write, journaling, checksums, snapshots, compression

Delivered: PCI enumeration and a virtio block driver.

Everything the machine has that is not on the processor is behind PCI, so
finding devices came first: every function of every device, through the legacy
port window, which reaches all of what enumeration and a virtio driver need.

The disk is driven the way a modern device is driven -- descriptors placed in
memory for the hardware to come and fetch, rather than bytes pushed through a
port. A virtqueue is three shared arrays: descriptors, an available ring the
driver appends to, and a used ring the device appends to. A block request is
three descriptors chained -- a header, the data, and one byte for the device to
report on.

The image every sector of which begins with its own number, written as text, is
the point of the test: the failure a block driver has to be caught making is
fetching a *different* sector than it was asked for, and a disk of zeroes cannot
tell that apart from working. The boot test reads the first sector, one in the
middle and the last, checks that a sector past the end is refused, writes a
position-dependent pattern to a scratch sector, reads it back, and restores what
was there.

And the image is a real one, built here rather than by a formatter: a protective
master boot record, a GPT with its backup at the far end, and a FAT32 written
sector by sector -- boot sector, FSInfo, two file allocation tables, directories
as cluster chains of 32-byte entries. A test boots a machine with nothing but
that image attached, so the firmware has to find the partition table, recognise
the EFI system partition, read the filesystem and load the bootloader out of it;
NexusOS had never done any of that before, because QEMU had always been
pretending a directory was a filesystem on its behalf.

The data disk and the bootable image are separate files on purpose. A machine
given two bootable disks leaves the firmware to choose between them, and it
chose the one whose kernel was older -- which made every fault-injection build
boot a binary that was not the one under test, and every result mean nothing.

And the kernel reads both for itself. The GPT is checked rather than trusted --
the header's own checksum and the one over the entry array are both verified,
because a partition table is the one structure where believing a corrupt value
means writing to the wrong part of a disk. FAT32 follows: the boot parameter
block, cluster chains through the allocation table, directories as runs of
32-byte entries, and files by path.

The test reads three files and refuses a fourth. One in the root; one a
directory down, so that walking a path is exercised rather than merely compiled;
one longer than a cluster with position-dependent contents, so that clusters
stitched together in the wrong order fail rather than being the right length;
and a name that does not exist, which has to come back as an error rather than
as whatever was next in the directory.

And a program now comes off it. `init` is built as its own binary for its own
target -- the same custom-target machinery as the kernel, with the small code
model, because a user program lives in the low half of the address space -- and
the kernel reads it off the filesystem, loads its segments into an address space
that did not exist a moment earlier, and enters it in ring 3. Everything that
ran at user privilege before this was assembled into the kernel and copied into
a page. This is the difference between a system that can run user code and one
that can run *programs*.

The ELF loader is the same one the bootloader uses, moved into the shared crate
now that there are two callers. The one thing that differed between them is how
physical memory is reached -- identity map for the bootloader, direct map for the
kernel -- so that became a parameter rather than an assumption either of them
made about the other.

Every page of a loaded image is mapped, gaps between segments included, with the
permissions of whichever segment covers it and the least of everything for the
gaps. That is not tidiness: the image is one contiguous block from the buddy
allocator, and the pages are handed back individually when the address space is
dropped. A page that was allocated and never mapped would never be freed, and
the block it came from would stay split for the life of the system.

And a program can ask for another. There is no system call that creates a
process: there is a channel, and holding one end of it is the authority to ask.
`init` is given that end when the kernel starts it, sends the path of a program
down it, and gets back a message carrying a *handle* — a channel to whatever was
started. The two then talk, and neither can name the other or find any way to.
A program that was never given the spawner handle cannot ask, and there is no
name it could use instead, which is the argument for handles over a global
namespace made concrete rather than argued.

Finding that took a deadlock with it, and a quiet one. Reaping a finished thread
held the scheduler's lock while dropping it, and dropping a thread can drop the
last reference to its process, which drops its handle table, which drops the
channel endpoints in it — and an endpoint's destructor wakes whoever was blocked
on the other end, which takes that same lock. The machine ran every test
correctly and then simply stopped. Threads are now taken out of the table under
the lock and dropped outside it.

And now the system has a filesystem of its own. The disk carries a second
partition, empty when the build writes it, with a type GUID that means NexusFS
lives there. The first boot finds no superblock and formats it; every boot after
that mounts what the first one made. That order is deliberate. A filesystem the
build script laid out would prove the build script works; the thing worth
proving is that the kernel can make a filesystem it can then read.

The format is the plain one, chosen so that every part of it can be explained.
Four-kilobyte blocks. A superblock saying where each region is, checksummed over
every field that says where something is, so a half-written one says so instead
of sending a reader to the wrong block. A bitmap of free blocks, with the
metadata region and the bits past the end of the volume marked taken before the
bitmap first reaches the disk. A table of 128-byte inodes. Directories are
ordinary files whose contents happen to be a list of names.

An inode carries eleven direct block numbers and one indirect, which puts the
largest file at just over two megabytes. That is a small number and an honest
one: a second level of indirection is four lines and would make it a gigabyte,
and adding it before anything needs it would be adding a path nothing has ever
walked. Files are read and written entire, for the same reason -- there is no
buffer cache underneath, so a byte-at-a-time interface would be a byte-at-a-time
disk.

Two things are checked that a single boot cannot check. The first is leakage:
the self-test records the free-block and free-inode counts, makes a directory, a
small file, a file past the direct blocks with position-dependent contents,
shrinks it, refuses a duplicate name, refuses a name with a separator, refuses
removing a directory with something in it, refuses a file larger than the format
can describe, deletes everything, and requires both counts to be exactly what
they were. A write path that allocated a block and forgot it passes every other
test ever written and fails that one.

The second is persistence, which needs two boots and therefore its own test.
The kernel keeps `/system/boots` and `/system/boot.log` and writes to both on
every start. `scripts/test-persistence.ps1` makes a fresh disk, boots twice
without rebuilding, and requires the first boot to *make* a filesystem and
report boot 1 with one line in the log, and the second to *mount* one and report
boot 2 with two. Reading a file back in the same boot proves the code agrees
with itself; only the second boot proves anything reached the platter.

Writing that test brought the two-bootable-disks trap back for a second visit --
the new script built the data disk with the bootloader in it, the firmware
preferred it, and all six injection tests began booting a kernel that was not
the one under test. It is now a check rather than a thing to remember: attaching
the data disk scans it for `BOOTX64.EFI` and refuses to start a machine that
would have a choice.

And programs reach it -- through handles, and only through handles. There is no
system call that takes a path. A process opens one name inside a directory it
already holds, so what it can reach is exactly the subtree under what it was
given, and a `/` in a name is refused rather than walked because a directory
handle that could be escaped with `../..` would not be an authority over
anything. A handle opened through another carries no more rights than its
parent, which is what makes handing a program a read-only directory mean
something.

`init` starts holding two things: the channel to the spawn service, and the root
directory. It lists the root, makes its own directory, reads what the previous
boot left there, writes a file, reads it back through the same handle, and
checks the refusals -- a buffer too small, a name with a separator, a name that
is not there, and removing a file that is still open. The persistence test now
requires it to *make* that file on the first boot and *find* it on the second,
which is the claim one level up from the kernel's own: not that the filesystem
persists, but that a program can put something in it and get it back across the
system-call boundary.

Two things fall out of doing it this way. A buffer too small is an error and
never a truncation, because half a file that reports its own length is
indistinguishable from a whole one. And removing a name is refused while any
handle still names it: a handle carries an inode number, an inode number is not
a reference, and freeing the inode would leave the handle pointing at a number
the filesystem is free to give to the next file.

And a program can now be waited for. Until this, a process could start another
and talk to it and had no way to learn that it had finished or whether it had
worked; the channel closing said the other end was gone, which is not the same
claim. `Exit` takes a status, the spawn service replies with two handles rather
than one -- a channel to talk to it and the process to wait for it -- and
`ProcessWait` blocks until it ends and returns the number.

The handle names a *completion* and not the process, and that is the whole
design decision. A handle to the process would keep its address space alive for
as long as anybody remembered it, so a parent that never closed one would be a
memory leak shaped like politeness. A completion is an identifier, a name and an
outcome; it outlives the process by design and costs nothing to keep.

Waiting has two paths and only one of them is easy. `init` waits for the program
it asked for, and by then that program has almost always exited already -- so
what a boot exercises is a wait on something already finished, which returns
without ever blocking. The other path is the one with the lost wake-up in it,
and it gets its own self-test: a thread waits *first*, is checked to have left
the run queues rather than spun, and only then is the completion finished. If
the ending were published without waking the queue, or the waiter joined after
the ending was published, that thread would wait forever and so would every
program that ever waits for a child.

Two flags rather than one, for the same reason. One claims the ending, so that
exactly one caller ever stores a status; the other publishes it, so a waiter
that sees the flag cannot read a status that has not been written yet.

Six assembly programs had to be edited for this, and the edit is the point:
`Exit` now reads `rdi`, and they had been written when it took no arguments, so
they exited with whatever happened to be in that register. One of them reported
a pointer as its status. The suite now requires every process in a boot to exit
with zero, because a garbage status looks exactly like a working system until a
parent believes it means failure.

And a program can wait for whichever of several things happens first, which is
the thing that had to exist before anything could be a *server*. Every blocking
call until now named one object: a thread read this channel or waited for that
process, and while it did it could do nothing else. Something holding channels
to four clients could not serve the second while blocked on the first, and a
thread per client is the arrangement that stops scaling first and hides
deadlocks in the meantime.

A wait set is an object held by a handle, like everything else. A process puts
handles into one under keys of its own choosing, waits, and is told which keys
are ready. The keys are the caller's and not the kernel's, because the caller is
the one who has to recognise them: a handle number would make the answer a thing
to look up, and it already has a name for that client.

Level-triggered, on purpose. Waiting re-tests every member rather than
remembering which one signalled. An edge -- "a message arrived" -- is a fact
about a moment, and a set that stored edges would have to be right about every
one of them forever; an edge delivered while nobody was waiting is a client that
never gets served again. A level -- "there is a message waiting" -- is a fact
about now, costs a lock per member to re-read, and cannot be lost. So a signal
from a channel or a process is only a hint that something may have changed: it
need not be accurate, need not arrive once, and a spurious one costs a re-poll.

A channel is ready when it holds a message *or* its peer has gone, because both
are things the holder must act on, and a set that reported only the first would
hang on a client that died.

Building it turned up a lost wake-up in the wait queues underneath, present
since they were written. `wait_until` tested its condition outside the queue's
lock -- it has to, because the condition lives behind an inbox or a status word
and taking those locks in that order is a deadlock rather than a race -- so a
waker landing between the test and the block found an empty queue, woke nobody,
and left the thread asleep with its condition already true. The queue now
carries a wake counter: a waiter reads it before testing and blocks only if it
has not moved, and the comparison happens under the same lock as joining the
queue. There is no third case, which is the point. A wake that a set depends on
is much easier to lose than one a single blocking receive depends on, because
there is always another message coming on a channel and there is not always
another client.

### Checking the filesystem

The journal finishes an operation that was interrupted, and says nothing about
damage that predates it: a block the bitmap calls taken that no file points at,
a block two files claim, a name pointing at an inode that is not there. No
amount of journalling finds those, because from the journal's point of view
every one of those operations completed.

So there is a check now, and it runs at every mount before anything has started
writing. It walks the inode table, builds its own picture of what is reachable,
and corrects the bitmap to match. The asymmetry is the design: a block the
bitmap calls taken that nothing reaches is leaked and safe to reclaim, and a
block something reaches that the bitmap calls free is dangerous, so the bit is
set. In both cases the *bitmap* is corrected, because it is the derived thing --
the files are what the filesystem is for and are never edited to make the
bookkeeping agree. A block two files claim, a dangling name and an unreadable
inode are reported and left alone, because each can only be fixed by choosing
which file to damage.

The test damages the filesystem the only way that damage can be made: it takes a
block from the allocator and does nothing with it. It then requires the check to
have been quiet before, to find exactly one leaked block, to be quiet again
after, and to hand the same block out again -- a repair that had to be run twice
would not be a repair.

It had to be made thirty-two times faster before it was usable, by reading the
inode table a block at a time rather than a block per inode. The first version
ran eight full walks a boot and pushed the tests past their window, which is a
good reminder that a check nobody can afford to run is a check that gets turned
off.

And the slowdown flushed out a race that had been waiting since the compositor
got its own spawn service. `start_spawn_service` handed the endpoint to its
thread through a single global, and starting the second service could overwrite
it before the first thread had read it -- so both threads served the same
channel and the other had nobody answering on it. Whoever was waiting for a
reply waited forever, and which of the two it was depended on scheduling, which
is why it only appeared once something else got slower. Each service has its own
slot now, given to its thread when it is started.

### Arguments

A spawn request may carry a path, a zero byte, and whatever the asker wants the
new program to be told. The kernel sends those bytes down the new program's
channel before handing the other end back.

Arguments *are* the first message, and that is the resolution rather than a
shortcut around one. This system already has a way for a program to be told
things; a second would be a second thing to bound, check, copy across an address
space and explain. What arguments need that a message does not is *timing* --
they have to be there before the program's first read and before the asker can
send anything of its own, so a program can rely on them being the first thing it
hears and not merely an early one. Sending them before the channel is handed
over is exactly that guarantee and nothing more.

### Part of a file

A file could only be read and written whole, which was deliberate for a reason
that stopped being true when the block cache landed: without one, changing four
bytes in the middle of a block meant four kilobytes off the platter and four
kilobytes back, every call.

So there is `read_at` and `write_at`, in the filesystem and as two system calls.
Read-modify-write at the ends, whole blocks in between, and the read skipped
where a change covers a whole block. Writing past the end grows the file and the
gap reads as zeroes -- a promise rather than an accident, since a block is zeroed
when it is allocated. There is no cursor and no seek: a file has no position,
only the offsets its holder chooses, which is the arrangement two programs
sharing a file can both be right about.

The boot log is the first user. It used to be read entire and written entire to
add one line -- sixteen kilobytes each way for forty bytes, all on the boot path.
It appends now, and still rewrites when it has to be trimmed, because taking
bytes off the front of a file means moving every byte after them.

### What is left, and why some of it is left on purpose

Four things on this phase's list are still not done, and they divide into two
kinds. Writing them down apart matters, because a list that mixes "not yet" with
"deliberately not" turns into a list nobody trusts.

**Not yet.** Timestamps are the tick a thing was made at, because there is no
real-time clock driver: they are comparable within a boot and meaningless across
one. That is a driver away, and the driver is worth writing.

**Deliberately not**, and each for a reason that would have to change first:

*Permissions.* This system already has an answer to "what may this program
touch", and it is the handle. A program reaches a file by naming a component
inside a directory it was given; one that was handed nothing can open nothing,
and there is no name it could use instead. Adding owners and mode bits would put
a second, weaker mechanism beside that one -- weaker because a mode bit is a
property of the file that everyone shares, and a handle is a property of the
holder. Two mechanisms that answer the same question is how a system ends up
with a rule that is enforced in one of them and not the other.

*More than one disk request in flight.* The journal's guarantee is an argument
about the order writes reach the device, and what makes that argument true today
is that the driver issues one request and waits for it. Raising the queue depth
without a barrier the device honours would not make the filesystem slower to
notice -- it would make it silently wrong, and only on a machine that lost
power. The order to do these in is: negotiate a flush, then use it in the
journal, then raise the depth. Doing the third first is the tempting one and the
one that breaks everything.

*Writing FAT32, and long names.* The boot partition is not ours to design. A
FAT32 writer has to keep two allocation tables and a free-cluster count
consistent through a power failure, for a filesystem this project reads once at
boot and never writes -- and a writer that is nearly safe is worse than none,
because the failure is somebody else's disk. Long names are the same trade
smaller: every name this reads fits the short form, and a partial implementation
would look like it worked.

### A journal

An operation touches several blocks and has to be all or none of them. Making a
file writes an inode, a directory, a bitmap and a superblock, and a power
failure between any two of them left the filesystem saying something that was
not true. This was the outstanding item on this phase from the day NexusFS
landed.

Metadata is written twice now. The blocks an operation changes go to a reserved
run near the front of the partition; then a descriptor naming them all goes down
with a checksum over itself; then the blocks are written where they belong; then
the descriptor is erased. Writing the descriptor *is* the commit — before it the
operation did not happen, after it the operation will happen even if the machine
stops.

That leaves three crashes and one recovery. Before the descriptor, its checksum
fails and nothing is replayed, so the operation never happened. After it and
part-way home, the next mount finishes the job. After the blocks are home but
before the descriptor is erased, the next mount writes the same blocks again,
which changes nothing — replaying is idempotent by construction, which is why
recovery needs no notion of how far it got last time.

File contents are not journalled: a two-megabyte file would need a
two-megabyte journal to protect a write nobody promised was atomic. Contents go
down first and the metadata pointing at them second, so a failure leaves the old
file rather than a new one pointing at blocks that were never written.

The layout changed to make room, so the format is version 2 and version 1 is
refused rather than misread — a version-one superblock has its checksum where
this one has a block number, and a reader that ignored the version would find
the inode table where the journal is.

**The test crashes it on purpose.** Recovery cannot be proved by reading it, and
the state it recovers from cannot be produced by a machine that is working. So
the filesystem has exactly one way to stop half way — write the transaction and
return without carrying it out — and nothing but the test uses it. The test then
mounts and requires the new contents to be there, mounts again and requires
nothing left to replay, and abandons a transaction without committing it and
requires that one to have left no trace. Both halves matter: a recovery that
replayed everything it found would be as wrong as one that replayed nothing,
because it would finish operations that never happened.

What it rests on is worth naming. The block driver issues one request at a time
and waits for each, so writes reach the *device* in the order above. Whether the
host or the drive then reorders them onto the platter is beyond this without
negotiating a flush, and that is a gap rather than a guarantee.

### A block cache

Every read NexusFS made went to the platter. Reading a 128-byte inode cost a
four-kilobyte block; reading the next inode in the same block cost it again;
walking a directory read the same bitmap and the same inode table over and over.
A megabyte of cache -- two hundred and fifty-six blocks, second-chance
replacement -- now serves 99% of those reads from memory, and the boot's sector
reads fell from 2652 to 892.

**Write-through, not write-back**, and that is a decision rather than a
simplification. Everything this filesystem claims about surviving a power
failure is an argument about the *order* writes reach the disk: an inode is
written before the directory entry that names it, so a failure in between leaks
an inode rather than leaving a name pointing at nothing; an inode is written
before its old blocks are freed, so no inode ever points at a block the bitmap
calls free. A write-back cache reorders writes by construction, and would turn
every one of those arguments into a comment that used to be true -- silently,
and only visibly on a machine that lost power. When there is a journal the cache
can hold writes back, because then the journal is what orders them.

The disk self-test writes a raw sector straight to the driver, which is the one
thing on this machine that goes behind the cache's back. It throws the cache
away afterwards rather than reasoning about it: it costs a few re-reads once per
boot, and reasoning about it is how a cache ends up serving a block that was
overwritten underneath it.

### The disk takes its interrupt

A request is submitted and the thread that made it *blocks*; the device's
interrupt wakes it. Before this it spun -- holding a processor for the whole of
a request, which on real hardware is the whole of a seek.

The driver proves the interrupt before relying on it. The first request of the
system's life is made the old way, spinning, and only if the handler is seen to
have run does the driver switch to blocking. A driver that trusted a routing
call returning `Ok` would hang on the first firmware that had wired the pin
elsewhere, and it would look like a disk that stopped answering rather than like
an interrupt that never came. Which mode it settled into is printed, and the
suite requires it to be the blocking one -- the fallback is there to be correct,
not to be used.

Two things had to be got right and one of them was got wrong first. The routing
had to happen after PCI enumeration rather than beside the keyboard's, because
a pin routed for a device that does not exist yet routes nothing. And the
*filesystem's own lock* was an `IrqSpinLock`, held across every read.

That second one is the interesting failure. While the disk spun, holding an
interrupt-safe spinlock across a read was merely wasteful. The moment a read
could sleep it became a whole-machine hang: a thread asleep with interrupts off
on its processor, every other processor spinning on a lock whose owner is
waiting for the very interrupt that would wake it. It did not fail every time --
it needed a second thread to touch the filesystem in the window -- which is
exactly the kind of bug that gets committed. It failed twice in a row in the
suite, differently each time, which is what said it was a hang and not a flaky
assertion.

So there is a third kind of lock now. [`SleepLock`] is held by blocking rather
than by spinning, which makes it the only kind that may be held across anything
slow. The volume is behind one. The status panel reads it with `try_lock` and
takes "no answer just now" for an answer, because a panel that waited for the
disk would stop redrawing the clock every time something touched a file.

## Phase 8 — Drivers and user space 🚧

- PCI/PCIe enumeration, MSI/MSI-X, IOMMU
- User-space driver model over IPC

Delivered: a compositor, which is where the display now lives.

Everything drawn before it was drawn by whoever could reach the framebuffer.
The kernel drew the banner and the status panel because it *has* the
framebuffer; `paint` drew a rectangle because it was handed the framebuffer.
Both are the same arrangement — draw by having the display — and it does not
survive a second program wanting to draw.

So `paint` is gone and a compositor has taken its place. One process holds the
display. Everyone else holds a *surface*: memory of its own, of a size it was
told, that it draws into and never sees the destination of. A client cannot
scribble over another client's window because it cannot reach one; cannot read
what another is showing for the same reason; and cannot be broken by the
compositor moving things around, because it was never told where it was. Two
clients run, each drawing a gradient of its own into its own surface, and the
compositor lays them out side by side in the rectangle the kernel keeps for it.

It waits on every client channel and every client process at once, in one wait
set, and does one of two things with what it hears: a client says it has drawn,
so its surface is copied to the display and it is told it may draw again; or a
client has ended, so its tile is cleared and it is forgotten. Both arrive
through the same wait, which is why the wait set had to exist first. A
compositor blocked reading one client stops compositing for everyone the moment
that client stops talking, and one that cannot hear a client *end* holds a dead
client's tile on screen forever, which is a lie about what is running.

Three primitives were missing and are now there. **Handle duplication**: handles
move when they cross a channel, so a compositor that sent a client its surface
would have *given it away* — the object would die with the client and take the
frames out from under the compositor's own mapping. Rights can only be dropped
in a duplicate, never gained, or a capability system is undone in one call. The
surface a client gets carries read, write and transfer but not close, so it can
draw and it cannot pull the buffer out from under the thing compositing it.
**Sleep**: a client that redrew flat out would spend a processor animating a
rectangle, and there was no way for a program to pace itself. And the reply per
frame, which is not a primitive but is the same kind of thing: the compositor
answers each "damaged" so the client knows the buffer is free, because without
it the two race for the surface and tearing is what that looks like.

The kernel still owns the rest of the screen. That is the honest halfway house:
the banner and the status panel are the kernel's, the rectangle is the
compositor's, and the boundary is a number both agree on. Moving the whole
screen behind the compositor means moving the panel into a program, which is
worth doing and is not what this establishes — which is that the path from a
client's pixel to the display runs through a process rather than through the
kernel.

And keys reach a client. The kernel goes on decoding scancodes and showing what
was typed, because the panel is still the kernel's and F1 still switches the
interface language; what changed is that a copy of every key crosses a channel,
and the compositor decides which program it is for. Routing is policy, and
knowing which window someone is looking at is exactly the kind of policy that
does not belong in a kernel: the kernel knows a key was pressed and has no idea
what a window is.

Tab is the key the compositor keeps. It moves the focus, and the focused client
is drawn with a ring around its tile — by the compositor, over the client's own
pixels, after its surface has been copied out. That is what a decoration is:
something the client did not draw, cannot draw, and cannot remove. A client that
could paint its own focus ring could claim a focus it does not have.

The input test types `n e x tab u s f1` and requires exactly three keys to reach
the first client and exactly two the second. Both halves matter: a client that
heard all five heard someone else's keys, which is the difference between
routing and broadcasting, and it is how what is typed into one window ends up in
another.

### Stopping a program

Whoever holds a process handle can now end it. Not by reaching into it -- a
thread cannot be torn off a processor it is running on, and one stopped
half-way through a system call would leave the kernel holding whatever it was
holding. A kill sets a flag and wakes everything the process had asleep; the
threads notice and leave, because a thread is the only thing that knows what it
is holding.

So it is cooperative in mechanism and not in effect. Every place a thread can
wait consults the flag, and so does the system-call boundary on the way in and
on the way out. A blocked program stops at once; a program making calls stops at
its next one. A loop that touches nothing -- no calls, no waiting, just arithmetic -- never
reaches that boundary, so it is stopped somewhere else: on the way back to ring
3 from the timer, which arrives whether a program asks for anything or not. Only
when the interrupt came from ring 3, because kernel code has no process to have
been asked, and stopping a thread part-way through what the kernel was doing on
its behalf is the thing this whole arrangement exists to avoid. It is safe there
for the same reason preemption is: the handler runs on the interrupted thread's
own kernel stack.

`idle` therefore waits in two ways. Told nothing it blocks, and stopping it
means reaching a thread that is off every run queue. Told `spin` it loops on a
number and asks the kernel for nothing at all -- and a kernel that only checked
at the system-call boundary would run that loop until the machine was turned
off.

Read and write are different rights on a process handle: watching something end
is not the same as being able to end it, so a program handed a read-only one can
wait and nothing more. A stopped process ends with a status above anything
`exit` can be given -- `exit` takes a 32-bit number -- so a waiter tells "it
decided to fail" from "it was stopped" without a second call.

Two things had to be found by running it. Waking a killed thread was not enough:
`wait_until` re-tested its condition, found it still false, and went back to
sleep forever, so the check had to go inside the wait queue rather than in each
caller. And the first version leaked an address space per kill, because it held
an `Arc<Process>` across a call to `exit` -- which never returns, so nothing
after it runs, destructors included. The accounting said fourteen address spaces
created and thirteen freed, which is exactly the kind of thing that invariant is
kept for.

`idle` exists to be stopped. Every other program here ends because it has
finished, which says nothing about whether one can be *made* to end: a program
that was going to exit anyway would exit at about the right moment whether or
not the kill worked. That one blocks on a channel nobody will ever send to, and
`init` starts it, lets it reach its wait, stops it, and requires the ending to
be a stop rather than an exit.

### A pointer

The mouse shares its controller with the keyboard and almost nothing else. The
same two ports carry both, and one bit in the status register says whose byte is
waiting -- so the two drivers are readers of one wire, and getting that wrong
means one of them decoding the other's bytes *and* the other finding its own
missing.

It went wrong immediately, and instructively. Bringing the mouse up means asking
the controller questions and reading its answers out of the one output buffer,
and the keyboard's interrupt was already unmasked by then: its handler took the
configuration byte, and the mouse reported that the controller would not say how
it was configured. Two fixes, and both were needed. The device is brought up
before any pin on that controller is unmasked, and each handler now checks the
bit and leaves the other's bytes alone.

Movement is relative and always will be: a mouse reports that it *moved*, never
where it is, because it cannot know. Turning that into a position needs a screen
to be a position on and a set of windows to be over, and the kernel has neither.
So the kernel decodes packets and sends them on, and the compositor keeps the
pointer -- clamped to the rectangle it owns, so it cannot be drawn over the
panel that is not its to touch, and with the vertical axis flipped where the two
conventions meet rather than in the driver, which has no screen to be upside
down with respect to.

Drawing it costs one thing worth naming. There is no second buffer to composite
from, so the pixels under the cursor have nowhere to be kept but in the
compositor: it saves them before drawing and puts them back before moving. It
also lifts the cursor before painting a tile and draws it again after, because
compositing writes over whatever was there -- and what was there includes the
pointer. A compositor that forgot leaves a trail of them.

A press inside a tile focuses it, and a *press* rather than a button being
down: one that acted on "down" would refocus a window forty times a second while
somebody held it. That is the second piece of policy this program owns. The
kernel knows a button went down and has no idea what it went down on.

The input test drives it through QEMU's monitor with `mouse_move`, which is
relative -- the same shape of input the hardware produces, not a position
injected past the driver. It requires packets rather than bytes, because three
bytes that never became a packet is a driver reading the stream without
understanding it; it requires *no* resynchronisations, because a stream that had
to resynchronise is one where a byte went to the wrong driver; and it clicks on
the tile the keyboard did *not* leave focused, so the click has something to
change.

### Windows that move

A window can be picked up by its title bar and carried, and one that is clicked
comes to the front. Both are the compositor's alone: the client is never told
where it is, so it cannot notice being moved, and the bar is a decoration --
drawn over the client's own pixels after its surface has been copied out, so a
client can neither draw one nor remove the one it has. A window with no bar
would be a window that cannot be picked up without picking up whatever is
inside it.

Once windows can move they can overlap, and once they overlap there has to be
an order. It is kept back to front, a click raises what was clicked, and a press
searches front to back -- a compositor that searched the other way would give
focus to the window *under* the one clicked, which looks exactly like the click
going through it.

Painting became a full recomposite of the rectangle the compositor owns: clear
it, then every window in order, then the pointer. That is more work than
repainting what changed, and it is what makes overlap simply work. A compositor
that repainted only the damaged window would leave a hole in whatever was above
it, and getting that right needs damage arithmetic this does not have and does
not yet need -- the rectangle is ninety thousand pixels.

The drag remembers where within the window it was grabbed. Without that offset a
window would jump so its corner met the pointer the instant it was picked up,
which is not what picking something up looks like. And it says so when the
window first *moves* rather than when it is taken hold of: a press that goes
nowhere is not a window being carried.

Driving this from the test found a flip worth writing down. QEMU's monitor takes
screen coordinates, its PS/2 emulation negates Y on the way to the guest, and
the compositor negates it again on the way to a position. All three are correct
and they are in three different places, which is why the first drag went
confidently in the wrong direction.

### Windows that resize

A window has a grip in its bottom-right corner, and dragging it changes the
size. A surface is tightly packed, so its size *is* its shape -- there is no
changing one without replacing the other. So the compositor makes a new memory
object, copies across what still fits, hands the client a handle to it, and
unmaps and drops the old one.

That needed the one primitive nothing had asked for yet: **unmapping**. Without
it an address, once used, is used forever, and a program handed a replacement
for something it already maps would have to put the new one somewhere else and
leak the old address -- which is how a window resized often enough runs out of
address space rather than out of memory. The handle says *what* to unmap,
because the frames belong to the object and not to the space, and the pages are
checked against it before any of them is removed: an address that happens to be
mapped to something else is not this object.

The client is told a size and given a handle and nothing else. Not why, not
where the window is, not that anybody can see it -- a client that had to be told
why its window changed size would be a client that knew it had a window.

The copy across is not necessary and it is what keeps a resize from flashing:
without it the window is blank until the client's next frame, which at two
frames a second is half a second of black. It is also the harder direction that
the test drives, shrinking rather than growing, because padding a larger surface
leaves zeroes and cropping has to get the row stride right on both sides at
once.

The grip is checked before the title bar, because on a window small enough for
the two to overlap the grip has to win: a window can always be moved by the rest
of its bar, and a window too small to resize can never be made bigger.

### Putting a window away

The right button on a title bar minimises a window, and a strip along the bottom
of the compositor's rectangle holds a tab for each one. Clicking a tab brings its
window back, raised and focused.

The tab is the whole reason this is a feature rather than a trap. A window that
can be put away and not brought back has been destroyed with extra steps, and
its client would go on drawing frames into a surface nobody would ever see
again. So the strip is *reserved*: windows are laid out and clamped above it and
cannot be moved or resized over the one place that undoes the thing. A tab sits
at its window's index rather than being packed beside the other minimised ones,
so it does not move when another window is put away -- a tab that shuffled
sideways under the pointer would be a tab somebody clicked and missed.

A minimised client is still told its frames are shown. It has not been stopped
and it does not know: a client that could tell whether it was visible would be a
client that could behave differently when nobody was looking.

Outstanding here: no window that is not a rectangle, and no damage tracking --
the compositor repaints its whole rectangle on every change, which is what makes
overlap simply work and is the thing that stops scaling first. Both belong to
Phase 9, which the compositor has already begun.



- PCI/PCIe enumeration, MSI/MSI-X, IOMMU
- User-space driver model over IPC
- `init`, service manager, libc, runtime, shell

## Phase 9 — Graphics and the compositor 🚧

- Nexus Graphics abstraction over the framebuffer, later a GPU
- Nexus Compositor: surfaces, damage tracking, frame scheduling, multi-monitor
- Input routing: focus, event delivery to processes, mouse and touchpad

Delivered ahead of schedule, because Phase 8 could not be finished without it:
the compositor is described under that phase and is real. Surfaces, windows that
move and stack and resize and minimise, a pointer, focus, and keys routed to one
client and not the others.

What this phase still means, now that the shape exists:

**Damage tracking.** The compositor repaints its whole rectangle on every
change. That is ninety thousand pixels and it is what makes overlap simply
work; on a full screen at sixty frames a second it would be two hundred million
pixels a second, and the arithmetic that avoids it -- which parts of which
windows are actually covered -- is the real content of this item.

**Frame scheduling.** Clients draw when they feel like it and the compositor
composites when they say so. There is no vertical blank, no deadline, and
nothing that says a frame arriving halfway through a scanout should wait.

**Multi-monitor.** One framebuffer, from the firmware. A second output means
asking the hardware rather than being handed one, which means a real display
driver, which is what "Nexus Graphics over a GPU" is.

**The rest of the screen.** The kernel still owns the banner and the status
panel; the compositor owns a rectangle. Moving the whole screen behind the
compositor means moving the panel into a program, which needs a font in user
space -- and that is Phase 10's business, not a hole in this one.

## Phase 10 — NexusUI 🚧

Declarative, reactive, GPU-accelerated widgets: layout, text, theming,
animation, accessibility, DPI scaling, localisation.

Started, from the bottom. A toolkit is drawing, layout and text, and none of
those could exist in user space until three things did.

**A heap.** `nexus_user::heap` asks the kernel for a memory object and maps it,
and gives the region to `nexus_mm::Heap` -- the kernel's own allocator, so the
thirty-four host tests that already exercise coalescing and alignment and
exhaustion cover the user side too. There is no `brk` and no anonymous `mmap`:
a heap is a memory object like any other and the only thing that makes it a
heap is what the allocator does with it. A program could have two.

**A face both sides can draw with.** The rasteriser moved out of the kernel's
build script into `shared/nexus-font`, which the kernel and every program now
depend on. Two faces would be a system where the same string is two widths
depending on who drew it. The face still covers exactly the characters the
translations use, plus whatever `shared/nexus-font/charset.txt` adds -- which
is how a program declares characters no translation happens to contain.

**Something to draw with.** `user/nexus-ui`: `Canvas` over packed or strided
memory, `Colour`, `Rect`, a `Column` that hands out rows, `wrap` that breaks
text to a width, `measure` that says how wide a string is. `nexus-client` now
draws a panel, a title bar, wrapped bilingual body text and a bar that grows
with the keys it has been routed -- and reports its own layout in a line built
at run time, which is the heap and the shared face both proving themselves:

```
[user] client: laid out 5 lines, widest 144 px, in 156 px of surface
```

Wrapping was where the first real bug was. Slicing by byte index put a cut in
the middle of a multi-byte character, which ASCII never notices and Japanese
hits on the first line.

What the phase still means: widgets rather than drawing calls, a retained tree
rather than repainting from nothing, state that invalidates only what changed,
theming, animation, accessibility, and DPI scaling -- and "GPU-accelerated"
waits on a display driver, which is Phase 9's remaining half.

## Phase 11 — Nexus Desktop ⬜

Dock, launcher, workspaces, snap layouts, Mission Control, notifications,
settings, files, terminal, search.

## Phase 12 — Networking ⬜

Ethernet, ARP, IPv4/IPv6, ICMP, UDP, TCP, DHCP, DNS, sockets, firewall.

## Phase 13 — Packages ⬜

The `.nexus` format, repositories, dependency resolution, signatures, rollback.

## Phase 14 — Security ⬜

Capabilities, sandboxing, the permission model, secure storage, code signing,
application identity, audit logging.

## Phase 15 — Compatibility ⬜

Linux: ELF loader, POSIX layer, libc, syscall translation.
Windows: PE loader, Win32 layer, DirectX translation.

## Phase 16 — Gaming ⬜

Vulkan, GPU drivers, controllers, HDR, VRR, shader cache, frame pacing.

## Phase 17 — AI ⬜

Nexus Intelligence (local and remote models, embeddings, semantic index),
Nexus Agent under capability control, Nexus Workflow automation.

Non-negotiable: the agent gets explicit, revocable, auditable capabilities.
Never ambient root.

## Phase 18 — Virtualisation ⬜

VT-x/AMD-V, virtio devices, containers.

## Phase 19 — Production hardening ⬜

Installer, recovery environment, A/B updates with rollback, crash reporting,
diagnostics, performance work, security audit.

---

## Internationalisation 🚧

Not a phase of its own: the specification asks for Japanese and English from the
start, so it was built alongside the display rather than retrofitted.

Delivered: translations in `locales/*.txt` with named placeholders so languages
can reorder their arguments, a build that fails on a missing translation, UTF-8
text rendering with half- and full-width advances, build-time glyph
rasterisation for CJK, and runtime language switching. See
[i18n.md](i18n.md).

Outstanding: input methods — the keyboard now exists and F1 switches language,
but composing Japanese needs a conversion engine and a candidate window — plus
text shaping, vertical writing, and further languages.

## Cross-cutting work

Carried alongside the phases rather than scheduled as one:

- **Real disk images.** A GPT + FAT32 builder, replacing QEMU's VVFAT, before
  any hardware boot.
- **CI.** `cargo fmt --check`, `clippy`, tests, both builds, and an automated
  QEMU boot test that fails on a missing serial marker.
- **Benchmarks.** Boot time, syscall and context-switch latency, IPC
  throughput, allocator throughput — measured from the phase that introduces
  each, so regressions are visible immediately.
- **Documentation.** A design document per subsystem, written with the code.
