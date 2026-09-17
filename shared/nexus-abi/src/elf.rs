//! A minimal, allocation-free ELF64 loader.
//!
//! Used twice, which is why it is here rather than in either of the crates that
//! use it: the bootloader loads the kernel with it, and the kernel loads user
//! programs with it. Both load a non-PIE static executable linked at a fixed
//! address, so neither needs relocation processing or dynamic symbols -- the
//! loader reserves one contiguous physical span, copies every `PT_LOAD` segment
//! into it, and reports where it landed.
//!
//! The one thing that differs between the two is how physical memory is
//! reached: the bootloader runs with an identity map, the kernel through its
//! direct map. That is a parameter rather than an assumption.
//!
//! All multi-byte fields are read byte-wise. The file buffer comes straight
//! from firmware or a disk and carries no alignment guarantee, so a
//! `read_unaligned` of a `repr(C)` header would be the only other correct
//! option and this is clearer.

use core::fmt;

/// `EI_NIDENT`: the size of the identification prefix of an ELF header.
const EI_NIDENT: usize = 16;
/// Size of `Elf64_Ehdr`.
const EHDR_SIZE: usize = 64;
/// Size of `Elf64_Phdr`.
const PHDR_SIZE: usize = 56;

pub(crate) const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];
/// `ELFCLASS64`.
const ELFCLASS64: u8 = 2;
/// `ELFDATA2LSB`.
const ELFDATA2LSB: u8 = 1;
/// `ET_EXEC`: a non-relocatable executable.
const ET_EXEC: u16 = 2;
/// `ET_DYN`: an image whose addresses are all relative to wherever it is put.
///
/// A shared object is one, and so is a position-independent executable. The
/// difference between the two is not in the header -- it is whether anything
/// else loads it -- so this loader treats them the same and the caller decides
/// where each one goes.
const ET_DYN: u16 = 3;
/// `EM_X86_64`.
const EM_X86_64: u16 = 0x3E;
/// `PT_LOAD`.
pub(crate) const PT_LOAD: u32 = 1;
/// The path of the program that has to load this one before it can run.
const PT_INTERP: u32 = 3;

pub(crate) const PF_X: u32 = 1;
pub(crate) const PF_W: u32 = 2;
pub(crate) const PF_R: u32 = 4;

/// Why a kernel image could not be loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// The file is shorter than an ELF header.
    TooSmall,
    /// The first four bytes are not `\x7fELF`.
    BadMagic,
    /// Not a little-endian 64-bit object.
    NotElf64,
    /// Not a statically linked x86-64 executable.
    WrongType,
    /// A program header table entry lies outside the file.
    PhdrOutOfBounds,
    /// A segment's file contents lie outside the file.
    SegmentOutOfBounds,
    /// The image contains no `PT_LOAD` segment.
    NoLoadableSegments,
    /// The loader could not obtain physical memory for the image.
    OutOfMemory,
    /// A segment claims `p_filesz > p_memsz`, which is malformed.
    InconsistentSegment,
    /// An `ET_DYN` image was asked for at no bias, or an `ET_EXEC` one at some.
    ///
    /// Two different mistakes with one name because they are the same mistake
    /// seen from either side: an image that carries its own addresses cannot be
    /// moved, and one that does not carry any cannot be left where it is.
    WrongBias,
    /// `PT_INTERP` names a path that is not inside the file, or is not text.
    BadInterpreter,
}

impl fmt::Display for ElfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::TooSmall => "file is smaller than an ELF64 header",
            Self::BadMagic => "missing \\x7fELF signature",
            Self::NotElf64 => "not a little-endian ELF64 object",
            Self::WrongType => "not an x86-64 ET_EXEC or ET_DYN image",
            Self::PhdrOutOfBounds => "program header table extends past end of file",
            Self::SegmentOutOfBounds => "segment contents extend past end of file",
            Self::NoLoadableSegments => "no PT_LOAD segments",
            Self::OutOfMemory => "could not allocate physical memory for the image",
            Self::InconsistentSegment => "segment has p_filesz greater than p_memsz",
            Self::WrongBias => "an ET_EXEC image cannot be moved and an ET_DYN one must be",
            Self::BadInterpreter => "PT_INTERP does not name a readable path inside the file",
        };
        f.write_str(text)
    }
}

/// Access permissions requested by one loaded segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentFlags {
    pub readable: bool,
    pub writable: bool,
    pub executable: bool,
}

/// One `PT_LOAD` segment, described in terms of where it ended up.
#[derive(Debug, Clone, Copy)]
pub struct LoadedSegment {
    /// Virtual address the segment is linked at, rounded down to a page.
    pub virt_start: u64,
    /// Physical address it was copied to, rounded down to a page.
    pub phys_start: u64,
    /// Length in bytes, rounded up to a page.
    pub size: u64,
    /// Permissions the page tables should grant.
    pub flags: SegmentFlags,
}

/// The maximum number of `PT_LOAD` segments the loader will record.
///
/// A linker script that produces more than this many loadable segments would
/// mean the kernel's layout changed substantially, so overflowing is a bug
/// worth reporting rather than silently tolerating.
pub const MAX_SEGMENTS: usize = 16;

/// Result of loading a kernel image into physical memory.
pub struct LoadedImage {
    /// Virtual entry point taken from `e_entry`.
    pub entry_point: u64,
    /// Lowest virtual address of the image, page aligned.
    pub virt_base: u64,
    /// Physical address the image span starts at, page aligned.
    pub phys_base: u64,
    /// Total span in bytes, page aligned.
    pub image_size: u64,
    /// The `PT_LOAD` segments, in file order.
    pub segments: [LoadedSegment; MAX_SEGMENTS],
    /// How many entries of `segments` are valid.
    pub segment_count: usize,
    /// Where the program header table ended up in the loaded image, as a
    /// virtual address, or zero if no loadable segment covers it.
    ///
    /// A System V program is told this in its auxiliary vector as `AT_PHDR`,
    /// and a C library reads it to find its own `PT_TLS` and `PT_GNU_RELRO`
    /// before `main` runs. It is not the file offset: the table has to be
    /// *inside* a segment that was loaded, and where it landed is what the
    /// program can actually read.
    pub program_headers: u64,
    /// Bytes in one program header, from `e_phentsize`.
    pub program_header_size: u16,
    /// How many there are, from `e_phnum`.
    pub program_header_count: u16,
    /// What was added to every address in the file to get where it now is.
    ///
    /// Zero for an `ET_EXEC` image, which is linked at an address and has to be
    /// there. For an `ET_DYN` one this is the number a dynamic linker calls the
    /// load base, and the number the auxiliary vector carries as `AT_BASE` when
    /// the image is an interpreter. A program that has to relocate itself
    /// cannot do it without this, and cannot work it out from anything else it
    /// is given.
    pub bias: u64,
    /// Whether the image was `ET_DYN`.
    pub relocatable: bool,
}

