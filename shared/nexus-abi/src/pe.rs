//! Minimal, allocation-free PE32+ (PE/COFF) 64-bit loader and parser.
//!
//! Mirrors `elf.rs` for Windows binaries, enabling NexusOS to inspect,
//! validate, and map PE/COFF 64-bit executables and dynamic libraries.

use core::fmt;

/// The DOS 'MZ' magic number (`0x5A4D`).
pub const DOS_MAGIC: [u8; 2] = *b"MZ";

/// The NT 'PE\0\0' signature (`0x00004550`).
pub const PE_SIGNATURE: [u8; 4] = [b'P', b'E', 0, 0];

/// Machine type for AMD64 (x86-64).
pub const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;

/// Magic number for PE32+ (64-bit optional header).
pub const IMAGE_NT_OPTIONAL_HDR64_MAGIC: u16 = 0x020B;

/// Magic number for standard PE32 (32-bit optional header).
pub const IMAGE_NT_OPTIONAL_HDR32_MAGIC: u16 = 0x010B;

/// Windows GUI subsystem.
pub const IMAGE_SUBSYSTEM_WINDOWS_GUI: u16 = 2;

/// Windows Console subsystem.
pub const IMAGE_SUBSYSTEM_WINDOWS_CUI: u16 = 3;

/// Section characteristics flags.
pub const IMAGE_SCN_CNT_CODE: u32 = 0x0000_0020;
pub const IMAGE_SCN_CNT_INITIALIZED_DATA: u32 = 0x0000_0040;
pub const IMAGE_SCN_CNT_UNINITIALIZED_DATA: u32 = 0x0000_0080;
pub const IMAGE_SCN_MEM_EXECUTE: u32 = 0x2000_0000;
pub const IMAGE_SCN_MEM_READ: u32 = 0x4000_0000;
pub const IMAGE_SCN_MEM_WRITE: u32 = 0x8000_0000;

/// Directory indices in PE optional header.
pub const IMAGE_DIRECTORY_ENTRY_EXPORT: usize = 0;
pub const IMAGE_DIRECTORY_ENTRY_IMPORT: usize = 1;
pub const IMAGE_DIRECTORY_ENTRY_RESOURCE: usize = 2;
pub const IMAGE_DIRECTORY_ENTRY_EXCEPTION: usize = 3;
pub const IMAGE_DIRECTORY_ENTRY_SECURITY: usize = 4;
pub const IMAGE_DIRECTORY_ENTRY_BASERELOC: usize = 5;
pub const IMAGE_DIRECTORY_ENTRY_DEBUG: usize = 6;
pub const IMAGE_DIRECTORY_ENTRY_TLS: usize = 9;
pub const IMAGE_DIRECTORY_ENTRY_LOAD_CONFIG: usize = 10;
pub const IMAGE_DIRECTORY_ENTRY_IAT: usize = 12;

/// Maximum number of sections supported without dynamic allocation.
pub const MAX_PE_SECTIONS: usize = 32;

/// Standard page size.
const PAGE_SIZE: u64 = 4096;

/// Detected executable binary format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryFormat {
    /// 64-bit ELF (System V AMD64 / Linux / NexusOS).
    Elf64,
    /// 32-bit ELF (i386 Linux).
    Elf32,
    /// 64-bit PE/COFF (PE32+ / Windows x64).
    Pe64,
    /// 32-bit PE/COFF (PE32 / Windows x86).
    Pe32,
    /// Unknown or unsupported format.
    Unknown,
}

