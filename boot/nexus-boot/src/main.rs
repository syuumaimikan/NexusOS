//! # The Nexus Bootloader
//!
//! A dependency-free UEFI application that prepares the machine for the Nexus
//! kernel and hands control to it.
//!
//! The sequence is:
//!
//! 1. bring up serial logging, so every later step is observable;
//! 2. disable the firmware watchdog;
//! 3. select a display mode and record the framebuffer;
//! 4. find the ACPI RSDP in the configuration table;
//! 5. read `\nexus\kernel.elf` off the EFI System Partition;
//! 6. load its `PT_LOAD` segments into physical memory;
//! 7. build page tables: an identity map, the direct physical map, the kernel
//!    image at its linked address, and a guarded boot stack;
//! 8. take the final memory map, normalize it, and exit boot services;
//! 9. install the new page tables and jump to the kernel.
//!
//! Nothing after step 8 may call firmware. Serial logging survives because it
//! is direct port I/O rather than a firmware service.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

use core::ffi::c_void;
use core::panic::PanicInfo;

use nexus_abi::{layout, BootInfo, MemoryMapInfo, MemoryRegion, BOOT_MAGIC, BOOT_VERSION};

use nexus_boot::uefi::{BootServices, Handle, MemoryType, Status, SystemTable};
use nexus_boot::{elf, fs, graphics, log, log_raw, memory, paging, serial, uefi};

/// Size of the stack the kernel starts on, before it builds its own.
const BOOT_STACK_SIZE: u64 = layout::KERNEL_BOOT_STACK_SIZE;

/// Allocate `pages` pages of `memory_type`, zeroed.
///
/// # Safety
///
/// `services` must point at live boot services.
unsafe fn allocate_zeroed_pages(
    services: &BootServices,
    pages: usize,
    memory_type: MemoryType,
) -> Option<u64> {
    let mut address: u64 = 0;
    // SAFETY: upheld by the caller; `address` is a valid out pointer.
    let status = unsafe {
        (services.allocate_pages)(
            uefi::AllocateType::AnyPages,
            memory_type,
            pages,
            &mut address,
        )
    };
    if uefi::is_error(status) {
        return None;
    }
    // SAFETY: the firmware just gave us `pages * 4096` identity-mapped,
    // writable bytes at `address`.
    unsafe { core::ptr::write_bytes(address as *mut u8, 0, pages * 4096) };
    Some(address)
}

/// Print a NUL-terminated UTF-16 string on the firmware console.
///
/// Used only for the boot banner, so that a machine with no serial cable still
/// shows that the loader started.
///
/// # Safety
///
/// `system_table` must point at a live system table with a valid `con_out`.
unsafe fn console_print(system_table: *mut SystemTable, text: &[u16]) {
    // SAFETY: upheld by the caller.
    unsafe {
        let con_out = (*system_table).con_out;
        if !con_out.is_null() {
            let _ = ((*con_out).output_string)(con_out, text.as_ptr());
        }
    }
}

