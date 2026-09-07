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
// Exception and IRQ handlers must use the `x86-interrupt` calling convention:
// the compiler has to emit `iretq` and preserve every register the interrupted
// code was using, which no stable ABI expresses.
#![feature(abi_x86_interrupt)]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

mod arch;
mod framebuffer;
mod memory;
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

    // Descriptor tables, exception handlers and the timer. Until this runs, any
    // fault is a triple fault, so it happens as early as anything can.
    // SAFETY: single-threaded, and interrupts are still disabled — the CPU has
    // not been told to enable them since reset.
    unsafe { arch::interrupts::init(TIMER_FREQUENCY_HZ) };

    // Physical memory. Everything above this point runs on statically
    // allocated storage; everything after it can allocate.
    // SAFETY: called once, and `boot_info` was validated in `_start`.
    if let Err(error) = unsafe { memory::init(boot_info) } {
        kprintln!("FATAL: could not initialise physical memory: {error}");
        arch::halt_forever();
    }

    // The kernel heap, which is what makes `alloc` usable above this point.
    // SAFETY: called once, with the physical allocator running.
    if let Err(error) = unsafe { memory::heap::init() } {
        kprintln!("FATAL: could not initialise the kernel heap: {error}");
        arch::halt_forever();
    }

    // The bootloader's identity map has done its job. Dropping it turns a null
    // or small-integer pointer dereference into a fault instead of a silent
    // success, and takes physical memory out of reach of low addresses.
    //
    // SAFETY: the kernel runs at its higher-half address on the higher-half
    // boot stack, `boot_info` was copied out of identity-mapped memory in
    // `_start`, and every remaining physical access goes through the direct
    // map.
    unsafe { memory::paging::tear_down_identity_map() };
    kprintln!("[mem ] identity map torn down; low addresses now fault");

    self_test();
    inject_fault_if_requested();

    kprintln!();
    kprintln!("[boot] early initialisation complete");
    kprintln!("[boot] {} MiB of usable RAM", usable / (1024 * 1024));
    kprintln!("[boot] idling: memory management and scheduling are next");

    idle_loop()
}

/// Tick rate of the early timer.
///
/// 1000 Hz gives millisecond resolution, which is fine-grained enough for boot
/// timing and for calibrating the APIC timer later, and cheap enough that the
/// handler's cost does not matter before there is real work to preempt.
const TIMER_FREQUENCY_HZ: u32 = 1000;

/// Prove that interrupt dispatch works, rather than assuming it.
///
/// Both checks are cheap and both fail loudly. Discovering that the IDT is
/// wrong here, in three lines of output, is worth a great deal more than
/// discovering it later from a machine that reboots without saying why.
fn self_test() {
    kprintln!("[test] raising a breakpoint to verify exception dispatch");
    // SAFETY: vector 3 has a handler installed that reports and returns, so
    // execution resumes at the following instruction.
    unsafe {
        core::arch::asm!("int3", options(nomem, nostack));
    }

    kprintln!("[test] enabling interrupts and waiting for timer ticks");
    arch::interrupts::enable();

    // Spin until the timer proves itself, but not forever: if ticks never
    // arrive, say so instead of hanging with no explanation.
    let deadline = arch::read_tsc() + 10_000_000_000;
    while arch::pit::ticks() < 10 && arch::read_tsc() < deadline {
        core::hint::spin_loop();
    }

    let ticks = arch::pit::ticks();
    if ticks == 0 {
        kprintln!("[test] FAILED: no timer interrupts were delivered");
    } else {
        kprintln!("[test] timer is live: {ticks} ticks in the first few milliseconds");
    }

    memory_self_test();
    heap_self_test();
}

