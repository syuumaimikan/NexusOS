//! NexusFS: the filesystem NexusOS keeps its own things in.
//!
//! FAT32 is read because UEFI requires it on the partition a machine boots
//! from. This is the one the system chose, and the difference shows in what it
//! can do: it is written as well as read, it knows what a file is rather than
//! inferring it from a directory entry, and it says no to the things that would
//! corrupt it rather than doing them.
//!
//! # The shape of it
//!
//! Blocks of four kilobytes, eight sectors each. A superblock says where
//! everything is; a bitmap says which blocks are free; a table of fixed-size
//! inodes says what each file is and where its blocks are; and directories are
//! ordinary files whose contents happen to be a list of names.
//!
//! An inode carries eleven direct block numbers and one indirect, which is a
//! block full of block numbers. That puts the largest file at just over two
//! megabytes. It is a small number and it is an honest one: a second level of
//! indirection is four lines and would make the limit a gigabyte, and adding it
//! before anything needs it would be adding a path nothing has ever walked.
//!
//! # Reading and writing part of a file
//!
//! A file can be read and written whole, and it can be read and written from an
//! offset. The second only became affordable when there was a [block
//! cache](super::cache) underneath: without one, writing four bytes in the
//! middle of a block meant reading four kilobytes off the platter, changing
//! four bytes, and writing four kilobytes back -- for every call.
//!
//! With the cache the read is usually free and the write is one block. It is
//! still write-*through*, so every write reaches the disk before the call
//! returns; what changed is how much of the file has to be touched, not how
//! durable the touch is.
//!
//! Appending is the case this exists for. The boot log used to be read entire
//! and written entire on every start, which is sixteen kilobytes each way to
//! add one line.
//!
//! # The journal
//!
//! An operation touches several blocks and has to be all or none of them.
//! Making a file writes an inode, a directory, a bitmap and a superblock; a
//! power failure between any two of them used to leave the filesystem saying
//! something that was not true.
//!
//! So metadata is written twice. Every block an operation changes goes to a
//! reserved run near the front of the partition, then a descriptor naming them
//! all goes down with a checksum over itself, and only then are the blocks
//! written where they belong. The descriptor is erased once they are.
//!
//! That makes every crash recoverable and every recovery the same act:
//!
//! * before the descriptor — its checksum fails, nothing is replayed, and the
//!   operation simply never happened;
//! * after the descriptor, part-way through writing the blocks home — the next
//!   mount finds it and finishes the job;
//! * after the blocks are home but before the descriptor is erased — the next
//!   mount writes the same blocks again, which changes nothing, because
//!   replaying is idempotent by construction.
//!
//! File *contents* are not journalled. A two-megabyte file would need a
//! two-megabyte journal to protect a write nobody promised was atomic, and the
//! data is written before the metadata that points at it, so a failure leaves
//! the old file rather than a new one full of someone else's blocks.
//!
//! # Checking it
//!
//! The journal finishes an operation that was interrupted. It says nothing
//! about damage that predates it: a block the bitmap calls taken that no file
//! points at, a block two files both claim, a name pointing at an inode that is
//! not there. Those come from a kernel that had a bug, or a disk that lied, or
//! a version of this code that is no longer running -- and no amount of
//! journalling finds them, because from the journal's point of view every one
//! of those operations completed.
//!
//! So there is a check, and it runs at every mount. It walks every inode and
//! every directory, builds its own picture of which blocks are reachable, and
//! compares that with what the bitmap says.
//!
//! What it does about a disagreement depends on which way round it is, and the
//! asymmetry is the whole design. A block the bitmap calls taken that nothing
//! reaches is *leaked*: reclaiming it is safe, because nothing can be pointing
//! at it. A block something reaches that the bitmap calls free is *dangerous*:
//! the allocator would hand it out from under a file that is using it, so the
//! bit is set rather than the file being touched. In both cases the fix is to
//! the bitmap, which is the derived thing; the files are what the filesystem is
//! for and are never edited to make the bookkeeping agree.
//!
//! # What it does not do yet
//!
//! No permissions, no timestamps beyond the tick a thing was made at, no links
//! beyond the one a directory entry is.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::drivers::virtio_blk::{self, SECTOR_SIZE};

/// Bytes in a NexusFS block.
pub const BLOCK_SIZE: usize = 4096;
/// Sectors in a block.
const SECTORS_PER_BLOCK: u64 = (BLOCK_SIZE / SECTOR_SIZE) as u64;

/// What a NexusFS superblock says it is.
const MAGIC: &[u8; 8] = b"NEXUSFS\0";
/// The only version this understands.
///
/// Two, because version one had no journal and its regions start in different
/// places. A reader that took a version-one superblock for this one would find
/// the inode table where the journal is.
const VERSION: u32 = 2;

/// What a journal descriptor says it is.
const JOURNAL_MAGIC: &[u8; 8] = b"NEXUSJRN";

/// Blocks reserved for the journal.
///
/// One for the descriptor and the rest for the blocks it describes, so an
/// operation may touch a hundred and twenty-seven of them. That is far more
/// than any operation here does -- making a file touches four -- and the
/// margin is the point: a transaction that will not fit is refused, and a
/// refusal is only acceptable if it cannot happen for anything ordinary.
const JOURNAL_BLOCKS: u64 = 128;

/// Blocks one transaction may carry.
const MAX_JOURNALLED: usize = (JOURNAL_BLOCKS - 1) as usize;

/// Bytes in an inode.
const INODE_SIZE: usize = 128;
/// Inodes in a block.
const INODES_PER_BLOCK: usize = BLOCK_SIZE / INODE_SIZE;

/// Direct block numbers an inode holds.
const DIRECT: usize = 11;
/// Block numbers in one indirect block.
const PER_INDIRECT: usize = BLOCK_SIZE / 8;
/// Blocks a file can occupy.
const MAX_BLOCKS: usize = DIRECT + PER_INDIRECT;
/// Bytes a file can hold.
pub const MAX_FILE: usize = MAX_BLOCKS * BLOCK_SIZE;

/// Longest name a directory entry can carry.
pub const MAX_NAME: usize = 255;

/// The root directory's inode number. Inode zero means "none".
pub const ROOT: u32 = 1;

/// One byte of partition per this many, turned into an inode.
///
/// A guess, and the only one in the layout. Too few inodes wastes the space
/// they would have described; too many wastes the table. Sixteen kilobytes an
/// inode is what a filesystem holding programs and configuration wants.
const BYTES_PER_INODE: u64 = 16 * 1024;

/// What an inode is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Nothing; the inode is free.
    Free,
    /// An ordinary file.
    File,
    /// A directory.
    Directory,
}

impl Kind {
    const fn to_raw(self) -> u16 {
        match self {
            Self::Free => 0,
            Self::File => 1,
            Self::Directory => 2,
        }
    }

    const fn from_raw(value: u16) -> Self {
        match value {
            1 => Self::File,
            2 => Self::Directory,
            _ => Self::Free,
        }
    }
}

impl core::fmt::Display for Kind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Free => "free",
            Self::File => "file",
            Self::Directory => "directory",
        })
    }
}

/// Why an operation could not be completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    /// The disk could not be read or written.
    Disk(virtio_blk::BlockError),
    /// There is no NexusFS here.
    NotFormatted,
    /// The superblock is for a version this does not understand.
    WrongVersion(u32),
    /// The superblock's checksum does not match its contents.
    BadSuperblock,
    /// The superblock describes a filesystem that cannot exist.
    BadGeometry,
    /// The partition is too small to hold a filesystem.
    TooSmall,
    /// No free blocks.
    NoSpace,
    /// No free inodes.
    NoInodes,
    /// No such file or directory.
    NotFound,
    /// A name that is already taken.
    Exists,
    /// The name is empty, too long, or contains a separator.
    BadName,
    /// The operation wanted the other kind of thing.
    WrongKind,
    /// A directory that still has something in it.
    NotEmpty,
    /// The file would be longer than this filesystem can describe.
    TooLarge,
    /// An inode number outside the table, or one that is free.
    BadInode(u32),
    /// A block number outside the filesystem, which means the metadata lies.
    BadBlock(u64),
    /// A directory's contents are not a list of entries.
    Corrupt,
}

impl core::fmt::Display for FsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Disk(error) => write!(f, "the disk could not be reached: {error}"),
            Self::NotFormatted => f.write_str("there is no NexusFS on this partition"),
            Self::WrongVersion(version) => write!(f, "this filesystem is version {version}"),
            Self::BadSuperblock => f.write_str("the superblock's checksum does not match"),
            Self::BadGeometry => f.write_str("the superblock does not describe a filesystem"),
            Self::TooSmall => f.write_str("the partition is too small for a filesystem"),
            Self::NoSpace => f.write_str("the filesystem is full"),
            Self::NoInodes => f.write_str("the filesystem has no free inodes"),
            Self::NotFound => f.write_str("no such file or directory"),
            Self::Exists => f.write_str("that name is already taken"),
            Self::BadName => f.write_str("that is not a usable name"),
            Self::WrongKind => f.write_str("that name is the other kind of thing"),
            Self::NotEmpty => f.write_str("the directory is not empty"),
            Self::TooLarge => f.write_str("the file would be too large for this filesystem"),
            Self::BadInode(number) => write!(f, "inode {number} is not a live inode"),
            Self::BadBlock(number) => write!(f, "block {number} is outside the filesystem"),
            Self::Corrupt => f.write_str("a directory's contents are not entries"),
        }
    }
}

/// Where everything is.
///
/// Read once at mount and written back whenever the free counts change, which
/// are the only fields that move.
#[derive(Debug, Clone, Copy)]
struct Superblock {
    total_blocks: u64,
    inode_count: u64,
    journal_start: u64,
    journal_blocks: u64,
    bitmap_start: u64,
    bitmap_blocks: u64,
    inode_start: u64,
    inode_blocks: u64,
    data_start: u64,
    free_blocks: u64,
    free_inodes: u64,
}

