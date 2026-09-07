//! Translation of the UEFI memory map into the kernel's normalized form.
//!
//! The firmware map is verbose, unsorted in principle, and uses a descriptor
//! stride the kernel would rather not care about. This module walks it once,
//! classifies each range, sorts by address, and coalesces neighbours of the
//! same kind so the kernel's physical allocator starts from something compact.

use nexus_abi::{MemoryKind, MemoryRegion};

use crate::uefi::{self, MemoryDescriptor};

/// Upper bound on the number of normalized regions handed to the kernel.
///
/// Firmware maps on real machines run to a few dozen entries after
/// coalescing; 256 leaves a wide margin without making the handoff buffer
/// large.
pub const MAX_REGIONS: usize = 256;

/// Classify a raw `EFI_MEMORY_TYPE`.
///
/// Boot-services code and data become usable RAM: by the time the kernel reads
/// this map, `ExitBootServices` has returned and the firmware no longer owns
/// them. The same is true of the loader's own image, which the kernel never
/// re-enters.
fn classify(uefi_type: u32) -> MemoryKind {
    match uefi_type {
        // ConventionalMemory, BootServicesCode/Data, LoaderCode/Data.
        1 | 2 | 3 | 4 | 7 => MemoryKind::Usable,
        // RuntimeServicesCode/Data: still owned by firmware, and where the
        // bootloader placed everything the kernel must keep.
        5 | 6 => MemoryKind::Reserved,
        9 => MemoryKind::AcpiReclaimable,
        10 => MemoryKind::AcpiNvs,
        8 => MemoryKind::Defective,
        // ReservedMemoryType, MMIO, MMIO port space, PAL code, persistent
        // memory, and any type the firmware invented.
        _ => MemoryKind::Reserved,
    }
}

/// A borrowed view over a raw UEFI memory map.
pub struct UefiMemoryMap<'a> {
    buffer: &'a [u8],
    descriptor_size: usize,
}

impl<'a> UefiMemoryMap<'a> {
    /// Wrap `buffer`, which holds `buffer.len() / descriptor_size` descriptors.
    #[must_use]
    pub fn new(buffer: &'a [u8], descriptor_size: usize) -> Self {
        Self {
            buffer,
            descriptor_size,
        }
    }

    /// Number of descriptors in the map.
    #[must_use]
    pub fn len(&self) -> usize {
        if self.descriptor_size == 0 {
            0
        } else {
            self.buffer.len() / self.descriptor_size
        }
    }

    /// Whether the map is empty.
    ///
    /// Present because a `len` without an `is_empty` is a trap for callers.
    #[allow(dead_code)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Read descriptor `index`.
    ///
    /// Descriptors are copied out by value because the firmware buffer has no
    /// alignment guarantee for `MemoryDescriptor`.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<MemoryDescriptor> {
        if index >= self.len() {
            return None;
        }
        let offset = index * self.descriptor_size;
        let bytes = self
            .buffer
            .get(offset..offset + core::mem::size_of::<MemoryDescriptor>())?;
        // SAFETY: `bytes` is exactly one descriptor long and `read_unaligned`
        // imposes no alignment requirement. `MemoryDescriptor` is plain data.
        Some(unsafe { (bytes.as_ptr() as *const MemoryDescriptor).read_unaligned() })
    }

    /// Iterate every descriptor in the map.
    pub fn iter(&self) -> impl Iterator<Item = MemoryDescriptor> + '_ {
        (0..self.len()).filter_map(|index| self.get(index))
    }

    /// Highest physical address backed by RAM, rounded up to 2 MiB.
    ///
    /// This bounds the identity and direct maps the bootloader builds, so it
    /// deliberately ignores firmware-reserved and memory-mapped I/O windows:
    /// QEMU, for one, reports a 12 GiB MMIO hole at 0xFD_0000_0000, and
    /// stretching the direct map to cover it would cost thousands of page-table
    /// pages to map address space nothing will ever read through the direct
    /// map. Device memory is mapped on demand instead, with the cache
    /// attributes each device actually needs.
    #[must_use]
    pub fn ram_limit(&self) -> u64 {
        let mut limit = 0u64;
        for descriptor in self.iter() {
            // UEFI types 1..=10 and 14 are all backed by real memory:
            // loader/boot-services/runtime-services allocations, conventional
            // memory, unusable (defective) RAM, the ACPI tables and NVS, and
            // persistent memory. Type 0 is firmware-reserved and types 11..=13
            // are MMIO, port space and PAL code.
            let is_ram = matches!(descriptor.kind, 1..=10 | 14);
            if !is_ram {
                continue;
            }
            let end = descriptor
                .physical_start
                .saturating_add(descriptor.number_of_pages.saturating_mul(4096));
            limit = limit.max(end);
        }
        const TWO_MIB: u64 = 2 * 1024 * 1024;
        limit.div_ceil(TWO_MIB) * TWO_MIB
    }
}