/// Exercise the kernel heap and cross-check the page tables that back it.
///
/// The heap allocator's own logic is covered by host tests in `nexus-mm`. What
/// only the machine can answer is whether the mappings the kernel built
/// actually point where it thinks: so this allocates through `alloc`, then
/// translates a heap address back to a physical one and reads the same bytes
/// through the direct map. Agreement between those two paths is the real proof
/// that `map_range` did what it claimed.
fn heap_self_test() {
    use alloc::boxed::Box;
    use alloc::collections::BTreeMap;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::fmt::Write;

    let before = memory::heap::stats();

    // A Vec large enough to force several reallocations, so growth and freeing
    // of the old buffers both happen.
    let mut values: Vec<u64> = Vec::new();
    for index in 0..50_000u64 {
        values.push(index * 3);
    }
    let sum: u64 = values.iter().sum();
    let expected: u64 = (0..50_000u64).map(|index| index * 3).sum();
    if sum != expected {
        kprintln!("[test] FAILED: Vec held {sum}, expected {expected}");
        return;
    }

    // The cross-check. Read the first element through the heap's virtual
    // address, then again through the direct map at whatever physical address
    // the page tables say that virtual address resolves to.
    let probe_virt = values.as_ptr() as u64;
    match memory::paging::translate(probe_virt) {
        Some(phys) => {
            // SAFETY: `phys` is what the page tables say backs `probe_virt`,
            // and the direct map covers all of physical memory.
            let through_direct_map = unsafe { (layout::phys_to_virt(phys) as *const u64).read() };
            if through_direct_map != values[0] {
                kprintln!(
                    "[test] FAILED: heap address {probe_virt:#018x} maps to {phys:#018x}, \
                     which holds {through_direct_map:#x} rather than {:#x}",
                    values[0]
                );
                return;
            }
        }
        None => {
            kprintln!("[test] FAILED: heap address {probe_virt:#018x} is not mapped");
            return;
        }
    }

    let boxed = Box::new([0xA5u8; 4096]);
    if boxed.iter().any(|&byte| byte != 0xA5) {
        kprintln!("[test] FAILED: a boxed array did not hold its contents");
        return;
    }

    let mut map = BTreeMap::new();
    for index in 0..1000u32 {
        map.insert(index, index * index);
    }
    if map.get(&999) != Some(&(999 * 999)) {
        kprintln!("[test] FAILED: BTreeMap lookup returned the wrong value");
        return;
    }

    let mut text = String::new();
    let _ = write!(text, "NexusOS heap {} entries", map.len());
    if text != "NexusOS heap 1000 entries" {
        kprintln!("[test] FAILED: string formatting produced {text:?}");
        return;
    }

    drop(values);
    drop(boxed);
    drop(map);
    drop(text);

    let after = memory::heap::stats();
    if after.used != before.used {
        kprintln!(
            "[test] FAILED: {} bytes leaked from the heap",
            after.used - before.used
        );
        return;
    }

    // The identity map should be gone by now, so a low address must no longer
    // translate. This is checked through the page tables rather than by
    // dereferencing, which would halt the machine.
    if memory::paging::translate(0x1000).is_some() {
        kprintln!("[test] FAILED: the identity map is still present");
        return;
    }

    kprintln!(
        "[test] heap verified: 50k-element Vec, 4 KiB Box, 1000-entry map, \
         translation cross-checked, nothing leaked"
    );
    kprintln!(
        "[heap] {} KiB used of {} KiB, {} free blocks",
        after.used / 1024,
        after.total / 1024,
        after.free_blocks
    );
}

/// Exercise the frame allocator against real memory.
///
/// The allocator's logic is covered by host tests in `nexus-mm`. What those
/// cannot check is the part that only exists on the machine: that the direct
/// map really does give every frame its own writable storage, and that the free
/// lists the allocator threads through those frames survive being written to
/// physical RAM. So this allocates, writes a distinct pattern into every frame,
/// reads it all back, and only then frees.
///
/// A frame handed out twice shows up here as a pattern that does not match.
fn memory_self_test() {
    const FRAMES: usize = 64;
    /// A recognisable, non-zero base for the per-frame stamp, so a mismatch is
    /// obviously a bad frame rather than uninitialised memory that happens to
    /// look plausible.
    const PATTERN: u64 = 0x4E45_5855_5300_0000;

    let Some(before) = memory::stats() else {
        kprintln!("[test] FAILED: the frame allocator did not start");
        return;
    };

    let mut frames = [0u64; FRAMES];
    let mut allocated = 0usize;

    for slot in frames.iter_mut() {
        let Some(frame) = memory::allocate_frame() else {
            break;
        };
        if frame == 0 {
            kprintln!("[test] FAILED: the allocator handed out physical frame 0");
            return;
        }
        if !frame.is_multiple_of(4096) {
            kprintln!("[test] FAILED: frame {frame:#x} is not page aligned");
            return;
        }
        *slot = frame;
        allocated += 1;
    }

    if allocated != FRAMES {
        kprintln!("[test] FAILED: only {allocated} of {FRAMES} frames were available");
        return;
    }

    // Stamp each frame with a value derived from its index, then verify every
    // one. If two allocations aliased, the second write clobbers the first and
    // the mismatch is caught here.
    for (index, &frame) in frames.iter().enumerate() {
        let pointer = layout::phys_to_virt(frame) as *mut u64;
        // SAFETY: the allocator owns this frame, it is mapped writable through
        // the direct map, and nothing else holds it.
        unsafe {
            pointer.write(PATTERN ^ index as u64);
        }
    }

    for (index, &frame) in frames.iter().enumerate() {
        let pointer = layout::phys_to_virt(frame) as *const u64;
        // SAFETY: as above; the frame is still allocated.
        let value = unsafe { pointer.read() };
        let expected = PATTERN ^ index as u64;
        if value != expected {
            kprintln!(
                "[test] FAILED: frame {frame:#x} held {value:#x}, expected {expected:#x}; \
                 two allocations overlap"
            );
            return;
        }
    }

    // A large block, which exercises splitting and the alignment guarantee that
    // page tables and DMA buffers depend on.
    const LARGE_ORDER: usize = 8; // 1 MiB
    let large = memory::allocate_block(LARGE_ORDER);
    if let Some(block) = large {
        if !block.is_multiple_of(4096u64 << LARGE_ORDER) {
            kprintln!("[test] FAILED: 1 MiB block {block:#x} is not 1 MiB aligned");
            return;
        }
        // SAFETY: the block came from `allocate_block` at this order and has
        // not been freed.
        unsafe { memory::free_block(block, LARGE_ORDER) };
    } else {
        kprintln!("[test] FAILED: could not allocate a 1 MiB block");
        return;
    }

    for &frame in frames.iter() {
        // SAFETY: each frame came from `allocate_frame` and is freed once.
        unsafe { memory::free_frame(frame) };
    }

    let Some(after) = memory::stats() else {
        kprintln!("[test] FAILED: the frame allocator disappeared");
        return;
    };

    if after.free_frames != before.free_frames {
        kprintln!(
            "[test] FAILED: {} frames leaked ({} free before, {} after)",
            before.free_frames as i64 - after.free_frames as i64,
            before.free_frames,
            after.free_frames
        );
        return;
    }

    kprintln!(
        "[test] frame allocator verified: {FRAMES} frames written and read back, \
         1 MiB block aligned, nothing leaked"
    );
    kprintln!("[mem ] {after}");
}

