//! Checked R615 GSP-RM ELF container and separate external bindata.
//! Reference: NVIDIA _kgspPrepareGspRmBinaryImage and bin_data_pvt.h.
use crate::{Error, Span, VERSION};
pub const MAX_FILE: usize = 128 * 1024 * 1024;
pub const RM_SIZE: usize = 27043992;
pub const UCODES_SIZE: usize = 58215936;
pub const RM_SHA: [u8; 32] = [
    0xa2, 0x01, 0x00, 0xca, 0xfd, 0x5a, 0x85, 0xe2, 0x2b, 0x3e, 0x13, 0x18, 0x83, 0x39, 0x73, 0x0c,
    0x6c, 0xed, 0x94, 0xbd, 0x5f, 0x65, 0x49, 0x84, 0xdd, 0x2e, 0xd2, 0x74, 0xed, 0x58, 0xd5, 0x84,
];
pub const UCODES_SHA: [u8; 32] = [
    0xee, 0xd5, 0x3d, 0x76, 0x24, 0x85, 0x82, 0xd7, 0x1c, 0x62, 0x9e, 0x8b, 0x62, 0x30, 0xae, 0xe4,
    0x8c, 0xe6, 0x73, 0xc9, 0x1d, 0x78, 0x23, 0x01, 0xc9, 0x4e, 0x98, 0xac, 0x6a, 0x6d, 0xb7, 0xaa,
];

pub trait Read {
    fn len(&self) -> usize;
    fn read(&self, offset: usize, output: &mut [u8]) -> Result<(), Error>;
}
impl Read for [u8] {
    fn len(&self) -> usize {
        <[u8]>::len(self)
    }
    fn read(&self, offset: usize, output: &mut [u8]) -> Result<(), Error> {
        let bytes = self
            .get(offset..offset.checked_add(output.len()).ok_or(Error::Bounds)?)
            .ok_or(Error::Truncated)?;
        output.copy_from_slice(bytes);
        Ok(())
    }
}

pub fn hash(reader: &(impl Read + ?Sized)) -> Result<[u8; 32], Error> {
    let mut hash = nexus_crypto::sha256::Sha256::new();
    let mut chunk = [0u8; 4096];
    for offset in (0..reader.len()).step_by(chunk.len()) {
        let take = chunk.len().min(reader.len() - offset);
        reader.read(offset, &mut chunk[..take])?;
        hash.update(&chunk[..take]);
    }
    Ok(hash.finish())
}

#[derive(Debug)]
pub struct Gsp {
    pub image: Span,
    pub signature: Span,
    pub build_id: Span,
}
fn u64_at(bytes: &[u8], at: usize) -> Result<usize, Error> {
    crate::wide(bytes, at)
}

