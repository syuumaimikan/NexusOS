//! A FAT32 reader.
//!
//! FAT32 is not a good filesystem and it is the one every machine agrees on:
//! UEFI requires it for the partition it boots from, so NexusOS has to read it
//! whether or not it likes it. NexusFS, when there is one, will be somewhere
//! else on the disk; this is how the kernel reaches the partition it was
//! started from.
//!
//! # What is here and what is not
//!
//! Reading: the boot parameter block, cluster chains through the file
//! allocation table, directories as runs of 32-byte entries, and files by name.
//! Long names are skipped rather than assembled — every name this needs fits
//! the short form, and a partial implementation of long names would be worse
//! than none, because it would look like it worked.
//!
//! Writing is absent entirely. A writer has to keep two allocation tables and
//! the free-cluster count consistent through a power failure, and there is
//! nothing yet that needs to write to the boot partition.

use alloc::string::String;
use alloc::vec::Vec;

use crate::drivers::virtio_blk::{self, SECTOR_SIZE};

/// Why a filesystem could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatError {
    /// The disk could not be read.
    Disk(virtio_blk::BlockError),
    /// A USB drive could not be read.
    Usb,
    /// The boot sector is not a FAT32 one.
    NotFat32,
    /// A field in the boot sector cannot describe a working filesystem.
    BadGeometry,
    /// A cluster number outside the filesystem was followed.
    BadCluster(u32),
    /// The chain of clusters is longer than the filesystem has.
    ChainTooLong,
    /// No such entry in that directory.
    NotFound,
    /// The name names a directory where a file was wanted, or the reverse.
    WrongKind,
    /// This volume will not be written to.
    ReadOnly,
    /// There is no room left on it.
    Full,
    /// The name cannot be written in the eight-and-three form this uses.
    BadName,
}

impl core::fmt::Display for FatError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Disk(error) => write!(f, "the disk could not be read: {error}"),
            Self::Usb => f.write_str("the USB drive could not be read"),
            Self::NotFat32 => f.write_str("this partition does not hold a FAT32 filesystem"),
            Self::BadGeometry => f.write_str("the boot sector does not describe a filesystem"),
            Self::BadCluster(cluster) => write!(f, "cluster {cluster} is outside the filesystem"),
            Self::ChainTooLong => f.write_str("a cluster chain does not end"),
            Self::NotFound => f.write_str("no such file or directory"),
            Self::WrongKind => f.write_str("that name is the other kind of thing"),
            Self::ReadOnly => f.write_str("this volume is not written to"),
            Self::Full => f.write_str("there is no room left on it"),
            Self::BadName => {
                f.write_str("that name cannot be written in the eight-and-three form this uses")
            }
        }
    }
}

/// A mounted filesystem.
/// Where a volume's sectors come from.
///
/// The reader below does not care, and that is the point: a FAT32 filesystem is
/// a FAT32 filesystem whether the sectors arrive over virtio or over four
/// layers of USB. Before this existed the reader called the virtio driver by
/// name, which made "mount the stick" a rewrite rather than an argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The disk this machine boots with.
    Virtio,
    /// A USB drive, by its position in the list the USB driver keeps.
    Usb(usize),
}

impl Source {
    /// Read one sector from wherever this is.
    ///
    /// # Errors
    ///
    /// Whatever the driver underneath said.
    pub fn read_sector(self, lba: u64, into: &mut [u8]) -> Result<(), FatError> {
        match self {
            Self::Virtio => virtio_blk::read_sector(lba, into).map_err(FatError::Disk),
            Self::Usb(index) => {
                crate::drivers::usb_storage::read_block(index, lba, into).map_err(|_| FatError::Usb)
            }
        }
    }

