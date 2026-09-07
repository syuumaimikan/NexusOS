//! Construction of the kernel's initial 4-level page tables.
//!
//! The bootloader runs under the firmware's identity mapping, so while these
//! tables are being built a physical address and a virtual address are the same
//! thing and the tables can be written directly.
//!
//! Three regions are established:
//!
//! * an **identity map** of all physical memory, which keeps the bootloader's
//!   own code and stack addressable across the `mov cr3` that installs these
//!   tables;
//! * the **direct map** at `layout::PHYS_MAP_BASE`, the kernel's permanent
//!   window onto physical memory;
//! * the **kernel image** at `layout::KERNEL_IMAGE_BASE`, mapped per segment
//!   so that read-only and non-executable permissions survive into the kernel.

/// Page-table entry: the mapping is valid.
pub const PRESENT: u64 = 1 << 0;
/// Page-table entry: writes are permitted.
pub const WRITABLE: u64 = 1 << 1;
/// Page-table entry: reachable from CPL 3.
pub const USER: u64 = 1 << 2;
/// Page-table entry: write-through caching.
///
/// Part of the page-table entry vocabulary; the bootloader itself maps only
/// write-back memory, but the constant belongs with its siblings.
#[allow(dead_code)]
pub const WRITE_THROUGH: u64 = 1 << 3;
/// Page-table entry: caching disabled (required for MMIO).
#[allow(dead_code)]
pub const NO_CACHE: u64 = 1 << 4;
/// Page-table entry (PD/PDPT level): maps a large page directly.
pub const HUGE: u64 = 1 << 7;
/// Page-table entry: the translation is not flushed on a `cr3` reload.
pub const GLOBAL: u64 = 1 << 8;
/// Page-table entry: instruction fetches fault. Requires `EFER.NXE`.
pub const NO_EXECUTE: u64 = 1 << 63;

/// Bits of an entry that hold the physical frame address.
const ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;

const PAGE_SIZE: u64 = 4096;
const LARGE_PAGE_SIZE: u64 = 2 * 1024 * 1024;

/// A bump allocator over a contiguous run of zeroed physical pages.
///
/// Page tables are only ever allocated, never freed, so a bump pointer is the
/// whole allocator the bootloader needs.
pub struct PagePool {
    base: u64,
    next: u64,
    end: u64,
}

impl PagePool {
    /// Create a pool over `pages` pages starting at physical address `base`.
    ///
    /// # Safety
    ///
    /// The range must be allocated, identity mapped, writable, and already
    /// zeroed by the caller.
    #[must_use]
    pub unsafe fn new(base: u64, pages: usize) -> Self {
        Self {
            base,
            next: base,
            end: base + pages as u64 * PAGE_SIZE,
        }
    }

    /// Hand out one zeroed page, or `None` when the pool is exhausted.
    pub fn allocate(&mut self) -> Option<u64> {
        if self.next >= self.end {
            return None;
        }
        let page = self.next;
        self.next += PAGE_SIZE;
        Some(page)
    }

    /// How many pages have been handed out so far.
    #[must_use]
    pub fn used_pages(&self) -> u64 {
        // `next` only ever moves forward from `base`, so this cannot underflow.
        (self.next - self.base) / PAGE_SIZE
    }
}

/// Why a mapping could not be established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    /// The page pool ran out of table pages.
    OutOfTables,
    /// A 4 KiB mapping collided with an existing 2 MiB mapping.
    ///
    /// The bootloader maps disjoint regions, so this means a layout constant is
    /// wrong rather than a transient condition.
    OverlapsLargePage,
}

/// Builder for a fresh address space.
pub struct AddressSpace {
    root: u64,
    pool: PagePool,
}

impl AddressSpace {
    /// Allocate and zero a new PML4 from `pool`.
    pub fn new(mut pool: PagePool) -> Result<Self, MapError> {
        let root = pool.allocate().ok_or(MapError::OutOfTables)?;
        Ok(Self { root, pool })
    }

    /// Physical address of the PML4, for loading into `cr3`.
    #[must_use]
    pub fn root(&self) -> u64 {
        self.root
    }

    /// Number of pages consumed for page tables.
    #[must_use]
    pub fn table_pages_used(&self) -> u64 {
        self.pool.used_pages()
    }