/// Take a deliberate fault, when the kernel was built to.
///
/// Enabled by the `inject-*` features and driven by `scripts/test-faults.ps1`.
/// Each one provokes a different path through the exception machinery, and each
/// build takes exactly one fault because a fault report ends in a halt.
///
/// In an ordinary build every branch below compiles away to nothing.
fn inject_fault_if_requested() {
    #[cfg(feature = "inject-page-fault")]
    {
        // A canonical higher-half address the bootloader never mapped: far
        // above the direct map's coverage and outside the kernel image.
        const UNMAPPED: u64 = 0xFFFF_A000_0000_0000;
        kprintln!("[test] fault injection: writing to unmapped {UNMAPPED:#018x}");
        // SAFETY: intentionally unsound. The whole point is to fault, and this
        // build exists only to verify that the fault is reported.
        unsafe {
            core::ptr::write_volatile(UNMAPPED as *mut u64, 0);
        }
        kprintln!("[test] FAILED: the write to unmapped memory did not fault");
    }

    #[cfg(feature = "inject-stack-overflow")]
    {
        kprintln!("[test] fault injection: overflowing the kernel stack");
        // Recursing past the bottom of the boot stack hits its guard page. The
        // page-fault handler then cannot push its own frame — the stack is
        // exactly what is broken — so the CPU escalates to a double fault,
        // which is why that handler runs on its own IST stack. This checks the
        // guard page and the IST together.
        overflow_stack(0);
        kprintln!("[test] FAILED: the stack overflow did not fault");
    }

    #[cfg(feature = "inject-divide-error")]
    {
        kprintln!("[test] fault injection: dividing by zero");
        // Built through volatile reads so the divisor is not a compile-time
        // zero, which the compiler would reject outright.
        let mut zero = 0u64;
        // SAFETY: a volatile access to a local; the volatility is what stops
        // the optimiser from folding the division away.
        let divisor = unsafe { core::ptr::read_volatile(&mut zero) };
        let dividend = arch::pit::ticks() | 1;
        // SAFETY: `div` by zero raises vector 0, which is the point.
        unsafe {
            core::arch::asm!(
                "div {divisor}",
                divisor = in(reg) divisor,
                inout("rax") dividend => _,
                inout("rdx") 0u64 => _,
                options(nomem, nostack),
            );
        }
        kprintln!("[test] FAILED: the division by zero did not fault");
    }
}

/// Recurse until the kernel stack runs into its guard page.
///
/// The volatile access after the recursive call is load-bearing: without it the
/// compiler turns this into a loop and the stack never grows.
#[cfg(feature = "inject-stack-overflow")]
// Recursing forever is exactly the intent: the guard page is what stops it.
#[allow(unconditional_recursion)]
fn overflow_stack(depth: u64) -> u64 {
    let mut frame = [depth; 32];
    // SAFETY: a volatile access to a local array, purely to consume stack.
    unsafe {
        core::ptr::write_volatile(frame.as_mut_ptr(), depth);
        let deeper = overflow_stack(depth + 1);
        core::ptr::read_volatile(frame.as_ptr()).wrapping_add(deeper)
    }
}

/// Park the processor until there is work for it.
///
/// This becomes the idle thread once the scheduler exists. `hlt` rather than a
/// spin so the host CPU is not burned while the guest has nothing to do.
fn idle_loop() -> ! {
    let mut last_report = 0u64;
    loop {
        arch::wait_for_interrupt();

        // A heartbeat every five seconds, which is how a boot test tells a
        // healthy idle apart from a hang.
        let uptime = arch::pit::uptime_ms();
        if uptime >= last_report + 5000 {
            last_report = uptime - (uptime % 5000);
            kprintln!(
                "[idle] uptime {}.{:03}s, {} ticks, {} spurious interrupts",
                uptime / 1000,
                uptime % 1000,
                arch::pit::ticks(),
                arch::interrupts::spurious_count()
            );
        }
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