/// Automatically identify the binary executable format from initial bytes.
pub fn detect_binary_format(bytes: &[u8]) -> BinaryFormat {
    if bytes.len() >= 4 && bytes[0..4] == crate::elf::ELF_MAGIC {
        if bytes.len() >= 5 {
            if bytes[4] == 2 {
                return BinaryFormat::Elf64;
            } else if bytes[4] == 1 {
                return BinaryFormat::Elf32;
            }
        }
        return BinaryFormat::Elf64;
    }

    if bytes.len() >= 0x40 && bytes[0..2] == DOS_MAGIC {
        let lfanew =
            u32::from_le_bytes([bytes[0x3C], bytes[0x3D], bytes[0x3E], bytes[0x3F]]) as usize;
        if lfanew + 24 <= bytes.len() && bytes[lfanew..lfanew + 4] == PE_SIGNATURE {
            let magic_offset = lfanew + 24;
            if magic_offset + 2 <= bytes.len() {
                let opt_magic = u16::from_le_bytes([bytes[magic_offset], bytes[magic_offset + 1]]);
                if opt_magic == IMAGE_NT_OPTIONAL_HDR64_MAGIC {
                    return BinaryFormat::Pe64;
                } else if opt_magic == IMAGE_NT_OPTIONAL_HDR32_MAGIC {
                    return BinaryFormat::Pe32;
                }
            }
            return BinaryFormat::Pe64;
        }
    }

    BinaryFormat::Unknown
}

/// Errors encountered while parsing or loading a PE32+ image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeError {
    /// The file buffer is smaller than a DOS header.
    TooSmall,
    /// The DOS header magic is not `MZ`.
    BadDosMagic,
    /// The `e_lfanew` pointer is invalid or out of bounds.
    InvalidPeOffset,
    /// The NT signature is not `PE\0\0`.
    BadNtSignature,
    /// Target machine is not AMD64 (0x8664).
    NotAmd64,
    /// Optional header magic is not PE32+ (0x020B).
    NotPe32Plus,
    /// Section headers run past the end of the file.
    SectionOutOfBounds,
    /// Image has more sections than MAX_PE_SECTIONS.
    TooManySections,
    /// Physical frame allocation failed.
    OutOfMemory,
    /// Misaligned section virtual address.
    InvalidAlignment,
}

impl fmt::Display for PeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooSmall => f.write_str("file is smaller than a DOS header"),
            Self::BadDosMagic => f.write_str("missing DOS 'MZ' magic"),
            Self::InvalidPeOffset => f.write_str("PE header offset out of bounds"),
            Self::BadNtSignature => f.write_str("missing 'PE\\0\\0' signature"),
            Self::NotAmd64 => f.write_str("target machine is not AMD64 (x86-64)"),
            Self::NotPe32Plus => f.write_str("not a 64-bit PE32+ optional header"),
            Self::SectionOutOfBounds => f.write_str("section raw data exceeds file length"),
            Self::TooManySections => f.write_str("image exceeds maximum supported sections"),
            Self::OutOfMemory => f.write_str("not enough memory to load PE image"),
            Self::InvalidAlignment => f.write_str("invalid section alignment in PE header"),
        }
    }
}

/// Parsed PE32+ image metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeInfo {
    /// Preferred virtual load base address.
    pub image_base: u64,
    /// Relative virtual address (RVA) of entry point.
    pub entry_point_rva: u32,
    /// Total virtual memory required for the mapped image.
    pub size_of_image: u32,
    /// Section alignment requirement in bytes (typically 4096).
    pub section_alignment: u32,
    /// File alignment in bytes (typically 512).
    pub file_alignment: u32,
    /// Number of sections.
    pub number_of_sections: u16,
    /// Subsystem (e.g. GUI or Console).
    pub subsystem: u16,
}

/// Parsed PE section header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeSection {
    /// 8-byte ASCII name (null-padded).
    pub name: [u8; 8],
    /// Size in memory when loaded.
    pub virtual_size: u32,
    /// Relative virtual address where section is mapped.
    pub virtual_address: u32,
    /// Size of raw data in file.
    pub size_of_raw_data: u32,
    /// File offset to raw data.
    pub pointer_to_raw_data: u32,
    /// Characteristics flags (read, write, execute, code, data).
    pub characteristics: u32,
}

impl PeSection {
    /// Whether section contains executable instructions.
    pub fn is_executable(&self) -> bool {
        (self.characteristics & IMAGE_SCN_MEM_EXECUTE) != 0
    }

    /// Whether section is writable.
    pub fn is_writable(&self) -> bool {
        (self.characteristics & IMAGE_SCN_MEM_WRITE) != 0
    }