    /// Write one sector.
    ///
    /// # Errors
    ///
    /// Whatever the driver underneath said, or [`FatError::ReadOnly`] for a
    /// source this will not write to.
    pub fn write_sector(self, lba: u64, from: &[u8]) -> Result<(), FatError> {
        match self {
            // The machine's own disk is written through the store's own
            // journalled path, not through here. A FAT writer let loose on the
            // partition the firmware boots from could make the machine
            // unbootable, and nothing needs it: the ESP is built by the build
            // and read by the loader.
            Self::Virtio => Err(FatError::ReadOnly),
            Self::Usb(index) => {
                crate::drivers::usb_storage::write_block(index, lba, from).map_err(|_| FatError::Usb)
            }
        }
    }
}

pub struct Volume {
    /// Where its sectors come from.
    source: Source,
    /// First sector of the partition, which everything below is relative to.
    start_lba: u64,
    sectors_per_cluster: u32,
    /// First sector of the first file allocation table.
    fat_lba: u64,
    /// First sector of the data region, which cluster two begins at.
    data_lba: u64,
    /// Clusters in the data region, so a chain that leaves it is caught.
    cluster_count: u32,
    /// Cluster the root directory starts at.
    root_cluster: u32,
    /// The volume label from the boot sector, trimmed.
    pub label: String,
    /// How many copies of the allocation table there are, and how long each is.
    ///
    /// Kept because a *writer* needs them and a reader does not: an entry
    /// changed in one copy and not the others is a filesystem that disagrees
    /// with itself, and the next thing to mount it may believe either.
    fat_count: u32,
    fat_size: u32,
}

/// One entry in a directory.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The name, as `NAME.EXT` rather than the padded eleven bytes on disk.
    pub name: String,
    /// Where its data starts.
    pub cluster: u32,
    /// Bytes, which is zero for a directory.
    pub size: u32,
    pub is_directory: bool,
}

/// Attribute bits, of which three matter here.
mod attribute {
    /// The entry is a directory rather than a file.
    pub const DIRECTORY: u8 = 0x10;
    /// The entry names the volume rather than anything in it.
    pub const VOLUME_LABEL: u8 = 0x08;
    /// The entry is one fragment of a long name, to be skipped.
    pub const LONG_NAME: u8 = 0x0F;
}

/// Entries in a chain before this reader decides it does not end.
const MAX_CHAIN: usize = 1 << 20;

impl Volume {
    /// Read the boot sector of the partition starting at `start_lba`.
    pub fn mount(start_lba: u64) -> Result<Self, FatError> {
        Self::mount_on(Source::Virtio, start_lba)
    }

    /// Mount a volume whose sectors come from somewhere named.
    ///
    /// # Errors
    ///
    /// As [`mount`](Self::mount).
    pub fn mount_on(source: Source, start_lba: u64) -> Result<Self, FatError> {
        let mut boot = [0u8; SECTOR_SIZE];
        source.read_sector(start_lba, &mut boot)?;

        // The last two bytes are the signature every boot sector carries, and
        // the type string is what separates FAT32 from its predecessors --
        // though the authoritative test is the one below it, since the string is
        // documented as not authoritative.
        if boot[510] != 0x55 || boot[511] != 0xAA {
            return Err(FatError::NotFat32);
        }

        let bytes_per_sector = u16::from_le_bytes([boot[11], boot[12]]);
        let sectors_per_cluster = u32::from(boot[13]);
        let reserved = u32::from(u16::from_le_bytes([boot[14], boot[15]]));
        let fat_count = u32::from(boot[16]);
        let root_entries = u16::from_le_bytes([boot[17], boot[18]]);
        let fat_size = u32::from_le_bytes(boot[36..40].try_into().unwrap());
        let total_sectors = u32::from_le_bytes(boot[32..36].try_into().unwrap());
        let root_cluster = u32::from_le_bytes(boot[44..48].try_into().unwrap());

        if bytes_per_sector as usize != SECTOR_SIZE
            || sectors_per_cluster == 0
            || reserved == 0
            || fat_count == 0
            || fat_size == 0
            || total_sectors == 0
        {
            return Err(FatError::BadGeometry);
        }
        // A FAT32 volume has no fixed root directory, which is what that field
        // being zero means. It is also how the type is really determined.
        if root_entries != 0 {
            return Err(FatError::NotFat32);
        }

        let fat_lba = start_lba + u64::from(reserved);
        let data_lba = fat_lba + u64::from(fat_count * fat_size);
        let data_sectors = total_sectors - (reserved + fat_count * fat_size);
        let cluster_count = data_sectors / sectors_per_cluster;

        // Below this the volume is FAT16 whatever the string at offset 82 says.
        if cluster_count < 65525 {
            return Err(FatError::NotFat32);
        }

        let label = core::str::from_utf8(&boot[71..82])
            .unwrap_or("")
            .trim_end()
            .into();

        Ok(Self {
            source,
            fat_count,
            fat_size,
            start_lba,
            sectors_per_cluster,
            fat_lba,
            data_lba,
            cluster_count,
            root_cluster,
            label,
        })
    }

