//! Checked NVIDIA R615 production GB202/GB206 bootstrap formats.
//! Layout references: NVIDIA's pinned nouveau extractor and rmRiscvUcode.h.
//! Parsing and pinned SHA256 checks do not authenticate a signature in hardware,
//! boot GSP, or constitute a Vulkan driver.
#![no_std]

pub mod gsp;
pub mod radix;
pub mod amdvi;
pub mod boot;
pub mod fsp;

pub const VERSION: &str = "615.78.08";
pub const SOURCE_REVISION: &str = "ca2d03e6fafd4d4b56b383497d931c25b283bfcf";
pub const MAX_IMAGE: usize = 1024 * 1024;

const BOOT_DIGEST: [u8; 32] = [
    0x8e, 0x8b, 0x81, 0x38, 0x1f, 0x50, 0x46, 0xde, 0x52, 0x68, 0x65, 0xa5, 0x5a, 0xd3, 0x85, 0xa4,
    0x2d, 0x0c, 0x1d, 0x22, 0x83, 0xf8, 0x76, 0x2a, 0x06, 0x96, 0x5f, 0x0e, 0x95, 0x8a, 0x8a, 0x25,
];
const FMC_DIGEST: [u8; 32] = [
    0x8d, 0x51, 0x1e, 0x4b, 0xf6, 0x4d, 0x35, 0x77, 0xcb, 0xae, 0x8f, 0xa4, 0xe3, 0x3a, 0x0b, 0x5b,
    0x60, 0x5b, 0x45, 0xa5, 0xee, 0xcc, 0x66, 0x50, 0x0a, 0x53, 0x73, 0x4e, 0x27, 0xd7, 0x4a, 0xb7,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Truncated,
    Header,
    Version,
    Bounds,
    Overlap,
    Section,
    Checksum,
    ReleaseMismatch,
}

fn word(bytes: &[u8], at: usize) -> Result<u32, Error> {
    Ok(u32::from_le_bytes(
        bytes
            .get(at..at.checked_add(4).ok_or(Error::Bounds)?)
            .ok_or(Error::Truncated)?
            .try_into()
            .unwrap(),
    ))
}
fn wide(bytes: &[u8], at: usize) -> Result<usize, Error> {
    let value = u64::from_le_bytes(
        bytes
            .get(at..at.checked_add(8).ok_or(Error::Bounds)?)
            .ok_or(Error::Truncated)?
            .try_into()
            .unwrap(),
    );
    usize::try_from(value).map_err(|_| Error::Bounds)
}
fn half(bytes: &[u8], at: usize) -> Result<u16, Error> {
    Ok(u16::from_le_bytes(
        bytes
            .get(at..at + 2)
            .ok_or(Error::Truncated)?
            .try_into()
            .unwrap(),
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub offset: usize,
    pub size: usize,
}
impl Span {
    fn checked(offset: usize, size: usize, limit: usize) -> Result<Self, Error> {
        if offset.checked_add(size).ok_or(Error::Bounds)? > limit {
            return Err(Error::Bounds);
        }
        Ok(Self { offset, size })
    }
    pub fn bytes<'a>(&self, image: &'a [u8]) -> Option<&'a [u8]> {
        image.get(self.offset..self.offset.checked_add(self.size)?)
    }
    fn overlaps(self, other: Self) -> bool {
        self.size != 0
            && other.size != 0
            && self.offset < other.offset + other.size
            && other.offset < self.offset + self.size
    }
}

pub struct Bootloader<'a> {
    pub image: &'a [u8],
    pub descriptor: &'a [u8],
    pub manifest: Span,
    /// WPR metadata uses descriptor.manifestOffset, distinct from the monitor.
    pub boot_manifest_offset: usize,
    pub code: Span,
    pub data: Span,
}
impl<'a> Bootloader<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_IMAGE || word(bytes, 0)? != 0x10de || word(bytes, 4)? != 1 {
            return Err(Error::Header);
        }
        let declared = word(bytes, 8)? as usize;
        let descriptor_at = word(bytes, 12)? as usize;
        let image_at = word(bytes, 16)? as usize;
        let image_size = word(bytes, 20)? as usize;
        // NVIDIA's extractor stores the rounded length but omits final padding.
        if descriptor_at != 24
            || image_at != 116
            || image_size == 0
            || declared
                != image_at
                    .checked_add(image_size)
                    .ok_or(Error::Bounds)?
                    .checked_add(255)
                    .ok_or(Error::Bounds)?
                    & !255
            || bytes.len() != image_at + image_size
        {
            return Err(Error::Header);
        }
        let descriptor = bytes.get(descriptor_at..image_at).ok_or(Error::Truncated)?;
        if word(descriptor, 0)? != 6 {
            return Err(Error::Version);
        }
        let mut spans = [Span { offset: 0, size: 0 }; 8];
        for (index, pair) in [1, 3, 5, 8, 10, 12, 15, 17].into_iter().enumerate() {
            spans[index] = Span::checked(
                word(descriptor, pair * 4)? as usize,
                word(descriptor, (pair + 1) * 4)? as usize,
                image_size,
            )?;
        }
        for index in [14, 20, 21, 22] {
            if word(descriptor, index * 4)? > 1 {
                return Err(Error::Header);
            }
        }
        let manifest = spans[3];
        let data = spans[4];
        let code = spans[5];
        if word(descriptor, 14 * 4)? != 1 || manifest.size == 0 || code.size == 0 || data.size == 0
        {
            return Err(Error::Header);
        }
        if manifest.overlaps(code) || manifest.overlaps(data) || code.overlaps(data) {
            return Err(Error::Overlap);
        }
        if [manifest, code, data]
            .iter()
            .any(|span| span.offset % 256 != 0 || span.size % 256 != 0)
        {
            return Err(Error::Bounds);
        }
        Ok(Self {
            image: &bytes[image_at..],
            descriptor,
            manifest,
            boot_manifest_offset: spans[0].offset,
            code,
            data,
        })
    }
}

