# NexusFS — the on-disk format

Version 2. This document is the format; `kernel/nexus-kernel/src/fs/nexusfs.rs`
is one implementation of it, and where the two disagree the disk is right.

Everything is little-endian. Every offset is in bytes from the start of the
structure being described.

## Why there is one at all

FAT32 is read because UEFI requires it on the partition a machine boots from.
It is somebody else's format, it cannot express anything NexusOS wants to say,
and writing it safely means keeping two allocation tables and a free-cluster
count consistent through a power failure for no benefit.

NexusFS is the one the system chose. It is written as well as read, it knows
what a file is rather than inferring it from a directory entry, and it refuses
the operations that would corrupt it instead of performing them.

## Where it lives

Its own GPT partition, with the type GUID

    4E 45 58 55 53 46 53 00 00 01 4E 45 58 55 53 00

stored as those bytes in that order. It is not registered with anyone and is not
meant to be; a type GUID is an identifier, and this one identifies the thing to
the only system that reads it.

The partition is found by type and never by position. The build writes zeroes
into it. The first boot finds no superblock and formats it.

## Blocks

4096 bytes, eight 512-byte sectors. The block size is written into the
superblock and checked at mount, because a filesystem made with a different one
needs a different reader rather than a different constant — the buffers in the
implementation are fixed-size arrays.

Block numbers are relative to the first sector of the partition. Block 0 is the
superblock, so a block number of zero inside an inode means "no block", and any
code that would write file data to block 0 has a bug rather than a small file.

## Layout

| Region | Blocks | Contents |
|--------|--------|----------|
| superblock | 1, at block 0 | where everything else is |
| journal | `journal_blocks` (128), from block 1 | operations not yet carried out |
| block bitmap | `bitmap_blocks` | one bit per block of the volume |
| inode table | `inode_blocks` | `inode_count` inodes of 128 bytes |
| data | the rest | file and directory contents |

The regions are contiguous and in that order. The journal comes first because
finding it needs only the two fields that say where it is, and recovery runs
before anything else on the disk is trusted.

`mount` checks the layout rather than assuming it: `journal_start == 1`,
`bitmap_start == journal_start + journal_blocks`, `inode_start == bitmap_start +
bitmap_blocks`, `data_start == inode_start + inode_blocks`, `data_start <
total_blocks`, each region large enough for what it claims to describe, and
neither free count larger than its total. A checksum says the bytes are the ones
that were written; these say the writer was not confused.

`format` chooses one inode per 16 KiB of partition, with a floor of 16. It is
the only guess in the layout: too few inodes wastes the space they would have
described, too many wastes the table, and 16 KiB is what a filesystem holding
programs and configuration wants.

## Superblock

At block 0. Only the two free counts ever change after `format`.

| Offset | Size | Field |
|--------|------|-------|
| 0 | 8 | magic, `NEXUSFS\0` |
| 8 | 4 | version, 1 |
| 12 | 4 | block size in bytes |
| 16 | 8 | `total_blocks` |
| 24 | 8 | `inode_count` |
| 32 | 8 | `bitmap_start` |
| 40 | 8 | `bitmap_blocks` |
| 48 | 8 | `inode_start` |
| 56 | 8 | `inode_blocks` |
| 64 | 8 | `data_start` |
| 72 | 8 | `free_blocks` |
| 80 | 8 | `free_inodes` |
| 88 | 8 | `journal_start` |
| 96 | 8 | `journal_blocks` |
| 104 | 4 | CRC-32 of bytes 0..104 |
| 108 | 3988 | reserved, zero |

The checksum is the ordinary reflected CRC-32 with polynomial `0xEDB88320`, the
same one GPT uses, and it covers every field that says where something is. It is
written last during `format`, so a partition either holds a filesystem or does
not; there is no half-formatted state a reader would accept.

Version 2 rather than 1 because the journal's two fields took the word the
checksum used to sit in, and moved every region after it. A reader that took a
version-one superblock for this one would find the inode table where the journal
is, which is why the version is checked before anything else in it is believed.

A missing magic is *not formatted*, which is a thing to create. A bad checksum
is *damage*, which is a thing to report. Only the first leads to a format:
reformatting over a filesystem someone had files in is the worst thing this code
could do.