    /// First sector of `cluster`.
    fn cluster_lba(&self, cluster: u32) -> u64 {
        self.data_lba + u64::from(cluster - 2) * u64::from(self.sectors_per_cluster)
    }

    /// The entry in the allocation table for `cluster`.
    ///
    /// The top four bits are reserved and must be ignored: a table written by
    /// something that used them would otherwise send this chasing a cluster
    /// number a thousand times too large.
    fn next_cluster(&self, cluster: u32) -> Result<Option<u32>, FatError> {
        if cluster < 2 || cluster >= self.cluster_count + 2 {
            return Err(FatError::BadCluster(cluster));
        }

        let byte = u64::from(cluster) * 4;
        let sector = self.fat_lba + byte / SECTOR_SIZE as u64;
        let offset = (byte % SECTOR_SIZE as u64) as usize;

        let mut buffer = [0u8; SECTOR_SIZE];
        self.source.read_sector(sector, &mut buffer)?;

        let entry =
            u32::from_le_bytes(buffer[offset..offset + 4].try_into().unwrap()) & 0x0FFF_FFFF;
        // Anything at or above this marks the end of a chain; the range below
        // it that is still reserved would be a corrupt table, which
        // `next_cluster` catches on the following call.
        if entry >= 0x0FFF_FFF8 {
            return Ok(None);
        }
        Ok(Some(entry))
    }

    /// Every cluster of the chain starting at `first`.
    fn chain(&self, first: u32) -> Result<Vec<u32>, FatError> {
        let mut clusters = Vec::new();
        let mut cluster = first;
        loop {
            clusters.push(cluster);
            if clusters.len() > MAX_CHAIN {
                return Err(FatError::ChainTooLong);
            }
            match self.next_cluster(cluster)? {
                Some(next) => cluster = next,
                None => return Ok(clusters),
            }
        }
    }

    /// Read a whole cluster chain into memory.
    fn read_chain(&self, first: u32, limit: usize) -> Result<Vec<u8>, FatError> {
        let mut data = Vec::new();
        let mut buffer = [0u8; SECTOR_SIZE];

        for cluster in self.chain(first)? {
            for index in 0..self.sectors_per_cluster {
                self.source
                    .read_sector(self.cluster_lba(cluster) + u64::from(index), &mut buffer)?;
                data.extend_from_slice(&buffer);
                if data.len() >= limit {
                    data.truncate(limit);
                    return Ok(data);
                }
            }
        }
        Ok(data)
    }

