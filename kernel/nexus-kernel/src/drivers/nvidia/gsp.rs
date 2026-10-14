//! CPU-owned GSP-RM and UCODES scatter pages and unpublished radix tables.
//! Physical addresses below are diagnostic only, not mapped device addresses.
use super::bootstrap::Error;
use crate::{
    fs::{
        fat32::{Source, Volume},
        gpt,
    },
    kprintln, memory,
};
use alloc::vec::Vec;
use nexus_abi::layout;
use nexus_nvidia::{
    Span,
    gsp::{self, Read},
    radix::{PAGE, Plan},
};

struct Pages {
    frames: Vec<u64>,
    length: usize,
}
impl Pages {
    fn allocate(length: usize) -> Result<Self, Error> {
        Self::allocate_with(length, memory::allocate_frame)
    }
    fn allocate_with(
        length: usize,
        mut allocate: impl FnMut() -> Option<u64>,
    ) -> Result<Self, Error> {
        if length == 0 || length > gsp::MAX_FILE {
            return Err(Error::Size);
        }
        let count = length.div_ceil(PAGE);
        let mut pages = Self {
            frames: Vec::new(),
            length,
        };
        pages
            .frames
            .try_reserve_exact(count)
            .map_err(|_| Error::Memory)?;
        for _ in 0..count {
            let frame = allocate().ok_or(Error::Memory)?;
            // SAFETY: the allocator returned an exclusively owned RAM frame.
            unsafe {
                core::ptr::write_bytes(layout::phys_to_virt(frame) as *mut u8, 0, PAGE);
            }
            pages.frames.push(frame);
        }
        Ok(pages)
    }
    fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<(), Error> {
        if offset.checked_add(bytes.len()).ok_or(Error::Size)? > self.length {
            return Err(Error::Size);
        }
        let mut at = offset;
        let mut source = bytes;
        while !source.is_empty() {
            let within = at % PAGE;
            let take = source.len().min(PAGE - within);
            // SAFETY: owned RAM, bounded to this page; it is not published.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    source.as_ptr(),
                    (layout::phys_to_virt(self.frames[at / PAGE]) as usize + within) as *mut u8,
                    take,
                );
            }
            source = &source[take..];
            at += take;
        }
        Ok(())
    }
    fn copy_span(reader: &impl Read, span: Span) -> Result<Self, Error> {
        let mut output = Self::allocate(span.size)?;
        let mut chunk = [0u8; PAGE];
        let mut copied = [0u8; PAGE];
        for at in (0..span.size).step_by(PAGE) {
            let take = PAGE.min(span.size - at);
            reader
                .read(span.offset + at, &mut chunk[..take])
                .map_err(Error::Image)?;
            output.write(at, &chunk[..take])?;
            output.read(at, &mut copied[..take]).map_err(Error::Image)?;
            if copied[..take] != chunk[..take] {
                return Err(Error::Memory);
            }
        }
        Ok(output)
    }
    fn padding_is_zero(&self) -> bool {
        let used = self.length % PAGE;
        if used == 0 {
            return true;
        }
        // SAFETY: the live last frame is owned, initialized and unpublished.
        unsafe {
            core::slice::from_raw_parts(
                (layout::phys_to_virt(*self.frames.last().unwrap()) as usize + used) as *const u8,
                PAGE - used,
            )
        }
        .iter()
        .all(|&byte| byte == 0)
    }
}
impl Read for Pages {
    fn len(&self) -> usize {
        self.length
    }
    fn read(&self, offset: usize, output: &mut [u8]) -> Result<(), nexus_nvidia::Error> {
        if offset
            .checked_add(output.len())
            .ok_or(nexus_nvidia::Error::Bounds)?
            > self.length
        {
            return Err(nexus_nvidia::Error::Truncated);
        }
        let mut at = offset;
        let mut output = output;
        while !output.is_empty() {
            let within = at % PAGE;
            let take = output.len().min(PAGE - within);
            // SAFETY: bounded to an initialized, live owned frame.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    (layout::phys_to_virt(self.frames[at / PAGE]) as usize + within) as *const u8,
                    output.as_mut_ptr(),
                    take,
                );
            }
            output = &mut output[take..];
            at += take;
        }
        Ok(())
    }
}
impl Drop for Pages {
    fn drop(&mut self) {
        for &frame in &self.frames {
            // SAFETY: never submitted to a device, and each allocation is freed once.
            unsafe {
                core::ptr::write_bytes(layout::phys_to_virt(frame) as *mut u8, 0, PAGE);
                memory::free_frame(frame);
            }
        }
    }
}

