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
/// The path of the program that has to load this one before it can run.
const PT_INTERP: u32 = 3;

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

        let destination = phys_base + (header.vaddr - virt_base);
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
            program_headers = header.vaddr + (phoff - start);
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
    })
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
}

/// The dynamic loader this image needs, if it needs one.
///
/// `None` for a static binary, which is one that can simply be mapped and
/// entered -- and is the only kind this system runs today.
///
/// This exists so that the answer to "why will this program not start" can be a
/// sentence instead of a crash. A program written for Linux and downloaded here
/// almost always names `/lib64/ld-linux-x86-64.so.2`, which is not on this
/// machine and cannot be built here: it is part of a C library that is itself
/// larger than this operating system. Saying *that*, by name, is worth a great
/// deal more than mapping the segments and jumping to an entry point that
/// immediately reaches for a symbol table nothing filled in.
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
    let (_entry, phoff, phentsize, phnum) = parse_header(image)?;
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
