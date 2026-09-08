# NexusFS — the on-disk format

Version 1. This document is the format; `kernel/nexus-kernel/src/fs/nexusfs.rs`
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
| block bitmap | `bitmap_blocks`, from block 1 | one bit per block of the volume |
| inode table | `inode_blocks` | `inode_count` inodes of 128 bytes |
| data | the rest | file and directory contents |

The regions are contiguous and in that order. `mount` checks this rather than
assuming it: `bitmap_start == 1`, `inode_start == bitmap_start +
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
| 88 | 8 | reserved, zero |
| 96 | 4 | CRC-32 of bytes 0..96 |
| 100 | 3996 | reserved, zero |

The checksum is the ordinary reflected CRC-32 with polynomial `0xEDB88320`, the
same one GPT uses, and it covers every field that says where something is. It is
written last during `format`, so a partition either holds a filesystem or does
not; there is no half-formatted state a reader would accept.

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

Reads and writes go straight to the disk; there is no cache. A file is read and
written entire, because with no cache underneath a byte-at-a-time interface
would be a byte-at-a-time disk.

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

**No journal.** A power failure between two writes can leave the bitmap saying a
block is taken that no file points at. That leaks space and corrupts nothing,
which is the right way round for the failure to be, and it is written down here
rather than glossed. There is no `fsck` yet either, so the space stays leaked.

**No permissions, no ownership, no hard links.** `links` exists in the inode and
is always 1.

**No timestamps worth the name.** Created and modified are timer ticks since
boot, because there is no real-time clock driver yet. They are comparable within
a boot and meaningless across one.

**No partial writes, no append, no seek.** See above.

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

`scripts/test-persistence.ps1` boots twice on the same image without rebuilding.
The first boot must *make* the filesystem and report boot 1 with one line in
`/system/boot.log`; the second must *mount* it and report boot 2 with two lines.
One boot cannot tell a filesystem from a convincing pretence at one — a
filesystem that kept everything in memory would pass every same-boot check ever
written. It also requires `init` to *make* its file on the first boot and *find*
it on the second, which is the same claim one level up: not only that the
filesystem persists, but that a program can put something in it and get it back
through the system-call boundary and the handle table.
