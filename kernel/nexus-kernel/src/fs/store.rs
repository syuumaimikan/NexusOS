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

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::gpt;
use super::nexusfs::{FsError, Kind, Volume, MAX_FILE};
use crate::drivers::virtio_blk;
use crate::sync::{IrqSpinLock, SleepLock};

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
///
/// A sleeping lock and not a spinning one, because it is held across disk
/// reads and a disk read blocks. An `IrqSpinLock` here means a thread asleep
/// with interrupts off on its processor and every other processor spinning on
/// a lock whose owner is waiting for the very interrupt that would wake it --
/// which is not a slow system, it is a stopped one.
static VOLUME: SleepLock<Option<Volume>> = SleepLock::new(None);

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

    // Checked before it is used. The journal finishes an operation that was
    // interrupted and says nothing about damage that predates it, and a
    // filesystem is easiest to repair before anything has started writing to
    // it. It costs a walk of the inode table, which is forty-seven blocks on
    // this partition and all of them in the cache by the time it is done.
    let checked = volume.check()?;
    if !checked.clean() {
        crate::kprintln!(
            "[fs  ] check: {} blocks reclaimed, {} claimed back, {} shared, {} dangling, {} unreadable{}",
            checked.leaked,
            checked.unclaimed,
            checked.shared,
            checked.dangling,
            checked.unreadable,
            if checked.counts_corrected {
                ", free counts corrected"
            } else {
                ""
            }
        );
    }
    if checked.needs_attention() {
        crate::kprintln!(
            "[fs  ] check: some of that cannot be repaired without deciding which file to damage"
        );
    }

    let boots = record_boot(&mut volume)?;

    *VOLUME.lock() = Some(volume);
    Ok(Mounted {
        formatted,
        start_lba: partition.first_lba,
        boots,
    })
}

