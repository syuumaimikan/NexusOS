//! The GUID Partition Table.
//!
//! A disk is a run of numbered sectors and nothing else. The partition table is
//! the first thing on it that means something, and reading it is what turns
//! "the disk" into "the filesystem NexusOS boots from" and "everything else".
//!
//! # Checked, not trusted
//!
//! The header carries a checksum of itself and another of the entry array, and
//! both are verified here. That is not ceremony: a partition table is the one
//! structure where believing a corrupt value means writing to the wrong part of
//! someone's disk. A table that does not check out is refused, and the kernel
//! says so rather than guessing at what it meant.
//!
//! Only the primary table is read. The backup at the far end exists and is
//! written by the image builder; using it when the primary fails is recovery,
//! which wants a policy about when to repair the disk and is not something to
//! do silently on the way past.

use alloc::string::String;
use alloc::vec::Vec;

use crate::drivers::virtio_blk::{self, SECTOR_SIZE};

/// What a GPT header says it is.
const SIGNATURE: &[u8; 8] = b"EFI PART";

/// Where the primary header lives, always.
const HEADER_LBA: u64 = 1;

/// Bytes of the header the checksum covers, per the specification's own field.
const MIN_HEADER_SIZE: u32 = 92;

/// The EFI system partition's type, in the mixed-endian form GPT stores it:
/// C12A7328-F81F-11D2-BA4B-00A0C93EC93B.
const ESP_TYPE: [u8; 16] = [
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
];

/// Why a partition table could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GptError {
    /// The disk could not be read.
    Disk(virtio_blk::BlockError),
    /// There is no GPT header where one belongs.
    NoSignature,
    /// The header claims a size the specification does not allow.
    BadHeaderSize(u32),
    /// The header's checksum does not match its contents.
    BadHeaderChecksum,
    /// The entry array's checksum does not match its contents.
    BadEntryChecksum,
    /// The header describes more entries than this reader will walk.
    TooManyEntries(u32),
}

impl core::fmt::Display for GptError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Disk(error) => write!(f, "the disk could not be read: {error}"),
            Self::NoSignature => f.write_str("there is no GPT header on this disk"),
            Self::BadHeaderSize(size) => write!(f, "the header claims to be {size} bytes"),
            Self::BadHeaderChecksum => f.write_str("the header's checksum does not match"),
            Self::BadEntryChecksum => f.write_str("the partition entries' checksum does not match"),
            Self::TooManyEntries(count) => write!(f, "the table claims {count} entries"),
        }
    }
}

/// Most entries this reader will walk.
///
/// The specification's usual number is 128, and a header claiming vastly more
/// is either corrupt or hostile; either way it is not something to allocate for.
const MAX_ENTRIES: u32 = 256;

/// One partition.
#[derive(Debug, Clone)]
pub struct Partition {
    /// Index in the table, which is how a partition is usually named.
    pub index: usize,
    /// First and last sector, both inclusive, as GPT states them.
    pub first_lba: u64,
    pub last_lba: u64,
    /// The type GUID, as stored.
    pub type_guid: [u8; 16],
    /// The label, converted from the UTF-16 the table holds.
    pub name: String,
}

impl Partition {
    /// Whether this is an EFI system partition.
    #[must_use]
    pub fn is_esp(&self) -> bool {
        self.type_guid == ESP_TYPE
    }

    /// How many sectors it covers.
    #[must_use]
    pub fn sectors(&self) -> u64 {
        self.last_lba - self.first_lba + 1
    }
}

/// Read the primary partition table.
pub fn read() -> Result<Vec<Partition>, GptError> {
    let mut header = [0u8; SECTOR_SIZE];
    virtio_blk::read_sector(HEADER_LBA, &mut header).map_err(GptError::Disk)?;

    if &header[0..8] != SIGNATURE {
        return Err(GptError::NoSignature);
    }

    let header_size = u32::from_le_bytes(header[12..16].try_into().unwrap());
    if header_size < MIN_HEADER_SIZE || header_size as usize > SECTOR_SIZE {
        return Err(GptError::BadHeaderSize(header_size));
    }

    // The checksum covers the header with its own checksum field zeroed, which
    // is why the field is cleared in a copy rather than skipped: the bytes have
    // to be the ones the writer checksummed, in place.
    let stated = u32::from_le_bytes(header[16..20].try_into().unwrap());
    let mut checked = header;
    checked[16..20].fill(0);
    if crc32(&checked[..header_size as usize]) != stated {
        return Err(GptError::BadHeaderChecksum);
    }

    let entries_lba = u64::from_le_bytes(header[72..80].try_into().unwrap());
    let entry_count = u32::from_le_bytes(header[80..84].try_into().unwrap());
    let entry_size = u32::from_le_bytes(header[84..88].try_into().unwrap());
    let entries_crc = u32::from_le_bytes(header[88..92].try_into().unwrap());

    if entry_count > MAX_ENTRIES {
        return Err(GptError::TooManyEntries(entry_count));
    }

    // The whole array, checksummed as one run of bytes, so a single-entry
    // change anywhere in it is caught.
    let total = entry_count as usize * entry_size as usize;
    let sectors = total.div_ceil(SECTOR_SIZE);
    let mut array = Vec::with_capacity(sectors * SECTOR_SIZE);
    let mut sector = [0u8; SECTOR_SIZE];
    for index in 0..sectors {
        virtio_blk::read_sector(entries_lba + index as u64, &mut sector).map_err(GptError::Disk)?;
        array.extend_from_slice(&sector);
    }
    if crc32(&array[..total]) != entries_crc {
        return Err(GptError::BadEntryChecksum);
    }

    let mut partitions = Vec::new();
    for index in 0..entry_count as usize {
        let entry = &array[index * entry_size as usize..][..entry_size as usize];

        // An entry whose type is all zeroes is an empty slot, not a partition.
        let type_guid: [u8; 16] = entry[0..16].try_into().unwrap();
        if type_guid == [0u8; 16] {
            continue;
        }

        partitions.push(Partition {
            index,
            first_lba: u64::from_le_bytes(entry[32..40].try_into().unwrap()),
            last_lba: u64::from_le_bytes(entry[40..48].try_into().unwrap()),
            type_guid,
            name: utf16_name(&entry[56..128.min(entry_size as usize)]),
        });
    }

    Ok(partitions)
}

/// Turn the table's little-endian UTF-16 label into a string.
///
/// Stops at the first null, which is how the field is terminated when the name
/// is shorter than its 36 characters.
fn utf16_name(bytes: &[u8]) -> String {
    let mut units = Vec::new();
    for pair in bytes.as_chunks::<2>().0 {
        let unit = u16::from_le_bytes(*pair);
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    char::decode_utf16(units)
        .map(|result| result.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// CRC-32, the ordinary reflected one GPT uses.
///
/// Computed a bit at a time rather than from a table. Ninety-two bytes twice
/// per boot is nothing, and a table would be a kilobyte of static data and a
/// initialisation order to think about, in exchange for time nobody is waiting
/// for.
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