    /// Whether section is readable.
    pub fn is_readable(&self) -> bool {
        (self.characteristics & IMAGE_SCN_MEM_READ) != 0
    }
}

/// Inspect and extract metadata from a PE32+ binary buffer.
pub fn parse_pe_info(image: &[u8]) -> Result<PeInfo, PeError> {
    if image.len() < 0x40 {
        return Err(PeError::TooSmall);
    }
    if image[0..2] != DOS_MAGIC {
        return Err(PeError::BadDosMagic);
    }

    let pe_offset =
        u32::from_le_bytes([image[0x3C], image[0x3D], image[0x3E], image[0x3F]]) as usize;
    if pe_offset + 24 + 112 > image.len() {
        return Err(PeError::InvalidPeOffset);
    }
    if image[pe_offset..pe_offset + 4] != PE_SIGNATURE {
        return Err(PeError::BadNtSignature);
    }

    let file_header = pe_offset + 4;
    let machine = u16::from_le_bytes([image[file_header], image[file_header + 1]]);
    if machine != IMAGE_FILE_MACHINE_AMD64 {
        return Err(PeError::NotAmd64);
    }

    let number_of_sections = u16::from_le_bytes([image[file_header + 2], image[file_header + 3]]);
    let size_of_optional_header =
        u16::from_le_bytes([image[file_header + 16], image[file_header + 17]]) as usize;

    let opt_header = file_header + 20;
    if opt_header + size_of_optional_header > image.len() {
        return Err(PeError::InvalidPeOffset);
    }

    let opt_magic = u16::from_le_bytes([image[opt_header], image[opt_header + 1]]);
    if opt_magic != IMAGE_NT_OPTIONAL_HDR64_MAGIC {
        return Err(PeError::NotPe32Plus);
    }

    let entry_point_rva = u32::from_le_bytes([
        image[opt_header + 16],
        image[opt_header + 17],
        image[opt_header + 18],
        image[opt_header + 19],
    ]);

    let image_base = u64::from_le_bytes([
        image[opt_header + 24],
        image[opt_header + 25],
        image[opt_header + 26],
        image[opt_header + 27],
        image[opt_header + 28],
        image[opt_header + 29],
        image[opt_header + 30],
        image[opt_header + 31],
    ]);

    let section_alignment = u32::from_le_bytes([
        image[opt_header + 32],
        image[opt_header + 33],
        image[opt_header + 34],
        image[opt_header + 35],
    ]);

    let file_alignment = u32::from_le_bytes([
        image[opt_header + 36],
        image[opt_header + 37],
        image[opt_header + 38],
        image[opt_header + 39],
    ]);

    let size_of_image = u32::from_le_bytes([
        image[opt_header + 56],
        image[opt_header + 57],
        image[opt_header + 58],
        image[opt_header + 59],
    ]);

    let subsystem = u16::from_le_bytes([image[opt_header + 68], image[opt_header + 69]]);

    Ok(PeInfo {
        image_base,
        entry_point_rva,
        size_of_image,
        section_alignment,
        file_alignment,
        number_of_sections,
        subsystem,
    })
}