impl Gsp {
    pub fn parse(reader: &(impl Read + ?Sized)) -> Result<Self, Error> {
        if reader.len() > MAX_FILE {
            return Err(Error::Bounds);
        }
        let mut header = [0u8; 64];
        reader.read(0, &mut header)?;
        if header[..8] != [0x7f, b'E', b'L', b'F', 2, 1, 1, 0]
            || crate::half(&header, 16)? != 1
            || crate::half(&header, 18)? != 243
            || crate::word(&header, 20)? != 1
            || u64_at(&header, 24)? != 0
            || u64_at(&header, 32)? != 0
            || crate::half(&header, 52)? != 64
            || crate::half(&header, 54)? != 0
            || crate::half(&header, 56)? != 0
            || crate::half(&header, 58)? != 64
        {
            return Err(Error::Header);
        }
        let table_at = u64_at(&header, 40)?;
        let count = crate::half(&header, 60)? as usize;
        let names_index = crate::half(&header, 62)? as usize;
        if !(4..=128).contains(&count) || names_index == 0 || names_index >= count {
            return Err(Error::Section);
        }
        let table = Span::checked(table_at, count * 64, reader.len())?;
        if table.offset < 64 {
            return Err(Error::Bounds);
        }
        let mut record = [0u8; 64];
        reader.read(table_at, &mut record)?;
        if record.iter().any(|&b| b != 0) {
            return Err(Error::Header);
        }
        reader.read(table_at + names_index * 64, &mut record)?;
        let strings = Span::checked(u64_at(&record, 24)?, u64_at(&record, 32)?, reader.len())?;
        if crate::word(&record, 4)? != 3 || strings.size == 0 || strings.size > 4096 {
            return Err(Error::Section);
        }
        let mut names = [0u8; 4096];
        reader.read(strings.offset, &mut names[..strings.size])?;
        let mut ranges = [Span { offset: 0, size: 0 }; 128];
        let (mut image, mut signature, mut version, mut build_id) = (None, None, None, None);
        for index in 1..count {
            reader.read(table_at + index * 64, &mut record)?;
            let range = Span::checked(u64_at(&record, 24)?, u64_at(&record, 32)?, reader.len())?;
            if range.size != 0
                && (range.offset < 64
                    || range.overlaps(table)
                    || ranges[..index].iter().any(|&prior| range.overlaps(prior)))
            {
                return Err(Error::Overlap);
            }
            ranges[index] = range;
            let name_at = crate::word(&record, 0)? as usize;
            let tail = names[..strings.size].get(name_at..).ok_or(Error::Section)?;
            let end = tail.iter().position(|&b| b == 0).ok_or(Error::Section)?;
            let target = match &tail[..end] {
                b".fwimage" => Some(&mut image),
                b".fwsignature_gb20x" => Some(&mut signature),
                b".fwversion" => Some(&mut version),
                b".note.gnu.build-id" => Some(&mut build_id),
                _ => None,
            };
            if let Some(target) = target {
                let expected_type = if &tail[..end] == b".note.gnu.build-id" {
                    7
                } else {
                    1
                };
                if target.is_some() || range.size == 0 || crate::word(&record, 4)? != expected_type
                {
                    return Err(Error::Section);
                }
                *target = Some(range);
            }
        }
        let version = version.ok_or(Error::Section)?;
        let mut text = [0u8; 64];
        if version.size != VERSION.len() + 1 {
            return Err(Error::Version);
        }
        reader.read(version.offset, &mut text[..version.size])?;
        if &text[..VERSION.len()] != VERSION.as_bytes() || text[VERSION.len()] != 0 {
            return Err(Error::Version);
        }
        let signature = signature.ok_or(Error::Section)?;
        if signature.size != 4096 {
            return Err(Error::Section);
        }
        Ok(Self {
            image: image.ok_or(Error::Section)?,
            signature,
            build_id: build_id.ok_or(Error::Section)?,
        })
    }
}

pub fn check_ucodes(reader: &(impl Read + ?Sized)) -> Result<(), Error> {
    let mut header = [0u8; 24];
    reader.read(0, &mut header)?;
    if &header[..8] != b"NVUCODES"
        || crate::word(&header, 8)? != 1
        || crate::word(&header, 12)? != 1293
        || &header[16..24] != [0xa6, 0x03, 0xb7, 0xcd, 0xfe, 0xbe, 0x38, 0x2d]
    {
        return Err(Error::Header);
    }
    if reader.len() != UCODES_SIZE {
        return Err(Error::Bounds);
    }
    Ok(())
}

pub fn verify(reader: &(impl Read + ?Sized), ucodes: bool) -> Result<(), Error> {
    let (size, expected) = if ucodes {
        (UCODES_SIZE, UCODES_SHA)
    } else {
        (RM_SIZE, RM_SHA)
    };
    if reader.len() != size || hash(reader)? != expected {
        return Err(Error::ReleaseMismatch);
    }
    if ucodes {
        check_ucodes(reader)
    } else {
        Gsp::parse(reader).map(|_| ())
    }
}
