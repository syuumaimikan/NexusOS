//! The boot-information handoff block.
//!
//! The Nexus bootloader builds one [`BootInfo`] in memory it has marked as
//! `MemoryKind::Bootloader`, then jumps to the kernel entry point with a
//! pointer to it in `rdi` (SysV C ABI, first integer argument).

/// Magic value in [`BootInfo::magic`]: ASCII `"NEXUSBI\0"`.
pub const BOOT_MAGIC: u64 = 0x0049_4253_5558_454E;

/// Current handoff revision.  Bumped on any incompatible layout change.
pub const BOOT_VERSION: u32 = 1;

/// How the firmware described a range of physical address space.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    /// Free RAM the kernel may hand to its physical allocator.
    Usable = 0,
    /// Firmware-reserved or MMIO; never allocate from this.
    Reserved = 1,
    /// ACPI tables; reclaimable once the kernel has parsed them.
    AcpiReclaimable = 2,
    /// ACPI non-volatile storage; must be preserved.
    AcpiNvs = 3,
    /// Allocated by the bootloader (boot info, page tables, memory map).
    Bootloader = 4,
    /// The loaded kernel image itself.
    KernelImage = 5,
    /// The linear framebuffer.
    Framebuffer = 6,
    /// Memory the firmware reported as defective.
    Defective = 7,
}

/// One normalized physical memory range.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MemoryRegion {
    /// Physical start address; always page aligned.
    pub start: u64,
    /// Length in 4 KiB pages.
    pub pages: u64,
    /// What this range is used for.
    pub kind: MemoryKind,
    pub _reserved: u32,
}

impl MemoryRegion {
    /// Exclusive end address of the region.
    #[inline]
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.start + self.pages * 4096
    }

    /// Length of the region in bytes.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.pages * 4096
    }

    /// Whether the region contains no pages.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.pages == 0
    }
}

/// Physical memory map handed to the kernel.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MemoryMapInfo {
    /// Physical address of a `[MemoryRegion; count]` array, sorted by `start`.
    pub regions_phys: u64,
    /// Number of entries in that array.
    pub count: u64,
}

/// Byte order of the framebuffer's colour channels.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Bytes in memory: blue, green, red, unused.
    Bgrx8888 = 0,
    /// Bytes in memory: red, green, blue, unused.
    Rgbx8888 = 1,
    /// A layout we could not classify; the framebuffer is unusable.
    Unknown = 2,
}

/// The linear framebuffer the firmware left us.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    /// Physical base address, or 0 when no framebuffer is available.
    pub phys_addr: u64,
    /// Total size of the framebuffer in bytes.
    pub size: u64,
    /// Visible width in pixels.
    pub width: u32,
    /// Visible height in pixels.
    pub height: u32,
    /// Distance between two scanlines, in *pixels* (may exceed `width`).
    pub stride: u32,
    /// Bytes occupied by one pixel.
    pub bytes_per_pixel: u32,
    /// Channel order.
    pub format: PixelFormat,
    pub _reserved: u32,
}

impl FramebufferInfo {
    /// Whether the firmware gave us a usable framebuffer.
    #[inline]
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.phys_addr != 0 && self.width != 0 && self.height != 0
    }
}

/// Everything the bootloader tells the kernel.
///
/// All addresses are *physical* unless the field name says otherwise; the
/// kernel reaches them through the direct map at
/// [`crate::layout::PHYS_MAP_BASE`].
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BootInfo {
    /// Must equal [`BOOT_MAGIC`].
    pub magic: u64,
    /// Must equal [`BOOT_VERSION`].
    pub version: u32,
    /// `size_of::<BootInfo>()` as written by the bootloader.
    pub size: u32,
    /// The linear framebuffer.
    pub framebuffer: FramebufferInfo,
    /// The normalized physical memory map.
    pub memory_map: MemoryMapInfo,
    /// Physical address of the ACPI RSDP, or 0 if the firmware exposed none.
    pub acpi_rsdp: u64,
    /// Physical base the kernel image was loaded at.
    pub kernel_phys_base: u64,
    /// Virtual base the kernel image is mapped at.
    pub kernel_virt_base: u64,
    /// Size of the loaded kernel image in bytes, page aligned.
    pub kernel_image_size: u64,
    /// Virtual base of the direct physical map (mirrors
    /// [`crate::layout::PHYS_MAP_BASE`], carried explicitly so the kernel can
    /// assert agreement with the bootloader).
    pub phys_map_base: u64,
    /// Highest physical address covered by the direct map.
    pub phys_map_limit: u64,
    /// Top of the initial kernel stack, as a virtual address.
    pub boot_stack_top: u64,
    /// Physical address of the root page table (the value loaded into `cr3`).
    pub page_table_root: u64,
    /// Physical address of the UEFI system table, for runtime services.
    pub uefi_system_table: u64,
    /// Nanosecond-resolution TSC frequency estimate, or 0 if unknown.
    pub tsc_frequency_hz: u64,
}

impl BootInfo {
    /// Validate the magic, version and size stamped by the bootloader.
    ///
    /// This is the kernel's only defence against being handed a structure built
    /// by an incompatible bootloader, so it is checked before any field is used.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.magic == BOOT_MAGIC
            && self.version == BOOT_VERSION
            && self.size as usize == core::mem::size_of::<BootInfo>()
    }
}
