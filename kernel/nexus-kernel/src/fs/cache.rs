//! A cache of disk blocks.
//!
//! Every read NexusFS makes goes to the platter. Reading a 128-byte inode costs
//! a four-kilobyte block; reading the next inode in the same block costs it
//! again; walking a directory reads the same bitmap and the same inode table
//! over and over. On the machine this is developed on that is fast enough not
//! to notice, which is exactly why it is worth fixing before something slower
//! makes it obvious.
//!
//! # Write-through, and why not write-back
//!
//! Reads are served from memory. Writes go to the disk *and* update the cache,
//! rather than being held and written later.
//!
//! That is not the fast choice and it is the only correct one here. Everything
//! NexusFS claims about surviving a power failure is an argument about the
//! *order* writes reach the disk: an inode is written before the directory
//! entry that names it, so a failure in between leaks an inode rather than
//! leaving a name pointing at nothing; an inode is written before its old
//! blocks are freed, so no inode ever points at a block the bitmap calls free.
//! A write-back cache reorders writes by construction. It would turn every one
//! of those arguments into a comment that used to be true, silently, and the
//! failure would only ever show up on a machine that lost power.
//!
//! When there is a journal there can be a write-back cache, because then the
//! journal is what orders the writes and the cache is free to reorder what it
//! likes. Until then this accelerates reads and leaves durability exactly where
//! it was.
//!
//! # Replacement
//!
//! Second chance: each entry carries a bit that a hit sets and the hand clears,
//! and the hand takes the first entry whose bit is already clear. It is what
//! you get for one bit and one counter, it keeps something that was used
//! recently, and it does not need a list to be reordered on every hit -- which
//! matters because a hit is meant to be cheap.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::drivers::virtio_blk::{self, BlockError, SECTOR_SIZE};
use crate::sync::SleepLock;

/// Bytes in a cached block.
///
/// The filesystem's block size, because that is the unit everything above asks
/// for; a cache in a different unit would turn one request into two.
pub const BLOCK_SIZE: usize = 4096;
/// Sectors in a block.
const SECTORS_PER_BLOCK: u64 = (BLOCK_SIZE / SECTOR_SIZE) as u64;

/// How many blocks are kept.
///
/// Two hundred and fifty-six of them, which is a megabyte. Enough to hold the
/// whole of a small filesystem's metadata -- its bitmap and inode table are
/// forty-seven blocks on the partition this ships with -- so the repeated reads
/// that motivated this all hit, and small enough that a system with sixteen
/// megabytes of heap does not notice.
const CAPACITY: usize = 256;

/// One cached block.
struct Entry {
    /// First sector of the block, which is what names it. Absolute, so blocks
    /// from two different partitions cannot collide.
    lba: u64,
    /// Boxed rather than inline, so the cache is one allocation per block
    /// instead of one two-megabyte array that has to be contiguous.
    data: Box<[u8; BLOCK_SIZE]>,
    /// Set by a hit, cleared by the hand: the second chance.
    referenced: bool,
}

/// The cache.
struct Cache {
    entries: Vec<Entry>,
    /// Where replacement resumes looking.
    hand: usize,
}

impl Cache {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
            hand: 0,
        }
    }

    /// Where `lba` is, if it is here.
    fn find(&mut self, lba: u64) -> Option<usize> {
        // A linear scan over at most `CAPACITY` integers. It is a few hundred
        // comparisons against a disk read, and it needs no second structure to
        // keep in step with this one.
        let found = self.entries.iter().position(|entry| entry.lba == lba)?;
        self.entries[found].referenced = true;
        Some(found)
    }

    /// Make room for `lba` and return where it goes.
    fn place(&mut self, lba: u64) -> usize {
        if self.entries.len() < CAPACITY {
            self.entries.push(Entry {
                lba,
                data: Box::new([0u8; BLOCK_SIZE]),
                referenced: true,
            });
            return self.entries.len() - 1;
        }

        // Second chance. The loop is bounded at twice the capacity because one
        // pass may find every bit set, and the pass that clears them is what
        // makes the next one terminate.
        for _ in 0..CAPACITY * 2 {
            let index = self.hand;
            self.hand = (self.hand + 1) % CAPACITY;
            if self.entries[index].referenced {
                self.entries[index].referenced = false;
                continue;
            }
            self.entries[index].lba = lba;
            self.entries[index].referenced = true;
            EVICTIONS.fetch_add(1, Ordering::Relaxed);
            return index;
        }

        // Unreachable: the pass above clears a bit every time it declines an
        // entry, so the second pass finds one. Taking the hand's slot rather
        // than panicking, because being wrong about that should cost a
        // re-read and not the machine.
        let index = self.hand;
        self.entries[index].lba = lba;
        EVICTIONS.fetch_add(1, Ordering::Relaxed);
        index
    }
}