/// Halt this processor permanently.
fn halt() -> ! {
    loop {
        // SAFETY: `cli` and `hlt` are always safe to execute in the boot
        // environment; this is the standard way to stop a core.
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Report a fatal boot failure and stop.
///
/// The bootloader has no recovery path: if it cannot hand off to a kernel there
/// is nothing else for it to do, and stopping with a clear serial message is
/// far more useful than continuing into undefined behaviour.
fn fail(message: &str) -> ! {
    log!("FATAL: {message}");
    log!("boot aborted");
    halt()
}

/// UEFI application entry point.
#[no_mangle]
pub extern "efiapi" fn efi_main(image_handle: Handle, system_table: *mut SystemTable) -> Status {
    serial::init();

    log_raw!("\n");
    log!("NexusOS bootloader v{}", env!("CARGO_PKG_VERSION"));
    log!("handoff revision {BOOT_VERSION}");

    // "NexusOS\r\n" as UTF-16, NUL terminated.
    const BANNER: [u16; 10] = [
        b'N' as u16,
        b'e' as u16,
        b'x' as u16,
        b'u' as u16,
        b's' as u16,
        b'O' as u16,
        b'S' as u16,
        b'\r' as u16,
        b'\n' as u16,
        0,
    ];
    // SAFETY: `system_table` is the firmware-provided table.
    unsafe { console_print(system_table, &BANNER) };

    // SAFETY: the firmware guarantees a valid system table with boot services.
    let boot_services = unsafe { (*system_table).boot_services };
    // SAFETY: as above.
    let services = unsafe { &*boot_services };

    // The firmware arms a five-minute watchdog before calling us. Disarm it so
    // a slow console or a debugger session cannot reset the machine mid-boot.
    // SAFETY: live boot services; a zero timeout is the documented "disable".
    let status = unsafe { (services.set_watchdog_timer)(0, 0, 0, core::ptr::null_mut()) };
    if uefi::is_error(status) {
        log!("warning: could not disable the watchdog timer ({status:#x})");
    }

    // SAFETY: live boot services.
    let framebuffer = unsafe { graphics::init(boot_services) };
    if framebuffer.is_valid() {
        log!(
            "framebuffer {}x{} stride {} at {:#x} ({} KiB, {:?})",
            framebuffer.width,
            framebuffer.height,
            framebuffer.stride,
            framebuffer.phys_addr,
            framebuffer.size / 1024,
            framebuffer.format
        );
    } else {
        log!("no usable framebuffer; the kernel will run headless");
    }

    // SAFETY: `system_table` is live.
    let acpi_rsdp = unsafe { memory::find_acpi_rsdp(system_table) };
    if acpi_rsdp == 0 {
        log!("warning: no ACPI RSDP in the configuration table");
    } else {
        log!("ACPI RSDP at {acpi_rsdp:#x}");
    }

    // SAFETY: `image_handle` is ours and boot services are live.
    let image = match unsafe { fs::load_kernel_image(image_handle, boot_services) } {
        Ok(image) => image,
        Err(error) => {
            log!("could not read the kernel image: {error}");
            fail("kernel image unavailable")
        }
    };
    log!("read kernel image, {} bytes", image.len());

    // Load the ELF segments into freshly allocated physical memory. The image
    // is allocated as runtime-services data so that the firmware, and later the
    // kernel's own allocator, treat it as off-limits.
    // SAFETY: the closure returns identity-mapped, writable pages, which is
    // what `elf::load` requires.
    let kernel = match unsafe {
        elf::load(image, |pages| {
            allocate_zeroed_pages(services, pages, MemoryType::RuntimeServicesData)
        })
    } {
        Ok(kernel) => kernel,
        Err(error) => {
            log!("could not load kernel image: {error}");
            fail("malformed kernel image")
        }
    };
    log!(
        "kernel loaded: entry {:#x}, virt {:#x}, phys {:#x}, {} KiB in {} segments",
        kernel.entry_point,
        kernel.virt_base,
        kernel.phys_base,
        kernel.image_size / 1024,
        kernel.segment_count
    );

    // Probe the memory map once to size the address space we need to cover.
    // This is only an upper bound; the authoritative map is taken later, right
    // before `ExitBootServices`.
    const TWO_MIB: u64 = 2 * 1024 * 1024;
    // Leave a 64 MiB margin: allocations made between here and the final map
    // can extend the highest address in use.
    let physical_limit = probe_ram_limit(services)
        .saturating_add(64 * 1024 * 1024)
        .div_ceil(TWO_MIB)
        * TWO_MIB;
    log!(
        "mapping {} MiB of physical memory (up to {physical_limit:#x})",
        physical_limit / (1024 * 1024)
    );

    // Build the kernel's address space.
    let pool_pages = paging::table_pool_pages(physical_limit, framebuffer.size);
    let Some(pool_base) =
        // SAFETY: live boot services.
        (unsafe { allocate_zeroed_pages(services, pool_pages, MemoryType::RuntimeServicesData) })
    else {
        fail("could not allocate page-table memory")
    };
    // SAFETY: the pool was just allocated, zeroed and is identity mapped.
    let pool = unsafe { paging::PagePool::new(pool_base, pool_pages) };
    let Ok(mut address_space) = paging::AddressSpace::new(pool) else {
        fail("could not allocate the root page table")
    };

    // SAFETY: every table reachable from the root lives in the identity-mapped
    // pool, which is what these mapping calls require.
    let mapping = unsafe {
        build_address_space(
            &mut address_space,
            physical_limit,
            &framebuffer,
            &kernel,
            services,
        )
    };
    let Ok(boot_stack_phys) = mapping else {
        fail("could not build the kernel address space")
    };
    log!(
        "address space ready: root {:#x}, {} table pages used",
        address_space.root(),
        address_space.table_pages_used()
    );

    // Reserve the handoff block and the memory-region array. Both must outlive
    // boot services, so they are runtime-services data too.
    let handoff_pages =
        1 + (memory::MAX_REGIONS * core::mem::size_of::<MemoryRegion>()).div_ceil(4096);
    let Some(handoff_phys) =
        // SAFETY: live boot services.
        (unsafe { allocate_zeroed_pages(services, handoff_pages, MemoryType::RuntimeServicesData) })
    else {
        fail("could not allocate the boot information block")
    };
    let boot_info_phys = handoff_phys;
    let regions_phys = handoff_phys + 4096;

    // Everything that can allocate has now run. Take the final map, describe it
    // to the kernel, and leave the firmware behind.
    // SAFETY: live boot services; the handoff buffers are allocated and sized.
    let region_count = unsafe {
        exit_boot_services(
            image_handle,
            services,
            regions_phys,
            kernel.phys_base,
            kernel.image_size,
        )
    };

    // --- No firmware calls beyond this point. ---

    let boot_info = BootInfo {
        magic: BOOT_MAGIC,
        version: BOOT_VERSION,
        size: core::mem::size_of::<BootInfo>() as u32,
        framebuffer,
        memory_map: MemoryMapInfo {
            regions_phys,
            count: region_count as u64,
        },
        acpi_rsdp,
        kernel_phys_base: kernel.phys_base,
        kernel_virt_base: kernel.virt_base,
        kernel_image_size: kernel.image_size,
        phys_map_base: layout::PHYS_MAP_BASE,
        phys_map_limit: physical_limit,
        boot_stack_top: layout::KERNEL_BOOT_STACK_BASE + BOOT_STACK_SIZE,
        page_table_root: address_space.root(),
        uefi_system_table: system_table as u64,
        tsc_frequency_hz: 0,
    };
    // SAFETY: `boot_info_phys` is a page we allocated, identity mapped, and
    // large enough for a `BootInfo`.
    unsafe { core::ptr::write(boot_info_phys as *mut BootInfo, boot_info) };

    log!("exited boot services; {region_count} memory regions");
    log!("boot stack at {boot_stack_phys:#x}, entering kernel");

    // `NO_EXECUTE` bits are set throughout the tables built above, so NXE has
    // to be on before `cr3` is loaded or every such entry faults.
    // SAFETY: interrupts are quiescent and NXE only widens what paging accepts.
    unsafe { paging::enable_no_execute() };

    // SAFETY: the identity map keeps this code addressable across the `cr3`
    // load, the kernel image is mapped executable at `entry_point`, and the
    // boot stack is mapped writable below `boot_stack_top`.
    unsafe {
        paging::enter_kernel(
            address_space.root(),
            layout::KERNEL_BOOT_STACK_BASE + BOOT_STACK_SIZE,
            kernel.entry_point,
            boot_info_phys,
        )
    }
}

/// Ask the firmware how high RAM goes.
///
/// This deliberately uses a stack buffer: calling `AllocatePool` here would
/// perturb the very map being measured.
fn probe_ram_limit(services: &BootServices) -> u64 {
    // 16 KiB holds roughly 400 descriptors, comfortably more than firmware
    // reports on the machines NexusOS targets today.
    let mut buffer = [0u8; 16 * 1024];
    let mut map_size = buffer.len();
    let mut map_key: usize = 0;
    let mut descriptor_size: usize = 0;
    let mut descriptor_version: u32 = 0;

    // SAFETY: all five out pointers are valid and `map_size` describes
    // `buffer` accurately.
    let status = unsafe {
        (services.get_memory_map)(
            &mut map_size,
            buffer.as_mut_ptr() as *mut uefi::MemoryDescriptor,
            &mut map_key,
            &mut descriptor_size,
            &mut descriptor_version,
        )
    };
    if uefi::is_error(status) || descriptor_size == 0 {
        // Fall back to 4 GiB: enough to cover the framebuffer and low RAM on
        // any machine that can run this loader at all.
        log!("warning: memory map probe failed ({status:#x}); assuming 4 GiB");
        return 4 * 1024 * 1024 * 1024;
    }

    memory::UefiMemoryMap::new(&buffer[..map_size], descriptor_size).ram_limit()
}

/// Establish every mapping the kernel needs, returning the boot stack's
/// physical base.
///
/// # Safety
///
/// `space`'s tables must be identity mapped, and `services` must be live.
unsafe fn build_address_space(
    space: &mut paging::AddressSpace,
    physical_limit: u64,
    framebuffer: &nexus_abi::FramebufferInfo,
    kernel: &elf::LoadedImage,
    services: &BootServices,
) -> Result<u64, paging::MapError> {
    // The identity map exists purely so that the instruction after `mov cr3`
    // is still fetchable. It stays executable for that reason, and the kernel
    // tears it down once it is running on its own stack.
    // SAFETY: upheld by the caller.
    unsafe {
        space.map_large_range(0, 0, physical_limit, paging::WRITABLE)?;
    }

    // The direct map is the kernel's permanent window onto physical memory. It
    // is never executed from, so it is mapped non-executable, and it is global
    // because it is identical in every address space.
    // SAFETY: upheld by the caller.
    unsafe {
        space.map_large_range(
            layout::PHYS_MAP_BASE,
            0,
            physical_limit,
            paging::WRITABLE | paging::NO_EXECUTE | paging::GLOBAL,
        )?;
    }

    // The framebuffer usually sits in the PCI aperture, well above the last
    // byte of RAM, so the direct map built above does not reach it. Extend the
    // direct map over it explicitly, keeping `phys_to_virt` valid for
    // framebuffer addresses. Both maps are needed: the identity one so the
    // loader could still touch it, the direct one because that is where the
    // kernel looks.
    if framebuffer.is_valid() {
        const TWO_MIB: u64 = 2 * 1024 * 1024;
        let start = framebuffer.phys_addr & !(TWO_MIB - 1);
        let end = framebuffer
            .phys_addr
            .saturating_add(framebuffer.size)
            .div_ceil(TWO_MIB)
            * TWO_MIB;

        if start >= physical_limit {
            // SAFETY: upheld by the caller.
            unsafe {
                space.map_large_range(start, start, end - start, paging::WRITABLE)?;
                space.map_large_range(
                    layout::phys_to_virt(start),
                    start,
                    end - start,
                    paging::WRITABLE | paging::NO_EXECUTE | paging::GLOBAL,
                )?;
            }
        }
    }

    // The kernel image, one segment at a time, so that `.text` stays read-only
    // and executable while `.data` stays writable and non-executable.
    for index in 0..kernel.segment_count {
        let segment = kernel.segments[index];
        let mut flags = paging::GLOBAL;
        if segment.flags.writable {
            flags |= paging::WRITABLE;
        }
        if !segment.flags.executable {
            flags |= paging::NO_EXECUTE;
        }
        // SAFETY: upheld by the caller.
        unsafe {
            space.map_range(segment.virt_start, segment.phys_start, segment.size, flags)?;
        }
    }

    // The boot stack is mapped at a dedicated address rather than through the
    // direct map, so that the unmapped page below it acts as a guard page: a
    // stack overflow faults instead of quietly corrupting physical memory.
    let stack_pages = (BOOT_STACK_SIZE / 4096) as usize;
    // SAFETY: live boot services.
    let stack_phys =
        unsafe { allocate_zeroed_pages(services, stack_pages, MemoryType::RuntimeServicesData) }
            .ok_or(paging::MapError::OutOfTables)?;
    // SAFETY: upheld by the caller.
    unsafe {
        space.map_range(
            layout::KERNEL_BOOT_STACK_BASE,
            stack_phys,
            BOOT_STACK_SIZE,
            paging::WRITABLE | paging::NO_EXECUTE | paging::GLOBAL,
        )?;
    }

    Ok(stack_phys)
}

/// Take the final memory map, write it out for the kernel, and exit boot
/// services. Returns the number of regions written.
///
/// UEFI requires the `map_key` passed to `ExitBootServices` to match the most
/// recent `GetMemoryMap`. Any allocation invalidates it, so this retries a
/// bounded number of times.
///
/// # Safety
///
/// `services` must be live, and `regions_phys` must point at storage for
/// [`memory::MAX_REGIONS`] [`MemoryRegion`]s.
unsafe fn exit_boot_services(
    image_handle: Handle,
    services: &BootServices,
    regions_phys: u64,
    kernel_phys: u64,
    kernel_size: u64,
) -> usize {
    // Sized generously and allocated once, before the loop: allocating inside
    // it would invalidate the key it just obtained.
    const MAP_BUFFER_BYTES: usize = 32 * 1024;
    let mut buffer: *mut c_void = core::ptr::null_mut();
    // SAFETY: upheld by the caller; `buffer` is a valid out pointer.
    let status =
        unsafe { (services.allocate_pool)(MemoryType::LoaderData, MAP_BUFFER_BYTES, &mut buffer) };
    if uefi::is_error(status) {
        fail("could not allocate a buffer for the final memory map")
    }

    // SAFETY: `regions_phys` is identity-mapped storage for `MAX_REGIONS`
    // regions, as the caller guarantees.
    let regions = unsafe {
        core::slice::from_raw_parts_mut(regions_phys as *mut MemoryRegion, memory::MAX_REGIONS)
    };

    // Five attempts is ample: the map only changes if firmware ran an event
    // handler in between, and nothing here allocates.
    for attempt in 0..5 {
        let mut map_size = MAP_BUFFER_BYTES;
        let mut map_key: usize = 0;
        let mut descriptor_size: usize = 0;
        let mut descriptor_version: u32 = 0;

        // SAFETY: all out pointers are valid; `map_size` describes `buffer`.
        let status = unsafe {
            (services.get_memory_map)(
                &mut map_size,
                buffer as *mut uefi::MemoryDescriptor,
                &mut map_key,
                &mut descriptor_size,
                &mut descriptor_version,
            )
        };
        if uefi::is_error(status) {
            log!("GetMemoryMap failed on attempt {attempt} ({status:#x})");
            continue;
        }

        // SAFETY: the firmware wrote `map_size` bytes into `buffer`.
        let raw = unsafe { core::slice::from_raw_parts(buffer as *const u8, map_size) };
        let map = memory::UefiMemoryMap::new(raw, descriptor_size);
        let count = memory::normalize(&map, kernel_phys, kernel_size, regions);

        // SAFETY: `map_key` is from the `GetMemoryMap` immediately above and
        // nothing has allocated since.
        let status = unsafe { (services.exit_boot_services)(image_handle, map_key) };
        if !uefi::is_error(status) {
            return count;
        }
        log!("ExitBootServices rejected the map key on attempt {attempt}; retrying");
    }

    fail("could not exit boot services")
}

/// Last-resort handler: report the panic on the serial port and stop.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    log_raw!("\n");
    log!("PANIC in the bootloader");
    if let Some(location) = info.location() {
        log!("  at {}:{}", location.file(), location.line());
    }
    log!("  {}", info.message());
    halt()
}
