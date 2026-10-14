//! Native, unpublished DMA preparation for validated GB202/GB206 bootstrap.
//! No bus-master enable, MMIO writes, firmware execution or command submission.
use crate::{
    fs::{
        fat32::{Source, Volume},
        gpt,
    },
    kprintln, memory,
};
use alloc::vec::Vec;
use nexus_abi::layout;

#[derive(Debug)]
pub enum Error {
    Missing,
    Storage,
    Image(nexus_nvidia::Error),
    Size,
    Memory,
}
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Image(reason) => write!(f, "image rejected: {reason:?}"),
            Self::Missing => f.write_str("bootstrap files missing"),
            Self::Storage => f.write_str("bootstrap storage read failed"),
            Self::Size => f.write_str("bootstrap size/offset rejected"),
            Self::Memory => f.write_str("bootstrap memory unavailable or inconsistent"),
        }
    }
}

/// Physically contiguous, zero-filled CPU-owned storage. It is deliberately
/// not exposed to an active device, so rollback and Drop can reclaim it safely.
pub struct DmaImage {
    physical: u64,
    order: usize,
    length: usize,
}
impl DmaImage {
    fn copy(bytes: &[u8]) -> Result<Self, Error> {
        let mut storage = Self::zero(bytes.len())?;
        storage.bytes_mut().copy_from_slice(bytes);
        Ok(storage)
    }
    pub(super) fn zero(length: usize) -> Result<Self, Error> {
        if length == 0 || length > nexus_nvidia::MAX_IMAGE {
            return Err(Error::Size);
        }
        let pages = length.div_ceil(layout::PAGE_SIZE as usize);
        let order = pages.next_power_of_two().trailing_zeros() as usize;
        let physical = memory::allocate_block(order).ok_or(Error::Memory)?;
        let storage = Self {
            physical,
            order,
            length,
        };
        // SAFETY: this live buddy block is exclusive, direct-mapped RAM.
        unsafe {
            let target = layout::phys_to_virt(physical) as *mut u8;
            core::ptr::write_bytes(target, 0, storage.capacity());
        }
        Ok(storage)
    }
    fn capacity(&self) -> usize {
        (layout::PAGE_SIZE as usize) << self.order
    }
    pub(super) fn bytes(&self) -> &[u8] {
        // SAFETY: owned direct-mapped allocation is live and never published.
        unsafe {
            core::slice::from_raw_parts(
                layout::phys_to_virt(self.physical) as *const u8,
                self.length,
            )
        }
    }
    pub(super) fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: exclusive CPU-owned, unpublished direct-mapped allocation.
        unsafe {
            core::slice::from_raw_parts_mut(
                layout::phys_to_virt(self.physical) as *mut u8,
                self.length,
            )
        }
    }
    pub(super) fn buffer(&self) -> nexus_nvidia::boot::Buffer {
        nexus_nvidia::boot::Buffer {
            address: self.physical,
            size: self.length as u64,
        }
    }
    pub(super) fn map_into(
        &self,
        domain: &mut crate::drivers::amdvi::Domain,
        rights: u64,
    ) -> Result<(), Error> {
        for offset in (0..self.length).step_by(layout::PAGE_SIZE as usize) {
            let frame = self.physical + offset as u64;
            // SAFETY: this owned allocation and domain are never published.
            unsafe { domain.map(frame, frame, rights) }.map_err(|_| Error::Memory)?;
            if domain.resolve(frame, rights) != Some(frame)
                || (rights & nexus_nvidia::amdvi::WRITE == 0
                    && domain.resolve(frame, nexus_nvidia::amdvi::WRITE).is_some())
            {
                return Err(Error::Memory);
            }
        }
        Ok(())
    }
    pub(super) fn padding_is_zero(&self) -> bool {
        unsafe {
            core::slice::from_raw_parts(
                (layout::phys_to_virt(self.physical) as usize + self.length) as *const u8,
                self.capacity() - self.length,
            )
        }
        .iter()
        .all(|&byte| byte == 0)
    }
}
impl Drop for DmaImage {
    fn drop(&mut self) {
        // Still unpublished, with no outstanding GPU reads or writes.
        unsafe {
            core::ptr::write_bytes(
                layout::phys_to_virt(self.physical) as *mut u8,
                0,
                self.capacity(),
            );
            memory::free_block(self.physical, self.order);
        }
    }
}