/// The one cache.
///
/// A sleeping lock, because a miss reads the disk while holding it and a disk
/// read blocks. Holding it across the read also serialises misses for the same
/// block, which is the point: two threads that both miss should produce one
/// read and not two.
static CACHE: SleepLock<Cache> = SleepLock::new(Cache::new());

/// Reads served from memory.
static HITS: AtomicU64 = AtomicU64::new(0);
/// Reads that had to go to the disk.
static MISSES: AtomicU64 = AtomicU64::new(0);
/// Blocks written through.
static WRITES: AtomicU64 = AtomicU64::new(0);
/// Blocks thrown out to make room.
static EVICTIONS: AtomicU64 = AtomicU64::new(0);

/// Read the block whose first sector is `lba`.
pub fn read(lba: u64, buffer: &mut [u8; BLOCK_SIZE]) -> Result<(), BlockError> {
    let mut cache = CACHE.lock();

    if let Some(index) = cache.find(lba) {
        buffer.copy_from_slice(&cache.entries[index].data[..]);
        HITS.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }

    // Read into the caller's buffer first, and only then take a slot. A slot
    // taken before the read would hold a block number whose contents are
    // whatever the evicted block left there, and a second reader arriving in
    // between -- which cannot happen while this lock is held, but would the day
    // someone made the miss path concurrent -- would be handed it.
    for index in 0..SECTORS_PER_BLOCK {
        let offset = index as usize * SECTOR_SIZE;
        virtio_blk::read_sector(lba + index, &mut buffer[offset..offset + SECTOR_SIZE])?;
    }
    MISSES.fetch_add(1, Ordering::Relaxed);

    let slot = cache.place(lba);
    cache.entries[slot].data.copy_from_slice(&buffer[..]);
    Ok(())
}

/// Write the block whose first sector is `lba`, and remember it.
///
/// The disk first, then the cache. A cache updated before a failed write would
/// serve the new contents from memory while the disk still held the old ones,
/// which is the one way a cache can turn a reported error into silent
/// corruption.
pub fn write(lba: u64, buffer: &[u8; BLOCK_SIZE]) -> Result<(), BlockError> {
    let mut cache = CACHE.lock();

    for index in 0..SECTORS_PER_BLOCK {
        let offset = index as usize * SECTOR_SIZE;
        virtio_blk::write_sector(lba + index, &buffer[offset..offset + SECTOR_SIZE])?;
    }
    WRITES.fetch_add(1, Ordering::Relaxed);

    let slot = match cache.find(lba) {
        Some(slot) => slot,
        None => cache.place(lba),
    };
    cache.entries[slot].data.copy_from_slice(&buffer[..]);
    Ok(())
}

/// Forget everything.
///
/// For whoever writes to the disk behind the cache's back. Nothing does today;
/// it exists so that the answer to "what if something did" is a function call
/// rather than a redesign.
pub fn invalidate() {
    let mut cache = CACHE.lock();
    cache.entries.clear();
    cache.hand = 0;
}

/// Hits, misses, writes and evictions.
#[must_use]
pub fn statistics() -> (u64, u64, u64, u64) {
    (
        HITS.load(Ordering::Relaxed),
        MISSES.load(Ordering::Relaxed),
        WRITES.load(Ordering::Relaxed),
        EVICTIONS.load(Ordering::Relaxed),
    )
}

/// Hits as a percentage of reads, for reporting.
#[must_use]
pub fn hit_rate() -> u64 {
    let hits = HITS.load(Ordering::Relaxed);
    let misses = MISSES.load(Ordering::Relaxed);
    match hits + misses {
        0 => 0,
        total => hits * 100 / total,
    }
}