/// Read a little-endian `u16` at `offset`, or `None` if it would run past the end.
fn read_u16(buf: &[u8], offset: usize) -> Option<u16> {
    let bytes = buf.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

/// Read a little-endian `u32` at `offset`.
fn read_u32(buf: &[u8], offset: usize) -> Option<u32> {
    let bytes = buf.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Read a little-endian `u64` at `offset`.
fn read_u64(buf: &[u8], offset: usize) -> Option<u64> {
    let bytes = buf.get(offset..offset + 8)?;
    Some(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

pub(crate) const PAGE_SIZE: u64 = 4096;

pub(crate) const fn align_down(value: u64) -> u64 {
    value & !(PAGE_SIZE - 1)
}

pub(crate) const fn align_up(value: u64) -> u64 {
    (value + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

/// What kind of image this is: one that names its own addresses, or one that
/// has to be told where it is going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `ET_EXEC`. Linked at an address, and only correct there.
    Fixed,
    /// `ET_DYN`. A shared object or a position-independent executable.
    Movable,
}

/// What the file says it is, without loading any of it.
///
/// The caller needs this *before* it can choose a bias, and choosing a bias is
/// the first decision in loading a dynamic program -- so it is a separate pass
/// over the header rather than something the load result reports too late.
///
/// # Errors
///
/// The same header checks [`load`] makes.
pub fn kind(image: &[u8]) -> Result<Kind, ElfError> {
    let (_, _, _, _, e_type) = parse_header(image)?;
    Ok(if e_type == ET_DYN {
        Kind::Movable
    } else {
        Kind::Fixed
    })
}

/// Validate the ELF header and return `(entry, phoff, phentsize, phnum, type)`.
fn parse_header(image: &[u8]) -> Result<(u64, u64, u16, u16, u16), ElfError> {
    if image.len() < EHDR_SIZE {
        return Err(ElfError::TooSmall);
    }
    if image[0..4] != ELF_MAGIC {
        return Err(ElfError::BadMagic);
    }
    if image[4] != ELFCLASS64 || image[5] != ELFDATA2LSB {
        return Err(ElfError::NotElf64);
    }

    let e_type = read_u16(image, EI_NIDENT).ok_or(ElfError::TooSmall)?;
    let e_machine = read_u16(image, EI_NIDENT + 2).ok_or(ElfError::TooSmall)?;
    if (e_type != ET_EXEC && e_type != ET_DYN) || e_machine != EM_X86_64 {
        return Err(ElfError::WrongType);
    }

    let e_entry = read_u64(image, 24).ok_or(ElfError::TooSmall)?;
    let e_phoff = read_u64(image, 32).ok_or(ElfError::TooSmall)?;
    let e_phentsize = read_u16(image, 54).ok_or(ElfError::TooSmall)?;
    let e_phnum = read_u16(image, 56).ok_or(ElfError::TooSmall)?;

    Ok((e_entry, e_phoff, e_phentsize, e_phnum, e_type))
}

/// A raw `PT_LOAD` program header.
#[derive(Clone, Copy)]
struct ProgramHeader {
    flags: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
}

/// Iterate the program header table, invoking `f` for each `PT_LOAD` entry.
fn for_each_load_segment<F>(
    image: &[u8],
    phoff: u64,
    phentsize: u16,
    phnum: u16,
    mut f: F,
) -> Result<(), ElfError>
where
    F: FnMut(ProgramHeader) -> Result<(), ElfError>,
{
    // A conforming linker emits 56-byte entries, but honour a larger stride so
    // that a toolchain with extra header padding still loads.
    let stride = core::cmp::max(phentsize as usize, PHDR_SIZE);

    for index in 0..phnum as usize {
        let base = (phoff as usize)
            .checked_add(index * stride)
            .ok_or(ElfError::PhdrOutOfBounds)?;
        if base + PHDR_SIZE > image.len() {
            return Err(ElfError::PhdrOutOfBounds);
        }

        let p_type = read_u32(image, base).ok_or(ElfError::PhdrOutOfBounds)?;
        if p_type != PT_LOAD {
            continue;
        }

        let header = ProgramHeader {
            flags: read_u32(image, base + 4).ok_or(ElfError::PhdrOutOfBounds)?,
            offset: read_u64(image, base + 8).ok_or(ElfError::PhdrOutOfBounds)?,
            vaddr: read_u64(image, base + 16).ok_or(ElfError::PhdrOutOfBounds)?,
            filesz: read_u64(image, base + 32).ok_or(ElfError::PhdrOutOfBounds)?,
            memsz: read_u64(image, base + 40).ok_or(ElfError::PhdrOutOfBounds)?,
        };

        if header.filesz > header.memsz {
            return Err(ElfError::InconsistentSegment);
        }
        f(header)?;
    }
    Ok(())
}

/// Load `image` into physical memory.
///
/// `allocate` is called at most once, with a page count, and must return the
/// physical base address of that many contiguous, page-aligned pages, or `None`
/// if it cannot satisfy the request.
///
/// `to_virtual` says how to reach a physical address for writing. The
/// bootloader runs identity mapped and passes the identity; the kernel passes
/// its direct map. Making it a parameter is what lets one loader serve both,
/// and stops either of them assuming the other's arrangement.
///
/// # Safety
///
/// The physical range returned by `allocate` must be writable through
/// `to_virtual` for the duration of this call; the loader writes the image
/// there directly.
pub unsafe fn load<A, V>(image: &[u8], allocate: A, to_virtual: V) -> Result<LoadedImage, ElfError>
where
    A: FnMut(usize) -> Option<u64>,
    V: Fn(u64) -> u64,
{
    // SAFETY: forwarded to the caller's promise.
    unsafe { load_at(image, 0, allocate, to_virtual) }
}

/// Load `image`, adding `bias` to every address in it.
///
/// This is [`load`] with the one thing a dynamic program needs: somewhere to
/// be put. An `ET_EXEC` image names the addresses it must be loaded at and can
/// only be loaded at `bias` of zero; an `ET_DYN` image names none, is correct
/// anywhere, and must be given a nonzero one -- because a bias of zero would
/// put it over the null page, where the whole point of the null page is that
/// nothing is.
///
/// What this does *not* do is relocate. An `ET_DYN` image is full of addresses
/// that still have to have `bias` added to them, and the list of where they are
/// is in the image's own `DT_RELA`. Applying it is the dynamic linker's work,
/// in user space, after the kernel has entered it -- and for the dynamic linker
/// itself it is work it does on its own image, which is why a linker is built
/// to run before it has been relocated. A kernel that did the relocation here
/// would be a kernel that had to understand symbol versioning.
///
/// # Errors
///
/// [`ElfError::WrongBias`] if the kind of image and the bias do not agree.
///
/// # Safety
///
/// As [`load`].
pub unsafe fn load_at<A, V>(
    image: &[u8],
    bias: u64,
    mut allocate: A,
    to_virtual: V,
) -> Result<LoadedImage, ElfError>
where
    A: FnMut(usize) -> Option<u64>,
    V: Fn(u64) -> u64,
{
    let (file_entry, phoff, phentsize, phnum, e_type) = parse_header(image)?;
    let relocatable = e_type == ET_DYN;
    if relocatable == (bias == 0) || !bias.is_multiple_of(PAGE_SIZE) {
        return Err(ElfError::WrongBias);
    }
    let entry_point = file_entry + bias;

    // First pass: find the virtual span the image occupies.
    let mut min_vaddr = u64::MAX;
    let mut max_vaddr_end = 0u64;
    let mut found_any = false;

    for_each_load_segment(image, phoff, phentsize, phnum, |header| {
        if header.memsz == 0 {
            return Ok(());
        }
        found_any = true;
        min_vaddr = min_vaddr.min(align_down(header.vaddr + bias));
        max_vaddr_end = max_vaddr_end.max(align_up(header.vaddr + bias + header.memsz));
        Ok(())
    })?;

    if !found_any {
        return Err(ElfError::NoLoadableSegments);
    }

    let virt_base = min_vaddr;
    let image_size = max_vaddr_end - virt_base;
    let page_count = (image_size / PAGE_SIZE) as usize;

    let phys_base = allocate(page_count).ok_or(ElfError::OutOfMemory)?;

    // Zero the whole span up front so that `.bss` and any inter-segment padding
    // start clean; the second pass then only has to copy file-backed bytes.
    //
    // SAFETY: the caller guarantees `phys_base .. phys_base + image_size` is
    // allocated and writable through `to_virtual`.
    unsafe {
        core::ptr::write_bytes(to_virtual(phys_base) as *mut u8, 0, image_size as usize);
    }

    let mut segments = [LoadedSegment {
        virt_start: 0,
        phys_start: 0,
        size: 0,
        flags: SegmentFlags {
            readable: false,
            writable: false,
            executable: false,
        },
    }; MAX_SEGMENTS];
    let mut segment_count = 0usize;

    for_each_load_segment(image, phoff, phentsize, phnum, |header| {
        if header.memsz == 0 {
            return Ok(());
        }

        let file_start = header.offset as usize;
        let file_end = file_start
            .checked_add(header.filesz as usize)
            .ok_or(ElfError::SegmentOutOfBounds)?;
        let source = image
            .get(file_start..file_end)
            .ok_or(ElfError::SegmentOutOfBounds)?;

        let destination = phys_base + (header.vaddr + bias - virt_base);
        // SAFETY: `destination .. destination + filesz` lies inside the span we
        // allocated and zeroed above, because `vaddr + memsz <= max_vaddr_end`
        // and `filesz <= memsz`.
        unsafe {
            core::ptr::copy_nonoverlapping(
                source.as_ptr(),
                to_virtual(destination) as *mut u8,
                source.len(),
            );
        }

        if segment_count < MAX_SEGMENTS {
            let start = align_down(header.vaddr + bias);
            let end = align_up(header.vaddr + bias + header.memsz);
            segments[segment_count] = LoadedSegment {
                virt_start: start,
                phys_start: phys_base + (start - virt_base),
                size: end - start,
                flags: SegmentFlags {
                    readable: header.flags & PF_R != 0,
                    writable: header.flags & PF_W != 0,
                    executable: header.flags & PF_X != 0,
                },
            };
            segment_count += 1;
        }
        Ok(())
    })?;

    // Where the program header table can be read from, once the image is in
    // place. Found by asking which loadable segment covers the file offset the
    // header gave, rather than assuming the first one starts at offset zero --
    // which is true of every static executable seen so far and is not a rule.
    let mut program_headers = 0u64;
    for_each_load_segment(image, phoff, phentsize, phnum, |header| {
        if program_headers != 0 || header.filesz == 0 {
            return Ok(());
        }
        let start = header.offset;
        let end = start + header.filesz;
        let table_end = phoff + u64::from(phentsize) * u64::from(phnum);
        if phoff >= start && table_end <= end {
            program_headers = header.vaddr + bias + (phoff - start);
        }
        Ok(())
    })?;

    Ok(LoadedImage {
        entry_point,
        virt_base,
        phys_base,
        image_size,
        segments,
        segment_count,
        program_headers,
        program_header_size: phentsize,
        program_header_count: phnum,
        bias,
        relocatable,
    })
}

/// A thirty-two bit image, for a program built for i386.
///
/// A separate parser rather than a parameter on the one above, and the reason
/// is that almost nothing is shared. `Elf32_Ehdr` is fifty-two bytes where
/// `Elf64_Ehdr` is sixty-four; `Elf32_Phdr` is thirty-two where the other is
/// fifty-six; and the program header's fields are in a *different order* --
/// `p_flags` comes after `p_memsz` in the thirty-two bit form and immediately
/// after `p_type` in the sixty-four bit one. A loader that read one layout with
/// the other's offsets would find a segment's permissions where its size should
/// be, which is not an error it would report.
pub mod elf32 {
    use super::{
        align_down, align_up, ElfError, LoadedImage, LoadedSegment, SegmentFlags, ELF_MAGIC,
        MAX_SEGMENTS, PAGE_SIZE, PF_R, PF_W, PF_X, PT_LOAD,
    };

    /// `ELFCLASS32`.
    const ELFCLASS32: u8 = 1;
    /// `ELFDATA2LSB`.
    const ELFDATA2LSB: u8 = 1;
    /// `ET_EXEC`.
    const ET_EXEC: u16 = 2;
    /// `EM_386`.
    const EM_386: u16 = 3;
    /// Size of `Elf32_Ehdr`, and of one `Elf32_Phdr`.
    const EHDR_SIZE: usize = 52;
    const PHDR_SIZE: usize = 32;

    fn read_u16(buf: &[u8], offset: usize) -> Option<u16> {
        let bytes = buf.get(offset..offset + 2)?;
        Some(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_u32(buf: &[u8], offset: usize) -> Option<u32> {
        let bytes = buf.get(offset..offset + 4)?;
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Whether this is a thirty-two bit x86 image.
    ///
    /// Asked before anything else, because it decides which of the two loaders
    /// a file goes to -- and the answer is in the fifth byte, which is the one
    /// field both layouts agree on.
    #[must_use]
    pub fn is_thirty_two_bit(image: &[u8]) -> bool {
        image.len() > 18
            && image[0..4] == ELF_MAGIC
            && image[4] == ELFCLASS32
            && read_u16(image, 18) == Some(EM_386)
    }

    /// Validate the header and return `(entry, phoff, phentsize, phnum)`.
    fn parse_header(image: &[u8]) -> Result<(u32, u32, u16, u16), ElfError> {
        if image.len() < EHDR_SIZE {
            return Err(ElfError::TooSmall);
        }
        if image[0..4] != ELF_MAGIC {
            return Err(ElfError::BadMagic);
        }
        if image[4] != ELFCLASS32 || image[5] != ELFDATA2LSB {
            return Err(ElfError::NotElf64);
        }
        let e_type = read_u16(image, 16).ok_or(ElfError::TooSmall)?;
        let e_machine = read_u16(image, 18).ok_or(ElfError::TooSmall)?;
        // `ET_DYN` is not accepted: a thirty-two bit program that needs an
        // interpreter needs a thirty-two bit interpreter, and there is none on
        // this machine. Refusing here is better than loading one and entering
        // an entry point whose relocations nobody applied.
        if e_type != ET_EXEC || e_machine != EM_386 {
            return Err(ElfError::WrongType);
        }
        Ok((
            read_u32(image, 24).ok_or(ElfError::TooSmall)?,
            read_u32(image, 28).ok_or(ElfError::TooSmall)?,
            read_u16(image, 42).ok_or(ElfError::TooSmall)?,
            read_u16(image, 44).ok_or(ElfError::TooSmall)?,
        ))
    }

    /// One `PT_LOAD` segment of a thirty-two bit image.
    #[derive(Clone, Copy)]
    struct ProgramHeader {
        flags: u32,
        offset: u32,
        vaddr: u32,
        filesz: u32,
        memsz: u32,
    }

    fn for_each_load_segment<F>(
        image: &[u8],
        phoff: u32,
        phentsize: u16,
        phnum: u16,
        mut f: F,
    ) -> Result<(), ElfError>
    where
        F: FnMut(ProgramHeader) -> Result<(), ElfError>,
    {
        let stride = core::cmp::max(phentsize as usize, PHDR_SIZE);
        for index in 0..phnum as usize {
            let base = (phoff as usize)
                .checked_add(index * stride)
                .ok_or(ElfError::PhdrOutOfBounds)?;
            if base + PHDR_SIZE > image.len() {
                return Err(ElfError::PhdrOutOfBounds);
            }
            if read_u32(image, base).ok_or(ElfError::PhdrOutOfBounds)? != PT_LOAD {
                continue;
            }
            // The order here is the thirty-two bit one: offset, vaddr, paddr,
            // filesz, memsz, *then* flags. See the note at the top.
            let header = ProgramHeader {
                offset: read_u32(image, base + 4).ok_or(ElfError::PhdrOutOfBounds)?,
                vaddr: read_u32(image, base + 8).ok_or(ElfError::PhdrOutOfBounds)?,
                filesz: read_u32(image, base + 16).ok_or(ElfError::PhdrOutOfBounds)?,
                memsz: read_u32(image, base + 20).ok_or(ElfError::PhdrOutOfBounds)?,
                flags: read_u32(image, base + 24).ok_or(ElfError::PhdrOutOfBounds)?,
            };
            if header.filesz > header.memsz {
                return Err(ElfError::InconsistentSegment);
            }
            f(header)?;
        }
        Ok(())
    }

    /// Load a thirty-two bit image into physical memory.
    ///
    /// Reports its result as the same [`LoadedImage`] the sixty-four bit loader
    /// does, with every address widened -- because every address in it really is
    /// below four gigabytes, and a caller mapping it does not have to care which
    /// loader produced it.
    ///
    /// # Safety
    ///
    /// As [`super::load`].
    pub unsafe fn load<A, V>(
        image: &[u8],
        mut allocate: A,
        to_virtual: V,
    ) -> Result<LoadedImage, ElfError>
    where
        A: FnMut(usize) -> Option<u64>,
        V: Fn(u64) -> u64,
    {
        let (entry_point, phoff, phentsize, phnum) = parse_header(image)?;

        let mut min_vaddr = u64::MAX;
        let mut max_vaddr_end = 0u64;
        let mut found_any = false;
        for_each_load_segment(image, phoff, phentsize, phnum, |header| {
            if header.memsz == 0 {
                return Ok(());
            }
            found_any = true;
            min_vaddr = min_vaddr.min(align_down(u64::from(header.vaddr)));
            max_vaddr_end =
                max_vaddr_end.max(align_up(u64::from(header.vaddr) + u64::from(header.memsz)));
            Ok(())
        })?;
        if !found_any {
            return Err(ElfError::NoLoadableSegments);
        }

        let virt_base = min_vaddr;
        let image_size = max_vaddr_end - virt_base;
        let phys_base = allocate((image_size / PAGE_SIZE) as usize).ok_or(ElfError::OutOfMemory)?;

        // SAFETY: the caller guarantees the span is allocated and writable
        // through `to_virtual`.
        unsafe {
            core::ptr::write_bytes(to_virtual(phys_base) as *mut u8, 0, image_size as usize);
        }

        let mut segments = [LoadedSegment {
            virt_start: 0,
            phys_start: 0,
            size: 0,
            flags: SegmentFlags {
                readable: false,
                writable: false,
                executable: false,
            },
        }; MAX_SEGMENTS];
        let mut segment_count = 0usize;

        for_each_load_segment(image, phoff, phentsize, phnum, |header| {
            if header.memsz == 0 {
                return Ok(());
            }
            let file_start = header.offset as usize;
            let file_end = file_start
                .checked_add(header.filesz as usize)
                .ok_or(ElfError::SegmentOutOfBounds)?;
            let source = image
                .get(file_start..file_end)
                .ok_or(ElfError::SegmentOutOfBounds)?;
            let destination = phys_base + (u64::from(header.vaddr) - virt_base);
            // SAFETY: inside the span allocated and zeroed above.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    source.as_ptr(),
                    to_virtual(destination) as *mut u8,
                    source.len(),
                );
            }
            if segment_count < MAX_SEGMENTS {
                let start = align_down(u64::from(header.vaddr));
                let end = align_up(u64::from(header.vaddr) + u64::from(header.memsz));
                segments[segment_count] = LoadedSegment {
                    virt_start: start,
                    phys_start: phys_base + (start - virt_base),
                    size: end - start,
                    flags: SegmentFlags {
                        readable: header.flags & PF_R != 0,
                        writable: header.flags & PF_W != 0,
                        executable: header.flags & PF_X != 0,
                    },
                };
                segment_count += 1;
            }
            Ok(())
        })?;

        let mut program_headers = 0u64;
        for_each_load_segment(image, phoff, phentsize, phnum, |header| {
            if program_headers != 0 || header.filesz == 0 {
                return Ok(());
            }
            let start = header.offset;
            let end = start + header.filesz;
            let table_end = phoff + u32::from(phentsize) * u32::from(phnum);
            if phoff >= start && table_end <= end {
                program_headers = u64::from(header.vaddr) + u64::from(phoff - start);
            }
            Ok(())
        })?;

        Ok(LoadedImage {
            entry_point: u64::from(entry_point),
            virt_base,
            phys_base,
            image_size,
            segments,
            segment_count,
            program_headers,
            program_header_size: phentsize,
            program_header_count: phnum,
            bias: 0,
            relocatable: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate alloc;
    use alloc::vec;
    use alloc::vec::Vec;

    /// Build one whose single `PT_LOAD` starts at file offset zero, so the
    /// headers are inside the image the program can read.
    ///
    /// That is what a real static executable looks like, and it is the shape
    /// `program_headers` exists to describe. The helper above deliberately
    /// makes the other shape -- segments after the headers -- so both are
    /// covered.
    fn synthetic_elf_covering_headers(base: u64, entry: u64, payload: &[u8]) -> Vec<u8> {
        let phoff = EHDR_SIZE as u64;
        let data_offset = EHDR_SIZE + PHDR_SIZE;
        let mut file = vec![0u8; data_offset];

        file[0..4].copy_from_slice(&ELF_MAGIC);
        file[4] = ELFCLASS64;
        file[5] = ELFDATA2LSB;
        file[6] = 1; // EV_CURRENT
        file[16..18].copy_from_slice(&ET_EXEC.to_le_bytes());
        file[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
        file[24..32].copy_from_slice(&entry.to_le_bytes());
        file[32..40].copy_from_slice(&phoff.to_le_bytes());
        file[54..56].copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
        file[56..58].copy_from_slice(&1u16.to_le_bytes());

        file.extend_from_slice(payload);
        let length = file.len() as u64;

        // One segment, from offset zero, covering the whole file.
        file[EHDR_SIZE..EHDR_SIZE + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        file[EHDR_SIZE + 4..EHDR_SIZE + 8].copy_from_slice(&(PF_R | PF_X).to_le_bytes());
        file[EHDR_SIZE + 8..EHDR_SIZE + 16].copy_from_slice(&0u64.to_le_bytes());
        file[EHDR_SIZE + 16..EHDR_SIZE + 24].copy_from_slice(&base.to_le_bytes());
        file[EHDR_SIZE + 32..EHDR_SIZE + 40].copy_from_slice(&length.to_le_bytes());
        file[EHDR_SIZE + 40..EHDR_SIZE + 48].copy_from_slice(&length.to_le_bytes());

        file
    }

    /// The program header table has to be reported at the address it can be
    /// read from once the image is in place, not at its offset in the file.
    ///
    /// A System V program is handed this as `AT_PHDR`, and a C library follows
    /// it to find its own `PT_TLS`. Pointing it at a file offset would have it
    /// reading whatever happens to live at address 64.
    #[test]
    fn reports_where_the_program_headers_landed() {
        const BASE: u64 = 0x40_0000;
        let payload = b"code";
        let image = synthetic_elf_covering_headers(BASE, BASE + EHDR_SIZE as u64, payload);

        let destination = Destination::new(4);
        let at = destination.base;
        // SAFETY: `at` points at page-aligned, writable, host-owned memory large
        // enough for the one-page image above.
        let loaded = unsafe { load(&image, |_pages| Some(at), |physical| physical) }
            .expect("image should load");

        assert_eq!(loaded.program_headers, BASE + EHDR_SIZE as u64);
        assert_eq!(loaded.program_header_size, PHDR_SIZE as u16);
        assert_eq!(loaded.program_header_count, 1);
    }

    /// And zero when no loadable segment covers them, which is honest rather
    /// than a plausible address.
    ///
    /// `synthetic_elf` puts every segment *after* the headers, so nothing maps
    /// them. A loader that answered `virt_base + phoff` regardless would hand a
    /// program a pointer into its own code.
    #[test]
    fn reports_no_program_headers_when_none_are_mapped() {
        const VADDR: u64 = 0x40_0000;
        let payload = b"code";
        let image = synthetic_elf(
            VADDR,
            &[(VADDR, PF_R | PF_X, payload, payload.len() as u64)],
        );

        let destination = Destination::new(4);
        let at = destination.base;
        // SAFETY: as above.
        let loaded = unsafe { load(&image, |_pages| Some(at), |physical| physical) }
            .expect("image should load");

        assert_eq!(loaded.program_headers, 0);
    }

    /// Build a minimal but valid ELF64 executable with the given `PT_LOAD`
    /// segments, described as `(vaddr, flags, file_bytes, memsz)`.
    fn synthetic_elf(entry: u64, segments: &[(u64, u32, &[u8], u64)]) -> Vec<u8> {
        let phoff = EHDR_SIZE as u64;
        let data_offset = EHDR_SIZE + PHDR_SIZE * segments.len();

        let mut file = vec![0u8; data_offset];

        file[0..4].copy_from_slice(&ELF_MAGIC);
        file[4] = ELFCLASS64;
        file[5] = ELFDATA2LSB;
        file[6] = 1; // EV_CURRENT
        file[16..18].copy_from_slice(&ET_EXEC.to_le_bytes());
        file[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
        file[24..32].copy_from_slice(&entry.to_le_bytes());
        file[32..40].copy_from_slice(&phoff.to_le_bytes());
        file[54..56].copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
        file[56..58].copy_from_slice(&(segments.len() as u16).to_le_bytes());

        for (index, (vaddr, flags, bytes, memsz)) in segments.iter().enumerate() {
            let offset = file.len() as u64;
            file.extend_from_slice(bytes);

            let base = EHDR_SIZE + index * PHDR_SIZE;
            file[base..base + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
            file[base + 4..base + 8].copy_from_slice(&flags.to_le_bytes());
            file[base + 8..base + 16].copy_from_slice(&offset.to_le_bytes());
            file[base + 16..base + 24].copy_from_slice(&vaddr.to_le_bytes());
            file[base + 32..base + 40].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
            file[base + 40..base + 48].copy_from_slice(&memsz.to_le_bytes());
        }

        file
    }

    /// Page-aligned destination memory standing in for the physical pages the
    /// firmware would hand the loader.
    struct Destination {
        storage: Vec<u8>,
        base: u64,
    }

    impl Destination {
        fn new(pages: usize) -> Self {
            // Over-allocate so a page-aligned window is guaranteed inside.
            let mut storage = vec![0u8; (pages + 2) * PAGE_SIZE as usize];
            let raw = storage.as_mut_ptr() as u64;
            let base = (raw + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
            Self { storage, base }
        }

        fn bytes_at(&self, offset: u64, len: usize) -> &[u8] {
            let start = (self.base - self.storage.as_ptr() as u64 + offset) as usize;
            &self.storage[start..start + len]
        }
    }

    #[test]
    fn loads_a_single_segment_and_reports_where_it_landed() {
        const VADDR: u64 = 0xFFFF_FFFF_8000_0000;
        let payload = b"nexus kernel text";
        let image = synthetic_elf(
            VADDR,
            &[(VADDR, PF_R | PF_X, payload, payload.len() as u64)],
        );

        let destination = Destination::new(4);
        let base = destination.base;
        // SAFETY: `base` points at page-aligned, writable, host-owned memory
        // large enough for the one-page image below.
        let loaded = unsafe { load(&image, |_pages| Some(base), |physical| physical) }
            .expect("image should load");

        assert_eq!(loaded.entry_point, VADDR);
        assert_eq!(loaded.virt_base, VADDR);
        assert_eq!(loaded.image_size, PAGE_SIZE);
        assert_eq!(loaded.segment_count, 1);
        assert_eq!(destination.bytes_at(0, payload.len()), payload);

        let flags = loaded.segments[0].flags;
        assert!(flags.readable && flags.executable && !flags.writable);
    }

    #[test]
    fn zeroes_the_bss_tail_of_a_segment() {
        const VADDR: u64 = 0xFFFF_FFFF_8000_0000;
        let payload = [0xAAu8; 8];
        // `memsz` exceeds `filesz` by 64 bytes: that tail is `.bss`.
        let image = synthetic_elf(VADDR, &[(VADDR, PF_R | PF_W, &payload, 72)]);

        let mut destination = Destination::new(4);
        // Dirty the destination so that zeroing is actually observable.
        destination.storage.fill(0xFF);
        let base = destination.base;
        // SAFETY: as above.
        unsafe { load(&image, |_pages| Some(base), |physical| physical) }
            .expect("image should load");

        assert_eq!(destination.bytes_at(0, 8), &payload);
        assert!(
            destination.bytes_at(8, 64).iter().all(|&b| b == 0),
            "the .bss tail must be zeroed"
        );
    }

    #[test]
    fn spans_multiple_segments_from_the_lowest_virtual_address() {
        const BASE: u64 = 0xFFFF_FFFF_8000_0000;
        let text = [0x11u8; 16];
        let data = [0x22u8; 16];
        let image = synthetic_elf(
            BASE,
            &[
                (BASE, PF_R | PF_X, &text, 16),
                (BASE + PAGE_SIZE, PF_R | PF_W, &data, 16),
            ],
        );

        let destination = Destination::new(8);
        let base = destination.base;
        // SAFETY: as above.
        let loaded = unsafe { load(&image, |_pages| Some(base), |physical| physical) }
            .expect("image should load");

        assert_eq!(loaded.virt_base, BASE);
        assert_eq!(loaded.image_size, 2 * PAGE_SIZE);
        assert_eq!(loaded.segment_count, 2);
        assert_eq!(destination.bytes_at(0, 16), &text);
        assert_eq!(destination.bytes_at(PAGE_SIZE, 16), &data);
    }

    #[test]
    fn rejects_input_that_is_not_a_static_x86_64_executable() {
        let good = synthetic_elf(0x1000, &[(0x1000, PF_R, &[1, 2, 3], 3)]);
        let base = Destination::new(2).base;

        let mut truncated = good.clone();
        truncated.truncate(32);
        // SAFETY: the loader rejects these before touching the destination.
        unsafe {
            assert_eq!(
                load(&truncated, |_| Some(base), |physical| physical).err(),
                Some(ElfError::TooSmall)
            );

            let mut bad_magic = good.clone();
            bad_magic[1] = b'X';
            assert_eq!(
                load(&bad_magic, |_| Some(base), |physical| physical).err(),
                Some(ElfError::BadMagic)
            );

            let mut bit32 = good.clone();
            bit32[4] = 1; // ELFCLASS32
            assert_eq!(
                load(&bit32, |_| Some(base), |physical| physical).err(),
                Some(ElfError::NotElf64)
            );

            let mut wrong_machine = good.clone();
            wrong_machine[18..20].copy_from_slice(&0x00F3u16.to_le_bytes()); // EM_RISCV
            assert_eq!(
                load(&wrong_machine, |_| Some(base), |physical| physical).err(),
                Some(ElfError::WrongType)
            );
        }
    }

    #[test]
    fn reports_out_of_memory_rather_than_loading_partially() {
        const VADDR: u64 = 0xFFFF_FFFF_8000_0000;
        let image = synthetic_elf(VADDR, &[(VADDR, PF_R, &[7u8; 32], 32)]);
        // SAFETY: the allocator refuses, so nothing is ever written.
        let result = unsafe { load(&image, |_pages| None, |physical| physical) };
        assert_eq!(result.err(), Some(ElfError::OutOfMemory));
    }

    #[test]
    fn rejects_a_segment_whose_contents_run_past_the_end_of_the_file() {
        const VADDR: u64 = 0xFFFF_FFFF_8000_0000;
        let mut image = synthetic_elf(VADDR, &[(VADDR, PF_R, &[1, 2, 3, 4], 4)]);
        // Claim a file size far larger than the bytes actually present.
        let base = EHDR_SIZE;
        image[base + 32..base + 40].copy_from_slice(&4096u64.to_le_bytes());
        image[base + 40..base + 48].copy_from_slice(&4096u64.to_le_bytes());

        let destination = Destination::new(4);
        let dest_base = destination.base;
        // SAFETY: the bounds check rejects the segment before any copy.
        let result = unsafe { load(&image, |_pages| Some(dest_base), |physical| physical) };
        assert_eq!(result.err(), Some(ElfError::SegmentOutOfBounds));
    }

    #[test]
    fn rejects_a_segment_claiming_more_file_bytes_than_memory() {
        const VADDR: u64 = 0xFFFF_FFFF_8000_0000;
        let mut image = synthetic_elf(VADDR, &[(VADDR, PF_R, &[1, 2, 3, 4], 4)]);
        let base = EHDR_SIZE;
        // filesz 4, memsz 2: malformed.
        image[base + 40..base + 48].copy_from_slice(&2u64.to_le_bytes());

        let destination = Destination::new(4);
        let dest_base = destination.base;
        // SAFETY: rejected during the first pass, before allocation.
        let result = unsafe { load(&image, |_pages| Some(dest_base), |physical| physical) };
        assert_eq!(result.err(), Some(ElfError::InconsistentSegment));
    }

    /// A movable image is loaded where it is told, not where the file says.
    ///
    /// Every address the loader reports has to have moved by the same amount --
    /// the entry point, the span, and the program header table a dynamic linker
    /// is about to read. One of them left behind is a dynamic linker relocating
    /// against a table in somebody else's memory, which does not fault and does
    /// not produce anything recognisable either.
    #[test]
    fn a_movable_image_lands_at_the_bias_it_was_given() {
        const BIAS: u64 = 0x7F00_0000_0000;
        let payload = b"code";
        let mut image = synthetic_elf_covering_headers(0, EHDR_SIZE as u64, payload);
        image[16..18].copy_from_slice(&ET_DYN.to_le_bytes());

        let destination = Destination::new(4);
        let at = destination.base;
        // SAFETY: `at` is page-aligned, writable, host-owned memory large
        // enough for the one-page image above.
        let loaded = unsafe { load_at(&image, BIAS, |_pages| Some(at), |physical| physical) }
            .expect("a movable image should load at a bias");

        assert!(loaded.relocatable);
        assert_eq!(loaded.bias, BIAS);
        assert_eq!(loaded.entry_point, BIAS + EHDR_SIZE as u64);
        assert_eq!(loaded.virt_base, BIAS);
        assert_eq!(loaded.program_headers, BIAS + EHDR_SIZE as u64);
        assert_eq!(loaded.segments[0].virt_start, BIAS);
    }

    /// And the two kinds of image cannot be asked for the other one's treatment.
    ///
    /// Refused rather than tolerated in either direction. An `ET_EXEC` moved off
    /// the address it was linked at is a program whose every absolute address is
    /// wrong; an `ET_DYN` left at zero is one loaded over the null page, where
    /// the entire value of the null page is that nothing is mapped there.
    #[test]
    fn the_kind_of_image_and_the_bias_have_to_agree() {
        let payload = b"code";
        let fixed = synthetic_elf_covering_headers(0x40_0000, 0x40_0000, payload);
        let mut movable = fixed.clone();
        movable[16..18].copy_from_slice(&ET_DYN.to_le_bytes());

        let at = Destination::new(4).base;
        // SAFETY: both are rejected on the header, before allocation.
        unsafe {
            assert_eq!(
                load_at(&fixed, 0x1000, |_| Some(at), |physical| physical).err(),
                Some(ElfError::WrongBias)
            );
            assert_eq!(
                load_at(&movable, 0, |_| Some(at), |physical| physical).err(),
                Some(ElfError::WrongBias)
            );
            // And a bias that is not a whole number of pages, which would put
            // every segment at an offset inside a page it does not own.
            assert_eq!(
                load_at(&movable, 0x800, |_| Some(at), |physical| physical).err(),
                Some(ElfError::WrongBias)
            );
        }
    }

    /// `kind` answers from the header alone, before anything is loaded.
    #[test]
    fn the_kind_of_an_image_is_readable_without_loading_it() {
        let fixed = synthetic_elf_covering_headers(0x40_0000, 0x40_0000, b"code");
        let mut movable = fixed.clone();
        movable[16..18].copy_from_slice(&ET_DYN.to_le_bytes());

        assert_eq!(kind(&fixed), Ok(Kind::Fixed));
        assert_eq!(kind(&movable), Ok(Kind::Movable));
    }
}

/// The dynamic loader this image needs, if it needs one.
///
/// `None` for a static binary, which is one that can simply be mapped and
/// entered.
///
/// For anything else this is the path the kernel has to load *as well*, and
/// enter instead: an image with a `PT_INTERP` has calls into shared libraries
/// that nothing has resolved, so jumping to its entry point would reach a
/// symbol table nobody filled in. The Linux compatibility layer walks this path
/// in its own root and loads what it finds with [`load_at`], which is what
/// makes a dynamically linked program start.
///
/// It is still the answer to "why will this program not start" when the path is
/// not there. A program written for Linux almost always names
/// `/lib64/ld-linux-x86-64.so.2`, which is part of a C library, and saying
/// *that* by name is worth a great deal more than a fault at an entry point.
///
/// The path is returned as it is written in the file, including its leading
/// slash, because it is a fact about the program rather than a path to open.
///
/// # Errors
///
/// [`ElfError`] if the header or the program headers do not parse. A file with
/// no `PT_INTERP` is `Ok(None)`, not an error: that is what a static binary
/// looks like.
pub fn interpreter(image: &[u8]) -> Result<Option<&str>, ElfError> {
    let (_entry, phoff, phentsize, phnum, _type) = parse_header(image)?;
    let stride = core::cmp::max(phentsize as usize, PHDR_SIZE);

    for index in 0..phnum as usize {
        let base = (phoff as usize)
            .checked_add(index * stride)
            .ok_or(ElfError::PhdrOutOfBounds)?;
        if base + PHDR_SIZE > image.len() {
            return Err(ElfError::PhdrOutOfBounds);
        }
        if read_u32(image, base).ok_or(ElfError::PhdrOutOfBounds)? != PT_INTERP {
            continue;
        }

        let offset = read_u64(image, base + 8).ok_or(ElfError::PhdrOutOfBounds)? as usize;
        let length = read_u64(image, base + 32).ok_or(ElfError::PhdrOutOfBounds)? as usize;
        let Some(field) = image.get(offset..offset.saturating_add(length)) else {
            return Err(ElfError::PhdrOutOfBounds);
        };
        // The field is NUL-terminated inside its own length, which is not the
        // same as being the length: a linker is free to pad it.
        let end = field
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(field.len());
        return match core::str::from_utf8(&field[..end]) {
            Ok(path) => Ok(Some(path)),
            Err(_) => Err(ElfError::PhdrOutOfBounds),
        };
    }
    Ok(None)
}

#[cfg(test)]
mod interpreter_tests {
    use super::{interpreter, ElfError};

    /// A 64-bit x86 executable whose only program header is a PT_INTERP naming
    /// the loader every Linux program built against glibc asks for.
    const NEEDS_A_LOADER: &[u8] = &[
        0x7F, 0x45, 0x4C, 0x46, 0x02, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x02, 0x00, 0x3E, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x10, 0x40, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x38, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x78, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1C,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x2F, 0x6C, 0x69, 0x62, 0x36, 0x34, 0x2F, 0x6C, 0x64, 0x2D, 0x6C, 0x69, 0x6E, 0x75, 0x78,
        0x2D, 0x78, 0x38, 0x36, 0x2D, 0x36, 0x34, 0x2E, 0x73, 0x6F, 0x2E, 0x32, 0x00,
    ];

    /// The same shape with a PT_LOAD instead, which is what a static binary --
    /// the only kind this system runs -- looks like.
    const STATIC: &[u8] = &[
        0x7F, 0x45, 0x4C, 0x46, 0x02, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x02, 0x00, 0x3E, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x10, 0x40, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x38, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x78, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ];

    #[test]
    fn a_program_that_needs_a_loader_says_which_one() {
        assert_eq!(
            interpreter(NEEDS_A_LOADER).unwrap(),
            Some("/lib64/ld-linux-x86-64.so.2")
        );
    }

    /// Not an error. A static binary is the normal case here, and reporting one
    /// as broken would make the only kind of program that *does* run look like
    /// the failure.
    #[test]
    fn a_static_program_needs_nothing() {
        assert_eq!(interpreter(STATIC).unwrap(), None);
    }

    #[test]
    fn a_file_that_is_not_an_elf_is_refused() {
        assert!(interpreter(
            b"#!/bin/sh
echo hi
"
        )
        .is_err());
        assert!(interpreter(&[]).is_err());
    }

    /// A header pointing past the end of the file is the shape a truncated
    /// download has, and it must be a refusal rather than a read past the end.
    #[test]
    fn a_truncated_image_is_refused() {
        for keep in [64, 80, NEEDS_A_LOADER.len() - 1] {
            let answer = interpreter(&NEEDS_A_LOADER[..keep]);
            assert!(
                matches!(
                    answer,
                    Err(ElfError::PhdrOutOfBounds) | Err(ElfError::TooSmall)
                ),
                "accepted {keep} bytes: {answer:?}"
            );
        }
    }
}
