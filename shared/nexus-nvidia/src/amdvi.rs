//! Original AMD-Vi protocol encodings, for native GPU DMA address translation.
//! References: AMD 48882, ACPI IVRS definitions; no Linux driver code is used.
use crate::{Error, Span};
pub const PAGE: u64 = 4096;
pub const ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;
pub const READ: u64 = 1 << 61;
pub const WRITE: u64 = 1 << 62;
pub const RW: u64 = READ | WRITE;
pub const IOVA_LIMIT: u64 = 1 << 39;

fn half(bytes: &[u8], at: usize) -> Result<u16, Error> {
    crate::half(bytes, at)
}
fn long(bytes: &[u8], at: usize) -> Result<u64, Error> {
    let data = bytes
        .get(at..at.checked_add(8).ok_or(Error::Bounds)?)
        .ok_or(Error::Truncated)?;
    Ok(u64::from_le_bytes(data.try_into().unwrap()))
}
pub fn page_address(address: u64) -> Result<(), Error> {
    if address == 0 || address & !ADDRESS_MASK != 0 {
        return Err(Error::Bounds);
    }
    Ok(())
}
pub fn pointer(address: u64, next_level: u8, permissions: u64) -> Result<u64, Error> {
    page_address(address)?;
    if next_level > 3 || permissions == 0 || permissions & !RW != 0 {
        return Err(Error::Bounds);
    }
    Ok(address | 1 | ((next_level as u64) << 9) | permissions)
}
pub fn device_entry(root: u64, domain: u16) -> Result<[u64; 4], Error> {
    if domain == 0 {
        return Err(Error::Bounds);
    }
    Ok([pointer(root, 3, RW)? | 2, domain as u64, 0, 0])
}
pub fn invalidate_device(device: u16) -> [u64; 2] {
    [(2u64 << 60) | device as u64, 0]
}
pub fn invalidate_domain(domain: u16) -> [u64; 2] {
    [
        (3u64 << 60) | ((domain as u64) << 32),
        0x7fff_ffff_ffff_f001,
    ]
}
pub fn completion(address: u64, value: u64) -> Result<[u64; 2], Error> {
    if address == 0 || address & 7 != 0 || address >= 1 << 52 {
        return Err(Error::Bounds);
    }
    Ok([(1u64 << 60) | address | 1, value])
}

/// Validated table; only supported hardware definitions are yielded.
pub struct Ivrs<'a> {
    bytes: &'a [u8],
}
impl<'a> Ivrs<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Error> {
        if bytes.len() < 48
            || bytes.len() > 1024 * 1024
            || &bytes[..4] != b"IVRS"
            || crate::word(bytes, 4)? as usize != bytes.len()
            || bytes.iter().fold(0u8, |sum, &b| sum.wrapping_add(b)) != 0
        {
            return Err(Error::Header);
        }
        // Validate every record up front; never silently use a prefix of a
        // damaged table when enabling DMA translation for a requester.
        let mut at = 48;
        while at < bytes.len() {
            let size = half(bytes, at + 2)? as usize;
            if size < 4 {
                return Err(Error::Bounds);
            }
            Span::checked(at, size, bytes.len())?;
            let kind = bytes[at];
            if matches!(kind, 0x10 | 0x11 | 0x40) {
                let header = if kind == 0x10 { 24 } else { 40 };
                if size < header {
                    return Err(Error::Truncated);
                }
                page_address(long(bytes, at + 8)?)?;
                Unit {
                    bytes: &bytes[at..at + size],
                    header,
                }
                .requester(0)?;
            }
            at += size;
        }
        Ok(Self { bytes })
    }
    pub fn units(&self) -> Units<'a> {
        Units {
            bytes: self.bytes,
            at: 48,
        }
    }
}
pub struct Units<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Iterator for Units<'a> {
    type Item = Unit<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        while self.at < self.bytes.len() {
            let at = self.at;
            let size = half(self.bytes, at + 2).ok()? as usize;
            self.at += size;
            let kind = self.bytes[at];
            if matches!(kind, 0x10 | 0x11 | 0x40) {
                return Some(Unit {
                    bytes: &self.bytes[at..at + size],
                    header: if kind == 0x10 { 24 } else { 40 },
                });
            }
        }
        None
    }
}
#[derive(Clone, Copy)]
pub struct Unit<'a> {
    bytes: &'a [u8],
    header: usize,
}
impl Unit<'_> {
    pub fn base(&self) -> u64 {
        long(self.bytes, 8).unwrap()
    }
    pub fn segment(&self) -> u16 {
        half(self.bytes, 16).unwrap()
    }
    pub fn device(&self) -> u16 {
        half(self.bytes, 4).unwrap()
    }
    pub fn capability(&self) -> u16 {
        half(self.bytes, 6).unwrap()
    }
    /// Resolve IVHD select/range/alias entries. Unsupported routing entries
    /// reject this unit rather than guessing a device's DMA requester ID.
    pub fn requester(&self, device: u16) -> Result<Option<u16>, Error> {
        let mut at = self.header;
        let mut range: Option<(u16, Option<u16>)> = None;
        let mut found = None;
        while at < self.bytes.len() {
            let kind = self.bytes[at];
            let size = 4usize << (kind >> 6);
            Span::checked(at, size, self.bytes.len())?;
            let id = half(self.bytes, at + 1)?;
            match kind {
                0 => {}
                1 => {
                    found = Some(device);
                }
                2 | 0x46 => {
                    if id == device {
                        found = Some(device);
                    }
                }
                3 | 0x47 => {
                    if range.replace((id, None)).is_some() {
                        return Err(Error::Section);
                    }
                }
                4 => {
                    let (start, alias) = range.take().ok_or(Error::Section)?;
                    if start > id {
                        return Err(Error::Bounds);
                    }
                    if (start..=id).contains(&device) {
                        found = Some(alias.unwrap_or(device));
                    }
                }
                0x42 => {
                    if id == device {
                        found = Some(half(self.bytes, at + 5)?);
                    }
                }
                0x43 => {
                    if range
                        .replace((id, Some(half(self.bytes, at + 5)?)))
                        .is_some()
                    {
                        return Err(Error::Section);
                    }
                }
                0x48 => {} // IOAPIC/HPET special entry, not a PCI DMA endpoint.
                _ => return Err(Error::Section),
            }
            at += size;
        }
        if range.is_some() {
            return Err(Error::Section);
        }
        Ok(found)
    }
}