## Block bitmap

One bit per block, starting at block 0 of the volume, counting from the low bit
of each byte. Set means taken.

`format` marks the superblock, the bitmap and the inode table taken before the
bitmap first reaches the disk, so there is no instant at which the bitmap says
the superblock is free. It also marks the bits past `total_blocks` that the last
bitmap block has room to describe: an allocator that found one would hand out a
block beyond the partition and write over whatever follows it.

## Inodes

128 bytes each, `4096 / 128 = 32` to a block. Inode 0 is never used, so a zeroed
directory entry names nothing. Inode 1 is the root directory, made by `format`.
The inode holding a given number `n` is at block `inode_start + n / 32`, offset
`(n % 32) * 128`.

| Offset | Size | Field |
|--------|------|-------|
| 0 | 2 | kind: 0 free, 1 file, 2 directory |
| 2 | 2 | links — directory entries naming this inode |
| 4 | 4 | reserved, zero |
| 8 | 8 | size in bytes |
| 16 | 8 | created, in timer ticks |
| 24 | 8 | modified, in timer ticks |
| 32 | 88 | eleven direct block numbers, 8 bytes each |
| 120 | 8 | indirect block number, or 0 |

An inode with kind 0 is free, which is why a zeroed inode table is a table of
free inodes and `format` writes nothing else into it.

The indirect block holds up to `4096 / 8 = 512` further block numbers. A file
therefore occupies at most `11 + 512 = 523` blocks, or 2 142 208 bytes — just
over two megabytes. That is small and it is honest: a second level of
indirection is four lines and would make the limit a gigabyte, and adding it
before anything needs it would be adding a path nothing has ever walked.

The indirect block is metadata and is not counted in the file's size. It is
allocated when a file first grows past eleven blocks and freed when it shrinks
back, cleared from the inode before it is freed so that no inode ever points at
a block the bitmap calls free.

## The journal

An operation touches several blocks and has to be all or none of them. Making a
file writes an inode, a directory, a bitmap and a superblock; a power failure
between any two of them leaves the filesystem saying something that is not true.

So metadata is written twice. The blocks an operation changes go to the journal
first; then a descriptor naming them all goes down with a checksum over itself;
then the blocks are written where they belong; then the descriptor is erased.

The descriptor is one block:

| Offset | Size | Field |
|--------|------|-------|
| 0 | 8 | magic, `NEXUSJRN` |
| 8 | 8 | sequence number |
| 16 | 4 | how many blocks follow |
| 20 | 4 | reserved, zero |
| 24 | 8 × *count* | the block each journalled block belongs to |
| 4092 | 4 | CRC-32 of bytes 0..4092 |

The blocks themselves are at `journal_start + 1` onwards, in the same order. A
transaction may carry `journal_blocks - 1` = 127 blocks; making a file touches
four, and the margin is the point — a transaction that will not fit is refused,
and a refusal is only acceptable if it cannot happen for anything ordinary.

**Writing the descriptor is the commit.** Before it the operation did not
happen; after it the operation will happen even if the machine stops. That gives
three crashes and one recovery:

* before the descriptor — its checksum fails, nothing is replayed, and the
  operation simply never happened;
* after the descriptor, part-way through writing the blocks home — the next
  mount finds it and finishes the job;
* after the blocks are home but before the descriptor is erased — the next mount
  writes the same blocks again, which changes nothing, because replaying is
  idempotent by construction.

A descriptor naming a block outside the filesystem is refused however well it
checksums: writing there would take a corrupt disk and make it worse.

**File contents are not journalled.** A two-megabyte file would need a
two-megabyte journal to protect a write nobody promised was atomic. Contents go
down first and the metadata pointing at them second, so a failure leaves the old
file rather than a new one pointing at blocks that were never written. Directory
contents *are* journalled, because a directory is metadata whatever it is stored
in.

**What this rests on.** The block driver issues one request at a time and waits
for each to complete, so writes reach the *device* in the order above. Whether
the host or the drive then reorders them onto the platter is beyond this without
negotiating a flush, and that is a gap worth naming rather than a guarantee
worth pretending to.

## Directories