/// Convert `map` into sorted, coalesced [`MemoryRegion`]s written to `out`.
///
/// Ranges overlapping `[kernel_phys, kernel_phys + kernel_size)` are re-tagged
/// as [`MemoryKind::KernelImage`] so the kernel can recognise its own image
/// without consulting the boot info.
///
/// Returns the number of regions written. Entries beyond `out.len()` are
/// dropped, which is reported by the caller rather than silently ignored.
pub fn normalize(
    map: &UefiMemoryMap<'_>,
    kernel_phys: u64,
    kernel_size: u64,
    out: &mut [MemoryRegion],
) -> usize {
    let mut count = 0usize;

    for descriptor in map.iter() {
        if descriptor.number_of_pages == 0 || count >= out.len() {
            continue;
        }

        let kind = if kernel_size != 0
            && descriptor.physical_start >= kernel_phys
            && descriptor.physical_start < kernel_phys + kernel_size
        {
            MemoryKind::KernelImage
        } else {
            classify(descriptor.kind)
        };

        out[count] = MemoryRegion {
            start: descriptor.physical_start,
            pages: descriptor.number_of_pages,
            kind,
            _reserved: 0,
        };
        count += 1;
    }

    sort_by_start(&mut out[..count]);
    coalesce(&mut out[..count])
}

/// Insertion sort by start address.
///
/// The map is short and nearly sorted already, which is exactly the case
/// insertion sort handles best, and it needs no scratch space.
fn sort_by_start(regions: &mut [MemoryRegion]) {
    for i in 1..regions.len() {
        let mut j = i;
        while j > 0 && regions[j - 1].start > regions[j].start {
            regions.swap(j - 1, j);
            j -= 1;
        }
    }
}

/// Merge adjacent regions that share a kind. Returns the new length.
fn coalesce(regions: &mut [MemoryRegion]) -> usize {
    if regions.is_empty() {
        return 0;
    }

    let mut write = 0usize;
    for read in 1..regions.len() {
        let current = regions[read];
        let previous = regions[write];
        if previous.kind == current.kind && previous.end() == current.start {
            regions[write].pages += current.pages;
        } else {
            write += 1;
            regions[write] = current;
        }
    }
    write + 1
}

/// Locate the ACPI RSDP in the UEFI configuration table.
///
/// ACPI 2.0+ is preferred; the 1.0 table is accepted as a fallback so the
/// bootloader still reports something on very old firmware.
///
/// # Safety
///
/// `system_table` must point at a live `EFI_SYSTEM_TABLE`.
pub unsafe fn find_acpi_rsdp(system_table: *const uefi::SystemTable) -> u64 {
    // SAFETY: upheld by the caller.
    let (entries, count) = unsafe {
        (
            (*system_table).configuration_table,
            (*system_table).number_of_table_entries,
        )
    };

    let mut fallback = 0u64;
    for index in 0..count {
        // SAFETY: the firmware guarantees `count` valid entries at `entries`.
        let entry = unsafe { &*entries.add(index) };
        if entry.vendor_guid == uefi::ACPI_20_TABLE_GUID {
            return entry.vendor_table as u64;
        }
        if entry.vendor_guid == uefi::ACPI_10_TABLE_GUID {
            fallback = entry.vendor_table as u64;
        }
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(start: u64, pages: u64, kind: MemoryKind) -> MemoryRegion {
        MemoryRegion {
            start,
            pages,
            kind,
            _reserved: 0,
        }
    }

    #[test]
    fn coalesce_merges_only_adjacent_regions_of_the_same_kind() {
        let mut regions = [
            region(0x0000, 1, MemoryKind::Usable),
            region(0x1000, 2, MemoryKind::Usable),
            region(0x3000, 1, MemoryKind::Reserved),
            region(0x4000, 1, MemoryKind::Usable),
        ];
        let count = coalesce(&mut regions);
        assert_eq!(count, 3);
        assert_eq!(regions[0].start, 0);
        assert_eq!(regions[0].pages, 3);
        assert_eq!(regions[1].kind, MemoryKind::Reserved);
        assert_eq!(regions[2].start, 0x4000);
    }

    #[test]
    fn coalesce_leaves_a_gap_between_same_kind_regions_intact() {
        let mut regions = [
            region(0x0000, 1, MemoryKind::Usable),
            region(0x8000, 1, MemoryKind::Usable),
        ];
        assert_eq!(coalesce(&mut regions), 2);
    }

    #[test]
    fn sort_orders_regions_by_start_address() {
        let mut regions = [
            region(0x3000, 1, MemoryKind::Usable),
            region(0x1000, 1, MemoryKind::Usable),
            region(0x2000, 1, MemoryKind::Reserved),
        ];
        sort_by_start(&mut regions);
        assert_eq!(regions[0].start, 0x1000);
        assert_eq!(regions[1].start, 0x2000);
        assert_eq!(regions[2].start, 0x3000);
    }

    #[test]
    fn boot_services_memory_is_reported_as_usable() {
        assert_eq!(classify(3), MemoryKind::Usable);
        assert_eq!(classify(4), MemoryKind::Usable);
        assert_eq!(classify(7), MemoryKind::Usable);
        assert_eq!(classify(6), MemoryKind::Reserved);
        assert_eq!(classify(9), MemoryKind::AcpiReclaimable);
        assert_eq!(classify(0xDEAD), MemoryKind::Reserved);
    }
}