    /// Read entry `index` of the table at physical address `table`.
    ///
    /// # Safety
    ///
    /// `table` must be an identity-mapped, page-aligned table page.
    unsafe fn entry(table: u64, index: usize) -> u64 {
        // SAFETY: caller guarantees `table` is a live, identity-mapped page
        // table; `index` is masked to 0..512 by every caller.
        unsafe { core::ptr::read_volatile((table as *const u64).add(index)) }
    }

    /// Write entry `index` of the table at physical address `table`.
    ///
    /// # Safety
    ///
    /// See [`AddressSpace::entry`].
    unsafe fn set_entry(table: u64, index: usize, value: u64) {
        // SAFETY: as above.
        unsafe { core::ptr::write_volatile((table as *mut u64).add(index), value) }
    }

    /// Follow `table[index]`, allocating and zeroing a new table if it is empty.
    ///
    /// Intermediate entries are created permissive (`PRESENT | WRITABLE | USER`)
    /// because on x86-64 the effective permission is the AND of every level;
    /// the leaf entry is what actually restricts access.
    ///
    /// # Safety
    ///
    /// `table` must be an identity-mapped page table.
    unsafe fn next_table(&mut self, table: u64, index: usize) -> Result<u64, MapError> {
        // SAFETY: upheld by the caller.
        let existing = unsafe { Self::entry(table, index) };
        if existing & PRESENT != 0 {
            if existing & HUGE != 0 {
                return Err(MapError::OverlapsLargePage);
            }
            return Ok(existing & ADDRESS_MASK);
        }

        let new_table = self.pool.allocate().ok_or(MapError::OutOfTables)?;
        // SAFETY: the pool hands out identity-mapped, writable pages.
        unsafe {
            core::ptr::write_bytes(new_table as *mut u8, 0, PAGE_SIZE as usize);
            Self::set_entry(table, index, new_table | PRESENT | WRITABLE | USER);
        }
        Ok(new_table)
    }

    /// Map one 4 KiB page.
    ///
    /// # Safety
    ///
    /// The tables reachable from `self.root` must be identity mapped.
    pub unsafe fn map_page(&mut self, virt: u64, phys: u64, flags: u64) -> Result<(), MapError> {
        let pml4_index = ((virt >> 39) & 0x1FF) as usize;
        let pdpt_index = ((virt >> 30) & 0x1FF) as usize;
        let pd_index = ((virt >> 21) & 0x1FF) as usize;
        let pt_index = ((virt >> 12) & 0x1FF) as usize;

        // SAFETY: upheld by the caller and preserved by `next_table`, which
        // only ever returns identity-mapped pool pages.
        unsafe {
            let pdpt = self.next_table(self.root, pml4_index)?;
            let pd = self.next_table(pdpt, pdpt_index)?;

            // A 4 KiB mapping cannot be punched into an existing 2 MiB page.
            if Self::entry(pd, pd_index) & (PRESENT | HUGE) == (PRESENT | HUGE) {
                return Err(MapError::OverlapsLargePage);
            }
            let pt = self.next_table(pd, pd_index)?;
            Self::set_entry(pt, pt_index, (phys & ADDRESS_MASK) | flags | PRESENT);
        }
        Ok(())
    }

    /// Map one 2 MiB large page. `virt` and `phys` must be 2 MiB aligned.
    ///
    /// # Safety
    ///
    /// See [`AddressSpace::map_page`].
    pub unsafe fn map_large_page(
        &mut self,
        virt: u64,
        phys: u64,
        flags: u64,
    ) -> Result<(), MapError> {
        debug_assert!(virt.is_multiple_of(LARGE_PAGE_SIZE));
        debug_assert!(phys.is_multiple_of(LARGE_PAGE_SIZE));

        let pml4_index = ((virt >> 39) & 0x1FF) as usize;
        let pdpt_index = ((virt >> 30) & 0x1FF) as usize;
        let pd_index = ((virt >> 21) & 0x1FF) as usize;

        // SAFETY: upheld by the caller.
        unsafe {
            let pdpt = self.next_table(self.root, pml4_index)?;
            let pd = self.next_table(pdpt, pdpt_index)?;
            Self::set_entry(pd, pd_index, (phys & ADDRESS_MASK) | flags | PRESENT | HUGE);
        }
        Ok(())
    }

