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
//! # Whole files
//!
//! A file is read and written entire. There is no partial write, no seek and no
//! append, because there is no buffer cache underneath to make one cheap: every
//! block goes to the disk the moment it is written, so a byte-at-a-time
//! interface would be a byte-at-a-time disk. When a cache exists this grows the
//! interface it deserves; until then the honest one is the small one.
//!
//! # What it does not do yet
//!
//! No journal, so a power failure between two writes can leave the bitmap
//! saying a block is taken that no file points at. That leaks space and does not
//! corrupt anything, which is the right way round for the failure to be, but it
//! is a failure and it is written down rather than glossed. No permissions, no
//! timestamps beyond the tick a thing was made at, no links beyond the one a
//! directory entry is.

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
const VERSION: u32 = 1;

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
    pub const BITMAP_START: usize = 32;
    pub const BITMAP_BLOCKS: usize = 40;
    pub const INODE_START: usize = 48;
    pub const INODE_BLOCKS: usize = 56;
    pub const DATA_START: usize = 64;
    pub const FREE_BLOCKS: usize = 72;
    pub const FREE_INODES: usize = 80;
    /// Everything above this is what the checksum covers.
    pub const CHECKSUM: usize = 96;
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

/// A mounted NexusFS.
pub struct Volume {
    /// First sector of the partition; every block number is relative to it.
    start_lba: u64,
    superblock: Superblock,
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

        let bitmap_start = 1;
        let inode_start = bitmap_start + bitmap_blocks;
        let data_start = inode_start + inode_blocks;
        if data_start + 1 >= total_blocks {
            return Err(FsError::TooSmall);
        }

        let volume = Self {
            start_lba,
            superblock: Superblock {
                total_blocks,
                inode_count,
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

        // The inode table, zeroed, which is what makes every inode in it free.
        let empty = [0u8; BLOCK_SIZE];
        for block in inode_start..data_start {
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
        let ordered = superblock.bitmap_start == 1
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

        Ok(Self {
            start_lba,
            superblock,
        })
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
        let mut contents = self.read_inode_data(&parent)?;
        if parse_directory(&contents)?
            .iter()
            .any(|(_, existing, _)| existing == name)
        {
            return Err(FsError::Exists);
        }

        let number = self.allocate_inode(kind)?;
        contents.extend_from_slice(&encode_entry(number, name, kind));
        // If the directory cannot be grown, the inode just taken is given back,
        // so a full disk costs nothing rather than leaking an inode per attempt.
        if let Err(error) = self.write_inode_data(directory, &contents) {
            self.free_inode(number).ok();
            return Err(error);
        }

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
        self.write_inode_data(directory, &rebuilt)?;

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
        self.write_inode_data(inode, data)?;
        self.write_superblock()
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
    fn write_inode_data(&mut self, number: u32, data: &[u8]) -> Result<(), FsError> {
        if data.len() > MAX_FILE {
            return Err(FsError::TooLarge);
        }
        let mut inode = self.read_inode(number)?;
        let wanted = data.len().div_ceil(BLOCK_SIZE);
        let mut blocks = self.block_list(&inode)?;

        // Grow first, into a list that is not on the disk yet. If an allocation
        // fails halfway, the blocks taken so far are handed back and the file is
        // exactly as it was.
        let mut taken = Vec::new();
        if wanted > blocks.len() {
            // A file that grows past its direct blocks needs somewhere to keep
            // the rest; that block is metadata and is not counted as content.
            if wanted > DIRECT && inode.indirect == 0 {
                // Nothing has been taken yet, so a failure here owes
                // nothing back.
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
                        self.give_back(&taken);
                        return Err(error);
                    }
                }
            }
        }

        // Shrink, keeping the blocks to free until the inode no longer points
        // at them.
        let released: Vec<u64> = blocks.split_off(wanted.min(blocks.len()));

        for (index, block) in blocks.iter().enumerate() {
            let offset = index * BLOCK_SIZE;
            let mut buffer = [0u8; BLOCK_SIZE];
            let end = (offset + BLOCK_SIZE).min(data.len());
            buffer[..end - offset].copy_from_slice(&data[offset..end]);
            self.write_block(*block, &buffer)?;
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
    fn set_block_list(&self, inode: &mut Inode, blocks: &[u64]) -> Result<(), FsError> {
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

    fn write_inode(&self, number: u32, inode: &Inode) -> Result<(), FsError> {
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
    fn set_block_used(&self, number: u64, used: bool) -> Result<(), FsError> {
        let bit = number as usize % (BLOCK_SIZE * 8);
        let block = self.superblock.bitmap_start + number / (BLOCK_SIZE as u64 * 8);
        let mut buffer = [0u8; BLOCK_SIZE];
        self.read_block(block, &mut buffer)?;
        set_bit(&mut buffer, bit, used);
        self.write_block(block, &buffer)
    }

    // -- The disk -----------------------------------------------------------

    fn read_block(&self, block: u64, buffer: &mut [u8; BLOCK_SIZE]) -> Result<(), FsError> {
        read_block_at(self.start_lba, block, buffer)
    }

    fn write_block(&self, block: u64, buffer: &[u8; BLOCK_SIZE]) -> Result<(), FsError> {
        let base = self.start_lba + block * SECTORS_PER_BLOCK;
        for index in 0..SECTORS_PER_BLOCK {
            let offset = index as usize * SECTOR_SIZE;
            virtio_blk::write_sector(base + index, &buffer[offset..offset + SECTOR_SIZE])
                .map_err(FsError::Disk)?;
        }
        Ok(())
    }

    /// Put the superblock back, with the free counts as they now stand.
    ///
    /// Written as a table so that a field cannot be given the wrong offset by
    /// the two lines being edited apart, and so that the checksum is taken over
    /// exactly the bytes the fields were written into.
    fn write_superblock(&self) -> Result<(), FsError> {
        let sb = &self.superblock;
        let mut block = [0u8; BLOCK_SIZE];
        block[field::MAGIC..field::MAGIC + 8].copy_from_slice(MAGIC);
        write_u32(&mut block, field::VERSION, VERSION);
        write_u32(&mut block, field::BLOCK_SIZE, BLOCK_SIZE as u32);

        for (offset, value) in [
            (field::TOTAL_BLOCKS, sb.total_blocks),
            (field::INODE_COUNT, sb.inode_count),
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
fn read_block_at(start_lba: u64, block: u64, buffer: &mut [u8; BLOCK_SIZE]) -> Result<(), FsError> {
    let base = start_lba + block * SECTORS_PER_BLOCK;
    for index in 0..SECTORS_PER_BLOCK {
        let offset = index as usize * SECTOR_SIZE;
        virtio_blk::read_sector(base + index, &mut buffer[offset..offset + SECTOR_SIZE])
            .map_err(FsError::Disk)?;
    }
    Ok(())
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