/// Enumerate sections from a PE32+ image.
pub fn for_each_section<F>(image: &[u8], mut callback: F) -> Result<(), PeError>
where
    F: FnMut(&PeSection) -> Result<(), PeError>,
{
    if image.len() < 0x40 {
        return Err(PeError::TooSmall);
    }
    let pe_offset =
        u32::from_le_bytes([image[0x3C], image[0x3D], image[0x3E], image[0x3F]]) as usize;
    if pe_offset + 24 > image.len() {
        return Err(PeError::InvalidPeOffset);
    }

    let file_header = pe_offset + 4;
    let number_of_sections =
        u16::from_le_bytes([image[file_header + 2], image[file_header + 3]]) as usize;
    if number_of_sections > MAX_PE_SECTIONS {
        return Err(PeError::TooManySections);
    }

    let size_of_optional_header =
        u16::from_le_bytes([image[file_header + 16], image[file_header + 17]]) as usize;
    let section_table_offset = file_header + 20 + size_of_optional_header;

    for i in 0..number_of_sections {
        let sec_offset = section_table_offset + i * 40;
        if sec_offset + 40 > image.len() {
            return Err(PeError::SectionOutOfBounds);
        }

        let mut name = [0u8; 8];
        name.copy_from_slice(&image[sec_offset..sec_offset + 8]);

        let virtual_size = u32::from_le_bytes([
            image[sec_offset + 8],
            image[sec_offset + 9],
            image[sec_offset + 10],
            image[sec_offset + 11],
        ]);
        let virtual_address = u32::from_le_bytes([
            image[sec_offset + 12],
            image[sec_offset + 13],
            image[sec_offset + 14],
            image[sec_offset + 15],
        ]);
        let size_of_raw_data = u32::from_le_bytes([
            image[sec_offset + 16],
            image[sec_offset + 17],
            image[sec_offset + 18],
            image[sec_offset + 19],
        ]);
        let pointer_to_raw_data = u32::from_le_bytes([
            image[sec_offset + 20],
            image[sec_offset + 21],
            image[sec_offset + 22],
            image[sec_offset + 23],
        ]);
        let characteristics = u32::from_le_bytes([
            image[sec_offset + 36],
            image[sec_offset + 37],
            image[sec_offset + 38],
            image[sec_offset + 39],
        ]);

        let section = PeSection {
            name,
            virtual_size,
            virtual_address,
            size_of_raw_data,
            pointer_to_raw_data,
            characteristics,
        };

        callback(&section)?;
    }

    Ok(())
}

/// Map a PE32+ image into a freshly allocated contiguous span.
///
/// Returns the physical base, virtual base, image size, and entry point virtual address.
///
/// # Safety
///
/// `allocate` must provide valid physical frames, and `to_virtual` must map those
/// frames to valid kernel virtual addresses.
pub unsafe fn load_pe<A, V>(
    image: &[u8],
    mut allocate: A,
    to_virtual: V,
) -> Result<(u64, u64, u64, u64), PeError>
where
    A: FnMut(usize) -> Option<u64>,
    V: Fn(u64) -> u64,
{
    let info = parse_pe_info(image)?;
    let pages = (info.size_of_image as u64).div_ceil(PAGE_SIZE) as usize;
    let phys_base = allocate(pages).ok_or(PeError::OutOfMemory)?;
    let virt_ptr = to_virtual(phys_base) as usize as *mut u8;

    // Zero out entire allocated region (BSS + padding).
    unsafe {
        core::ptr::write_bytes(virt_ptr, 0, (pages as u64 * PAGE_SIZE) as usize);
    }

    // Copy PE headers (up to SizeOfHeaders).
    let pe_offset =
        u32::from_le_bytes([image[0x3C], image[0x3D], image[0x3E], image[0x3F]]) as usize;
    let opt_header = pe_offset + 24;
    let size_of_headers = u32::from_le_bytes([
        image[opt_header + 60],
        image[opt_header + 61],
        image[opt_header + 62],
        image[opt_header + 63],
    ]) as usize;

    let headers_to_copy = size_of_headers.min(image.len());
    unsafe {
        core::ptr::copy_nonoverlapping(image.as_ptr(), virt_ptr, headers_to_copy);
    }

    // Map each section to its VirtualAddress RVA.
    for_each_section(image, |sec| {
        if sec.virtual_size == 0 && sec.size_of_raw_data == 0 {
            return Ok(());
        }
        let dest_offset = sec.virtual_address as usize;
        let copy_size = (sec.size_of_raw_data as usize).min(sec.virtual_size as usize);
        if copy_size > 0 {
            let src_offset = sec.pointer_to_raw_data as usize;
            if src_offset + copy_size > image.len() {
                return Err(PeError::SectionOutOfBounds);
            }
            unsafe {
                core::ptr::copy_nonoverlapping(
                    image.as_ptr().add(src_offset),
                    virt_ptr.add(dest_offset),
                    copy_size,
                );
            }
        }
        Ok(())
    })?;

    let entry_point = info.image_base + info.entry_point_rva as u64;
    Ok((
        phys_base,
        info.image_base,
        info.size_of_image as u64,
        entry_point,
    ))
}