struct RadixImage {
    tables: Pages,
    data: Pages,
}
impl RadixImage {
    fn new(data: Pages) -> Result<Self, Error> {
        let plan = Plan::new(data.length).map_err(Error::Image)?;
        let mut tables = Pages::allocate(plan.table_pages() * PAGE)?;
        let mut addresses = Vec::new();
        addresses
            .try_reserve_exact(tables.frames.len())
            .map_err(|_| Error::Memory)?;
        addresses.extend_from_slice(&tables.frames);
        plan.write(&addresses, &data.frames, |index, bytes| {
            tables
                .write(index * PAGE, bytes)
                .map_err(|_| nexus_nvidia::Error::Bounds)
        })
        .map_err(Error::Image)?;
        Ok(Self { tables, data })
    }
    /// Check every stored table entry and unused byte against the checked plan.
    fn validate(&self) -> Result<(), Error> {
        let plan = Plan::new(self.data.length).map_err(Error::Image)?;
        plan.write(&self.tables.frames, &self.data.frames, |index, expected| {
            let mut actual = [0u8; PAGE];
            self.tables.read(index * PAGE, &mut actual)?;
            if &actual != expected {
                return Err(nexus_nvidia::Error::Bounds);
            }
            Ok(())
        })
        .map_err(Error::Image)?;
        // Independently walk the stored root/middle/leaf addresses to each
        // original data frame. All pointers must name one of our table pages.
        let entry = |address: u64, slot: usize| -> Result<u64, Error> {
            let page = self
                .tables
                .frames
                .iter()
                .position(|&frame| frame == address)
                .ok_or(Error::Memory)?;
            if slot >= PAGE / 8 {
                return Err(Error::Size);
            }
            let mut bytes = [0u8; 8];
            self.tables
                .read(page * PAGE + slot * 8, &mut bytes)
                .map_err(Error::Image)?;
            Ok(u64::from_le_bytes(bytes))
        };
        for (index, &expected) in self.data.frames.iter().enumerate() {
            let middle = entry(self.tables.frames[0], index / (512 * 512))?;
            let leaf = entry(middle, (index / 512) % 512)?;
            if entry(leaf, index % 512)? != expected {
                return Err(Error::Memory);
            }
        }
        if !self.data.padding_is_zero() {
            return Err(Error::Memory);
        }
        Ok(())
    }
}

pub struct Prepared {
    image: RadixImage,
    ucodes: RadixImage,
    signature: Pages,
}
impl Prepared {
    fn from_volume(volume: &Volume) -> Result<Self, Error> {
        let rm = read_file(volume, "GSPRM.BIN", gsp::RM_SIZE, gsp::RM_SHA)?;
        let parsed = gsp::Gsp::parse(&rm).map_err(Error::Image)?;
        let signature = Pages::copy_span(&rm, parsed.signature)?;
        let image = Pages::copy_span(&rm, parsed.image)?;
        // Full-file hashes already matched. Copy the unaligned ELF payload to
        // its own zero-padded pages; signatures retain all opaque 4096 bytes.
        drop(rm);
        let ucodes = read_file(volume, "UCODES.BIN", gsp::UCODES_SIZE, gsp::UCODES_SHA)?;
        gsp::check_ucodes(&ucodes).map_err(Error::Image)?;
        let prepared = Self {
            image: RadixImage::new(image)?,
            ucodes: RadixImage::new(ucodes)?,
            signature,
        };
        prepared.image.validate()?;
        prepared.ucodes.validate()?;
        Ok(prepared)
    }
    pub fn report(&self) {
        kprintln!(
            "[nv  ] real {} GSP-RM image={} bytes radix root={:#x}; ucodes={} bytes radix root={:#x}; signature={} bytes",
            nexus_nvidia::VERSION,
            self.image.data.length,
            self.image.tables.frames[0],
            self.ucodes.data.length,
            self.ucodes.tables.frames[0],
            self.signature.length
        );
        kprintln!(
            "[nv  ] GSP pages/tables are CPU-owned: IOMMU mapping, WPR layout, hardware authentication and GSP boot pending"
        );
    }
}

