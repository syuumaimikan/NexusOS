//! The filesystem the system keeps, and what it keeps in it.
//!
//! [`nexusfs`](super::nexusfs) is the format. This is the one volume the
//! running system has mounted: where it was found, when it was made, and the
//! few things NexusOS writes into it about itself.
//!
//! # Finding it
//!
//! By partition type, not by position. The GPT written by the build carries a
//! type GUID that means "NexusFS lives here", and a disk with the partitions in
//! a different order, or with other partitions between them, has to work --
//! which is to say the kernel must not assume the second entry is its own.
//!
//! # Making it
//!
//! The build writes zeroes into that partition and stops. The first boot finds
//! no superblock and formats it, and every boot after that finds the filesystem
//! the first one made. That is deliberate: a filesystem the build script laid
//! out would test the build script, and the thing worth testing is whether the
//! kernel can make a filesystem it can then read.
//!
//! Only "there is nothing here" leads to a format. A superblock that fails its
//! checksum is damage, and reformatting over damage would turn a recoverable
//! disk into an empty one.
//!
//! # What it writes
//!
//! A count of boots and a line per boot. Small, and chosen because it is the
//! smallest thing that cannot be faked: the count can only be right on the
//! twelfth boot if the eleventh really wrote it down and the disk really kept
//! it.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::gpt;
use super::nexusfs::{FsError, Kind, Volume, MAX_FILE};
use crate::drivers::virtio_blk;
use crate::sync::IrqSpinLock;

/// The partition type that means NexusFS.
///
/// Not registered with anyone. A type GUID is an identifier, and this one says
/// what it says to the only system that reads it. The bytes are compared as
/// they are stored, which is how [`gpt`] hands them over.
const NEXUS_TYPE: [u8; 16] = [
    0x4E, 0x45, 0x58, 0x55, 0x53, 0x46, 0x53, 0x00, 0x00, 0x01, 0x4E, 0x45, 0x58, 0x55, 0x53, 0x00,
];

/// The directory the system's own files go in.
const SYSTEM_DIR: &str = "system";
/// The file holding how many times this disk has been booted from.
const BOOT_COUNT: &str = "boots";
/// The file holding a line per boot.
const BOOT_LOG: &str = "boot.log";
/// How much of the boot log to keep.
///
/// It is appended to forever, and a file has a largest size; when the log
/// reaches this, the oldest lines go. Trimming at a fraction of the maximum
/// rather than at the maximum means the trim happens once in a while instead of
/// on every boot once the file is full.
const BOOT_LOG_KEEP: usize = 16 * 1024;

/// Why the system's filesystem could not be brought up.
#[derive(Debug, Clone, Copy)]
pub enum StoreError {
    /// There is no disk.
    NoDisk,
    /// The partition table could not be read.
    PartitionTable(gpt::GptError),
    /// The disk has no NexusFS partition.
    NoPartition,
    /// The filesystem itself said no.
    Fs(FsError),
}

impl core::fmt::Display for StoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoDisk => f.write_str("there is no disk to keep a filesystem on"),
            Self::PartitionTable(error) => write!(f, "the partition table: {error}"),
            Self::NoPartition => f.write_str("the disk has no NexusFS partition"),
            Self::Fs(error) => write!(f, "{error}"),
        }
    }
}

impl From<FsError> for StoreError {
    fn from(error: FsError) -> Self {
        Self::Fs(error)
    }
}

/// The volume the system has mounted, once it has one.
static VOLUME: IrqSpinLock<Option<Volume>> = IrqSpinLock::new(None);

/// What came of bringing the filesystem up.
#[derive(Debug, Clone, Copy)]
pub struct Mounted {
    /// Whether this boot is the one that made the filesystem.
    pub formatted: bool,
    /// First sector of the partition it is in.
    pub start_lba: u64,
    /// How many times this disk has been booted from, this boot included.
    pub boots: u64,
}

/// Find the NexusFS partition, mount it -- making one if there is none -- and
/// write down that this boot happened.
pub fn mount() -> Result<Mounted, StoreError> {
    if !virtio_blk::is_present() {
        return Err(StoreError::NoDisk);
    }

    let partitions = gpt::read().map_err(StoreError::PartitionTable)?;
    let partition = partitions
        .iter()
        .find(|partition| partition.type_guid == NEXUS_TYPE)
        .ok_or(StoreError::NoPartition)?;

    let (mut volume, formatted) =
        Volume::mount_or_format(partition.first_lba, partition.sectors())?;
    let boots = record_boot(&mut volume)?;

    *VOLUME.lock() = Some(volume);
    Ok(Mounted {
        formatted,
        start_lba: partition.first_lba,
        boots,
    })
}