/// Byte offsets of the superblock's fields.
mod field {
    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 8;
    pub const BLOCK_SIZE: usize = 12;
    pub const TOTAL_BLOCKS: usize = 16;
    pub const INODE_COUNT: usize = 24;
    pub const JOURNAL_START: usize = 88;
    pub const JOURNAL_BLOCKS: usize = 96;
    pub const BITMAP_START: usize = 32;
    pub const BITMAP_BLOCKS: usize = 40;
    pub const INODE_START: usize = 48;
    pub const INODE_BLOCKS: usize = 56;
    pub const DATA_START: usize = 64;
    pub const FREE_BLOCKS: usize = 72;
    pub const FREE_INODES: usize = 80;
    /// Everything above this is what the checksum covers.
    ///
    /// Moved out to make room for the journal's two fields, which is why the
    /// version had to go up: a version-one superblock has its checksum where
    /// this one has a block number.
    pub const CHECKSUM: usize = 104;
}

/// What an inode holds, unpacked.
#[derive(Debug, Clone)]
struct Inode {
    kind: Kind,
    /// Directory entries pointing at this inode. One, so far, always.
    links: u16,
    size: u64,
    created: u64,
    modified: u64,
    direct: [u64; DIRECT],
    /// Block holding further block numbers, or zero for none.
    indirect: u64,
}

impl Inode {
    const fn empty() -> Self {
        Self {
            kind: Kind::Free,
            links: 0,
            size: 0,
            created: 0,
            modified: 0,
            direct: [0; DIRECT],
            indirect: 0,
        }
    }

    /// Read one out of the `INODE_SIZE` bytes it occupies on disk.
    fn decode(bytes: &[u8]) -> Self {
        let mut direct = [0u64; DIRECT];
        for (index, slot) in direct.iter_mut().enumerate() {
            *slot = read_u64(bytes, 32 + index * 8);
        }
        Self {
            kind: Kind::from_raw(read_u16(bytes, 0)),
            links: read_u16(bytes, 2),
            size: read_u64(bytes, 8),
            created: read_u64(bytes, 16),
            modified: read_u64(bytes, 24),
            direct,
            indirect: read_u64(bytes, 120),
        }
    }

    /// Write one into the `INODE_SIZE` bytes it occupies on disk.
    fn encode(&self, bytes: &mut [u8]) {
        bytes[..INODE_SIZE].fill(0);
        write_u16(bytes, 0, self.kind.to_raw());
        write_u16(bytes, 2, self.links);
        write_u64(bytes, 8, self.size);
        write_u64(bytes, 16, self.created);
        write_u64(bytes, 24, self.modified);
        for (index, block) in self.direct.iter().enumerate() {
            write_u64(bytes, 32 + index * 8, *block);
        }
        write_u64(bytes, 120, self.indirect);
    }

    /// Blocks the file's contents occupy, not counting the indirect block.
    fn data_blocks(&self) -> usize {
        (self.size as usize).div_ceil(BLOCK_SIZE)
    }
}

/// What a check found, and what it did about it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Check {
    /// Inodes in use.
    pub inodes: u64,
    /// Blocks reachable from them.
    pub blocks: u64,
    /// Blocks the bitmap called taken that nothing reaches. Reclaimed.
    pub leaked: u64,
    /// Blocks something reaches that the bitmap called free. Marked taken.
    pub unclaimed: u64,
    /// Blocks more than one file claims. Reported and not touched.
    pub shared: u64,
    /// Directory entries naming something that is not there. Reported.
    pub dangling: u64,
    /// Inodes whose block list could not be read. Reported.
    pub unreadable: u64,
    /// Whether the superblock's free counts were wrong and have been corrected.
    pub counts_corrected: bool,
}

impl Check {
    /// Whether anything at all was wrong.
    #[must_use]
    pub fn clean(&self) -> bool {
        self.leaked == 0
            && self.unclaimed == 0
            && self.shared == 0
            && self.dangling == 0
            && self.unreadable == 0
            && !self.counts_corrected
    }

    /// Whether something was found that this cannot fix.
    ///
    /// A leak is repaired and a miscounted superblock is corrected. A block two
    /// files claim is neither: choosing which of them keeps it is choosing
    /// which one to corrupt, and that is not a decision to make without being
    /// asked.
    #[must_use]
    pub fn needs_attention(&self) -> bool {
        self.shared > 0 || self.dangling > 0 || self.unreadable > 0
    }
}

/// A mounted NexusFS.
pub struct Volume {
    /// First sector of the partition; every block number is relative to it.
    start_lba: u64,
    superblock: Superblock,
    /// Blocks an operation has changed but not yet committed.
    ///
    /// `None` outside a transaction, when a write goes straight to its block.
    /// `Some` inside one, when it is held here instead -- and read back from
    /// here too, so that an operation reading a block it has already changed
    /// sees its own change and not what is still on the disk.
    pending: Option<Vec<(u64, Box<[u8; BLOCK_SIZE]>)>>,
    /// Which transaction this will be, for the descriptor and the log.
    sequence: u64,
}

/// One entry of a directory.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub inode: u32,
    pub kind: Kind,
    /// Bytes the thing the entry names holds.
    pub size: u64,
}

/// Bytes of a directory entry before its name.
///
/// Four for the inode number, one for the length of the name, one for what kind
/// of thing it is, and two spare so that the name starts on an even boundary
/// and there is somewhere to put a flag when one is needed.
const ENTRY_HEADER: usize = 8;

impl Volume {
    // -- Making one ---------------------------------------------------------

    /// Write a fresh filesystem over `sectors` sectors starting at `start_lba`.
    ///
    /// Everything before the data region is written here, including the root
    /// directory, so a volume that mounts is a volume that is complete. There
    /// is no half-formatted state to recover from because there is no point at
    /// which one exists that a reader would accept: the superblock goes down
    /// last, and until it does the partition reads as unformatted.
    pub fn format(start_lba: u64, sectors: u64) -> Result<Self, FsError> {
        let total_blocks = sectors / SECTORS_PER_BLOCK;
        if total_blocks < 16 {
            return Err(FsError::TooSmall);
        }

        let inode_count = (total_blocks * BLOCK_SIZE as u64 / BYTES_PER_INODE).max(16);
        let inode_blocks = inode_count.div_ceil(INODES_PER_BLOCK as u64);
        let bitmap_blocks = total_blocks.div_ceil(BLOCK_SIZE as u64 * 8);

        // The journal comes first, right after the superblock, so that finding
        // it needs only the two fields that say where it is -- which matters
        // because recovery runs before anything else is trusted.
        let journal_start = 1;
        let bitmap_start = journal_start + JOURNAL_BLOCKS;
        let inode_start = bitmap_start + bitmap_blocks;
        let data_start = inode_start + inode_blocks;
        if data_start + 1 >= total_blocks {
            return Err(FsError::TooSmall);
        }

        let mut volume = Self {
            start_lba,
            // Formatting is not journalled. There is nothing to protect: a
            // failure part-way through leaves a partition with no valid
            // superblock, which is a partition that has no filesystem on it --
            // exactly what it was before.
            pending: None,
            sequence: 0,
            superblock: Superblock {
                total_blocks,
                inode_count,
                journal_start,
                journal_blocks: JOURNAL_BLOCKS,
                bitmap_start,
                bitmap_blocks,
                inode_start,
                inode_blocks,
                data_start,
                free_blocks: total_blocks - data_start,
                // Inode zero is not one, and the root is spoken for.
                free_inodes: inode_count - 2,
            },
        };

        // The journal and the inode table, zeroed. A zeroed inode table is a
        // table of free inodes; a zeroed journal is one with nothing to replay,
        // which is what a fresh filesystem must look like or the first mount
        // would try to finish an operation from whatever was on the disk.
        let empty = [0u8; BLOCK_SIZE];
        for block in journal_start..data_start {
            volume.write_block(block, &empty)?;
        }

        // The bitmap, built in memory and written once. Every metadata block is
        // marked taken before the first one reaches the disk, so there is no
        // instant at which the bitmap says the superblock is free. So are the
        // blocks past the end of the filesystem that the last bitmap block has
        // room to describe -- an allocator that found one would hand out a block
        // beyond the partition.
        let mut bitmap = vec![0u8; bitmap_blocks as usize * BLOCK_SIZE];
        for block in 0..data_start {
            set_bit(&mut bitmap, block as usize, true);
        }
        for block in total_blocks..bitmap.len() as u64 * 8 {
            set_bit(&mut bitmap, block as usize, true);
        }
        for index in 0..bitmap_blocks {
            let offset = index as usize * BLOCK_SIZE;
            let mut chunk = [0u8; BLOCK_SIZE];
            chunk.copy_from_slice(&bitmap[offset..offset + BLOCK_SIZE]);
            volume.write_block(bitmap_start + index, &chunk)?;
        }

        // The root: a directory with nothing in it, which is a file of length
        // zero and so needs no blocks at all. Inode zero is left free forever,
        // so that a zeroed directory entry names nothing rather than the root.
        let now = crate::arch::time::ticks();
        let mut root = Inode::empty();
        root.kind = Kind::Directory;
        root.links = 1;
        root.created = now;
        root.modified = now;
        volume.write_inode(ROOT, &root)?;

        // Last, so that a partition either holds a filesystem or does not.
        volume.write_superblock()?;
        Ok(volume)
    }