/// Blocks and free blocks, turned into bytes, if there is a volume.
///
/// Never waits. This is what the status panel reads, and a panel that blocked
/// behind a disk read would stop redrawing the clock every time something
/// touched a file -- so a volume that is busy reads as "no answer just now"
/// rather than as a reason to stop painting the screen.
#[must_use]
pub fn space() -> Option<(u64, u64)> {
    let volume = VOLUME.try_lock()?;
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
    let line = format!("boot {boots} at {} ticks\n", crate::arch::time::ticks());

    // Appended rather than rewritten. The log used to be read entire and
    // written entire to add one line, which is sixteen kilobytes each way for
    // forty bytes -- and all of it on the boot path.
    let (_, length) = volume.stat(log_inode)?;
    if length + line.len() as u64 <= BOOT_LOG_KEEP as u64 {
        volume.write_at(log_inode, length, line.as_bytes())?;
        return Ok(boots);
    }

    // Trimming still means rewriting, because taking bytes off the front of a
    // file means moving every byte after them. It happens once the log is full
    // rather than on every boot, which is what trimming at a fraction of the
    // maximum buys.
    let mut log = volume.read(log_inode)?;
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
// Compiled always and called only by the build that runs the destructive
// filesystem checks; see `deep-selftest`.
#[cfg_attr(not(feature = "deep-selftest"), allow(dead_code))]
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

/// Leak a block on purpose, then check that the check finds it.
///
/// A third kind of claim again. The first says the filesystem does what it is
/// asked; the second says it survives not being allowed to finish; this one
/// says it can find damage that no journal would have caught -- because from
/// the journal's point of view the operation that leaked the block completed
/// perfectly.
///
/// The damage is made the only way it can be: a block is taken from the
/// allocator and then nothing is done with it. That is exactly what a kernel
/// with a bug in its write path leaves behind, and there is no other way to
/// produce it on a machine that is working.
// Compiled always and called only by the build that runs the destructive
// filesystem checks; see `deep-selftest`.
#[cfg_attr(not(feature = "deep-selftest"), allow(dead_code))]
pub fn check_self_test() -> Result<String, StoreError> {
    let mut guard = VOLUME.lock();
    let volume = guard.as_mut().ok_or(StoreError::NoDisk)?;
    Ok(volume.check_self_test()?)
}

/// Crash the filesystem between a commit and its writes, and check it recovers.
///
/// Separate from the test above because it is a different kind of claim. That
/// one says the filesystem does what it is asked; this one says it survives not
/// being allowed to finish -- which cannot be shown by using it correctly, only
/// by stopping it half way on purpose.
// Compiled always and called only by the build that runs the destructive
// filesystem checks; see `deep-selftest`.
#[cfg_attr(not(feature = "deep-selftest"), allow(dead_code))]
pub fn journal_self_test() -> Result<String, StoreError> {
    let mut guard = VOLUME.lock();
    let volume = guard.as_mut().ok_or(StoreError::NoDisk)?;
    Ok(volume.journal_self_test()?)
}

/// Remove a name and everything under it.
// Compiled always and called only by the build that runs the destructive
// filesystem checks; see `deep-selftest`.
#[cfg_attr(not(feature = "deep-selftest"), allow(dead_code))]
fn remove_tree(volume: &mut Volume, parent: u32, name: &str) -> Result<(), FsError> {
    let entry = volume.lookup(parent, name)?;
    if entry.kind == Kind::Directory {
        for child in volume.list(entry.inode)? {
            remove_tree(volume, entry.inode, &child.name)?;
        }
    }
    volume.unlink(parent, name)
}

// -- Open files and directories -----------------------------------------------

/// How many handles name each open inode.
///
/// A handle carries an inode number, and an inode number is not a reference: if
/// a name were removed while somebody held a handle to what it named, the inode
/// would be freed and the handle would go on naming it -- reading whatever the
/// next file to be created put there. The count is what makes that impossible,
/// and [`remove_child`] consults it.
static OPEN: IrqSpinLock<BTreeMap<u32, u32>> = IrqSpinLock::new(BTreeMap::new());

/// Inodes whose name has been taken away while somebody still held them.
///
/// The value is whether the name is *already* gone. It matters because the two
/// steps of removing an open file -- taking the name away, and freeing the
/// blocks once the last handle closes -- cannot happen under one lock: the
/// first is disk work, and holding a lock that masks interrupts across a
/// journalled write would stop every other open and close on the machine for
/// milliseconds.
///
/// So the ownership rule is written down instead, and it is short:
///
/// * This map is only ever touched while [`OPEN`] is held. That makes "is
///   anybody still holding this" and "and its name is gone" one atomic step,
///   which is the only thing that has to be atomic.
/// * Whoever *removes* an entry is the one who frees the inode. Removing is the
///   token; there is no second claim to it.
/// * An entry that says `false` means the name has not gone yet, and a handle
///   closing must leave it alone -- the removal still in flight will see that
///   nobody is left and finish the job itself.
///
/// Neither lock is ever held across the disk work.
static REMOVAL: IrqSpinLock<BTreeMap<u32, bool>> = IrqSpinLock::new(BTreeMap::new());

/// An open file or directory.
///
/// The unit of authority for the filesystem. There is no call that takes a
/// path: a process reaches a file by naming a single component inside a
/// directory it already holds, so what it can reach is exactly the subtree
/// under the handles it was given. A program that was never handed a directory
/// cannot open anything, and there is no name it could use instead -- the same
/// argument as for the spawner channel, applied to files.
pub struct Node {
    inode: u32,
    directory: bool,
}

impl Node {
    /// Register an open reference to `inode`.
    fn open(inode: u32, directory: bool) -> Arc<Self> {
        *OPEN.lock().entry(inode).or_insert(0) += 1;
        Arc::new(Self { inode, directory })
    }

    /// Whether it is a directory rather than a file.
    #[must_use]
    pub const fn is_directory(&self) -> bool {
        self.directory
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        // The decision under the lock; the disk work after it.
        let free = {
            let mut open = OPEN.lock();
            let mut last = false;
            if let Some(count) = open.get_mut(&self.inode) {
                *count -= 1;
                if *count == 0 {
                    open.remove(&self.inode);
                    last = true;
                }
            }
            // Freed here only if the name has already gone. An entry saying
            // `false` belongs to a removal still in flight, which will notice
            // that nobody is left and finish the job itself.
            if last {
                let mut removal = REMOVAL.lock();
                if removal.get(&self.inode) == Some(&true) {
                    removal.remove(&self.inode);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };

        if free {
            discard(self.inode);
        }
    }
}

/// Give an inode's blocks back. Only ever called on one nothing names.
fn discard(inode: u32) {
    let mut volume = VOLUME.lock();
    let Some(volume) = volume.as_mut() else {
        return;
    };
    if let Err(error) = volume.discard(inode) {
        // Said rather than silent: the inode and its blocks are leaked, which
        // is the safe direction and is exactly what the filesystem's own check
        // finds and reclaims. Nothing else can go wrong from here, because
        // nothing can name it.
        crate::kprintln!("[fs  ] inode {inode} could not be freed after its name went: {error}");
    }
}

/// Put a file into the store, under a directory, if it is not already there.
///
/// What a system image does to the filesystem it is installing onto. The
/// programs the kernel starts are read out of the image's FAT partition, which
/// nothing on this system writes to; anything a *program* has to open has to be
/// on the store, and on the first boot the store is empty because it was made
/// empty.
///
/// Idempotent, and deliberately so: a file that is already there is left
/// exactly as it is. The copy in the image is the original, and whatever is on
/// the store may have been changed by something with every right to change it.
///
/// # Errors
///
/// If the store is not mounted, or the directory or the file cannot be made.
pub fn seed(directory: &str, name: &str, contents: &[u8]) -> Result<bool, StoreError> {
    let root = root()?;
    let folder = match open_child(&root, directory) {
        Ok(node) => node,
        Err(_) => create_child(&root, directory, true)?,
    };
    if open_child(&folder, name).is_ok() {
        return Ok(false);
    }
    let file = create_child(&folder, name, false)?;
    write_node(&file, contents)?;
    Ok(true)
}

/// Put a file into the store under a directory, replacing what was there.
///
/// The counterpart to [`seed`], and the difference is the whole point of having
/// two: `seed` is for something that arrived with the image and whose copy on
/// the store may since have been changed by somebody entitled to change it,
/// while this is for a fact about the machine *now* -- an address it was just
/// leased, say -- where what was there before is out of date by definition.
///
/// # Errors
///
/// If the store is not mounted, or the directory or file cannot be made.
pub fn replace(directory: &str, name: &str, contents: &[u8]) -> Result<(), StoreError> {
    let root = root()?;
    let folder = match open_child(&root, directory) {
        Ok(node) => node,
        Err(_) => create_child(&root, directory, true)?,
    };
    // Removed first: the filesystem has no truncate, so a shorter file written
    // over a longer one would keep the old ending.
    if open_child(&folder, name).is_ok() {
        remove_child(&folder, name)?;
    }
    let file = create_child(&folder, name, false)?;
    write_node(&file, contents)
}

/// A handle to one directory below the root, made if it is not there.
///
/// What the kernel hands to a program that has business in one place and none
/// anywhere else. Made rather than refused when absent, because the first boot
/// of a fresh machine is exactly when the settings directory does not exist yet
/// and is exactly when something needs to write to it.
///
/// # Errors
///
/// If the store is not mounted, or the directory cannot be made.
pub fn directory(name: &str) -> Result<Arc<Node>, StoreError> {
    let root = root()?;
    match open_child(&root, name) {
        Ok(node) => Ok(node),
        Err(_) => create_child(&root, name, true),
    }
}

/// A handle to the root directory.
///
/// The whole filesystem, which is why it is handed out by the kernel to the
/// programs it starts and never obtained by a program for itself.
pub fn root() -> Result<Arc<Node>, StoreError> {
    let volume = VOLUME.lock();
    volume.as_ref().ok_or(StoreError::NoDisk)?;
    drop(volume);
    Ok(Node::open(super::nexusfs::ROOT, true))
}

/// Open one name inside a directory.
pub fn open_child(parent: &Node, name: &str) -> Result<Arc<Node>, StoreError> {
    if !parent.directory {
        return Err(StoreError::Fs(FsError::WrongKind));
    }
    let entry = {
        let mut volume = VOLUME.lock();
        let volume = volume.as_mut().ok_or(StoreError::NoDisk)?;
        volume.lookup(parent.inode, name)?
    };
    Ok(Node::open(entry.inode, entry.kind == Kind::Directory))
}

/// Make a file or directory inside a directory, and open it.
pub fn create_child(parent: &Node, name: &str, directory: bool) -> Result<Arc<Node>, StoreError> {
    if !parent.directory {
        return Err(StoreError::Fs(FsError::WrongKind));
    }
    let kind = if directory {
        Kind::Directory
    } else {
        Kind::File
    };
    let inode = {
        let mut volume = VOLUME.lock();
        let volume = volume.as_mut().ok_or(StoreError::NoDisk)?;
        volume.create(parent.inode, name, kind)?
    };
    Ok(Node::open(inode, directory))
}

/// Remove a name, and the thing it named.
///
/// A name nobody holds goes with its blocks, in one transaction.
///
/// A name somebody *does* hold goes on its own: the entry leaves the directory
/// now, so nothing can reach it again, and the blocks are freed when the last
/// handle closes. That is what unlink means everywhere else, and this
/// filesystem refused instead until it became clear what refusing costs --
/// several programs here read the settings file on a clock, their reads take
/// microseconds, and replacing that file therefore failed at random with an
/// error no program could do anything sensible about.
///
/// What refusing was protecting against is real and is still prevented. An
/// inode freed while somebody holds it would leave that handle naming a number
/// the filesystem is free to give the next file, and reading through it would
/// read that file. It cannot happen here because freeing is what the last close
/// does, not what the remove does.
pub fn remove_child(parent: &Node, name: &str) -> Result<(), StoreError> {
    if !parent.directory {
        return Err(StoreError::Fs(FsError::WrongKind));
    }
    let entry = {
        let mut volume = VOLUME.lock();
        let volume = volume.as_mut().ok_or(StoreError::NoDisk)?;
        volume.lookup(parent.inode, name)?
    };

    // Whether anybody holds it, decided and noted in the same breath, so that a
    // handle closing cannot fall between the two.
    let held = {
        let open = OPEN.lock();
        let held = open.contains_key(&entry.inode);
        if held {
            REMOVAL.lock().insert(entry.inode, false);
        }
        held
    };

    if !held {
        let mut volume = VOLUME.lock();
        let volume = volume.as_mut().ok_or(StoreError::NoDisk)?;
        volume.unlink(parent.inode, name)?;
        return Ok(());
    }

    // The name first. Until it is gone the inode is reachable, so freeing
    // anything before this would be freeing something a name still points at.
    let inode = {
        let mut volume = VOLUME.lock();
        let volume = volume.as_mut().ok_or(StoreError::NoDisk)?;
        match volume.unlink_name(parent.inode, name) {
            Ok(inode) => inode,
            Err(error) => {
                REMOVAL.lock().remove(&entry.inode);
                return Err(StoreError::Fs(error));
            }
        }
    };

    // And now either somebody is still holding it, in which case the note is
    // marked and the last one out frees it -- or every holder went while the
    // name was being taken away, in which case this is the last one out.
    let free_now = {
        let open = OPEN.lock();
        let mut removal = REMOVAL.lock();
        if open.contains_key(&inode) {
            removal.insert(inode, true);
            false
        } else {
            removal.remove(&inode).is_some()
        }
    };
    if free_now {
        discard(inode);
    }
    Ok(())
}

/// How many names have been taken away from something still open.
///
/// For the monitor. A number that grows and never falls is a handle nobody is
/// closing, which is worth being able to see.
#[must_use]
pub fn pending_removals() -> usize {
    REMOVAL.lock().len()
}

/// Everything in an open directory.
pub fn entries(node: &Node) -> Result<Vec<super::nexusfs::Entry>, StoreError> {
    if !node.directory {
        return Err(StoreError::Fs(FsError::WrongKind));
    }
    let mut volume = VOLUME.lock();
    let volume = volume.as_mut().ok_or(StoreError::NoDisk)?;
    Ok(volume.list(node.inode)?)
}

/// Everything in an open file.
pub fn read_node(node: &Node) -> Result<Vec<u8>, StoreError> {
    if node.directory {
        return Err(StoreError::Fs(FsError::WrongKind));
    }
    let mut volume = VOLUME.lock();
    let volume = volume.as_mut().ok_or(StoreError::NoDisk)?;
    Ok(volume.read(node.inode)?)
}

/// Replace everything in an open file.
pub fn write_node(node: &Node, data: &[u8]) -> Result<(), StoreError> {
    if node.directory {
        return Err(StoreError::Fs(FsError::WrongKind));
    }
    let mut volume = VOLUME.lock();
    let volume = volume.as_mut().ok_or(StoreError::NoDisk)?;
    volume.write(node.inode, data)?;
    Ok(())
}

/// Change part of an open file, growing it if the change runs past the end.
pub fn write_node_at(node: &Node, offset: u64, data: &[u8]) -> Result<u64, StoreError> {
    if node.is_directory() {
        return Err(StoreError::Fs(FsError::WrongKind));
    }
    let mut volume = VOLUME.lock();
    let volume = volume.as_mut().ok_or(StoreError::NoDisk)?;
    Ok(volume.write_at(node.inode, offset, data)?)
}

/// Read part of an open file, returning how much of it was there.
pub fn read_node_at(node: &Node, offset: u64, buffer: &mut [u8]) -> Result<usize, StoreError> {
    if node.is_directory() {
        return Err(StoreError::Fs(FsError::WrongKind));
    }
    let volume = VOLUME.lock();
    let volume = volume.as_ref().ok_or(StoreError::NoDisk)?;
    Ok(volume.read_at(node.inode, offset, buffer)?)
}

/// How many bytes an open file or directory holds.
pub fn size(node: &Node) -> Result<u64, StoreError> {
    let volume = VOLUME.lock();
    let volume = volume.as_ref().ok_or(StoreError::NoDisk)?;
    Ok(volume.stat(node.inode)?.1)
}