    /// Map `size` bytes starting at `phys` to `virt` using 2 MiB pages.
    ///
    /// `size` is rounded up to a whole number of large pages.
    ///
    /// # Safety
    ///
    /// See [`AddressSpace::map_page`].
    pub unsafe fn map_large_range(
        &mut self,
        virt: u64,
        phys: u64,
        size: u64,
        flags: u64,
    ) -> Result<(), MapError> {
        let pages = size.div_ceil(LARGE_PAGE_SIZE);
        for index in 0..pages {
            let offset = index * LARGE_PAGE_SIZE;
            // SAFETY: upheld by the caller.
            unsafe { self.map_large_page(virt + offset, phys + offset, flags)? };
        }
        Ok(())
    }

    /// Map `size` bytes starting at `phys` to `virt` using 4 KiB pages.
    ///
    /// # Safety
    ///
    /// See [`AddressSpace::map_page`].
    pub unsafe fn map_range(
        &mut self,
        virt: u64,
        phys: u64,
        size: u64,
        flags: u64,
    ) -> Result<(), MapError> {
        let pages = size.div_ceil(PAGE_SIZE);
        for index in 0..pages {
            let offset = index * PAGE_SIZE;
            // SAFETY: upheld by the caller.
            unsafe { self.map_page(virt + offset, phys + offset, flags)? };
        }
        Ok(())
    }
}

/// How many pool pages to reserve for a direct map covering `phys_limit`.
///
/// Budget: one PML4; one PDPT for each of the identity and direct maps; one
/// page directory per gibibyte in each map; the same again for the framebuffer
/// aperture, which sits at its own address far above RAM and so needs its own
/// PDPT and directories; plus headroom for the kernel image's PDPT/PD/PT chain
/// and the boot stack.
#[must_use]
pub fn table_pool_pages(phys_limit: u64, framebuffer_size: u64) -> usize {
    const GIB: u64 = 1024 * 1024 * 1024;
    let ram_directories = phys_limit.div_ceil(GIB) as usize;
    let framebuffer_directories = framebuffer_size.div_ceil(GIB) as usize;
    // 1 PML4
    // + 2 PDPTs and 2 sets of directories for the identity and direct maps
    // + 2 PDPTs and 2 sets of directories for the framebuffer aperture
    // + 32 pages of headroom for 4 KiB mappings.
    1 + 2 + 2 * ram_directories + 2 + 2 * framebuffer_directories + 32
}

/// Enable `EFER.NXE` so that [`NO_EXECUTE`] is a legal page-table bit.
///
/// Without this, setting bit 63 of an entry is a reserved-bit violation and
/// every access through it faults.
///
/// # Safety
///
/// Modifies a control register; must run with interrupts effectively quiescent,
/// which is the case throughout the bootloader.
pub unsafe fn enable_no_execute() {
    const IA32_EFER: u32 = 0xC000_0080;
    const EFER_NXE: u64 = 1 << 11;

    // SAFETY: `IA32_EFER` exists on every x86-64 CPU, and setting NXE only
    // widens what the paging structures may express.
    unsafe {
        let (low, high): (u32, u32);
        core::arch::asm!(
            "rdmsr",
            in("ecx") IA32_EFER,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
        let value = ((high as u64) << 32 | low as u64) | EFER_NXE;
        core::arch::asm!(
            "wrmsr",
            in("ecx") IA32_EFER,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Install `page_table_root`, switch to `stack_top`, and jump to `entry`.
///
/// `boot_info` is passed in `rdi`, matching the SysV C ABI the kernel entry
/// point is declared with.
///
/// # Safety
///
/// Every precondition of handing control to another program applies: the tables
/// must map the code at `entry` executable, the stack at `stack_top` writable,
/// and the currently executing instructions must remain mapped across the
/// `cr3` load. Boot services must already have been exited.
pub unsafe fn enter_kernel(page_table_root: u64, stack_top: u64, entry: u64, boot_info: u64) -> ! {
    // SAFETY: upheld by the caller. The identity map built above keeps this
    // very instruction stream valid after `cr3` changes; `rsp` is replaced
    // before anything touches the old firmware stack.
    unsafe {
        core::arch::asm!(
            "mov cr3, {root}",
            "mov rsp, {stack}",
            "xor rbp, rbp",
            // A zero return address terminates the kernel's first stack frame,
            // so unwinders and backtracers stop cleanly at the entry point.
            "push 0",
            "jmp {entry}",
            root = in(reg) page_table_root,
            stack = in(reg) stack_top,
            entry = in(reg) entry,
            in("rdi") boot_info,
            options(noreturn),
        );
    }
}