    /// Read the superblock of the filesystem at `start_lba`.
    pub fn mount(start_lba: u64) -> Result<Self, FsError> {
        let mut block = [0u8; BLOCK_SIZE];
        read_block_at(start_lba, 0, &mut block)?;

        if &block[field::MAGIC..field::MAGIC + 8] != MAGIC {
            return Err(FsError::NotFormatted);
        }
        let version = read_u32(&block, field::VERSION);
        if version != VERSION {
            return Err(FsError::WrongVersion(version));
        }

        // The checksum covers everything before it, which is every field that
        // says where something is. A superblock that has been half-written says
        // so rather than sending a reader to the wrong block.
        let stated = read_u32(&block, field::CHECKSUM);
        if crc32(&block[..field::CHECKSUM]) != stated {
            return Err(FsError::BadSuperblock);
        }

        // A block size other than this one would need a different reader, not a
        // different constant: the buffers here are fixed-size arrays.
        if read_u32(&block, field::BLOCK_SIZE) as usize != BLOCK_SIZE {
            return Err(FsError::BadGeometry);
        }

        let superblock = Superblock {
            total_blocks: read_u64(&block, field::TOTAL_BLOCKS),
            inode_count: read_u64(&block, field::INODE_COUNT),
            journal_start: read_u64(&block, field::JOURNAL_START),
            journal_blocks: read_u64(&block, field::JOURNAL_BLOCKS),
            bitmap_start: read_u64(&block, field::BITMAP_START),
            bitmap_blocks: read_u64(&block, field::BITMAP_BLOCKS),
            inode_start: read_u64(&block, field::INODE_START),
            inode_blocks: read_u64(&block, field::INODE_BLOCKS),
            data_start: read_u64(&block, field::DATA_START),
            free_blocks: read_u64(&block, field::FREE_BLOCKS),
            free_inodes: read_u64(&block, field::FREE_INODES),
        };

        // The regions have to be in order, inside the volume, and large enough
        // for what they claim to describe. A checksum says the bytes are the
        // ones that were written; this says the writer was not confused.
        let ordered = superblock.journal_start == 1
            && superblock.journal_blocks >= 2
            && superblock.bitmap_start == superblock.journal_start + superblock.journal_blocks
            && superblock.inode_start == superblock.bitmap_start + superblock.bitmap_blocks
            && superblock.data_start == superblock.inode_start + superblock.inode_blocks
            && superblock.data_start < superblock.total_blocks;
        let sized = superblock.bitmap_blocks
            >= superblock.total_blocks.div_ceil(BLOCK_SIZE as u64 * 8)
            && superblock.inode_blocks >= superblock.inode_count.div_ceil(INODES_PER_BLOCK as u64)
            && superblock.free_blocks <= superblock.total_blocks
            && superblock.free_inodes <= superblock.inode_count;
        if !ordered || !sized {
            return Err(FsError::BadGeometry);
        }

        let mut volume = Self {
            start_lba,
            superblock,
            pending: None,
            sequence: 0,
        };

        // Before anything else reads a block. A mount that served a request
        // from a filesystem with an unfinished operation still in the journal
        // would be serving a half-applied one.
        match volume.recover()? {
            0 => {}
            blocks => {
                crate::kprintln!(
                    "[fs  ] finished an operation the last boot did not: {blocks} blocks replayed"
                );
                // The superblock may have been one of them, so it is read
                // again rather than kept: the copy in hand is from before the
                // replay and would have the old free counts.
                let mut fresh = [0u8; BLOCK_SIZE];
                read_block_at(start_lba, 0, &mut fresh)?;
                volume.superblock.free_blocks = read_u64(&fresh, field::FREE_BLOCKS);
                volume.superblock.free_inodes = read_u64(&fresh, field::FREE_INODES);
            }
        }

        Ok(volume)
    }

    /// Mount the filesystem there, or make one if there is none.
    ///
    /// A filesystem that does not exist yet is a thing to create, and one that
    /// exists but is damaged is a thing to report: the difference matters, so
    /// only [`FsError::NotFormatted`] leads to a format. A bad checksum keeps
    /// its disk, because reformatting over a filesystem someone had files in is
    /// the worst thing this code could do.
    pub fn mount_or_format(start_lba: u64, sectors: u64) -> Result<(Self, bool), FsError> {
        match Self::mount(start_lba) {
            Ok(volume) => Ok((volume, false)),
            Err(FsError::NotFormatted) => Self::format(start_lba, sectors).map(|v| (v, true)),
            Err(error) => Err(error),
        }
    }

    // -- What it holds ------------------------------------------------------

    /// Blocks the filesystem has, and how many are free.
    #[must_use]
    pub fn space(&self) -> (u64, u64) {
        (self.superblock.total_blocks, self.superblock.free_blocks)
    }

    /// Inodes the filesystem has, and how many are free.
    #[must_use]
    // Compiled always and called only by the build that runs the destructive
    // filesystem checks. Kept rather than gated so that the code the test suite
    // exercises is the same code the shipped kernel contains.
    #[cfg_attr(not(feature = "deep-selftest"), allow(dead_code))]
    pub fn inodes(&self) -> (u64, u64) {
        (self.superblock.inode_count, self.superblock.free_inodes)
    }

    /// Bytes one block holds, so a caller can turn the counts into a size.
    #[must_use]
    pub const fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    /// The sector the filesystem starts at, which is how it is found again.
    #[must_use]
    // Compiled always and called only by the build that runs the destructive
    // filesystem checks. Kept rather than gated so that the code the test suite
    // exercises is the same code the shipped kernel contains.
    #[cfg_attr(not(feature = "deep-selftest"), allow(dead_code))]
    pub const fn start_lba(&self) -> u64 {
        self.start_lba
    }

    // -- Names --------------------------------------------------------------

    /// Everything in a directory.
    pub fn list(&self, directory: u32) -> Result<Vec<Entry>, FsError> {
        let inode = self.read_inode(directory)?;
        if inode.kind != Kind::Directory {
            return Err(FsError::WrongKind);
        }
        let bytes = self.read_inode_data(&inode)?;
        let mut entries = Vec::new();
        for (number, name, kind) in parse_directory(&bytes)? {
            // The size comes from the inode rather than the entry, so a
            // directory cannot disagree with the thing it names -- and an
            // entry pointing at an inode that is not there stops the listing
            // rather than being reported as an empty file.
            let (actual, size) = self.stat(number)?;
            if actual != kind {
                return Err(FsError::Corrupt);
            }
            entries.push(Entry {
                name,
                inode: number,
                kind,
                size,
            });
        }
        Ok(entries)
    }

    /// Find one name in one directory.
    pub fn lookup(&self, directory: u32, name: &str) -> Result<Entry, FsError> {
        self.list(directory)?
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or(FsError::NotFound)
    }

    /// Walk a path from the root.
    ///
    /// Absolute only, and without `.` or `..`, because a path with no current
    /// directory to be relative to has nothing for them to mean. Empty
    /// components are skipped, so `/a//b` and `/a/b/` are `/a/b`.
    pub fn resolve(&self, path: &str) -> Result<u32, FsError> {
        let mut inode = ROOT;
        for component in path.split('/') {
            if component.is_empty() {
                continue;
            }
            let entry = self.lookup(inode, component)?;
            inode = entry.inode;
        }
        Ok(inode)
    }

    /// Make a file or a directory in `directory`.
    ///
    /// Returns the new inode number. The entry and the inode go down together
    /// in the sense that matters: the inode is written first, so the moment a
    /// reader can see the name, the thing it names is already there.
    pub fn create(&mut self, directory: u32, name: &str, kind: Kind) -> Result<u32, FsError> {
        check_name(name)?;
        if kind == Kind::Free {
            return Err(FsError::WrongKind);
        }

        let parent = self.read_inode(directory)?;
        if parent.kind != Kind::Directory {
            return Err(FsError::WrongKind);
        }
        let contents = self.read_inode_data(&parent)?;
        if parse_directory(&contents)?
            .iter()
            .any(|(_, existing, _)| existing == name)
        {
            return Err(FsError::Exists);
        }

        // One transaction. Making a file changes an inode, a directory, a
        // bitmap and a superblock, and a filesystem in which some of those
        // happened is a filesystem that is wrong -- so either all of them
        // survive a power failure or none of them do.
        self.begin();
        match self.create_within(directory, name, kind, contents) {
            Ok(number) => {
                self.commit()?;
                Ok(number)
            }
            Err(error) => {
                // Nothing reached the disk, so there is nothing to undo.
                self.abandon();
                Err(error)
            }
        }
    }

    /// The body of [`create`](Self::create), inside a transaction.
    fn create_within(
        &mut self,
        directory: u32,
        name: &str,
        kind: Kind,
        mut contents: Vec<u8>,
    ) -> Result<u32, FsError> {
        let number = self.allocate_inode(kind)?;
        contents.extend_from_slice(&encode_entry(number, name, kind));
        self.write_inode_data(directory, &contents, true)?;
        self.write_superblock()?;
        Ok(number)
    }

    /// Remove a name, and the thing it named.
    ///
    /// There are no hard links yet, so the two are the same act. A directory has
    /// to be empty first: removing one that is not would strand everything
    /// inside it, and a filesystem that leaks a subtree on a typo is worse than
    /// one that makes you say what you mean.
    pub fn unlink(&mut self, directory: u32, name: &str) -> Result<(), FsError> {
        check_name(name)?;
        let parent = self.read_inode(directory)?;
        if parent.kind != Kind::Directory {
            return Err(FsError::WrongKind);
        }

        let contents = self.read_inode_data(&parent)?;
        let entries = parse_directory(&contents)?;
        let found = entries
            .iter()
            .find(|(_, existing, _)| existing == name)
            .ok_or(FsError::NotFound)?;
        let number = found.0;

        let victim = self.read_inode(number)?;
        if victim.kind == Kind::Directory && !self.read_inode_data(&victim)?.is_empty() {
            return Err(FsError::NotEmpty);
        }

        // The name goes first. Until it does the inode is reachable, and after
        // it does the inode is not, so a failure between the two leaks the
        // inode rather than leaving a name pointing at nothing.
        let mut rebuilt = Vec::with_capacity(contents.len());
        for (inode, existing, kind) in &entries {
            if existing != name {
                rebuilt.extend_from_slice(&encode_entry(*inode, existing, *kind));
            }
        }

        // One transaction, for the same reason as making one: a filesystem in
        // which the name is gone but the blocks are still taken is wrong, and
        // so is one in which the blocks are free but the name still points at
        // them. The second is the dangerous half.
        self.begin();
        match self.unlink_within(directory, number, &rebuilt) {
            Ok(()) => self.commit(),
            Err(error) => {
                self.abandon();
                Err(error)
            }
        }
    }

