//! A minimal, allocation-free ELF64 loader for the Nexus kernel image.
//!
//! The kernel is linked as a non-PIE static executable at a fixed higher-half
//! address, so loading it needs neither relocation processing nor dynamic
//! symbol resolution: the loader reserves one contiguous physical span, copies
//! every `PT_LOAD` segment into it, and reports where it landed.
//!
//! All multi-byte fields are read byte-wise. The file buffer comes straight
//! from firmware and carries no alignment guarantee, so a `read_unaligned` of a
//! `repr(C)` header would be the only other correct option and this is clearer.

use core::fmt;

/// `EI_NIDENT`: the size of the identification prefix of an ELF header.
const EI_NIDENT: usize = 16;
/// Size of `Elf64_Ehdr`.
const EHDR_SIZE: usize = 64;
/// Size of `Elf64_Phdr`.
const PHDR_SIZE: usize = 56;

const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];
/// `ELFCLASS64`.
const ELFCLASS64: u8 = 2;
/// `ELFDATA2LSB`.
const ELFDATA2LSB: u8 = 1;
/// `ET_EXEC`: a non-relocatable executable.
const ET_EXEC: u16 = 2;
/// `EM_X86_64`.
const EM_X86_64: u16 = 0x3E;
/// `PT_LOAD`.
const PT_LOAD: u32 = 1;

const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

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
}

impl fmt::Display for ElfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::TooSmall => "file is smaller than an ELF64 header",
            Self::BadMagic => "missing \\x7fELF signature",
            Self::NotElf64 => "not a little-endian ELF64 object",
            Self::WrongType => "not a static x86-64 executable (ET_EXEC/EM_X86_64)",
            Self::PhdrOutOfBounds => "program header table extends past end of file",
            Self::SegmentOutOfBounds => "segment contents extend past end of file",
            Self::NoLoadableSegments => "no PT_LOAD segments",
            Self::OutOfMemory => "could not allocate physical memory for the image",
            Self::InconsistentSegment => "segment has p_filesz greater than p_memsz",
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

const PAGE_SIZE: u64 = 4096;

const fn align_down(value: u64) -> u64 {
    value & !(PAGE_SIZE - 1)
}

const fn align_up(value: u64) -> u64 {
    (value + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

/// Validate the ELF header and return `(entry, phoff, phentsize, phnum)`.
fn parse_header(image: &[u8]) -> Result<(u64, u64, u16, u16), ElfError> {
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
    if e_type != ET_EXEC || e_machine != EM_X86_64 {
        return Err(ElfError::WrongType);
    }

    let e_entry = read_u64(image, 24).ok_or(ElfError::TooSmall)?;
    let e_phoff = read_u64(image, 32).ok_or(ElfError::TooSmall)?;
    let e_phentsize = read_u16(image, 54).ok_or(ElfError::TooSmall)?;
    let e_phnum = read_u16(image, 56).ok_or(ElfError::TooSmall)?;

    Ok((e_entry, e_phoff, e_phentsize, e_phnum))
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
/// # Safety
///
/// The physical range returned by `allocate` must be writable through its
/// identity mapping for the duration of this call; the loader writes the image
/// there directly.
pub unsafe fn load<A>(image: &[u8], mut allocate: A) -> Result<LoadedImage, ElfError>
where
    A: FnMut(usize) -> Option<u64>,
{
    let (entry_point, phoff, phentsize, phnum) = parse_header(image)?;

    // First pass: find the virtual span the image occupies.
    let mut min_vaddr = u64::MAX;
    let mut max_vaddr_end = 0u64;
    let mut found_any = false;

    for_each_load_segment(image, phoff, phentsize, phnum, |header| {
        if header.memsz == 0 {
            return Ok(());
        }
        found_any = true;
        min_vaddr = min_vaddr.min(align_down(header.vaddr));
        max_vaddr_end = max_vaddr_end.max(align_up(header.vaddr + header.memsz));
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
    // allocated, writable and identity mapped.
    unsafe {
        core::ptr::write_bytes(phys_base as *mut u8, 0, image_size as usize);
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

        let destination = phys_base + (header.vaddr - virt_base);
        // SAFETY: `destination .. destination + filesz` lies inside the span we
        // allocated and zeroed above, because `vaddr + memsz <= max_vaddr_end`
        // and `filesz <= memsz`.
        unsafe {
            core::ptr::copy_nonoverlapping(source.as_ptr(), destination as *mut u8, source.len());
        }

        if segment_count < MAX_SEGMENTS {
            let start = align_down(header.vaddr);
            let end = align_up(header.vaddr + header.memsz);
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

    Ok(LoadedImage {
        entry_point,
        virt_base,
        phys_base,
        image_size,
        segments,
        segment_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate alloc;
    use alloc::vec;
    use alloc::vec::Vec;

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
        let loaded = unsafe { load(&image, |_pages| Some(base)) }.expect("image should load");

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
        unsafe { load(&image, |_pages| Some(base)) }.expect("image should load");

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
        let loaded = unsafe { load(&image, |_pages| Some(base)) }.expect("image should load");

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
                load(&truncated, |_| Some(base)).err(),
                Some(ElfError::TooSmall)
            );

            let mut bad_magic = good.clone();
            bad_magic[1] = b'X';
            assert_eq!(
                load(&bad_magic, |_| Some(base)).err(),
                Some(ElfError::BadMagic)
            );

            let mut bit32 = good.clone();
            bit32[4] = 1; // ELFCLASS32
            assert_eq!(load(&bit32, |_| Some(base)).err(), Some(ElfError::NotElf64));

            let mut wrong_machine = good.clone();
            wrong_machine[18..20].copy_from_slice(&0x00F3u16.to_le_bytes()); // EM_RISCV
            assert_eq!(
                load(&wrong_machine, |_| Some(base)).err(),
                Some(ElfError::WrongType)
            );
        }
    }

    #[test]
    fn reports_out_of_memory_rather_than_loading_partially() {
        const VADDR: u64 = 0xFFFF_FFFF_8000_0000;
        let image = synthetic_elf(VADDR, &[(VADDR, PF_R, &[7u8; 32], 32)]);
        // SAFETY: the allocator refuses, so nothing is ever written.
        let result = unsafe { load(&image, |_pages| None) };
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
        let result = unsafe { load(&image, |_pages| Some(dest_base)) };
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
        let result = unsafe { load(&image, |_pages| Some(dest_base)) };
        assert_eq!(result.err(), Some(ElfError::InconsistentSegment));
    }
}