pub struct Prepared {
    pub(super) boot: DmaImage,
    pub(super) fmc: DmaImage,
    pub(super) fmc_hash: [u8; 48],
    pub(super) fmc_signature: [u8; 96],
    pub(super) fmc_public_key: [u8; 97],
    pub code_offset: usize,
    pub data_offset: usize,
    pub manifest_offset: usize,
}
impl Prepared {
    fn from_files(boot: &[u8], fmc: &[u8]) -> Result<Self, Error> {
        Self::from_files_with(boot, fmc, DmaImage::copy)
    }
    fn from_files_with(
        boot: &[u8],
        fmc: &[u8],
        second: impl FnOnce(&[u8]) -> Result<DmaImage, Error>,
    ) -> Result<Self, Error> {
        let (boot, fmc) = nexus_nvidia::verify_pinned(boot, fmc).map_err(Error::Image)?;
        // If the second allocation fails, the first block rolls back through
        // Drop. Neither address has been handed to a device.
        let boot_storage = DmaImage::copy(boot.image)?;
        let fmc_storage = second(fmc.image)?;
        if boot_storage.bytes() != boot.image
            || fmc_storage.bytes() != fmc.image
            || !boot_storage.padding_is_zero()
            || !fmc_storage.padding_is_zero()
        {
            return Err(Error::Memory);
        }
        Ok(Self {
            boot: boot_storage,
            fmc: fmc_storage,
            fmc_hash: fmc.hash.try_into().map_err(|_| Error::Size)?,
            fmc_signature: fmc.signature.try_into().map_err(|_| Error::Size)?,
            fmc_public_key: fmc.public_key.try_into().map_err(|_| Error::Size)?,
            code_offset: boot.code.offset,
            data_offset: boot.data.offset,
            manifest_offset: boot.boot_manifest_offset,
        })
    }
    pub fn report(&self) {
        kprintln!(
            "[nv  ] pinned {} bootstrap prepared: RISC-V={} bytes at {:#x}, FMC={} bytes at {:#x}",
            nexus_nvidia::VERSION,
            self.boot.length,
            self.boot.physical,
            self.fmc.length,
            self.fmc.physical
        );
        kprintln!(
            "[nv  ] monitor code +{:#x}, data +{:#x}, manifest +{:#x}; buffers are not submitted to GPU",
            self.code_offset,
            self.data_offset,
            self.manifest_offset
        );
        kprintln!(
            "[nv  ] FMC authentication material preserved: hash={}, signature={}, public key={} bytes; hardware authentication pending",
            self.fmc_hash.len(),
            self.fmc_signature.len(),
            self.fmc_public_key.len()
        );
    }
}

fn read_file(volume: &Volume, name: &str) -> Result<Vec<u8>, Error> {
    let entries = volume
        .read_directory_at("BIN/NVFW")
        .map_err(|_| Error::Missing)?;
    let entry = entries
        .iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(name) && !entry.is_directory)
        .ok_or(Error::Missing)?;
    let size = entry.size as usize;
    if size == 0 || size > nexus_nvidia::MAX_IMAGE {
        return Err(Error::Size);
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size).map_err(|_| Error::Memory)?;
    volume
        .read_file_chunks(&alloc::format!("BIN/NVFW/{name}"), |offset, chunk| {
            if offset != bytes.len() as u64
                || bytes.len().checked_add(chunk.len()).ok_or(Error::Size)? > size
            {
                return Err(Error::Size);
            }
            bytes.extend_from_slice(chunk);
            Ok::<(), Error>(())
        })
        .map_err(|_| Error::Storage)?;
    if bytes.len() != size {
        return Err(Error::Storage);
    }
    Ok(bytes)
}