    /// The body of [`unlink`](Self::unlink), inside a transaction.
    fn unlink_within(
        &mut self,
        directory: u32,
        number: u32,
        rebuilt: &[u8],
    ) -> Result<(), FsError> {
        self.write_inode_data(directory, rebuilt, true)?;
        self.truncate(number)?;
        self.free_inode(number)?;
        self.write_superblock()
    }

    // -- Contents -----------------------------------------------------------

    /// Everything in a file.
    pub fn read(&self, inode: u32) -> Result<Vec<u8>, FsError> {
        let inode = self.read_inode(inode)?;
        if inode.kind != Kind::File {
            return Err(FsError::WrongKind);
        }
        self.read_inode_data(&inode)
    }

    /// Replace everything in a file.
    pub fn write(&mut self, inode: u32, data: &[u8]) -> Result<(), FsError> {
        let existing = self.read_inode(inode)?;
        if existing.kind != Kind::File {
            return Err(FsError::WrongKind);
        }

        // The contents are written outside the transaction and the metadata
        // inside it. A two-megabyte file would need a two-megabyte journal to
        // protect a write nobody promised was atomic; what the journal is for
        // here is the inode and the bitmap, so that a failure leaves the old
        // file rather than a new one pointing at blocks that were never
        // written.
        self.begin();
        match self.write_inode_data(inode, data, false) {
            Ok(()) => {
                let result = self.write_superblock();
                if result.is_err() {
                    self.abandon();
                    return result;
                }
                self.commit()
            }
            Err(error) => {
                self.abandon();
                Err(error)
            }
        }
    }

    /// Change part of a file, growing it if the change runs past the end.
    ///
    /// The metadata goes through the journal and the contents do not, exactly as
    /// a whole-file write does: a failure leaves the old file rather than a new
    /// one pointing at blocks that were never written.
    pub fn write_at(&mut self, inode: u32, offset: u64, data: &[u8]) -> Result<u64, FsError> {
        self.begin();
        match self.write_at_inode(inode, offset, data) {
            Ok(written) => {
                if let Err(error) = self.write_superblock() {
                    self.abandon();
                    return Err(error);
                }
                self.commit()?;
                Ok(written)
            }
            Err(error) => {
                self.abandon();
                Err(error)
            }
        }
    }

    /// Read part of a file, returning how much there was.
    pub fn read_at(&self, inode: u32, offset: u64, buffer: &mut [u8]) -> Result<usize, FsError> {
        self.read_at_inode(inode, offset, buffer)
    }

    /// What a name is, without reading it.
    pub fn stat(&self, inode: u32) -> Result<(Kind, u64), FsError> {
        let inode = self.read_inode(inode)?;
        Ok((inode.kind, inode.size))
    }

    // -- Files, underneath --------------------------------------------------

    /// Read a file's blocks into one buffer, trimmed to its length.
    fn read_inode_data(&self, inode: &Inode) -> Result<Vec<u8>, FsError> {
        let blocks = self.block_list(inode)?;
        let mut data = Vec::with_capacity(inode.size as usize);
        let mut buffer = [0u8; BLOCK_SIZE];
        for block in blocks {
            self.read_block(block, &mut buffer)?;
            let remaining = inode.size as usize - data.len();
            data.extend_from_slice(&buffer[..remaining.min(BLOCK_SIZE)]);
        }
        Ok(data)
    }

