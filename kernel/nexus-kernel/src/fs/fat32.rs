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
}

impl core::fmt::Display for FatError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Disk(error) => write!(f, "the disk could not be read: {error}"),
            Self::NotFat32 => f.write_str("this partition does not hold a FAT32 filesystem"),
            Self::BadGeometry => f.write_str("the boot sector does not describe a filesystem"),
            Self::BadCluster(cluster) => write!(f, "cluster {cluster} is outside the filesystem"),
            Self::ChainTooLong => f.write_str("a cluster chain does not end"),
            Self::NotFound => f.write_str("no such file or directory"),
            Self::WrongKind => f.write_str("that name is the other kind of thing"),
        }
    }
}

/// A mounted filesystem.
pub struct Volume {
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
        let mut boot = [0u8; SECTOR_SIZE];
        virtio_blk::read_sector(start_lba, &mut boot).map_err(FatError::Disk)?;

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
        virtio_blk::read_sector(sector, &mut buffer).map_err(FatError::Disk)?;

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
                virtio_blk::read_sector(self.cluster_lba(cluster) + u64::from(index), &mut buffer)
                    .map_err(FatError::Disk)?;
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