    /// Everything in the directory starting at `cluster`.
    fn read_directory(&self, cluster: u32) -> Result<Vec<Entry>, FatError> {
        let raw = self.read_chain(cluster, usize::MAX)?;
        let mut entries = Vec::new();

        for record in raw.as_chunks::<32>().0 {
            match record[0] {
                // A zero first byte means this entry and every one after it in
                // this directory is unused, so the walk stops rather than
                // reading whatever the cluster held before.
                0x00 => break,
                // A deleted entry.
                0xE5 => continue,
                _ => {}
            }

            let attributes = record[11];
            if attributes & attribute::LONG_NAME == attribute::LONG_NAME {
                continue;
            }
            if attributes & attribute::VOLUME_LABEL != 0 {
                continue;
            }

            let cluster = (u32::from(u16::from_le_bytes([record[20], record[21]])) << 16)
                | u32::from(u16::from_le_bytes([record[26], record[27]]));

            entries.push(Entry {
                name: short_name(&record[0..11]),
                cluster,
                size: u32::from_le_bytes(record[28..32].try_into().unwrap()),
                is_directory: attributes & attribute::DIRECTORY != 0,
            });
        }

        Ok(entries)
    }

    /// Everything in the root directory.
    pub fn root(&self) -> Result<Vec<Entry>, FatError> {
        self.read_directory(self.root_cluster)
    }

    /// Read a directory named by a path, as [`Volume::read_file`] reads a file.
    ///
    /// Wanted by anything that has to act on *whatever* is in a directory
    /// rather than on a name it already knows -- copying every package out of
    /// an image, for instance, on a system that should not need changing to
    /// ship a second one.
    ///
    /// # Errors
    ///
    /// If the path does not exist or does not name a directory.
    pub fn read_directory_at(&self, path: &str) -> Result<Vec<Entry>, FatError> {
        let mut directory = self.root_cluster;
        for component in path.split(['/', '\\']).filter(|part| !part.is_empty()) {
            let entry = self.find(directory, component)?;
            if !entry.is_directory {
                return Err(FatError::WrongKind);
            }
            directory = entry.cluster;
        }
        self.read_directory(directory)
    }

    /// Look one name up in a directory, case-insensitively.
    fn find(&self, directory: u32, name: &str) -> Result<Entry, FatError> {
        self.read_directory(directory)?
            .into_iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(name))
            .ok_or(FatError::NotFound)
    }

    /// Read a file named by a path such as `EFI/BOOT/BOOTX64.EFI`.
    ///
    /// Separators may be either slash; the on-disk form has neither, so which
    /// one a caller writes is a matter of where the string came from.
    pub fn read_file(&self, path: &str) -> Result<Vec<u8>, FatError> {
        let mut directory = self.root_cluster;
        let mut components = path.split(['/', '\\']).filter(|part| !part.is_empty());

        let Some(mut current) = components.next() else {
            return Err(FatError::NotFound);
        };

        for next in components {
            let entry = self.find(directory, current)?;
            if !entry.is_directory {
                return Err(FatError::WrongKind);
            }
            directory = entry.cluster;
            current = next;
        }

        let entry = self.find(directory, current)?;
        if entry.is_directory {
            return Err(FatError::WrongKind);
        }
        // Bounded by the size the directory entry states, so a file shorter
        // than its last cluster does not come back padded with whatever was
        // there before.
        self.read_chain(entry.cluster, entry.size as usize)
    }

    /// The partition this was mounted from.
    #[must_use]
    pub fn start_lba(&self) -> u64 {
        self.start_lba
    }

    /// Bytes in a cluster.
    #[must_use]
    pub fn cluster_bytes(&self) -> u32 {
        self.sectors_per_cluster * SECTOR_SIZE as u32
    }

    /// Clusters the data region holds.
    #[must_use]
    pub fn cluster_count(&self) -> u32 {
        self.cluster_count
    }
}