/// Blocks and free blocks, turned into bytes, if there is a volume.
#[must_use]
pub fn space() -> Option<(u64, u64)> {
    let volume = VOLUME.lock();
    let volume = volume.as_ref()?;
    let (total, free) = volume.space();
    let size = volume.block_size() as u64;
    Some((total * size, free * size))
}

/// Everything in a directory named by path, if there is a volume.
pub fn list(path: &str) -> Result<Vec<super::nexusfs::Entry>, StoreError> {
    let mut volume = VOLUME.lock();
    let volume = volume.as_mut().ok_or(StoreError::NoDisk)?;
    let inode = volume.resolve(path)?;
    Ok(volume.list(inode)?)
}

/// Add one to the count of boots, and a line to the log.
///
/// The count is read before it is written, so it is the disk's number rather
/// than this boot's guess. A first boot finds no file, which is not an error --
/// it is what a first boot looks like.
fn record_boot(volume: &mut Volume) -> Result<u64, FsError> {
    let system = match volume.lookup(super::nexusfs::ROOT, SYSTEM_DIR) {
        Ok(entry) if entry.kind == Kind::Directory => entry.inode,
        Ok(_) => return Err(FsError::WrongKind),
        Err(FsError::NotFound) => {
            volume.create(super::nexusfs::ROOT, SYSTEM_DIR, Kind::Directory)?
        }
        Err(error) => return Err(error),
    };

    let count_inode = match volume.lookup(system, BOOT_COUNT) {
        Ok(entry) => entry.inode,
        Err(FsError::NotFound) => volume.create(system, BOOT_COUNT, Kind::File)?,
        Err(error) => return Err(error),
    };

    let existing = volume.read(count_inode)?;
    // A file of the wrong length is a file this code did not write, and
    // starting again from zero is better than reading eight bytes that are not
    // there.
    let previous = if existing.len() == 8 {
        u64::from_le_bytes(existing[..8].try_into().unwrap())
    } else {
        0
    };
    let boots = previous + 1;
    volume.write(count_inode, &boots.to_le_bytes())?;

    let log_inode = match volume.lookup(system, BOOT_LOG) {
        Ok(entry) => entry.inode,
        Err(FsError::NotFound) => volume.create(system, BOOT_LOG, Kind::File)?,
        Err(error) => return Err(error),
    };
    let mut log = volume.read(log_inode)?;
    let line = format!("boot {boots} at {} ticks\n", crate::arch::time::ticks());
    log.extend_from_slice(line.as_bytes());
    if log.len() > BOOT_LOG_KEEP {
        // Cut at a line boundary, so the file stays a list of lines rather than
        // becoming one that starts halfway through a word.
        let cut = log.len() - BOOT_LOG_KEEP;
        let cut = log[cut..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(log.len(), |offset| cut + offset + 1);
        log.drain(..cut);
    }
    volume.write(log_inode, &log)?;

    Ok(boots)
}

/// Read the whole boot log back, for whoever wants to see it.
pub fn boot_log() -> Result<String, StoreError> {
    let mut volume = VOLUME.lock();
    let volume = volume.as_mut().ok_or(StoreError::NoDisk)?;
    let inode = volume.resolve("/system/boot.log")?;
    let bytes = volume.read(inode)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Exercise the filesystem, on the real disk, and say whether it worked.
///
/// Everything here is done to the volume the system is actually running on,
/// because a test against a made-up one would test the code and not the disk.
/// It cleans up after itself and checks that it did: the free-block count at
/// the end has to be the count at the start, which is what catches a write path
/// that allocates a block it then forgets about.
///
/// Returns a line describing what happened, or the error that stopped it.
pub fn self_test() -> Result<String, StoreError> {
    let mut guard = VOLUME.lock();
    let volume = guard.as_mut().ok_or(StoreError::NoDisk)?;
    let root = super::nexusfs::ROOT;

    let (_, free_before) = volume.space();
    let (_, inodes_before) = volume.inodes();

    // A leftover from a previous boot that stopped partway would make the first
    // create fail with `Exists`, which would look like a bug in create.
    if volume.lookup(root, "scratch").is_ok() {
        remove_tree(volume, root, "scratch")?;
    }

    // A directory, a file in it, and the bytes back out again.
    let directory = volume.create(root, "scratch", Kind::Directory)?;
    let small = volume.create(directory, "small.txt", Kind::File)?;
    let text = b"NexusOS wrote this into its own filesystem.\n";
    volume.write(small, text)?;
    if volume.read(small)? != text {
        return Ok(String::from("FAILED: a small file did not read back"));
    }

    // A path walked from the root, rather than an inode number kept in hand.
    // A lookup that ignored all but the last component would pass everything
    // above this.
    if volume.resolve("/scratch/small.txt")? != small {
        return Ok(String::from(
            "FAILED: walking the path found a different file",
        ));
    }

    // A file past the eleven direct blocks, so the indirect block is used. The
    // contents depend on the offset, so blocks stitched together in the wrong
    // order fail rather than merely being the right length.
    let large_len = 60 * 1024;
    let pattern: Vec<u8> = (0..large_len)
        .map(|index| (index as u8).wrapping_mul(31).wrapping_add(7))
        .collect();
    let large = volume.create(directory, "large.bin", Kind::File)?;
    volume.write(large, &pattern)?;
    let read = volume.read(large)?;
    if read.len() != large_len {
        return Ok(format!(
            "FAILED: a {large_len}-byte file read back as {} bytes",
            read.len()
        ));
    }
    if read != pattern {
        let at = read
            .iter()
            .zip(&pattern)
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        return Ok(format!("FAILED: the large file differs at byte {at}"));
    }

    // Shrinking has to give the blocks back, not merely stop pointing at them.
    let (_, free_with_large) = volume.space();
    volume.write(large, b"small again")?;
    let (_, free_after_shrink) = volume.space();
    if free_after_shrink <= free_with_large {
        return Ok(String::from(
            "FAILED: shrinking a file did not release its blocks",
        ));
    }

    // The things that must be refused.
    if volume.lookup(directory, "nosuch").is_ok() {
        return Ok(String::from("FAILED: a name that is not there was found"));
    }
    if volume.create(directory, "small.txt", Kind::File) != Err(FsError::Exists) {
        return Ok(String::from("FAILED: a duplicate name was accepted"));
    }
    if volume.unlink(root, "scratch") != Err(FsError::NotEmpty) {
        return Ok(String::from(
            "FAILED: removing a directory with things in it was allowed",
        ));
    }
    if volume.create(directory, "a/b", Kind::File) != Err(FsError::BadName) {
        return Ok(String::from("FAILED: a name with a separator was accepted"));
    }
    let too_large: Vec<u8> = alloc::vec![0; MAX_FILE + 1];
    if volume.write(large, &too_large) != Err(FsError::TooLarge) {
        return Ok(String::from(
            "FAILED: a file larger than the format can describe was accepted",
        ));
    }

    // And it all goes away again.
    remove_tree(volume, root, "scratch")?;

    let (_, free_after) = volume.space();
    let (_, inodes_after) = volume.inodes();
    if free_after != free_before {
        return Ok(format!(
            "FAILED: {} blocks leaked ({free_before} free before, {free_after} after)",
            free_before as i64 - free_after as i64
        ));
    }
    if inodes_after != inodes_before {
        return Ok(format!(
            "FAILED: {} inodes leaked",
            inodes_before as i64 - inodes_after as i64
        ));
    }

    // The superblock has to survive the trip to the disk and back, because
    // every boot after this one starts by reading it.
    let start = volume.start_lba();
    drop(guard);
    let reread = Volume::mount(start)?;
    let (total, free) = reread.space();
    if free != free_after {
        return Ok(format!(
            "FAILED: the superblock on disk says {free} free blocks, not {free_after}"
        ));
    }

    Ok(format!(
        "NexusFS verified: made, written, read, emptied; {total} blocks, {free} free, nothing leaked"
    ))
}

/// Remove a name and everything under it.
fn remove_tree(volume: &mut Volume, parent: u32, name: &str) -> Result<(), FsError> {
    let entry = volume.lookup(parent, name)?;
    if entry.kind == Kind::Directory {
        for child in volume.list(entry.inode)? {
            remove_tree(volume, entry.inode, &child.name)?;
        }
    }
    volume.unlink(parent, name)
}