    /// Replace a file's contents, allocating and freeing blocks to suit.
    ///
    /// Blocks are taken before anything is written, so a write that will not fit
    /// changes nothing at all rather than half of a file.
    /// Take enough blocks for a file of `wanted` blocks, or none at all.
    ///
    /// Every block is taken before any of them is used, and a failure part-way
    /// hands back what was taken -- so a write that will not fit changes
    /// nothing rather than half of a file.
    fn grow_blocks(
        &mut self,
        inode: &mut Inode,
        blocks: &mut Vec<u64>,
        wanted: usize,
    ) -> Result<(), FsError> {
        if wanted <= blocks.len() {
            return Ok(());
        }

        let mut taken = Vec::new();
        // A file that grows past its direct blocks needs somewhere to keep the
        // rest; that block is metadata and is not counted as content.
        if wanted > DIRECT && inode.indirect == 0 {
            // Nothing has been taken yet, so a failure here owes nothing back.
            let block = self.allocate_block()?;
            inode.indirect = block;
            taken.push(block);
            let empty = [0u8; BLOCK_SIZE];
            if let Err(error) = self.write_block(block, &empty) {
                self.give_back(&taken);
                return Err(error);
            }
        }
        while blocks.len() < wanted {
            match self.allocate_block() {
                Ok(block) => {
                    blocks.push(block);
                    taken.push(block);
                }
                Err(error) => {
                    // The list is left as it was found, so the caller's inode
                    // still describes a file that exists.
                    blocks
                        .truncate(blocks.len() - (taken.len() - usize::from(inode.indirect != 0)));
                    self.give_back(&taken);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// Change part of a file, growing it if the change runs past the end.
    ///
    /// Read-modify-write on the two blocks at the ends, whole-block writes in
    /// between. The reads are what the cache is for: without one, changing four
    /// bytes in the middle of a file meant four kilobytes off the platter and
    /// four kilobytes back.
    ///
    /// A gap left by writing past the end reads as zeroes, because a block is
    /// zeroed when it is allocated. That is a promise worth stating: a file with
    /// a hole in it must not show whatever the last file to own that block left
    /// there.
    fn write_at_inode(&mut self, number: u32, offset: u64, data: &[u8]) -> Result<u64, FsError> {
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or(FsError::TooLarge)?;
        if end > MAX_FILE as u64 {
            return Err(FsError::TooLarge);
        }
        if data.is_empty() {
            return Ok(0);
        }

        let mut inode = self.read_inode(number)?;
        if inode.kind != Kind::File {
            return Err(FsError::WrongKind);
        }

        let mut blocks = self.block_list(&inode)?;
        let wanted = (end as usize).div_ceil(BLOCK_SIZE);
        self.grow_blocks(&mut inode, &mut blocks, wanted)?;

        let mut written = 0usize;
        while written < data.len() {
            let at = offset as usize + written;
            let index = at / BLOCK_SIZE;
            let within = at % BLOCK_SIZE;
            let take = (BLOCK_SIZE - within).min(data.len() - written);
            let block = *blocks.get(index).ok_or(FsError::Corrupt)?;

            let mut buffer = [0u8; BLOCK_SIZE];
            // Only where the change does not cover the whole block. A read
            // before a full overwrite is a read of something about to be
            // discarded, and on the growing edge it is a read of a block that
            // was just allocated and is already zero.
            if within != 0 || take != BLOCK_SIZE {
                self.read_block(block, &mut buffer)?;
            }
            buffer[within..within + take].copy_from_slice(&data[written..written + take]);
            self.write_block_now(block, &buffer)?;
            written += take;
        }

        if end > inode.size {
            inode.size = end;
        }
        inode.modified = crate::arch::time::ticks();
        self.set_block_list(&mut inode, &blocks)?;
        self.write_inode(number, &inode)?;
        Ok(written as u64)
    }

    /// Read part of a file into `buffer`, returning how much there was.
    ///
    /// Short at the end of the file rather than an error: a caller that asks for
    /// more than is there has reached the end, which is a thing that happens and
    /// not a thing that went wrong.
    fn read_at_inode(&self, number: u32, offset: u64, buffer: &mut [u8]) -> Result<usize, FsError> {
        let inode = self.read_inode(number)?;
        if inode.kind != Kind::File {
            return Err(FsError::WrongKind);
        }
        if offset >= inode.size || buffer.is_empty() {
            return Ok(0);
        }

        let blocks = self.block_list(&inode)?;
        let available = (inode.size - offset) as usize;
        let wanted = buffer.len().min(available);

        let mut done = 0usize;
        let mut block_buffer = [0u8; BLOCK_SIZE];
        while done < wanted {
            let at = offset as usize + done;
            let index = at / BLOCK_SIZE;
            let within = at % BLOCK_SIZE;
            let take = (BLOCK_SIZE - within).min(wanted - done);
            let block = *blocks.get(index).ok_or(FsError::Corrupt)?;

            self.read_block(block, &mut block_buffer)?;
            buffer[done..done + take].copy_from_slice(&block_buffer[within..within + take]);
            done += take;
        }
        Ok(done)
    }

    fn write_inode_data(
        &mut self,
        number: u32,
        data: &[u8],
        journal_contents: bool,
    ) -> Result<(), FsError> {
        if data.len() > MAX_FILE {
            return Err(FsError::TooLarge);
        }
        let mut inode = self.read_inode(number)?;
        let wanted = data.len().div_ceil(BLOCK_SIZE);
        let mut blocks = self.block_list(&inode)?;

        self.grow_blocks(&mut inode, &mut blocks, wanted)?;

        // Shrink, keeping the blocks to free until the inode no longer points
        // at them.
        let released: Vec<u64> = blocks.split_off(wanted.min(blocks.len()));

        for (index, block) in blocks.iter().enumerate() {
            let offset = index * BLOCK_SIZE;
            let mut buffer = [0u8; BLOCK_SIZE];
            let end = (offset + BLOCK_SIZE).min(data.len());
            buffer[..end - offset].copy_from_slice(&data[offset..end]);
            // A directory's contents are metadata and go through the journal;
            // a file's are not and go straight down, before the metadata that
            // will point at them.
            if journal_contents {
                self.write_block(*block, &buffer)?;
            } else {
                self.write_block_now(*block, &buffer)?;
            }
        }

        // A file small enough to fit in its direct blocks does not need an
        // indirect one, and keeping it would be a block held by a file that
        // cannot reach it. It is cleared from the inode before it is freed, so
        // no inode ever points at a block the bitmap says is free.
        let stranded = if wanted <= DIRECT && inode.indirect != 0 {
            let block = inode.indirect;
            inode.indirect = 0;
            Some(block)
        } else {
            None
        };

        inode.size = data.len() as u64;
        inode.modified = crate::arch::time::ticks();
        self.set_block_list(&mut inode, &blocks)?;
        self.write_inode(number, &inode)?;

        for block in released.into_iter().chain(stranded) {
            self.free_block(block)?;
        }
        Ok(())
    }

    /// Free everything a file holds, leaving it empty.
    fn truncate(&mut self, number: u32) -> Result<(), FsError> {
        let mut inode = self.read_inode(number)?;
        for block in self.block_list(&inode)? {
            self.free_block(block)?;
        }
        if inode.indirect != 0 {
            self.free_block(inode.indirect)?;
            inode.indirect = 0;
        }
        inode.direct = [0; DIRECT];
        inode.size = 0;
        self.write_inode(number, &inode)
    }

    /// The blocks a file's contents live in, in order.
    fn block_list(&self, inode: &Inode) -> Result<Vec<u64>, FsError> {
        let count = inode.data_blocks();
        if count > MAX_BLOCKS {
            return Err(FsError::Corrupt);
        }
        let mut blocks = Vec::with_capacity(count);
        for index in 0..count.min(DIRECT) {
            blocks.push(self.checked_data_block(inode.direct[index])?);
        }
        if count > DIRECT {
            if inode.indirect == 0 {
                return Err(FsError::Corrupt);
            }
            let mut buffer = [0u8; BLOCK_SIZE];
            self.read_block(self.checked_data_block(inode.indirect)?, &mut buffer)?;
            for index in 0..count - DIRECT {
                blocks.push(self.checked_data_block(read_u64(&buffer, index * 8))?);
            }
        }
        Ok(blocks)
    }

    /// Point an inode at exactly these blocks.
    fn set_block_list(&mut self, inode: &mut Inode, blocks: &[u64]) -> Result<(), FsError> {
        if blocks.len() > MAX_BLOCKS {
            return Err(FsError::TooLarge);
        }
        inode.direct = [0; DIRECT];
        for (index, block) in blocks.iter().take(DIRECT).enumerate() {
            inode.direct[index] = *block;
        }
        if blocks.len() > DIRECT {
            // Block zero is the superblock. Writing the overflow list there
            // because a caller forgot to allocate an indirect block would
            // destroy the filesystem, so it is refused rather than trusted.
            if inode.indirect == 0 {
                return Err(FsError::Corrupt);
            }
            let mut buffer = [0u8; BLOCK_SIZE];
            for (index, block) in blocks[DIRECT..].iter().enumerate() {
                write_u64(&mut buffer, index * 8, *block);
            }
            self.write_block(inode.indirect, &buffer)?;
        }
        Ok(())
    }

    /// Hand back blocks taken for a write that then failed.
    ///
    /// Errors are dropped on purpose: this runs while another error is already
    /// on its way out, and the caller is owed that one, not this one. What a
    /// failure here costs is leaked space, which is what a missing journal costs
    /// anyway.
    fn give_back(&mut self, blocks: &[u64]) {
        for block in blocks {
            self.free_block(*block).ok();
        }
    }

    /// Reject a block number the metadata should never have contained.
    fn checked_data_block(&self, block: u64) -> Result<u64, FsError> {
        if block < self.superblock.data_start || block >= self.superblock.total_blocks {
            return Err(FsError::BadBlock(block));
        }
        Ok(block)
    }

    // -- Inodes, underneath -------------------------------------------------

    /// Where an inode's bytes are.
    fn inode_place(&self, number: u32) -> Result<(u64, usize), FsError> {
        if number == 0 || u64::from(number) >= self.superblock.inode_count {
            return Err(FsError::BadInode(number));
        }
        let index = number as usize;
        Ok((
            self.superblock.inode_start + (index / INODES_PER_BLOCK) as u64,
            (index % INODES_PER_BLOCK) * INODE_SIZE,
        ))
    }

    fn read_inode(&self, number: u32) -> Result<Inode, FsError> {
        let (block, offset) = self.inode_place(number)?;
        let mut buffer = [0u8; BLOCK_SIZE];
        self.read_block(block, &mut buffer)?;
        let inode = Inode::decode(&buffer[offset..offset + INODE_SIZE]);
        if inode.kind == Kind::Free {
            return Err(FsError::BadInode(number));
        }
        Ok(inode)
    }

    fn write_inode(&mut self, number: u32, inode: &Inode) -> Result<(), FsError> {
        let (block, offset) = self.inode_place(number)?;
        let mut buffer = [0u8; BLOCK_SIZE];
        self.read_block(block, &mut buffer)?;
        inode.encode(&mut buffer[offset..offset + INODE_SIZE]);
        self.write_block(block, &buffer)
    }

    /// Find a free inode and make it the kind asked for.
    ///
    /// Claiming and filling in are one act, so there is no window in which a
    /// slot has been chosen but still reads as free -- a second search in that
    /// window would choose it again and two names would share one inode.
    ///
    /// A linear scan of the table. It is the slowest thing here and the table is
    /// small; when it stops being small the superblock gains a hint saying where
    /// the last search stopped, which is a field and a line, not a redesign.
    fn allocate_inode(&mut self, kind: Kind) -> Result<u32, FsError> {
        let mut buffer = [0u8; BLOCK_SIZE];
        for block_index in 0..self.superblock.inode_blocks {
            let block = self.superblock.inode_start + block_index;
            self.read_block(block, &mut buffer)?;
            for slot in 0..INODES_PER_BLOCK {
                let number = block_index as usize * INODES_PER_BLOCK + slot;
                // Inode zero means "nothing", and the root is made by `format`.
                if number <= ROOT as usize {
                    continue;
                }
                if number as u64 >= self.superblock.inode_count {
                    return Err(FsError::NoInodes);
                }
                let offset = slot * INODE_SIZE;
                if Kind::from_raw(read_u16(&buffer, offset)) != Kind::Free {
                    continue;
                }

                let now = crate::arch::time::ticks();
                let mut inode = Inode::empty();
                inode.kind = kind;
                inode.links = 1;
                inode.created = now;
                inode.modified = now;
                inode.encode(&mut buffer[offset..offset + INODE_SIZE]);
                self.write_block(block, &buffer)?;

                self.superblock.free_inodes -= 1;
                return Ok(number as u32);
            }
        }
        Err(FsError::NoInodes)
    }

    fn free_inode(&mut self, number: u32) -> Result<(), FsError> {
        let (block, offset) = self.inode_place(number)?;
        let mut buffer = [0u8; BLOCK_SIZE];
        self.read_block(block, &mut buffer)?;
        buffer[offset..offset + INODE_SIZE].fill(0);
        self.write_block(block, &buffer)?;
        self.superblock.free_inodes += 1;
        Ok(())
    }

    // -- Blocks, underneath -------------------------------------------------

    /// Take a free block, marking it used.
    fn allocate_block(&mut self) -> Result<u64, FsError> {
        let mut buffer = [0u8; BLOCK_SIZE];
        for index in 0..self.superblock.bitmap_blocks {
            let block = self.superblock.bitmap_start + index;
            self.read_block(block, &mut buffer)?;
            let Some(bit) = first_clear_bit(&buffer) else {
                continue;
            };
            let number = index * BLOCK_SIZE as u64 * 8 + bit as u64;
            // The bits past the end of the volume are set by `format`, so this
            // should not happen; it is checked because an allocator that hands
            // out a block outside the partition writes over the next one.
            if number >= self.superblock.total_blocks {
                break;
            }
            set_bit(&mut buffer, bit, true);
            self.write_block(block, &buffer)?;
            self.superblock.free_blocks -= 1;
            return Ok(number);
        }
        Err(FsError::NoSpace)
    }

    fn free_block(&mut self, number: u64) -> Result<(), FsError> {
        if number < self.superblock.data_start || number >= self.superblock.total_blocks {
            return Err(FsError::BadBlock(number));
        }
        self.set_block_used(number, false)?;
        self.superblock.free_blocks += 1;
        Ok(())
    }

    /// Set or clear one bit of the allocation bitmap.
    fn set_block_used(&mut self, number: u64, used: bool) -> Result<(), FsError> {
        let bit = number as usize % (BLOCK_SIZE * 8);
        let block = self.superblock.bitmap_start + number / (BLOCK_SIZE as u64 * 8);
        let mut buffer = [0u8; BLOCK_SIZE];
        self.read_block(block, &mut buffer)?;
        set_bit(&mut buffer, bit, used);
        self.write_block(block, &buffer)
    }

    // -- The disk -----------------------------------------------------------

    /// Read a block, seeing anything this transaction has already changed.
    ///
    /// The pending list is consulted first. Without that, an operation that
    /// changes a block and then reads it again -- which every bitmap update
    /// does -- would read what is still on the disk and undo itself.
    fn read_block(&self, block: u64, buffer: &mut [u8; BLOCK_SIZE]) -> Result<(), FsError> {
        if let Some(pending) = &self.pending {
            if let Some((_, data)) = pending.iter().rev().find(|(number, _)| *number == block) {
                buffer.copy_from_slice(&data[..]);
                return Ok(());
            }
        }
        read_block_at(self.start_lba, block, buffer)
    }

    /// Change a block, as part of a transaction if one is open.
    ///
    /// Inside a transaction the change is held rather than written, so that
    /// nothing reaches the disk until the whole operation can be replayed from
    /// the journal. Outside one it goes straight down, which is what file
    /// contents and the journal's own blocks do.
    fn write_block(&mut self, block: u64, buffer: &[u8; BLOCK_SIZE]) -> Result<(), FsError> {
        if let Some(pending) = &mut self.pending {
            if pending.len() >= MAX_JOURNALLED {
                return Err(FsError::TooLarge);
            }
            // Replacing rather than appending, so a block changed twice in one
            // operation is journalled once and applied once.
            if let Some((_, data)) = pending.iter_mut().find(|(number, _)| *number == block) {
                data.copy_from_slice(&buffer[..]);
            } else {
                pending.push((block, Box::new(*buffer)));
            }
            return Ok(());
        }
        self.write_block_now(block, buffer)
    }

    /// Change a block on the disk, transaction or no transaction.
    ///
    /// Through the cache, which writes it to the disk and then remembers it.
    /// Write-*through*, not write-back: the journal below orders its own
    /// writes, and a cache that held them would reorder them out from under it.
    fn write_block_now(&self, block: u64, buffer: &[u8; BLOCK_SIZE]) -> Result<(), FsError> {
        super::cache::write(self.start_lba + block * SECTORS_PER_BLOCK, buffer)
            .map_err(FsError::Disk)
    }

    // -- The journal --------------------------------------------------------

    /// Start collecting an operation's changes.
    ///
    /// Nested calls are not an error and not a nesting: an operation that
    /// begins inside another joins it, so the two commit together. Nothing here
    /// does that today, and getting it silently wrong later would be a
    /// half-written filesystem.
    fn begin(&mut self) {
        if self.pending.is_none() {
            self.pending = Some(Vec::new());
        }
    }

    /// Throw away an operation's changes.
    ///
    /// For a failure part-way through. Nothing has reached the disk, so there
    /// is nothing to undo -- which is the whole reason for holding them.
    fn abandon(&mut self) {
        self.pending = None;
    }

    /// Write the operation down, then carry it out.
    ///
    /// The order is the guarantee, and every step of it matters. The blocks go
    /// to the journal first, so they exist somewhere before anything is
    /// overwritten. The descriptor goes down second, with a checksum over
    /// itself, and that write is the commit: before it the operation did not
    /// happen, after it the operation will happen even if the machine stops.
    /// Then the blocks go where they belong, and only then is the descriptor
    /// erased.
    ///
    /// It relies on the driver underneath issuing one request at a time and
    /// waiting for each, which it does -- so the writes reach the device in
    /// this order. Whether the *host* then reorders them onto real hardware is
    /// beyond this without negotiating a flush, and that is a gap worth naming
    /// rather than a guarantee worth pretending to.
    fn commit(&mut self) -> Result<(), FsError> {
        let Some(pending) = self.pending.take() else {
            return Ok(());
        };
        if pending.is_empty() {
            return Ok(());
        }
        self.journal(&pending)?;
        self.apply(&pending)
    }

    /// Write the operation down. Everything after this is repeatable.
    fn journal(&mut self, pending: &[(u64, Box<[u8; BLOCK_SIZE]>)]) -> Result<(), FsError> {
        if pending.len() > MAX_JOURNALLED {
            return Err(FsError::TooLarge);
        }

        let journal = self.superblock.journal_start;
        self.sequence += 1;

        // The blocks first, so they exist somewhere before anything is
        // overwritten.
        for (index, (_, data)) in pending.iter().enumerate() {
            self.write_block_now(journal + 1 + index as u64, data)?;
        }

        // Then the descriptor, and *that write is the commit*: before it the
        // operation did not happen, after it the operation will happen even if
        // the machine stops here.
        let mut descriptor = [0u8; BLOCK_SIZE];
        descriptor[..8].copy_from_slice(JOURNAL_MAGIC);
        write_u64(&mut descriptor, 8, self.sequence);
        write_u32(&mut descriptor, 16, pending.len() as u32);
        for (index, (block, _)) in pending.iter().enumerate() {
            write_u64(&mut descriptor, 24 + index * 8, *block);
        }
        let checksum = crc32(&descriptor[..BLOCK_SIZE - 4]);
        write_u32(&mut descriptor, BLOCK_SIZE - 4, checksum);
        self.write_block_now(journal, &descriptor)
    }

    /// Carry the operation out, and erase the record of it.
    fn apply(&mut self, pending: &[(u64, Box<[u8; BLOCK_SIZE]>)]) -> Result<(), FsError> {
        for (block, data) in pending {
            self.write_block_now(*block, data)?;
        }

        // Erased last. A failure before this leaves a descriptor that will be
        // replayed, which writes the same blocks again and changes nothing.
        self.write_block_now(self.superblock.journal_start, &[0u8; BLOCK_SIZE])
    }

    // -- Checking ------------------------------------------------------------

    /// Walk everything, and make the bitmap agree with it.
    ///
    /// The picture is built from the files, not from the bitmap: what the
    /// filesystem *is* is its inodes and its directories, and the bitmap is a
    /// note about them that can be wrong. So the walk is the truth and the
    /// bitmap is what gets corrected.
    pub fn check(&mut self) -> Result<Check, FsError> {
        let mut report = Check::default();
        let total = self.superblock.total_blocks;
        let bitmap_bytes = (self.superblock.bitmap_blocks * BLOCK_SIZE as u64) as usize;
        let mut reachable = vec![0u8; bitmap_bytes];

        // The metadata regions, which no inode points at and which are taken
        // all the same. Marked first so that a file claiming one shows up as a
        // block two things claim rather than as agreement.
        for block in 0..self.superblock.data_start {
            set_bit(&mut reachable, block as usize, true);
        }
        // And the bits past the end of the volume, which `format` marks so the
        // allocator cannot hand out a block beyond the partition.
        for block in total..bitmap_bytes as u64 * 8 {
            set_bit(&mut reachable, block as usize, true);
        }

        // A block at a time, not an inode at a time. Thirty-two inodes share a
        // block, so reading one per inode is thirty-two times the work -- and
        // the work is a lock and a lookup in the block cache each time, which
        // showed up as a boot slow enough to run the tests out of their window.
        let mut table = [0u8; BLOCK_SIZE];
        for block_index in 0..self.superblock.inode_blocks {
            self.read_block(self.superblock.inode_start + block_index, &mut table)?;

            for slot in 0..INODES_PER_BLOCK {
                let number = block_index * INODES_PER_BLOCK as u64 + slot as u64;
                // Inode zero is never used, and the table can be longer than
                // the filesystem says it is.
                if number < u64::from(ROOT) {
                    continue;
                }
                if number >= self.superblock.inode_count {
                    break;
                }
                let offset = slot * INODE_SIZE;
                let inode = Inode::decode(&table[offset..offset + INODE_SIZE]);
                if inode.kind == Kind::Free {
                    continue;
                }
                report.inodes += 1;

                let blocks = match self.block_list(&inode) {
                    Ok(blocks) => blocks,
                    Err(_) => {
                        // An inode whose block list does not make sense. Left
                        // alone: its blocks cannot be identified, so reclaiming
                        // anything on its behalf would be guessing.
                        report.unreadable += 1;
                        continue;
                    }
                };

                let mut claims: Vec<u64> = blocks;
                if inode.indirect != 0 {
                    claims.push(inode.indirect);
                }
                for block in claims {
                    if block >= total {
                        report.unreadable += 1;
                        continue;
                    }
                    if bit(&reachable, block as usize) {
                        report.shared += 1;
                    } else {
                        set_bit(&mut reachable, block as usize, true);
                        report.blocks += 1;
                    }
                }

                if inode.kind == Kind::Directory {
                    let Ok(contents) = self.read_inode_data(&inode) else {
                        report.unreadable += 1;
                        continue;
                    };
                    let Ok(entries) = parse_directory(&contents) else {
                        report.unreadable += 1;
                        continue;
                    };
                    for (child, _, kind) in entries {
                        match self.read_inode(child) {
                            Ok(actual) if actual.kind == kind => {}
                            // A name pointing at nothing, or at the other kind of
                            // thing. Removing it is a change to a directory, which
                            // is the filesystem itself; that is a decision for
                            // whoever is looking at the report.
                            _ => report.dangling += 1,
                        }
                    }
                }
            }
        }

        // -- And now the bitmap, one block at a time --------------------------

        self.begin();
        let mut free_blocks = 0u64;
        for index in 0..self.superblock.bitmap_blocks {
            let mut stored = [0u8; BLOCK_SIZE];
            self.read_block(self.superblock.bitmap_start + index, &mut stored)?;
            let mut changed = false;

            for offset in 0..BLOCK_SIZE * 8 {
                let block = index * (BLOCK_SIZE as u64 * 8) + offset as u64;
                if block >= bitmap_bytes as u64 * 8 {
                    break;
                }
                let says_taken = bit(&stored, offset);
                let is_reached = bit(&reachable, block as usize);

                match (says_taken, is_reached) {
                    // Taken and unreachable: leaked. Safe to reclaim, because
                    // nothing can be pointing at it.
                    (true, false) => {
                        report.leaked += 1;
                        set_bit(&mut stored, offset, false);
                        changed = true;
                    }
                    // Reachable and free: dangerous. The allocator would hand
                    // it out from under a file that is using it, so the bit is
                    // set -- the bookkeeping is corrected, never the file.
                    (false, true) => {
                        report.unclaimed += 1;
                        set_bit(&mut stored, offset, true);
                        changed = true;
                    }
                    _ => {}
                }

                if block < total && !bit(&stored, offset) {
                    free_blocks += 1;
                }
            }

            if changed {
                self.write_block(self.superblock.bitmap_start + index, &stored)?;
            }
        }

        let free_inodes = self
            .superblock
            .inode_count
            .saturating_sub(report.inodes + 1);
        if free_blocks != self.superblock.free_blocks || free_inodes != self.superblock.free_inodes
        {
            report.counts_corrected = true;
            self.superblock.free_blocks = free_blocks;
            self.superblock.free_inodes = free_inodes;
            self.write_superblock()?;
        }

        self.commit()?;
        Ok(report)
    }

    /// Leak a block on purpose, and require the check to find and reclaim it.
    ///
    /// The damage is made the only way it can be made: a block is taken from
    /// the allocator and then nothing is done with it, which is exactly what a
    /// kernel with a bug in its write path leaves behind. A journal would not
    /// have caught it -- from the journal's point of view that operation
    /// completed perfectly.
    ///
    /// It also requires the check to be *quiet* first and quiet again after, so
    /// that a check which reported damage on every filesystem it saw could not
    /// pass this by accident.
    // Compiled always and called only by the build that runs the destructive
    // filesystem checks. Kept rather than gated so that the code the test suite
    // exercises is the same code the shipped kernel contains.
    #[cfg_attr(not(feature = "deep-selftest"), allow(dead_code))]
    pub fn check_self_test(&mut self) -> Result<String, FsError> {
        let before = self.check()?;
        if !before.clean() {
            return Err(FsError::Corrupt);
        }
        let (_, free_before) = self.space();

        // Taken and abandoned. Committed, so it is on the disk and not merely
        // in a transaction that could be thrown away -- the point is to leave
        // the filesystem genuinely wrong.
        self.begin();
        let orphan = self.allocate_block()?;
        self.write_superblock()?;
        self.commit()?;

        let found = self.check()?;
        if found.leaked != 1 {
            return Err(FsError::Corrupt);
        }
        if found.needs_attention() {
            return Err(FsError::Corrupt);
        }
        let (_, free_after) = self.space();
        if free_after != free_before {
            return Err(FsError::Corrupt);
        }

        // And quiet again, because a repair that had to be run twice would not
        // be a repair.
        if !self.check()?.clean() {
            return Err(FsError::Corrupt);
        }

        // The block really is available again, and it is the same one.
        //
        // Abandoning throws away the bitmap write but not the free count, which
        // `allocate_block` had already decremented in memory -- so it is put
        // back by hand. A test that left the filesystem one block out would be
        // a test the next check reported as damage.
        self.begin();
        let reissued = self.allocate_block()?;
        self.abandon();
        self.superblock.free_blocks += 1;
        if reissued != orphan {
            return Err(FsError::Corrupt);
        }

        Ok(String::from(
            "check verified: a block leaked on purpose was found, reclaimed and handed out again",
        ))
    }

    // -- Proving recovery ---------------------------------------------------

    /// Leave the filesystem exactly as a power failure after a commit would.
    ///
    /// The transaction is written to the journal and then nothing happens: the
    /// blocks are not written home and the descriptor is not erased. The disk
    /// is now in the state the recovery path exists for.
    ///
    /// It is here because there is no other way to test that path. Recovery
    /// cannot be proved by reading it, and the state it recovers from cannot be
    /// produced by a machine that is working -- so the machine is given a way
    /// to stop half way on purpose. Nothing calls it but the test below.
    // Compiled always and called only by the build that runs the destructive
    // filesystem checks. Kept rather than gated so that the code the test suite
    // exercises is the same code the shipped kernel contains.
    #[cfg_attr(not(feature = "deep-selftest"), allow(dead_code))]
    fn commit_and_stop(&mut self) -> Result<(), FsError> {
        let Some(pending) = self.pending.take() else {
            return Ok(());
        };
        if pending.is_empty() {
            return Ok(());
        }
        self.journal(&pending)
    }

    /// Crash between a commit and its writes, then mount and check.
    ///
    /// The claim is the one the journal exists to make: an operation that was
    /// committed but not carried out is carried out by the next mount, and one
    /// that was not committed leaves no trace. Both halves are checked, because
    /// a recovery that replayed everything it found would be as wrong as one
    /// that replayed nothing -- it would finish operations that never happened.
    ///
    /// Returns what it did, for the log.
    // Compiled always and called only by the build that runs the destructive
    // filesystem checks. Kept rather than gated so that the code the test suite
    // exercises is the same code the shipped kernel contains.
    #[cfg_attr(not(feature = "deep-selftest"), allow(dead_code))]
    pub fn journal_self_test(&mut self) -> Result<String, FsError> {
        const NAME: &str = "journal-test";
        const BEFORE: &[u8] = b"the contents before the crash";
        const AFTER: &[u8] = b"the contents the journal was holding";

        // A file whose contents are known, written the ordinary way.
        if self.lookup(ROOT, NAME).is_ok() {
            self.unlink(ROOT, NAME)?;
        }
        let file = self.create(ROOT, NAME, Kind::File)?;
        self.write(file, BEFORE)?;
        if self.read(file)? != BEFORE {
            return Err(FsError::Corrupt);
        }

        // -- A transaction that commits and then stops ----------------------

        self.begin();
        self.write_inode_data(file, AFTER, true)?;
        self.commit_and_stop()?;

        // Nothing has been written home, so a reader that ignored the journal
        // still sees the old contents. Checked from a *fresh* mount whose
        // recovery is skipped, because this volume's own cache of the
        // superblock would otherwise answer for the disk.
        let unrecovered = Self {
            start_lba: self.start_lba,
            superblock: self.superblock,
            pending: None,
            sequence: self.sequence,
        };
        if unrecovered.read(file)? != BEFORE {
            return Err(FsError::Corrupt);
        }
        drop(unrecovered);

        // -- And a mount, which is where recovery happens -------------------

        let mut recovered = Self::mount(self.start_lba)?;
        if recovered.read(file)? != AFTER {
            return Err(FsError::Corrupt);
        }

        // Mounting again must find nothing left to do. A descriptor that
        // survived its own replay would be replayed on every mount forever,
        // overwriting whatever those blocks had become in the meantime.
        let again = Self::mount(self.start_lba)?;
        let mut descriptor = [0u8; BLOCK_SIZE];
        read_block_at(
            self.start_lba,
            again.superblock.journal_start,
            &mut descriptor,
        )?;
        if descriptor[..8] == *JOURNAL_MAGIC {
            return Err(FsError::Corrupt);
        }
        drop(again);

        // -- A transaction that never commits -------------------------------

        // Abandoned rather than committed, so nothing was written down. The
        // file must be exactly as the replay left it: an operation that did not
        // commit did not happen.
        recovered.begin();
        recovered.write_inode_data(file, b"never committed", true)?;
        recovered.abandon();
        let after_abandon = Self::mount(self.start_lba)?;
        if after_abandon.read(file)? != AFTER {
            return Err(FsError::Corrupt);
        }
        drop(after_abandon);

        recovered.unlink(ROOT, NAME)?;

        // This volume's superblock is now behind the disk's, because the
        // recovery and the unlink happened through other handles on the same
        // filesystem. Taking the disk's is the only honest thing to do.
        *self = Self::mount(self.start_lba)?;

        Ok(String::from(
            "journal verified: a commit without its writes was replayed by the next mount, \
             a second mount found nothing left to do, and an abandoned transaction left no trace",
        ))
    }

    /// Finish anything the last mount of this filesystem did not.
    ///
    /// Returns how many blocks were replayed. A descriptor whose checksum does
    /// not match is a descriptor that was being written when the machine
    /// stopped, and it names an operation that had not committed -- so it is
    /// erased rather than acted on.
    fn recover(&mut self) -> Result<usize, FsError> {
        let journal = self.superblock.journal_start;
        let mut descriptor = [0u8; BLOCK_SIZE];
        read_block_at(self.start_lba, journal, &mut descriptor)?;

        if &descriptor[..8] != JOURNAL_MAGIC {
            return Ok(0);
        }
        let stated = read_u32(&descriptor, BLOCK_SIZE - 4);
        if crc32(&descriptor[..BLOCK_SIZE - 4]) != stated {
            // Torn. Erase it, because leaving it would mean checksumming it
            // again on every mount forever.
            self.write_block_now(journal, &[0u8; BLOCK_SIZE])?;
            return Ok(0);
        }

        let sequence = read_u64(&descriptor, 8);
        let count = read_u32(&descriptor, 16) as usize;
        if count == 0 || count > MAX_JOURNALLED {
            self.write_block_now(journal, &[0u8; BLOCK_SIZE])?;
            return Ok(0);
        }

        let mut block = [0u8; BLOCK_SIZE];
        for index in 0..count {
            let target = read_u64(&descriptor, 24 + index * 8);
            // A target outside the filesystem means the descriptor is not what
            // it claims however well it checksums, and writing there would be
            // taking a corrupt disk and making it worse.
            if target >= self.superblock.total_blocks {
                self.write_block_now(journal, &[0u8; BLOCK_SIZE])?;
                return Err(FsError::BadBlock(target));
            }
            read_block_at(self.start_lba, journal + 1 + index as u64, &mut block)?;
            self.write_block_now(target, &block)?;
        }

        self.write_block_now(journal, &[0u8; BLOCK_SIZE])?;
        self.sequence = sequence;
        Ok(count)
    }

    /// Put the superblock back, with the free counts as they now stand.
    ///
    /// Written as a table so that a field cannot be given the wrong offset by
    /// the two lines being edited apart, and so that the checksum is taken over
    /// exactly the bytes the fields were written into.
    fn write_superblock(&mut self) -> Result<(), FsError> {
        let sb = &self.superblock;
        let mut block = [0u8; BLOCK_SIZE];
        block[field::MAGIC..field::MAGIC + 8].copy_from_slice(MAGIC);
        write_u32(&mut block, field::VERSION, VERSION);
        write_u32(&mut block, field::BLOCK_SIZE, BLOCK_SIZE as u32);

        for (offset, value) in [
            (field::TOTAL_BLOCKS, sb.total_blocks),
            (field::INODE_COUNT, sb.inode_count),
            (field::JOURNAL_START, sb.journal_start),
            (field::JOURNAL_BLOCKS, sb.journal_blocks),
            (field::BITMAP_START, sb.bitmap_start),
            (field::BITMAP_BLOCKS, sb.bitmap_blocks),
            (field::INODE_START, sb.inode_start),
            (field::INODE_BLOCKS, sb.inode_blocks),
            (field::DATA_START, sb.data_start),
            (field::FREE_BLOCKS, sb.free_blocks),
            (field::FREE_INODES, sb.free_inodes),
        ] {
            write_u64(&mut block, offset, value);
        }

        let checksum = crc32(&block[..field::CHECKSUM]);
        write_u32(&mut block, field::CHECKSUM, checksum);
        self.write_block(0, &block)
    }
}

/// Whether a name can be put in a directory.
fn check_name(name: &str) -> Result<(), FsError> {
    if name.is_empty() || name.len() > MAX_NAME || name.contains('/') || name.contains('\0') {
        return Err(FsError::BadName);
    }
    Ok(())
}

/// Turn one entry into the bytes a directory holds.
fn encode_entry(inode: u32, name: &str, kind: Kind) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(ENTRY_HEADER + name.len());
    bytes.extend_from_slice(&inode.to_le_bytes());
    bytes.push(name.len() as u8);
    bytes.push(kind.to_raw() as u8);
    bytes.extend_from_slice(&[0, 0]);
    bytes.extend_from_slice(name.as_bytes());
    bytes
}

/// Read a directory's contents back into entries.
///
/// Every length is checked against what is left rather than trusted, because
/// these bytes came off a disk: a corrupt length is the difference between an
/// error and reading whatever follows the buffer.
#[allow(clippy::type_complexity)]
fn parse_directory(bytes: &[u8]) -> Result<Vec<(u32, String, Kind)>, FsError> {
    let mut entries = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes.len() - offset < ENTRY_HEADER {
            return Err(FsError::Corrupt);
        }
        let inode = read_u32(bytes, offset);
        let length = bytes[offset + 4] as usize;
        let kind = Kind::from_raw(u16::from(bytes[offset + 5]));
        offset += ENTRY_HEADER;
        if length == 0 || bytes.len() - offset < length {
            return Err(FsError::Corrupt);
        }
        let name = core::str::from_utf8(&bytes[offset..offset + length])
            .map_err(|_| FsError::Corrupt)?
            .into();
        offset += length;
        if inode == 0 || kind == Kind::Free {
            return Err(FsError::Corrupt);
        }
        entries.push((inode, name, kind));
    }
    Ok(entries)
}

/// Set or clear one bit of a bitmap, counting from the low bit of each byte.
fn set_bit(bitmap: &mut [u8], bit: usize, set: bool) {
    let mask = 1u8 << (bit % 8);
    if set {
        bitmap[bit / 8] |= mask;
    } else {
        bitmap[bit / 8] &= !mask;
    }
}

/// Whether one bit of a bitmap is set.
fn bit(bitmap: &[u8], index: usize) -> bool {
    bitmap[index / 8] & (1u8 << (index % 8)) != 0
}

/// The first zero bit of a bitmap, if it has one.
///
/// Whole bytes are compared first, so a full bitmap block costs one pass over
/// the bytes rather than eight.
fn first_clear_bit(bitmap: &[u8]) -> Option<usize> {
    for (index, byte) in bitmap.iter().enumerate() {
        if *byte != 0xFF {
            return Some(index * 8 + byte.trailing_ones() as usize);
        }
    }
    None
}

/// Read a little-endian `u16`.
fn read_u16(block: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(block[offset..offset + 2].try_into().unwrap())
}

/// Read a little-endian `u32`.
fn read_u32(block: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(block[offset..offset + 4].try_into().unwrap())
}

/// Read a little-endian `u64`.
fn read_u64(block: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(block[offset..offset + 8].try_into().unwrap())
}

/// Write a little-endian `u16`.
fn write_u16(block: &mut [u8], offset: usize, value: u16) {
    block[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

/// Write a little-endian `u32`.
fn write_u32(block: &mut [u8], offset: usize, value: u32) {
    block[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

/// Write a little-endian `u64`.
fn write_u64(block: &mut [u8], offset: usize, value: u64) {
    block[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

/// Read one block of the partition starting at `start_lba`.
///
/// Through the cache, which is where the repeated reads go: an inode is 128
/// bytes in a block of four thousand and ninety-six, and reading the next one
/// along used to cost the same block off the platter a second time.
fn read_block_at(start_lba: u64, block: u64, buffer: &mut [u8; BLOCK_SIZE]) -> Result<(), FsError> {
    super::cache::read(start_lba + block * SECTORS_PER_BLOCK, buffer).map_err(FsError::Disk)
}

/// CRC-32, the ordinary reflected one, as the partition table uses.
fn crc32(data: &[u8]) -> u32 {
    const POLYNOMIAL: u32 = 0xEDB8_8320;
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let carry = crc & 1;
            crc >>= 1;
            if carry != 0 {
                crc ^= POLYNOMIAL;
            }
        }
    }
    !crc
}

/// Check the on-disk format against itself, without a disk.
///
/// Encoding and decoding, packing and parsing, and the arithmetic that says how
/// large a file may be. None of it touches hardware, so it runs at boot rather
/// than under `cargo test`: the kernel is a bare-metal binary with its own panic
/// handler and cannot be linked against the test harness, and a test that only
/// exists in a configuration nobody builds is not a test.
///
/// Returns the name of the first check that failed.
pub fn format_self_test() -> Result<(), &'static str> {
    // An entry survives the round trip, and several pack end to end.
    let mut bytes = encode_entry(2, "bin", Kind::Directory);
    bytes.extend_from_slice(&encode_entry(3, "a", Kind::File));
    bytes.extend_from_slice(&encode_entry(4, "a-much-longer-name.txt", Kind::File));
    let entries = parse_directory(&bytes).map_err(|_| "a directory did not parse")?;
    if entries.len() != 3 {
        return Err("a directory lost an entry");
    }
    if entries[0] != (2, String::from("bin"), Kind::Directory) {
        return Err("the first entry came back wrong");
    }
    if entries[2].1 != "a-much-longer-name.txt" {
        return Err("a long name came back wrong");
    }

    // A length longer than what is left must be refused rather than read past.
    let mut truncated = encode_entry(7, "hello.txt", Kind::File);
    truncated.truncate(truncated.len() - 3);
    if parse_directory(&truncated) != Err(FsError::Corrupt) {
        return Err("a truncated entry was accepted");
    }
    let mut lying = encode_entry(7, "abc", Kind::File);
    lying[4] = 200;
    if parse_directory(&lying) != Err(FsError::Corrupt) {
        return Err("an entry claiming a name longer than the buffer was accepted");
    }

    // An inode survives the round trip, including its last pointer, which is
    // the one that would land in the next inode if the layout were wrong.
    let mut inode = Inode::empty();
    inode.kind = Kind::File;
    inode.links = 1;
    inode.size = 9_000;
    inode.created = 12;
    inode.modified = 34;
    inode.direct[0] = 100;
    inode.direct[DIRECT - 1] = 110;
    inode.indirect = 999;
    let mut raw = [0u8; INODE_SIZE];
    inode.encode(&mut raw);
    let read = Inode::decode(&raw);
    if read.kind != Kind::File || read.links != 1 || read.size != 9_000 {
        return Err("an inode's header did not survive the round trip");
    }
    if read.created != 12 || read.modified != 34 {
        return Err("an inode's times did not survive the round trip");
    }
    if read.direct[0] != 100 || read.direct[DIRECT - 1] != 110 || read.indirect != 999 {
        return Err("an inode's block numbers did not survive the round trip");
    }
    // Three blocks for nine thousand bytes: the last one is mostly empty and
    // still has to exist.
    if read.data_blocks() != 3 {
        return Err("a file's length rounded to the wrong number of blocks");
    }

    // Bits are found and set from the low end of each byte.
    let mut bitmap = [0u8; 8];
    if first_clear_bit(&bitmap) != Some(0) {
        return Err("an empty bitmap did not offer its first bit");
    }
    for bit in 0..8 {
        set_bit(&mut bitmap, bit, true);
    }
    if bitmap[0] != 0xFF || first_clear_bit(&bitmap) != Some(8) {
        return Err("setting eight bits did not fill the first byte");
    }
    set_bit(&mut bitmap, 3, false);
    if first_clear_bit(&bitmap) != Some(3) {
        return Err("a cleared bit was not offered again");
    }
    if first_clear_bit(&[0xFF; 16]).is_some() {
        return Err("a full bitmap offered a block anyway");
    }

    // Names.
    if check_name("") != Err(FsError::BadName)
        || check_name("a/b") != Err(FsError::BadName)
        || check_name("a b") != Err(FsError::BadName)
    {
        return Err("an unusable name was accepted");
    }
    if check_name("a-perfectly-ordinary.name").is_err() {
        return Err("an ordinary name was refused");
    }

    // And the size the pointers describe.
    if MAX_BLOCKS != DIRECT + PER_INDIRECT || MAX_FILE != MAX_BLOCKS * BLOCK_SIZE {
        return Err("the largest file is not what the pointers describe");
    }
    if 32 + DIRECT * 8 + 8 > INODE_SIZE {
        return Err("an inode does not fit the space it is given");
    }

    Ok(())
}