/// Turn the padded eleven bytes on disk into `NAME.EXT`.
fn short_name(raw: &[u8]) -> String {
    let stem = core::str::from_utf8(&raw[0..8]).unwrap_or("").trim_end();
    let extension = core::str::from_utf8(&raw[8..11]).unwrap_or("").trim_end();

    let mut name = String::from(stem);
    if !extension.is_empty() {
        name.push('.');
        name.push_str(extension);
    }
    name
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// What a free entry in the allocation table reads as.
const FREE: u32 = 0;
/// And what the last cluster of a chain reads as.
const END_OF_CHAIN: u32 = 0x0FFF_FFFF;

impl Volume {
    /// Set the allocation table entry for `cluster`, in every copy of it.
    ///
    /// Every copy, and that is the whole reason `fat_count` is kept. A FAT32
    /// volume normally has two, and a filesystem whose two tables disagree is
    /// one where the next thing to mount it may believe either — which is a
    /// corruption that appears later, somewhere else, for no visible reason.
    fn set_next_cluster(&self, cluster: u32, value: u32) -> Result<(), FatError> {
        if cluster < 2 || cluster >= self.cluster_count + 2 {
            return Err(FatError::BadCluster(cluster));
        }
        let byte = u64::from(cluster) * 4;
        let within = byte / SECTOR_SIZE as u64;
        let offset = (byte % SECTOR_SIZE as u64) as usize;

        for copy in 0..u64::from(self.fat_count) {
            let sector = self.fat_lba + copy * u64::from(self.fat_size) + within;
            let mut buffer = [0u8; SECTOR_SIZE];
            self.source.read_sector(sector, &mut buffer)?;
            // The top four bits are reserved and belong to whoever set them.
            // Read, modified and written back rather than replaced, because a
            // writer that cleared them would be changing something that is not
            // its business.
            let existing = u32::from_le_bytes(buffer[offset..offset + 4].try_into().unwrap());
            let written = (existing & 0xF000_0000) | (value & 0x0FFF_FFFF);
            buffer[offset..offset + 4].copy_from_slice(&written.to_le_bytes());
            self.source.write_sector(sector, &buffer)?;
        }
        Ok(())
    }

    /// Find `count` free clusters and chain them together.
    ///
    /// Returns the first. The chain is written into the table before this
    /// returns, so a machine that stopped here would have the clusters marked
    /// used and nothing pointing at them — lost space, which every filesystem
    /// checker since 1983 has reclaimed. The other ordering loses *data*, which
    /// is worse: a directory entry pointing at clusters the table still calls
    /// free is a file the next allocation will overwrite.
    fn allocate(&self, count: u32) -> Result<u32, FatError> {
        if count == 0 {
            return Err(FatError::Full);
        }
        let mut found: Vec<u32> = Vec::new();

        // Walked a sector at a time rather than a cluster at a time: a table
        // entry is four bytes, so one read answers a hundred and twenty-eight
        // of these questions.
        let mut buffer = [0u8; SECTOR_SIZE];
        let entries_per_sector = (SECTOR_SIZE / 4) as u32;
        let sectors = (self.cluster_count + 2).div_ceil(entries_per_sector);

        for index in 0..u64::from(sectors) {
            if found.len() as u32 == count {
                break;
            }
            self.source.read_sector(self.fat_lba + index, &mut buffer)?;
            for slot in 0..entries_per_sector {
                let cluster = index as u32 * entries_per_sector + slot;
                // Clusters zero and one are not clusters: their entries hold the
                // media descriptor and the dirty flags.
                if cluster < 2 || cluster >= self.cluster_count + 2 {
                    continue;
                }
                let at = (slot * 4) as usize;
                let entry =
                    u32::from_le_bytes(buffer[at..at + 4].try_into().unwrap()) & 0x0FFF_FFFF;
                if entry == FREE {
                    found.push(cluster);
                    if found.len() as u32 == count {
                        break;
                    }
                }
            }
        }

        if found.len() as u32 != count {
            return Err(FatError::Full);
        }

        // Linked last-first, so no partly built chain is ever reachable through
        // the table: each cluster points at one already marked, and the first is
        // written last.
        for index in (0..found.len()).rev() {
            let next = if index + 1 == found.len() {
                END_OF_CHAIN
            } else {
                found[index + 1]
            };
            self.set_next_cluster(found[index], next)?;
        }
        Ok(found[0])
    }

    /// Mark every cluster of a chain free.
    fn release(&self, first: u32) -> Result<(), FatError> {
        if first < 2 {
            return Ok(());
        }
        for cluster in self.chain(first)? {
            self.set_next_cluster(cluster, FREE)?;
        }
        Ok(())
    }

    /// Write `data` into the chain starting at `first`.
    fn fill(&self, first: u32, data: &[u8]) -> Result<(), FatError> {
        let mut at = 0usize;

        for cluster in self.chain(first)? {
            if at >= data.len() {
                break;
            }
            for index in 0..self.sectors_per_cluster {
                let mut buffer = [0u8; SECTOR_SIZE];
                let take = (data.len() - at).min(SECTOR_SIZE);
                buffer[..take].copy_from_slice(&data[at..at + take]);
                // The rest of the sector is zeroed rather than left as it was.
                // What was there belongs to whoever deleted it, and a file
                // shorter than its last sector would otherwise carry somebody
                // else's bytes past its own end.
                self.source
                    .write_sector(self.cluster_lba(cluster) + u64::from(index), &buffer)?;
                at += take;
                if at >= data.len() {
                    break;
                }
            }
        }
        Ok(())
    }

    /// Write a file into the root directory, replacing whatever was there.
    ///
    /// Creates it if the name is free.
    ///
    /// # Errors
    ///
    /// [`FatError::ReadOnly`] for a volume this will not write, [`FatError::Full`]
    /// when there is no room, [`FatError::BadName`] for a name that will not fit
    /// the eight-and-three form.
    pub fn write_file(&self, path: &str, data: &[u8]) -> Result<(), FatError> {
        let Some(short) = eight_and_three(path) else {
            return Err(FatError::BadName);
        };

        let per_cluster = self.sectors_per_cluster as usize * SECTOR_SIZE;
        let needed = if data.is_empty() {
            0
        } else {
            data.len().div_ceil(per_cluster) as u32
        };

        // What is already there, if anything.
        let existing = self
            .read_directory(self.root_cluster)?
            .into_iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(path));
        if let Some(entry) = &existing {
            if entry.is_directory {
                return Err(FatError::WrongKind);
            }
        }

        // The new chain first, so a failure part-way leaves the old file exactly
        // as it was. Only when the data is safely on the disk does the directory
        // entry start pointing at it.
        let first = if needed == 0 { 0 } else { self.allocate(needed)? };
        if needed > 0 {
            self.fill(first, data)?;
        }

        self.set_entry(&short, first, data.len() as u32)?;

        // And only now is the old chain released. The other order loses the file
        // if anything in between fails.
        if let Some(entry) = existing {
            if entry.cluster >= 2 {
                self.release(entry.cluster)?;
            }
        }
        Ok(())
    }

    /// Put a directory entry in the root, replacing one of the same name.
    fn set_entry(&self, short: &[u8; 11], cluster: u32, size: u32) -> Result<(), FatError> {
        for chain_cluster in self.chain(self.root_cluster)? {
            for index in 0..self.sectors_per_cluster {
                let sector = self.cluster_lba(chain_cluster) + u64::from(index);
                let mut buffer = [0u8; SECTOR_SIZE];
                self.source.read_sector(sector, &mut buffer)?;

                for slot in 0..(SECTOR_SIZE / 32) {
                    let at = slot * 32;
                    let first = buffer[at];
                    let matches = buffer[at..at + 11] == short[..];
                    // A free slot is one never used (zero) or deleted (0xE5).
                    let free = first == 0x00 || first == 0xE5;
                    if !matches && !free {
                        continue;
                    }

                    buffer[at..at + 11].copy_from_slice(&short[..]);
                    buffer[at + 11] = 0x20; // a plain file
                    // Times are left at zero. A wrong timestamp is worse than an
                    // obviously absent one, and this machine's clock is not in
                    // this function's reach.
                    buffer[at + 12..at + 20].fill(0);
                    buffer[at + 20..at + 22]
                        .copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
                    buffer[at + 22..at + 26].fill(0);
                    buffer[at + 26..at + 28]
                        .copy_from_slice(&((cluster & 0xFFFF) as u16).to_le_bytes());
                    buffer[at + 28..at + 32].copy_from_slice(&size.to_le_bytes());

                    // If this was the never-used slot at the end, the next one
                    // has to stay zero so the directory still has a terminator.
                    if first == 0x00 && at + 64 <= SECTOR_SIZE {
                        buffer[at + 32..at + 64].fill(0);
                    }
                    self.source.write_sector(sector, &buffer)?;
                    return Ok(());
                }
            }
        }
        // Every slot in the root is taken. Growing it means allocating another
        // cluster and linking it, which this does not do yet — said rather than
        // silently dropping the file.
        Err(FatError::Full)
    }

    /// Remove a file from the root and free what it held.
    ///
    /// # Errors
    ///
    /// As [`write_file`](Self::write_file), or [`FatError::NotFound`].
    pub fn remove_file(&self, path: &str) -> Result<(), FatError> {
        let Some(short) = eight_and_three(path) else {
            return Err(FatError::BadName);
        };
        let entry = self
            .read_directory(self.root_cluster)?
            .into_iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(path))
            .ok_or(FatError::NotFound)?;
        if entry.is_directory {
            return Err(FatError::WrongKind);
        }

        // The name goes first. A machine that stopped between the two leaves
        // clusters marked used with nothing pointing at them, which is lost
        // space; the other order leaves a name pointing at clusters the next
        // allocation will hand out, which is somebody else's file inside this
        // one.
        for chain_cluster in self.chain(self.root_cluster)? {
            for index in 0..self.sectors_per_cluster {
                let sector = self.cluster_lba(chain_cluster) + u64::from(index);
                let mut buffer = [0u8; SECTOR_SIZE];
                self.source.read_sector(sector, &mut buffer)?;
                for slot in 0..(SECTOR_SIZE / 32) {
                    let at = slot * 32;
                    if buffer[at..at + 11] != short[..] {
                        continue;
                    }
                    buffer[at] = 0xE5;
                    self.source.write_sector(sector, &buffer)?;
                    if entry.cluster >= 2 {
                        self.release(entry.cluster)?;
                    }
                    return Ok(());
                }
            }
        }
        Err(FatError::NotFound)
    }
}