fn read_file(volume: &Volume, name: &str, size: usize, expected: [u8; 32]) -> Result<Pages, Error> {
    let entries = volume
        .read_directory_at("BIN/NVFW")
        .map_err(|_| Error::Missing)?;
    let entry = entries
        .iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(name) && !entry.is_directory)
        .ok_or(Error::Missing)?;
    if entry.size as usize != size {
        return Err(Error::Size);
    }
    let mut pages = Pages::allocate(size)?;
    let mut hash = nexus_crypto::sha256::Sha256::new();
    let mut next = 0usize;
    volume
        .read_file_chunks(&alloc::format!("BIN/NVFW/{name}"), |offset, chunk| {
            if offset != next as u64 {
                return Err(Error::Size);
            }
            pages.write(next, chunk)?;
            next += chunk.len();
            hash.update(chunk);
            Ok::<(), Error>(())
        })
        .map_err(|_| Error::Storage)?;
    if next != size {
        return Err(Error::Storage);
    }
    if hash.finish() != expected || gsp::hash(&pages).map_err(Error::Image)? != expected {
        return Err(Error::Image(nexus_nvidia::Error::ReleaseMismatch));
    }
    Ok(pages)
}
fn from_source(source: Source) -> Result<Prepared, Error> {
    let partitions = gpt::read_on(source).map_err(|_| Error::Storage)?;
    for partition in partitions.iter().filter(|p| p.is_esp()) {
        let Ok(volume) = Volume::mount_on(source, partition.first_lba) else {
            continue;
        };
        match Prepared::from_volume(&volume) {
            Ok(prepared) => return Ok(prepared),
            Err(Error::Missing) => {}
            Err(error) => return Err(error),
        }
    }
    Err(Error::Missing)
}
pub fn prepare() -> Result<Prepared, Error> {
    if crate::drivers::virtio_blk::is_present() {
        match from_source(Source::Virtio) {
            Ok(p) => return Ok(p),
            Err(Error::Missing) => {}
            Err(e) => return Err(e),
        }
    }
    for (index, (_, block_size)) in crate::drivers::usb_storage::shapes().iter().enumerate() {
        if *block_size != 512 {
            continue;
        }
        match from_source(Source::Usb(index)) {
            Ok(p) => return Ok(p),
            Err(Error::Missing) | Err(Error::Storage) => {}
            Err(e) => return Err(e),
        }
    }
    Err(Error::Missing)
}

#[cfg(feature = "nvidia-gsp-test")]
pub fn self_test() {
    let before = memory::stats().map(|stats| stats.free_frames);
    let result = (|| {
        let before = memory::stats().ok_or(Error::Memory)?.free_frames;
        let prepared = prepare()?;
        prepared.report();
        drop(prepared);
        if memory::stats().ok_or(Error::Memory)?.free_frames != before {
            return Err(Error::Memory);
        }
        // Failure after allocating some frames must reclaim every owned frame.
        let mut left = 7usize;
        if !matches!(
            Pages::allocate_with(32 * PAGE, || {
                if left == 0 {
                    return None;
                }
                left -= 1;
                memory::allocate_frame()
            }),
            Err(Error::Memory)
        ) || memory::stats().ok_or(Error::Memory)?.free_frames != before
        {
            return Err(Error::Memory);
        }
        Ok::<(), Error>(())
    })();
    if before != memory::stats().map(|stats| stats.free_frames) {
        kprintln!("[test] NVIDIA GSP preparation FAILED: leaked frames on success/refusal");
        return;
    }
    match result {
        Ok(()) => kprintln!(
            "[test] NVIDIA GSP preparation PASS: real pinned RM/ucodes, bounded firmware sections, scatter pages, three-level radix tables, zero padding, reclamation and partial-allocation rollback; no GPU execution"
        ),
        Err(error) => kprintln!("[test] NVIDIA GSP preparation FAILED: {:?}", error),
    }
}