/// Load and map a PE32+ image producing a standard `LoadedImage` for kernel page mapping.
///
/// # Safety
///
/// `allocate` and `to_virtual` must uphold the physical memory access contract.
pub unsafe fn load_pe_at<A, V>(
    image: &[u8],
    bias: u64,
    mut allocate: A,
    to_virtual: V,
) -> Result<crate::elf::LoadedImage, PeError>
where
    A: FnMut(usize) -> Option<u64>,
    V: Fn(u64) -> u64,
{
    let info = parse_pe_info(image)?;
    let pages = (info.size_of_image as u64).div_ceil(PAGE_SIZE) as usize;
    let phys_base = allocate(pages).ok_or(PeError::OutOfMemory)?;
    let virt_ptr = to_virtual(phys_base) as usize as *mut u8;

    unsafe {
        core::ptr::write_bytes(virt_ptr, 0, pages * PAGE_SIZE as usize);
    }

    let virt_base = if bias != 0 { bias } else { info.image_base };
    let entry_point = virt_base + info.entry_point_rva as u64;

    let pe_offset =
        u32::from_le_bytes([image[0x3C], image[0x3D], image[0x3E], image[0x3F]]) as usize;
    let opt_header = pe_offset + 24;
    let size_of_headers = u32::from_le_bytes([
        image[opt_header + 60],
        image[opt_header + 61],
        image[opt_header + 62],
        image[opt_header + 63],
    ]) as usize;

    let headers_to_copy = size_of_headers.min(image.len());
    unsafe {
        core::ptr::copy_nonoverlapping(image.as_ptr(), virt_ptr, headers_to_copy);
    }

    let mut segments = [crate::elf::LoadedSegment {
        virt_start: 0,
        phys_start: 0,
        size: 0,
        flags: crate::elf::SegmentFlags {
            readable: false,
            writable: false,
            executable: false,
        },
    }; crate::elf::MAX_SEGMENTS];
    let mut segment_count = 0usize;

    for_each_section(image, |sec| {
        if sec.virtual_size == 0 && sec.size_of_raw_data == 0 {
            return Ok(());
        }
        let dest_offset = sec.virtual_address as usize;
        let copy_size = (sec.size_of_raw_data as usize).min(sec.virtual_size as usize);
        if copy_size > 0 {
            let src_offset = sec.pointer_to_raw_data as usize;
            if src_offset + copy_size > image.len() {
                return Err(PeError::SectionOutOfBounds);
            }
            unsafe {
                core::ptr::copy_nonoverlapping(
                    image.as_ptr().add(src_offset),
                    virt_ptr.add(dest_offset),
                    copy_size,
                );
            }
        }

        if segment_count < crate::elf::MAX_SEGMENTS {
            let aligned_vaddr = virt_base + sec.virtual_address as u64;
            let aligned_size = (sec.virtual_size as u64).div_ceil(PAGE_SIZE) * PAGE_SIZE;
            segments[segment_count] = crate::elf::LoadedSegment {
                virt_start: aligned_vaddr,
                phys_start: phys_base + sec.virtual_address as u64,
                size: aligned_size.max(PAGE_SIZE),
                flags: crate::elf::SegmentFlags {
                    readable: sec.is_readable(),
                    writable: sec.is_writable(),
                    executable: sec.is_executable(),
                },
            };
            segment_count += 1;
        }
        Ok(())
    })?;

    Ok(crate::elf::LoadedImage {
        entry_point,
        virt_base,
        phys_base,
        image_size: pages as u64 * PAGE_SIZE,
        segments,
        segment_count,
        program_headers: 0,
        program_header_size: 0,
        program_header_count: 0,
        bias,
        relocatable: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_dummy_pe() -> [u8; 1024] {
        let mut buf = [0u8; 1024];
        // DOS Header
        buf[0..2].copy_from_slice(&DOS_MAGIC);
        buf[0x3C..0x40].copy_from_slice(&64u32.to_le_bytes()); // e_lfanew = 64

        // PE Signature
        let pe = 64;
        buf[pe..pe + 4].copy_from_slice(&PE_SIGNATURE);

        // File Header
        buf[pe + 4..pe + 6].copy_from_slice(&IMAGE_FILE_MACHINE_AMD64.to_le_bytes());
        buf[pe + 6..pe + 8].copy_from_slice(&1u16.to_le_bytes()); // 1 section
        buf[pe + 20..pe + 22].copy_from_slice(&120u16.to_le_bytes()); // size of optional header

        // Optional Header 64
        let opt = pe + 24;
        buf[opt..opt + 2].copy_from_slice(&IMAGE_NT_OPTIONAL_HDR64_MAGIC.to_le_bytes());
        buf[opt + 16..opt + 20].copy_from_slice(&0x1000u32.to_le_bytes()); // EntryPoint RVA = 0x1000
        buf[opt + 24..opt + 32].copy_from_slice(&0x0000000140000000u64.to_le_bytes()); // ImageBase
        buf[opt + 32..opt + 36].copy_from_slice(&4096u32.to_le_bytes()); // SectionAlignment
        buf[opt + 36..opt + 40].copy_from_slice(&512u32.to_le_bytes()); // FileAlignment
        buf[opt + 56..opt + 60].copy_from_slice(&8192u32.to_le_bytes()); // SizeOfImage
        buf[opt + 60..opt + 64].copy_from_slice(&512u32.to_le_bytes()); // SizeOfHeaders
        buf[opt + 68..opt + 70].copy_from_slice(&IMAGE_SUBSYSTEM_WINDOWS_CUI.to_le_bytes());

        // Section Header
        let sec = opt + 120;
        buf[sec..sec + 5].copy_from_slice(b".text");
        buf[sec + 8..sec + 12].copy_from_slice(&0x1000u32.to_le_bytes()); // VirtualSize
        buf[sec + 12..sec + 16].copy_from_slice(&0x1000u32.to_le_bytes()); // VirtualAddress
        buf[sec + 16..sec + 20].copy_from_slice(&0x200u32.to_le_bytes()); // SizeOfRawData
        buf[sec + 20..sec + 24].copy_from_slice(&0x200u32.to_le_bytes()); // PointerToRawData
        buf[sec + 36..sec + 40].copy_from_slice(
            &(IMAGE_SCN_CNT_CODE | IMAGE_SCN_MEM_EXECUTE | IMAGE_SCN_MEM_READ).to_le_bytes(),
        );

        buf
    }

    #[test]
    fn detects_pe64_format() {
        let pe = create_dummy_pe();
        assert_eq!(detect_binary_format(&pe), BinaryFormat::Pe64);
    }

    #[test]
    fn parses_pe_info_successfully() {
        let pe = create_dummy_pe();
        let info = parse_pe_info(&pe).expect("valid PE");
        assert_eq!(info.image_base, 0x0000000140000000);
        assert_eq!(info.entry_point_rva, 0x1000);
        assert_eq!(info.size_of_image, 8192);
        assert_eq!(info.subsystem, IMAGE_SUBSYSTEM_WINDOWS_CUI);
    }

    #[test]
    fn enumerates_sections_correctly() {
        let pe = create_dummy_pe();
        let mut count = 0;
        for_each_section(&pe, |sec| {
            count += 1;
            assert_eq!(&sec.name[0..5], b".text");
            assert!(sec.is_executable());
            assert!(sec.is_readable());
            assert!(!sec.is_writable());
            Ok(())
        })
        .expect("valid sections");
        assert_eq!(count, 1);
    }

    #[test]
    fn loads_pe_into_loaded_image() {
        let pe = create_dummy_pe();
        let mut storage = [0u8; 8192];
        let storage_ptr = storage.as_mut_ptr() as u64;

        let loaded = unsafe { load_pe_at(&pe, 0, |_pages| Some(storage_ptr), |phys| phys) }
            .expect("loaded image");

        assert_eq!(loaded.virt_base, 0x0000000140000000);
        assert_eq!(loaded.entry_point, 0x0000000140001000);
        assert_eq!(loaded.image_size, 8192);
        assert_eq!(loaded.segment_count, 1);
        assert!(loaded.segments[0].flags.executable);
        assert!(loaded.segments[0].flags.readable);
    }
}