/// A name as the eleven padded bytes a directory entry holds.
///
/// `NOTES.TXT` becomes `NOTES   TXT`. Returns `None` for anything that will not
/// fit, which is refused rather than truncated: a file saved under a name that
/// is not the one that was typed is a file somebody will not find again.
fn eight_and_three(name: &str) -> Option<[u8; 11]> {
    /// The punctuation a short name may hold, beside letters and digits.
    /// Anything else — a space, a comma, a character above ASCII — needs the
    /// long-name format, which is a second kind of directory entry this does
    /// not write.
    const ALLOWED: &[u8] = b"_-~!#$%&(){}@^";

    let name = name.trim();
    if name.is_empty() || name.contains('/') || name.contains('\\') {
        return None;
    }
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) => (stem, extension),
        None => (name, ""),
    };
    if stem.is_empty() || stem.len() > 8 || extension.len() > 3 {
        return None;
    }

    let mut out = [b' '; 11];
    for (index, byte) in stem.bytes().enumerate() {
        if !byte.is_ascii_alphanumeric() && !ALLOWED.contains(&byte) {
            return None;
        }
        out[index] = byte.to_ascii_uppercase();
    }
    for (index, byte) in extension.bytes().enumerate() {
        if !byte.is_ascii_alphanumeric() {
            return None;
        }
        out[8 + index] = byte.to_ascii_uppercase();
    }
    Some(out)
}