pub struct Fmc<'a> {
    pub hash: &'a [u8],
    pub signature: &'a [u8],
    pub public_key: &'a [u8],
    pub image: &'a [u8],
}

impl<'a> Fmc<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_IMAGE
            || bytes.get(..8) != Some(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0])
            || half(bytes, 16)? != 0
            || half(bytes, 18)? != 0
            || word(bytes, 20)? != 1
            || wide(bytes, 24)? != 0
            || wide(bytes, 32)? != 0
            || wide(bytes, 40)? != 64
            || half(bytes, 52)? != 64
            || half(bytes, 54)? != 0
            || half(bytes, 56)? != 0
            || half(bytes, 58)? != 64
            || half(bytes, 60)? != 6
            || half(bytes, 62)? != 1
        {
            return Err(Error::Header);
        }
        let header_end = 64 + 6 * 64;
        if bytes
            .get(64..128)
            .ok_or(Error::Truncated)?
            .iter()
            .any(|&byte| byte != 0)
        {
            return Err(Error::Header);
        }
        let strings = Span::checked(wide(bytes, 128 + 24)?, wide(bytes, 128 + 32)?, bytes.len())?;
        if word(bytes, 128 + 4)? != 3 || strings.offset < header_end {
            return Err(Error::Section);
        }
        let names = strings.bytes(bytes).ok_or(Error::Bounds)?;
        let mut ranges = [strings; 5];
        let mut payloads = [&[][..]; 4];
        for (index, wanted) in [b"hash".as_slice(), b"signature", b"publickey", b"image"]
            .into_iter()
            .enumerate()
        {
            let at = 64 + (index + 2) * 64;
            let name_at = word(bytes, at)? as usize;
            let name = names.get(name_at..).ok_or(Error::Section)?;
            let end = name
                .iter()
                .position(|&byte| byte == 0)
                .ok_or(Error::Section)?;
            if &name[..end] != wanted
                || word(bytes, at + 4)? != 1
                || wide(bytes, at + 8)? != 0xfff00102
                || wide(bytes, at + 16)? != 0
                || word(bytes, at + 40)? != 0
                || wide(bytes, at + 48)? != 4
                || wide(bytes, at + 56)? != 0
            {
                return Err(Error::Section);
            }
            let range = Span::checked(wide(bytes, at + 24)?, wide(bytes, at + 32)?, bytes.len())?;
            if range.size == 0 || range.offset < header_end || range.offset % 4 != 0 {
                return Err(Error::Bounds);
            }
            if ranges[..index + 1]
                .iter()
                .any(|&other| range.overlaps(other))
            {
                return Err(Error::Overlap);
            }
            ranges[index + 1] = range;
            let payload = range.bytes(bytes).ok_or(Error::Bounds)?;
            if crc32(payload) != word(bytes, at + 44)? {
                return Err(Error::Checksum);
            }
            payloads[index] = payload;
        }
        if payloads[0].len() != 48
            || payloads[1].len() != 96
            || payloads[2].len() != 97
            || payloads[2][0] != 4
        {
            return Err(Error::Section);
        }
        Ok(Self {
            hash: payloads[0],
            signature: payloads[1],
            public_key: payloads[2],
            image: payloads[3],
        })
    }
}

pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

/// Exact production bundle extracted from the pinned, SHA256-checked source.
/// Opaque ECC signature/hash/key bytes stay intact for future GPU authentication.
pub fn verify_pinned<'a>(
    boot: &'a [u8],
    fmc: &'a [u8],
) -> Result<(Bootloader<'a>, Fmc<'a>), Error> {
    let parsed_boot = Bootloader::parse(boot)?;
    let parsed_fmc = Fmc::parse(fmc)?;
    if nexus_crypto::sha256::digest(boot) != BOOT_DIGEST
        || nexus_crypto::sha256::digest(fmc) != FMC_DIGEST
    {
        return Err(Error::ReleaseMismatch);
    }
    Ok((parsed_boot, parsed_fmc))
}

#[cfg(test)]
mod tests;