An ordinary file whose contents are entries packed end to end with no padding
and no gaps. Removing a name rewrites the file without it rather than leaving a
tombstone, which is affordable because the whole file is rewritten anyway.

| Offset | Size | Field |
|--------|------|-------|
| 0 | 4 | inode number |
| 4 | 1 | length of the name, 1..255 |
| 5 | 1 | kind, as in the inode |
| 6 | 2 | reserved, zero |
| 8 | *length* | the name, UTF-8, no terminator |

Entries may straddle block boundaries; a directory is parsed as one buffer.

Every length is checked against what is left of the buffer rather than trusted,
because these bytes came off a disk: a corrupt length is the difference between
an error and reading past the end of an allocation. An entry with inode 0, kind
0, a zero length, or a length longer than the remainder makes the whole
directory `Corrupt`. So does an entry whose kind disagrees with the inode it
names.

Names may not be empty, longer than 255 bytes, or contain `/` or a zero byte.
There are no `.` and `..` entries: paths are absolute and walked from the root,
so there is no current directory for them to be relative to.

## What is written when

A file can be read and written whole, and from an offset. The second only
became affordable once there was a block cache underneath: without one,
changing four bytes in the middle of a block meant four kilobytes off the
platter, four bytes changed, and four kilobytes back, for every call.

Writing past the end grows the file, and the gap reads as zeroes — a block is
zeroed when it is allocated, so a file with a hole in it cannot show whatever
the last file to own that block left there. There is no cursor and no seek: a
file has no position, only the offsets its holder chooses, which is the
arrangement two programs sharing a file can both be right about.

Growing a file takes every block it needs before writing any of them, and hands
them all back if one cannot be had, so a write that will not fit changes nothing
rather than half a file. Shrinking writes the inode first and frees the blocks
afterwards, so no inode ever points at a block the bitmap calls free.

Creating a name writes the inode before the directory entry, so the moment a
reader can see the name, the thing it names is there. Removing one writes the
directory first, so a failure part-way leaks an inode rather than leaving a name
pointing at nothing.

The superblock's free counts are written at the end of each operation that
changes them.

## What it does not do

**No permissions, no ownership, no hard links.** `links` exists in the inode and
is always 1.

**No timestamps worth the name.** Created and modified are timer ticks since
boot, because there is no real-time clock driver yet. They are comparable within
a boot and meaningless across one.

**No user-space `fsck`, no quotas, no mount points.** One volume, found by
partition type.

## Reaching it from user space

Through handles, and only through handles. There is no system call that takes a
path: a process opens one name inside a directory it already holds, so what it
can reach is exactly the subtree under what it was given. A program handed
nothing can open nothing, and there is no name it could use instead — the same
argument as for the spawner channel, applied to files.

`init` starts holding two things: handle 1, the channel to the spawn service,
and handle 2, the root directory. Both are given by the kernel before the
program runs. A machine whose disk carries no NexusFS still starts `init`, which
then finds that handle 2 is not a handle; the kernel does not invent an empty
filesystem so that the call succeeds.

| Call | Arguments | Result |
|------|-----------|--------|
| `NodeOpen` | directory, name | a handle |
| `NodeCreate` | directory, name, is-directory | a handle |
| `NodeRemove` | directory, name | — |
| `NodeList` | directory, buffer | bytes written |
| `NodeRead` | file, buffer | the file's length |
| `NodeWrite` | file, bytes | bytes written |
| `NodeSize` | handle | bytes |

A name is one component. A `/` in it is refused by the name check rather than
walked, because a directory handle that could be escaped with `../..` would not
be an authority over a subtree.

A handle opened through another carries no more rights than its parent, so
handing a program a read-only directory means something: nothing reachable
through it can be written.

A buffer too small is an error, never a truncation. Half a file that reports its
own length is indistinguishable from a whole one, and a program that acted on it
would act on a fragment.

Removing a name is refused while any handle still names what it names. A handle
carries an inode number, and an inode number is not a reference: freeing the
inode would leave the handle pointing at a number the filesystem is free to give
to the next file, and reading through it would then read that file. The kernel
keeps a count of open inodes for exactly this.