fn files_from_source(source: Source) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let partitions = gpt::read_on(source).map_err(|_| Error::Storage)?;
    for partition in partitions.iter().filter(|partition| partition.is_esp()) {
        let Ok(volume) = Volume::mount_on(source, partition.first_lba) else {
            continue;
        };
        match (
            read_file(&volume, "GSPBOOT.BIN"),
            read_file(&volume, "FMC.BIN"),
        ) {
            (Ok(boot), Ok(fmc)) => return Ok((boot, fmc)),
            (Err(Error::Missing), _) | (_, Err(Error::Missing)) => continue,
            (Err(error), _) | (_, Err(error)) => return Err(error),
        }
    }
    Err(Error::Missing)
}

fn files() -> Result<(Vec<u8>, Vec<u8>), Error> {
    if crate::drivers::virtio_blk::is_present() {
        match files_from_source(Source::Virtio) {
            Ok(found) => return Ok(found),
            Err(Error::Missing) => {}
            Err(error) => return Err(error),
        }
    }
    for (index, (_, block_size)) in crate::drivers::usb_storage::shapes().iter().enumerate() {
        if *block_size != 512 {
            continue;
        }
        match files_from_source(Source::Usb(index)) {
            Ok(found) => return Ok(found),
            Err(Error::Missing) | Err(Error::Storage) => {}
            Err(error) => return Err(error),
        }
    }
    Err(Error::Missing)
}

pub fn prepare() -> Result<Prepared, Error> {
    let (boot, fmc) = files()?;
    Prepared::from_files(&boot, &fmc)
}

#[cfg(feature = "nvidia-bootstrap-test")]
pub fn self_test() {
    let result = (|| {
        let (mut boot, fmc) = files()?;
        let before = memory::stats().ok_or(Error::Memory)?.free_frames;
        let prepared = Prepared::from_files(&boot, &fmc)?;
        let (parsed_boot, parsed_fmc) =
            nexus_nvidia::verify_pinned(&boot, &fmc).map_err(Error::Image)?;
        if prepared.boot.bytes() != parsed_boot.image
            || prepared.fmc.bytes() != parsed_fmc.image
            || prepared.fmc_hash.as_slice() != parsed_fmc.hash
            || prepared.fmc_signature.as_slice() != parsed_fmc.signature
            || prepared.fmc_public_key.as_slice() != parsed_fmc.public_key
            || !prepared.boot.padding_is_zero()
            || !prepared.fmc.padding_is_zero()
        {
            return Err(Error::Memory);
        }
        prepared.report();
        drop(prepared);
        if memory::stats().ok_or(Error::Memory)?.free_frames != before {
            return Err(Error::Memory);
        }
        if !matches!(
            Prepared::from_files_with(&boot, &fmc, |_| Err(Error::Memory)),
            Err(Error::Memory)
        ) || memory::stats().ok_or(Error::Memory)?.free_frames != before
        {
            return Err(Error::Memory);
        }
        // Alter a byte that is not descriptor metadata: SHA256 must still
        // reject it before any DMA memory is allocated.
        let last = boot.len() - 1;
        boot[last] ^= 1;
        if !matches!(
            Prepared::from_files(&boot, &fmc),
            Err(Error::Image(nexus_nvidia::Error::ReleaseMismatch))
        ) {
            return Err(Error::Memory);
        }
        if memory::stats().ok_or(Error::Memory)?.free_frames != before {
            return Err(Error::Memory);
        }
        Ok::<(), Error>(())
    })();
    match result {
        Ok(()) => kprintln!(
            "[test] NVIDIA bootstrap PASS: production hashes, bounded layouts, contiguous copies, zero padding, reclamation, allocation rollback and corrupt-image refusal; no GPU submission"
        ),
        Err(error) => {
            kprintln!("[test] NVIDIA bootstrap FAILED: {:?}", error);
        }
    }
}
