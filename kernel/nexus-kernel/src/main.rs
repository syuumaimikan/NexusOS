//! # The Nexus Kernel
//!
//! Entry point and early bring-up for NexusOS.
//!
//! The kernel is entered from the Nexus Bootloader with paging already
//! configured: the kernel image is mapped at its linked address, all physical
//! memory is mapped at [`layout::PHYS_MAP_BASE`], and `rsp` points into a
//! guarded boot stack. A pointer to the [`BootInfo`] handoff block arrives in
//! `rdi` per the SysV C ABI.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

mod arch;
mod framebuffer;
mod panic;
mod serial;
mod sync;

use nexus_abi::{layout, BootInfo, MemoryKind, MemoryRegion};

use framebuffer::{Color, Framebuffer};

/// Print a line to the kernel serial console.
#[macro_export]
macro_rules! kprintln {
    () => { $crate::serial::write_fmt(format_args!("\n")) };
    ($($arg:tt)*) => {
        $crate::serial::write_fmt(format_args!("{}\n", format_args!($($arg)*)))
    };
}

/// Print to the kernel serial console without a trailing newline.
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => { $crate::serial::write_fmt(format_args!($($arg)*)) };
}

/// The kernel entry point.
///
/// Declared `extern "sysv64"` because the bootloader passes the boot
/// information block in `rdi`, and placed in `.text.boot` so the linker script
/// puts it at the very start of the image.
///
/// # Safety
///
/// Called exactly once, by the bootloader, with `boot_info` pointing at a valid
/// [`BootInfo`]. Not callable from Rust.
#[no_mangle]
#[link_section = ".text.boot"]
pub extern "sysv64" fn _start(boot_info: *const BootInfo) -> ! {
    serial::init();

    kprintln!();
    kprintln!("=======================================================");
    kprintln!(" NexusOS  --  Nexus Kernel v{}", env!("CARGO_PKG_VERSION"));
    kprintln!("=======================================================");

    // The handoff block is still reachable through the bootloader's identity
    // map at this point, so the raw pointer is usable as-is. It is copied onto
    // the kernel stack immediately, before anything can tear that map down.
    if boot_info.is_null() {
        kprintln!("FATAL: the bootloader passed a null boot information pointer");
        arch::halt_forever();
    }
    // SAFETY: the bootloader guarantees a valid `BootInfo` here, and the
    // identity map that makes this address readable is still installed.
    let boot_info: BootInfo = unsafe { core::ptr::read(boot_info) };

    if !boot_info.is_valid() {
        kprintln!(
            "FATAL: bad boot handoff (magic {:#x}, version {}, size {})",
            boot_info.magic,
            boot_info.version,
            boot_info.size
        );
        arch::halt_forever();
    }

    if boot_info.phys_map_base != layout::PHYS_MAP_BASE {
        kprintln!(
            "FATAL: bootloader mapped physical memory at {:#x}, kernel expects {:#x}",
            boot_info.phys_map_base,
            layout::PHYS_MAP_BASE
        );
        arch::halt_forever();
    }

    kernel_main(&boot_info)
}

/// Early kernel bring-up, running on the boot stack with a validated handoff.
fn kernel_main(boot_info: &BootInfo) -> ! {
    report_handoff(boot_info);
    let usable = report_memory_map(boot_info);

    // SAFETY: the bootloader mapped the framebuffer through the direct map, and
    // this is the only `Framebuffer` the kernel constructs.
    match unsafe { Framebuffer::new(&boot_info.framebuffer) } {
        Some(mut fb) => {
            kprintln!("[fb  ] painting boot background");
            draw_boot_screen(&mut fb);
        }
        None => kprintln!("[fb  ] no usable framebuffer; running headless"),
    }

    kprintln!();
    kprintln!("[boot] early initialisation complete");
    kprintln!("[boot] {} MiB of usable RAM", usable / (1024 * 1024));
    kprintln!("[boot] idling: interrupts, memory management and scheduling are next");

    // Nothing schedules work yet, so park the core in a low-power idle rather
    // than spinning. Once interrupts are enabled this becomes the idle thread.
    loop {
        arch::wait_for_interrupt();
    }
}