Errors are deliberately few. `ENOENT` and `EEXIST` are distinguished because a
program will act on both; a full disk, a corrupt directory and an unreachable
disk are all "the filesystem said no", because inventing a code per internal
condition would publish the implementation as an interface. The kernel log
carries the detail.

## Checking it

The journal finishes an operation that was interrupted. It says nothing about
damage that predates it: a block the bitmap calls taken that no file points at,
a block two files both claim, a name pointing at an inode that is not there.
Those come from a kernel that had a bug, a disk that lied, or a version of this
code that is no longer running - and no amount of journalling finds them,
because from the journal's point of view every one of those operations
completed.

So there is a check, and it runs at every mount, before anything has started
writing. It walks the inode table a block at a time - thirty-two inodes share
one, and reading a block per inode is thirty-two times the work - builds its own
picture of which blocks are reachable, and compares that with the bitmap.

What it does about a disagreement depends on which way round it is, and the
asymmetry is the design:

* a block the bitmap calls **taken that nothing reaches** is leaked. Reclaiming
  it is safe, because nothing can be pointing at it.
* a block **something reaches that the bitmap calls free** is dangerous: the
  allocator would hand it out from under a file that is using it. The bit is set.

In both cases the fix is to the bitmap, which is the derived thing. The files
are what the filesystem is *for*, and are never edited to make the bookkeeping
agree.

Three things are reported and not repaired: a block two files claim, a directory
entry naming something that is not there, and an inode whose block list does not
parse. Each of them can only be fixed by choosing which file to damage, and that
is not a decision to make without being asked.

## How it is tested

Three layers, because they catch different things.

`nexusfs::format_self_test` checks the format against itself with no disk
involved: entries encode and parse, a truncated entry and one claiming a name
longer than the buffer are both refused, an inode survives the round trip
including its last pointer, bits are found and set from the low end, unusable
names are rejected, and the arithmetic for the largest file agrees with the
pointer counts. It runs at boot rather than under `cargo test`, because the
kernel is a bare-metal binary with its own panic handler and cannot be linked
against the test harness — and a test that only exists in a configuration nobody
builds is not a test.

`fs::store::self_test` exercises the real volume on the real disk, and its last
check is the one that matters: the free-block and free-inode counts at the end
must be exactly what they were at the start. A write path that allocates a block
and forgets it passes every other check and fails that one. It then re-reads the
superblock from the disk and requires it to agree, because every later boot
starts by reading it.

`init` exercises the interface from ring 3 on every boot: it lists the root,
makes its own directory (or opens it, on a later boot), reads what the previous
boot left, writes a file, reads it back through the same handle, and then checks
the refusals — a buffer too small, a name with a separator in it, a name that is
not there, and removing a file that is still open.

`Volume::check_self_test` damages the filesystem on purpose, the only way that
damage can be made: it takes a block from the allocator and does nothing with
it, which is exactly what a kernel with a bug in its write path leaves behind.
It then requires the check to have been quiet before, to find exactly one leaked
block, to be quiet again afterwards, and to hand the same block out again - a
repair that had to be run twice would not be a repair.

`Volume::journal_self_test` crashes the filesystem on purpose. It writes a
transaction to the journal and then stops — no blocks written home, no
descriptor erased — which is exactly the state a power failure after a commit
leaves. Then it mounts, and requires the new contents to be there; mounts again,
and requires nothing left to replay; and abandons a transaction without
committing it, and requires that one to have left no trace. Both halves matter:
a recovery that replayed everything it found would be as wrong as one that
replayed nothing, because it would finish operations that never happened.

That test exists because the recovery path cannot be proved by reading it, and
the state it recovers from cannot be produced by a machine that is working. So
the machine is given one way to stop half way on purpose, and nothing but the
test uses it.

`scripts/test-persistence.ps1` boots twice on the same image without rebuilding.
The first boot must *make* the filesystem and report boot 1 with one line in
`/system/boot.log`; the second must *mount* it and report boot 2 with two lines.
One boot cannot tell a filesystem from a convincing pretence at one — a
filesystem that kept everything in memory would pass every same-boot check ever
written. It also requires `init` to *make* its file on the first boot and *find*
it on the second, which is the same claim one level up: not only that the
filesystem persists, but that a program can put something in it and get it back
through the system-call boundary and the handle table.