/// Log what the bootloader handed over.
fn report_handoff(boot_info: &BootInfo) {
    kprintln!("[boot] handoff revision {}", boot_info.version);
    kprintln!(
        "[boot] kernel image: phys {:#018x} virt {:#018x} ({} KiB)",
        boot_info.kernel_phys_base,
        boot_info.kernel_virt_base,
        boot_info.kernel_image_size / 1024
    );
    kprintln!(
        "[boot] direct map:   {:#018x} covering {} MiB",
        boot_info.phys_map_base,
        boot_info.phys_map_limit / (1024 * 1024)
    );
    kprintln!(
        "[boot] page tables:  {:#018x} (cr3 reads {:#018x})",
        boot_info.page_table_root,
        arch::read_cr3()
    );
    kprintln!("[boot] boot stack:   {:#018x}", boot_info.boot_stack_top);

    if boot_info.acpi_rsdp == 0 {
        kprintln!("[acpi] no RSDP was provided");
    } else {
        kprintln!("[acpi] RSDP at {:#018x}", boot_info.acpi_rsdp);
    }

    let fb = &boot_info.framebuffer;
    if fb.is_valid() {
        kprintln!(
            "[fb  ] {}x{} stride {} at {:#018x} ({:?})",
            fb.width,
            fb.height,
            fb.stride,
            fb.phys_addr,
            fb.format
        );
    }
}

/// Log the physical memory map and return the number of usable bytes.
fn report_memory_map(boot_info: &BootInfo) -> u64 {
    let count = boot_info.memory_map.count as usize;
    // SAFETY: the bootloader allocated this array, kept it out of usable RAM,
    // and reported its length; the direct map makes it readable here.
    let regions: &[MemoryRegion] = unsafe {
        core::slice::from_raw_parts(
            layout::phys_to_virt(boot_info.memory_map.regions_phys) as *const MemoryRegion,
            count,
        )
    };

    kprintln!("[mem ] {count} physical memory regions:");

    let mut usable = 0u64;
    let mut reserved = 0u64;
    for region in regions {
        if region.kind == MemoryKind::Usable {
            usable += region.len();
        } else {
            reserved += region.len();
        }

        // A full dump is noisy on machines with many small firmware regions.
        // Ranges under 64 KiB are almost always firmware bookkeeping, so only
        // the substantial ones are listed individually.
        if region.len() >= 64 * 1024 {
            kprintln!(
                "[mem ]   {:#018x}..{:#018x}  {:>8} KiB  {:?}",
                region.start,
                region.end(),
                region.len() / 1024,
                region.kind
            );
        }
    }

    kprintln!(
        "[mem ] usable {} MiB, reserved {} MiB",
        usable / (1024 * 1024),
        reserved / (1024 * 1024)
    );
    usable
}

/// Paint the boot background.
///
/// This is deliberately simple: it exists to prove end to end that the
/// bootloader's mode selection, the framebuffer handoff and the direct map all
/// agree. The Nexus Compositor takes over this surface later.
fn draw_boot_screen(fb: &mut Framebuffer) {
    fb.vertical_gradient(Color::NEXUS_DEEP, Color(0x0014_2A4A));

    let width = fb.width();
    let height = fb.height();

    // A centred accent bar, sized as a fraction of the surface so it looks
    // right at any resolution the firmware gave us.
    let bar_width = (width / 3).max(64);
    let bar_height = (height / 90).max(4);
    let bar_x = (width - bar_width) / 2;
    let bar_y = height / 2;

    fb.fill_rect(bar_x, bar_y, bar_width, bar_height, Color::NEXUS_BLUE);

    // Three progress ticks below it, marking the boot stages reached so far:
    // bootloader, handoff, kernel entry.
    let tick = bar_height * 2;
    let gap = tick;
    let ticks_width = tick * 3 + gap * 2;
    let ticks_x = (width - ticks_width) / 2;
    let ticks_y = bar_y + bar_height * 4;
    for index in 0..3 {
        fb.fill_rect(
            ticks_x + index * (tick + gap),
            ticks_y,
            tick,
            tick,
            Color::WHITE,
        );
    }
}
