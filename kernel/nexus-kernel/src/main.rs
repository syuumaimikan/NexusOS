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

mod acpi;
mod arch;
mod display;
mod drivers;
mod framebuffer;
mod fs;
mod i18n;
mod input;
mod ipc;
mod memory;
mod panic;
mod process;
mod sched;
mod serial;
mod sync;
mod user;

use nexus_abi::{layout, BootInfo, MemoryKind, MemoryRegion};

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

    // Descriptor tables, exception handlers and the timer. Until this runs, any
    // fault is a triple fault, so it happens as early as anything can.
    // SAFETY: single-threaded, and interrupts are still disabled — the CPU has
    // not been told to enable them since reset.
    unsafe { arch::interrupts::init(TIMER_FREQUENCY_HZ) };

    // Interrupts come on here, as soon as there is somewhere for them to go.
    // Everything after this point may depend on the clock advancing -- APIC
    // calibration does, and it hangs silently without it.
    arch::interrupts::enable();
    kprintln!("[intr] interrupts enabled");

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

    // Adopt the running page tables as the kernel's own, and fill in every
    // upper-half top-level entry. Both have to happen before any process
    // exists: a new address space copies those entries, and copying is only
    // sound if they never change afterwards.
    // SAFETY: called once, on the boot processor, with the frame allocator
    // running and no address space yet created.
    if let Err(error) = unsafe { memory::address_space::init() } {
        kprintln!("FATAL: could not adopt the kernel address space: {error:?}");
        arch::halt_forever();
    }

    // ACPI and the local APIC. Both wait until here: the APIC's registers sit
    // far above RAM, so reaching them needs the virtual memory manager, and
    // calibrating its timer needs the PIT still ticking with interrupts on.
    // SAFETY: called once, with the direct map covering the firmware tables.
    match unsafe { acpi::init(boot_info.acpi_rsdp) } {
        Ok(info) => {
            acpi::report(&info);
            adopt_local_apic(&info);
            bring_up_device_interrupts(&info);
            start_other_processors(&info);
        }
        Err(error) => {
            // Not fatal. The PIT keeps the system ticking on one processor;
            // what is lost is SMP, MSI and per-core timers.
            kprintln!("[acpi] {error}; continuing on the legacy timer");
        }
    }

    // What the machine has. Everything that is not on the processor is behind
    // PCI, so this is the first thing the kernel does that is about the machine
    // rather than the CPU.
    // SAFETY: called once, before anything drives a device.
    let devices = unsafe { drivers::pci::enumerate() };
    drivers::pci::report(&devices);

    // The disk. Not fatal if there is none: everything else works without it,
    // and saying so beats refusing to boot a machine that has no storage the
    // kernel can drive yet.
    // SAFETY: called once, after enumeration, with the allocators running.
    if let Err(error) = unsafe { drivers::virtio_blk::init(&devices) } {
        kprintln!("[blk ] no block device: {error}");
    }

    // The display comes up only now, after the heap: translated strings are
    // built at runtime by substituting into templates, so drawing anything
    // localised allocates. Bringing the display up earlier cost a boot to an
    // allocation failure, which is exactly the kind of ordering mistake that
    // only shows up when the code is actually run.
    //
    // Pick the interface language before anything is drawn. A real system would
    // take this from stored settings; there is no storage yet, so it is a build
    // constant, and an unrecognised one falls back rather than leaving the
    // system with no strings.
    if !i18n::set_locale(DEFAULT_LANGUAGE) {
        kprintln!(
            "[i18n] no locale {DEFAULT_LANGUAGE}; using {}",
            i18n::current().tag
        );
    }
    kprintln!(
        "[i18n] interface language {}, {} available",
        i18n::current().tag,
        i18n::locale_count()
    );

    // SAFETY: the bootloader mapped the framebuffer through the direct map, and
    // this is the only place the kernel adopts it.
    unsafe { display::init(&boot_info.framebuffer) };

    // The scheduler. The context that got us here becomes thread #0 and keeps
    // running; from this point on it is preemptible like any other thread.
    // SAFETY: called once, from the boot context, with the heap available.
    if let Err(error) = unsafe { sched::init() } {
        kprintln!("FATAL: could not start the scheduler: {error}");
        arch::halt_forever();
    }

    self_test();
    inject_fault_if_requested();

    kprintln!();
    kprintln!("[boot] early initialisation complete");
    kprintln!("[boot] {} MiB of usable RAM", usable / (1024 * 1024));

    start_system_threads();

    // The boot thread has finished its work. Retiring it hands the processor to
    // the scheduler for good; the idle thread covers the moments when nothing
    // else is runnable.
    kprintln!("[boot] boot thread retiring; the system is now scheduler-driven");
    sched::exit()
}

/// Spawn the long-lived threads and hand the processor over to them.
fn start_system_threads() {
    display::start_thread();
    input::start_thread();

    // The first thing NexusOS runs that it does not trust. Not fatal if it
    // fails: the system is less of a system without it, but it is still one.
    // SAFETY: the heap and the scheduler are both running by now, and this is
    // the only call.
    if let Err(error) = unsafe { user::start() } {
        kprintln!("[user] could not start user mode: {error}");
    }
    match sched::spawn(
        "monitor",
        sched::thread::Priority::Interactive,
        monitor_thread,
        0,
    ) {
        Ok(id) => kprintln!("[boot] started monitor thread {id}"),
        Err(error) => kprintln!("[boot] could not start the monitor thread: {error}"),
    }
    kprintln!("[boot] handing the processor to the scheduler");
}

/// Reports system state periodically, and reclaims finished threads.
///
/// Reaping belongs in a thread other than the one that finished: a thread
/// cannot free the stack it is standing on, so a finished thread's storage is
/// released here, once it is certain nothing is running on it.
fn monitor_thread(_argument: usize) {
    let mut reported = 0u64;
    loop {
        sched::sleep_ms(5000);

        let reaped = sched::reap_finished();
        let stats = sched::stats();
        let memory = memory::stats();
        let heap = memory::heap::stats();
        reported += 5;

        kprintln!(
            "[mon ] {reported}s uptime {}.{:03}s | threads {} ({} running, {} ready, {} sleeping, \
             {} blocked) | {} switches, {} awaiting reaping{}",
            arch::time::uptime_ms() / 1000,
            arch::time::uptime_ms() % 1000,
            stats.threads,
            // Now worth reporting: with every processor scheduling, more than
            // one thread is running at a time, and a count stuck at one would
            // say the other cores had stopped taking work.
            stats.running,
            stats.ready,
            stats.sleeping,
            // Blocked, not sleeping: no amount of time will wake these. The
            // input thread lives here between keystrokes.
            stats.blocked,
            stats.context_switches,
            stats.finished,
            if reaped > 0 { " | reaped threads" } else { "" }
        );
        if let Some(memory) = memory {
            kprintln!(
                "[mon ] memory {} MiB free | heap {} KiB used of {} KiB",
                memory.free_frames * 4096 / (1024 * 1024),
                heap.used / 1024,
                heap.total / 1024
            );
        }
        kprintln!(
            "[mon ] {} threads waiting for a key",
            drivers::keyboard::waiting_threads()
        );
        let (received, dropped, decoded) = drivers::keyboard::statistics();
        if received > 0 {
            // The decoded line is included on purpose: a scancode count says
            // interrupts arrived, but only the text says the scancode table,
            // the modifier handling and the delivery to the input thread are
            // all correct.
            kprintln!(
                "[mon ] keyboard: {received} scancodes, {decoded} keys decoded,                  {} acted on{}, line \"{}\"",
                input::handled_count(),
                if dropped > 0 { " (some dropped)" } else { "" },
                input::line()
            );
        }
        if arch::smp::processor_count() > 1 {
            let (online, fewest) = arch::smp::summary();
            kprintln!(
                "[mon ] {online} processors online, least busy has taken {fewest} interrupts"
            );

            // Reported because a shootdown that is silently not happening looks
            // exactly like one that is: the symptom of a missing one is a stale
            // translation nobody notices. A count that moves when threads are
            // reaped is the evidence that it runs at all, and a timeout count
            // above zero says a processor stopped answering.
            let (shootdowns, timeouts) = arch::tlb::statistics();
            kprintln!("[mon ] {shootdowns} TLB shootdowns broadcast, {timeouts} unacknowledged");
        }

        // Interrupts taken from ring 3 are the evidence that user code ran at
        // user privilege and was preemptible while it did. System calls alone
        // would not say either: `syscall` is legal from ring 0 too.
        // Cheap, and it is the one invariant whose violation is silent: a
        // ready thread that is on no queue simply never runs again.
        sched::check_run_queues();

        let (spaces, freed) = memory::address_space::statistics();
        if spaces > 0 {
            let (started, ended) = process::statistics();
            let (channels, sent, taken) = ipc::statistics();
            kprintln!(
                "[mon ] {started} processes started, {ended} ended |                  {spaces} address spaces created, {freed} freed"
            );
            let (shared, released) = ipc::memory_statistics();
            kprintln!(
            "[mon ] {channels} channels, {sent} messages sent, {taken} received |              {shared} shared pages made, {released} released"
        );
            if drivers::virtio_blk::is_present() {
                let (read, wrote) = drivers::virtio_blk::statistics();
                kprintln!(
                    "[mon ] disk {} sectors, {read} read, {wrote} written",
                    drivers::virtio_blk::capacity()
                );
            }
        }
        let (calls, unknown) = arch::syscall::statistics();
        let (entered, returned) = arch::syscall::yield_statistics();
        if entered != returned {
            kprintln!("[mon ] {entered} yields begun, {returned} returned");
        }
        let from_user = arch::idt::entries_from_user();
        kprintln!(
            "[mon ] {calls} system calls ({unknown} unimplemented),              {from_user} interrupts taken from ring 3"
        );
    }
}

/// Move the system tick from the PIT to the local APIC timer.
///
/// Failure is reported and tolerated: the PIT is still ticking, so the system
/// keeps running on one processor rather than not at all.
fn adopt_local_apic(info: &acpi::AcpiInfo) {
    // SAFETY: the virtual memory manager is up, the PIT is running and
    // interrupts are enabled, which is what calibration requires.
    let result = unsafe {
        arch::apic::init(
            info.local_apic_address,
            u64::from(TIMER_FREQUENCY_HZ),
            arch::interrupts::APIC_TIMER_VECTOR,
        )
    };

    match result {
        Ok(()) => {
            // The APIC drives the clock now. Silence the 8259 rather than
            // leaving it to deliver interrupts nothing is expecting, and stop
            // the PIT so it is not counting down for no one.
            // SAFETY: nothing depends on legacy interrupt delivery any more.
            unsafe {
                arch::pic::mask_all();
                arch::pit::stop();
                // Only now: until the PIC is quiet, LINT0 is how its interrupts
                // reach this processor at all.
                arch::apic::disconnect_legacy_pic();
            }
            kprintln!("[apic] legacy PIC masked, PIT stopped, LINT0 disconnected");

            // The boot processor's own per-processor state. It could not be
            // installed earlier because its APIC identifier was not known, and
            // nothing before this point reads it.
            // SAFETY: called once, on the boot processor, with index 0 which
            // no other processor uses.
            unsafe {
                arch::percpu::install(
                    0,
                    arch::apic::local_id(),
                    nexus_abi::layout::KERNEL_BOOT_STACK_BASE
                        + nexus_abi::layout::KERNEL_BOOT_STACK_SIZE,
                );

                // The system-call boundary, which needs both the descriptor
                // table and per-CPU state and so cannot be opened before here.
                arch::syscall::init();
            }
            arch::syscall::report();
        }
        Err(error) => kprintln!("[apic] {error}; staying on the legacy timer"),
    }
}

/// Bring up the I/O APIC and route the devices the kernel has drivers for.
///
/// Reported and tolerated on failure: without it the system has no input, which
/// is a poorer system rather than no system.
fn bring_up_device_interrupts(info: &acpi::AcpiInfo) {
    // The legacy IRQ number is not the pin number. Firmware says what it really
    // is, and assuming otherwise programs the wrong pin on a great many
    // machines -- on this very platform IRQ 0 turns out to be global interrupt 2.
    let irq = drivers::keyboard::KEYBOARD_IRQ;
    let gsi = info.global_system_interrupt_for(irq);

    // Pick the I/O APIC that actually serves this interrupt rather than the
    // first one listed: a machine with several splits the range between them.
    let Some((io_apic, _pin)) = info.route(gsi) else {
        kprintln!("[ioapic] no I/O APIC serves global interrupt {gsi}; input is unavailable");
        return;
    };

    // SAFETY: called once, with the virtual memory manager running, using the
    // window firmware reported.
    if let Err(error) = unsafe { arch::ioapic::init(io_apic.address, io_apic.gsi_base) } {
        kprintln!("[ioapic] {error}; devices cannot raise interrupts");
        return;
    }

    let (active_low, level) = info.override_for(irq).map_or((false, false), |entry| {
        (entry.is_active_low(), entry.is_level_triggered())
    });

    // SAFETY: the controller is drained and scanning enabled before the pin is
    // unmasked, so the first interrupt has a byte to read and a handler to
    // read it.
    unsafe {
        drivers::keyboard::drain_controller();
        drivers::keyboard::enable_scanning();
        drivers::keyboard::drain_controller();

        match arch::ioapic::route(
            gsi,
            arch::interrupts::KEYBOARD_VECTOR,
            arch::apic::local_id(),
            active_low,
            level,
        ) {
            Ok(()) => kprintln!(
                "[input] keyboard on IRQ {irq} (global interrupt {gsi}) routed to vector {}",
                arch::interrupts::KEYBOARD_VECTOR
            ),
            Err(error) => kprintln!("[input] could not route the keyboard: {error}"),
        }
    }
}

/// Start the processors ACPI reported, other than this one.
///
/// Reported and tolerated on failure: a system running on fewer cores than it
/// has is far better than one that refuses to boot.
fn start_other_processors(info: &acpi::AcpiInfo) {
    if info.enabled_processor_count() <= 1 {
        kprintln!("[smp ] single processor; nothing to start");
        return;
    }

    // SAFETY: called once, from the boot processor, with the local APIC running
    // and the memory manager available.
    match unsafe { arch::smp::start_processors(&info.processors) } {
        Ok(started) => {
            kprintln!(
                "[smp ] {started} of {} additional processors started",
                info.enabled_processor_count() - 1
            );
            arch::smp::report();
        }
        Err(error) => kprintln!("[smp ] {error}; continuing on one processor"),
    }
}

/// Interface language selected at boot.
///
/// A build constant only because there is nowhere to persist a setting yet.
/// Both available languages are exercised at runtime regardless: the display
/// cycles between them, which is how the switch is shown to work rather than
/// merely compiled.
const DEFAULT_LANGUAGE: &str = "en-US";

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

    kprintln!("[test] waiting for timer ticks");

    // Spin until the timer proves itself, but not forever: if ticks never
    // arrive, say so instead of hanging with no explanation.
    let deadline = arch::read_tsc() + 10_000_000_000;
    while arch::time::ticks() < 10 && arch::read_tsc() < deadline {
        core::hint::spin_loop();
    }

    let ticks = arch::time::ticks();
    if ticks == 0 {
        kprintln!("[test] FAILED: no timer interrupts were delivered");
    } else {
        kprintln!("[test] timer is live: {ticks} ticks in the first few milliseconds");
    }

    memory_self_test();
    heap_self_test();
    scheduler_self_test();
    tlb_self_test();
    ipc_self_test();
    disk_self_test();
    filesystem_self_test();
}

/// The file the filesystem test reads, and what it must contain.
///
/// Fixed on both sides: the image builder writes exactly this, so a reader that
/// found *a* file rather than *the* file is caught by what it says rather than
/// by whether it managed to read something.
const FS_TEST_FILE: &str = "HELLO.TXT";
/// The file one directory down, which is what makes path walking a thing the
/// reader is tested on rather than a thing it merely contains code for.
const FS_DEEP_FILE: &str = "TESTS/DEEP.TXT";
const FS_DEEP_CONTENTS: &str = "This file is one directory down.\r\n";
/// A file longer than one cluster, so that following a chain is tested.
const FS_CHAIN_FILE: &str = "CHAIN.BIN";
const FS_CHAIN_SIZE: usize = 5000;
const FS_TEST_CONTENTS: &str = "NexusOS reads its own filesystem.\r\n";

/// Read the partition table and the filesystem inside it.
///
/// The two readers are tested together because neither is worth much alone: a
/// partition table that parses says nothing until something is read through it,
/// and a filesystem reader has to be pointed at a partition by something.
fn filesystem_self_test() {
    use fs::{fat32, gpt};

    if !drivers::virtio_blk::is_present() {
        return;
    }

    let partitions = match gpt::read() {
        Ok(partitions) => partitions,
        Err(error) => {
            kprintln!("[test] FAILED: could not read the partition table: {error}");
            return;
        }
    };

    for partition in &partitions {
        kprintln!(
            "[gpt ] partition {} \"{}\": sectors {}..{} ({} MiB){}",
            partition.index,
            partition.name.as_str(),
            partition.first_lba,
            partition.last_lba,
            partition.sectors() * drivers::virtio_blk::SECTOR_SIZE as u64 / (1024 * 1024),
            if partition.is_esp() {
                ", EFI system partition"
            } else {
                ""
            }
        );
    }

    let Some(esp) = partitions.iter().find(|partition| partition.is_esp()) else {
        kprintln!("[test] FAILED: the disk has no EFI system partition");
        return;
    };

    let volume = match fat32::Volume::mount(esp.first_lba) {
        Ok(volume) => volume,
        Err(error) => {
            kprintln!("[test] FAILED: could not mount the EFI system partition: {error}");
            return;
        }
    };

    kprintln!(
        "[fat ] \"{}\" mounted at sector {}: {} clusters of {} bytes",
        volume.label.as_str(),
        volume.start_lba(),
        volume.cluster_count(),
        volume.cluster_bytes()
    );

    let root = match volume.root() {
        Ok(root) => root,
        Err(error) => {
            kprintln!("[test] FAILED: could not read the root directory: {error}");
            return;
        }
    };
    for entry in &root {
        kprintln!(
            "[fat ]   {}{} {} bytes",
            entry.name.as_str(),
            if entry.is_directory { "/" } else { "" },
            entry.size
        );
    }

    let contents = match volume.read_file(FS_TEST_FILE) {
        Ok(contents) => contents,
        Err(error) => {
            kprintln!("[test] FAILED: could not read {FS_TEST_FILE}: {error}");
            return;
        }
    };

    // The length first, because a reader that returned the whole last cluster
    // instead of the file would otherwise pass a prefix comparison.
    if contents.len() != FS_TEST_CONTENTS.len() {
        kprintln!(
            "[test] FAILED: {FS_TEST_FILE} is {} bytes, expected {}",
            contents.len(),
            FS_TEST_CONTENTS.len()
        );
        return;
    }
    if contents != FS_TEST_CONTENTS.as_bytes() {
        kprintln!("[test] FAILED: {FS_TEST_FILE} does not contain what it should");
        return;
    }

    // A file one directory down, so that walking a path is tested rather than
    // merely present. A reader that ignored everything but the last component
    // would pass every check above this one.
    match volume.read_file(FS_DEEP_FILE) {
        Ok(deep) => {
            if deep != FS_DEEP_CONTENTS.as_bytes() {
                kprintln!("[test] FAILED: {FS_DEEP_FILE} does not contain what it should");
                return;
            }
        }
        Err(error) => {
            kprintln!("[test] FAILED: could not read {FS_DEEP_FILE}: {error}");
            return;
        }
    }

    // And a file longer than a cluster, so that following a chain is tested.
    // Its contents depend on the offset, so clusters stitched together in the
    // wrong order fail rather than merely being the right length.
    match volume.read_file(FS_CHAIN_FILE) {
        Ok(chain) => {
            if chain.len() != FS_CHAIN_SIZE {
                kprintln!(
                    "[test] FAILED: {FS_CHAIN_FILE} is {} bytes, expected {FS_CHAIN_SIZE}",
                    chain.len()
                );
                return;
            }
            for (index, byte) in chain.iter().enumerate() {
                let expected = (index as u8).wrapping_mul(31).wrapping_add(7);
                if *byte != expected {
                    kprintln!("[test] FAILED: {FS_CHAIN_FILE} differs at byte {index}");
                    return;
                }
            }
        }
        Err(error) => {
            kprintln!("[test] FAILED: could not read {FS_CHAIN_FILE}: {error}");
            return;
        }
    }

    // And a name that is not there has to come back as an error rather than as
    // whatever happened to be next in the directory.
    if volume.read_file("NOSUCH.TXT") != Err(fat32::FatError::NotFound) {
        kprintln!("[test] FAILED: reading a file that does not exist was not refused");
        return;
    }

    kprintln!(
        "[test] filesystem verified: {} partitions, {} entries in the root, {FS_TEST_FILE} read \
         and matched",
        partitions.len(),
        root.len()
    );
}

/// The sector the write test uses.
///
/// Inside the gap between the partition table and the first partition, which is
/// a megabyte of nothing on any disk laid out this century. Writing anywhere
/// inside the filesystem would be writing on the thing the next boot has to
/// read to start at all.
const DISK_SCRATCH_SECTOR: u64 = 200;

/// Read and write the disk.
///
/// The image is a real one: a protective master boot record, a GPT, and a FAT32
/// partition the firmware boots from. Everything outside the partition keeps its
/// own sector number written as text, and that is what the driver is tested
/// against -- the failure a block driver has to be caught making is fetching a
/// *different* sector than it was asked for, and a disk of zeroes cannot tell
/// that apart from working.
fn disk_self_test() {
    use drivers::virtio_blk::{self, SECTOR_SIZE};

    if !virtio_blk::is_present() {
        kprintln!("[test] no disk attached; skipping the block tests");
        return;
    }

    let capacity = virtio_blk::capacity();
    let mut buffer = [0u8; SECTOR_SIZE];

    // Sector zero is the protective master boot record. Its signature is two
    // bytes at a fixed offset, so a driver that returned a neighbouring sector
    // fails here before anything else is looked at.
    if let Err(error) = virtio_blk::read_sector(0, &mut buffer) {
        kprintln!("[test] FAILED: could not read the first sector: {error}");
        return;
    }
    if buffer[510] != 0x55 || buffer[511] != 0xAA || buffer[450] != 0xEE {
        kprintln!("[test] FAILED: sector 0 is not a protective master boot record");
        return;
    }

    // And sector one is the GPT header, which says so in ASCII.
    if let Err(error) = virtio_blk::read_sector(1, &mut buffer) {
        kprintln!("[test] FAILED: could not read the partition table: {error}");
        return;
    }
    if !buffer.starts_with(b"EFI PART") {
        kprintln!("[test] FAILED: sector 1 is not a GPT header");
        return;
    }

    // Then three labelled sectors: one near the start, one in the middle of the
    // gap, and the last one the disk has. An off-by-one in a descriptor address
    // shows up at an end and not in the middle.
    for sector in [100u64, 1000, capacity - 34] {
        if let Err(error) = virtio_blk::read_sector(sector, &mut buffer) {
            kprintln!("[test] FAILED: could not read sector {sector}: {error}");
            return;
        }
        let expected = alloc::format!("NEXUSOS-SECTOR-{sector:08}");
        if !buffer.starts_with(expected.as_bytes()) {
            let seen = core::str::from_utf8(&buffer[..23]).unwrap_or("<not text>");
            kprintln!("[test] FAILED: sector {sector} reads \"{seen}\", expected \"{expected}\"");
            return;
        }
    }

    // A sector past the end has to be refused rather than wrapped or clamped.
    // A driver that silently read something else here would be one that could
    // be asked to read anything.
    if virtio_blk::read_sector(capacity, &mut buffer) != Err(virtio_blk::BlockError::OutOfRange) {
        kprintln!("[test] FAILED: reading past the end of the disk was not refused");
        return;
    }

    // Keep what is there, so the image is the same afterwards as before. A test
    // that leaves the disk different from how it found it makes the next boot
    // depend on whether this one ran.
    let mut original = [0u8; SECTOR_SIZE];
    if let Err(error) = virtio_blk::read_sector(DISK_SCRATCH_SECTOR, &mut original) {
        kprintln!("[test] FAILED: could not read the scratch sector: {error}");
        return;
    }

    let mut written = [0u8; SECTOR_SIZE];
    for (index, byte) in written.iter_mut().enumerate() {
        // A pattern that depends on the offset, so a write that put the right
        // bytes in the wrong order still fails.
        *byte = (index as u8).wrapping_mul(31).wrapping_add(7);
    }

    if let Err(error) = virtio_blk::write_sector(DISK_SCRATCH_SECTOR, &written) {
        kprintln!("[test] FAILED: could not write the scratch sector: {error}");
        return;
    }
    if let Err(error) = virtio_blk::read_sector(DISK_SCRATCH_SECTOR, &mut buffer) {
        kprintln!("[test] FAILED: could not read back the scratch sector: {error}");
        return;
    }
    if buffer != written {
        kprintln!("[test] FAILED: the scratch sector did not read back as it was written");
        return;
    }

    if let Err(error) = virtio_blk::write_sector(DISK_SCRATCH_SECTOR, &original) {
        kprintln!("[test] FAILED: could not restore the scratch sector: {error}");
        return;
    }
    if let Err(error) = virtio_blk::read_sector(DISK_SCRATCH_SECTOR, &mut buffer) {
        kprintln!("[test] FAILED: could not verify the restored sector: {error}");
        return;
    }
    if buffer != original {
        kprintln!("[test] FAILED: the scratch sector was not restored");
        return;
    }

    let (read, wrote) = virtio_blk::statistics();
    kprintln!(
        "[test] disk verified: {capacity} sectors, a GPT at the front, three read by name, \
         one written and read back ({read} reads, {wrote} writes)"
    );
}

/// State for the IPC self-test.
mod ipc_test {
    use alloc::sync::Arc;
    use core::sync::atomic::AtomicU64;

    use crate::ipc::Endpoint;
    use crate::sync::IrqSpinLock;

    /// The end the receiving thread reads from.
    ///
    /// A static because a thread entry point takes one `usize` and this is an
    /// `Arc`; a table to index into would be the same thing with more parts.
    pub static RECEIVER: IrqSpinLock<Option<Arc<Endpoint>>> = IrqSpinLock::new(None);

    /// Set to the length of the message the receiver got.
    pub static RECEIVED: AtomicU64 = AtomicU64::new(0);
    /// Set when the receiver got a message whose contents were right.
    pub static MATCHED: AtomicU64 = AtomicU64::new(0);
    /// Set when the receiver was told the channel had closed instead.
    pub static CLOSED: AtomicU64 = AtomicU64::new(0);
}

/// What the IPC self-test sends.
const IPC_MESSAGE: &[u8] = b"a message that crossed a channel";

/// A thread that blocks on a channel until a message arrives.
fn ipc_test_receiver(_argument: usize) {
    use core::sync::atomic::Ordering;

    let endpoint = ipc_test::RECEIVER.lock().clone();
    let Some(endpoint) = endpoint else {
        return;
    };

    match endpoint.receive() {
        Some(message) => {
            if message.bytes == IPC_MESSAGE {
                ipc_test::MATCHED.store(1, Ordering::Relaxed);
            }
            // Published last, and with release ordering, because it is what the
            // waiting thread watches. Storing it first with relaxed ordering let
            // the compiler hoist it above the verdict, so the waiter could see a
            // length and not yet the answer -- and report a message that had
            // arrived intact as having arrived wrong.
            ipc_test::RECEIVED.store(message.bytes.len() as u64, Ordering::Release);
        }
        None => {
            ipc_test::CLOSED.store(1, Ordering::Relaxed);
        }
    }
}

/// Exercise channels and handles.
///
/// Two claims are worth making and neither is obvious from the code. The first
/// is that a thread waiting for a message *blocks* — that it leaves the run
/// queues rather than spinning — which is checked by waiting for the scheduler
/// to report it blocked before anything is sent. The second is that a right a
/// handle does not carry is refused, which is the whole of what a capability is
/// and is one missing check away from not being true.
fn ipc_self_test() {
    use core::sync::atomic::Ordering;

    use ipc::{Endpoint, Object, Rights};

    // --- a handle table, and the rights on it -------------------------------
    let table = ipc::HandleTable::new();
    let (held, _peer) = Endpoint::pair();
    let handle = table.insert(Object::Channel(held), Rights::READ | Rights::CLOSE);

    if table.rights(handle) != Ok(Rights::READ | Rights::CLOSE) {
        kprintln!("[test] FAILED: a handle did not report the rights it was given");
        return;
    }
    if table.channel(handle, Rights::WRITE).is_ok() {
        kprintln!("[test] FAILED: a read-only handle was accepted for writing");
        return;
    }
    if table.channel(handle, Rights::READ).is_err() {
        kprintln!("[test] FAILED: a read handle was refused for reading");
        return;
    }
    let described = table.describe();
    if table.len() != 1 || described.len() != 1 || described[0].1 != "channel" {
        kprintln!("[test] FAILED: the handle table does not describe its one handle");
        return;
    }
    if table.close(handle).is_err() || table.rights(handle).is_ok() {
        kprintln!("[test] FAILED: a closed handle is still usable");
        return;
    }

    // --- a message across a channel, with the receiver blocked --------------
    let (sender, receiver) = Endpoint::pair();
    *ipc_test::RECEIVER.lock() = Some(receiver);

    let before = sched::stats().blocked;
    if sched::spawn(
        "ipc-receiver",
        sched::thread::Priority::Normal,
        ipc_test_receiver,
        0,
    )
    .is_err()
    {
        kprintln!("[test] FAILED: could not start the IPC receiver");
        return;
    }

    // Wait for it to be *blocked*, not merely started. This is the claim: a
    // thread waiting for a message is off every run queue, so it shows up in
    // the scheduler's blocked count and nowhere else.
    let deadline = arch::time::ticks() + 2000;
    while sched::stats().blocked <= before && arch::time::ticks() < deadline {
        sched::sleep_ms(1);
    }
    if sched::stats().blocked <= before {
        kprintln!("[test] FAILED: the receiver never blocked; it is polling the channel");
        return;
    }
    if sender.queued() != 0 {
        kprintln!("[test] FAILED: a message appeared on the sending end");
        return;
    }

    if let Err(error) = sender.send(IPC_MESSAGE, alloc::vec::Vec::new()) {
        kprintln!("[test] FAILED: could not send on the channel: {error}");
        return;
    }

    let deadline = arch::time::ticks() + 2000;
    while ipc_test::RECEIVED.load(Ordering::Acquire) == 0 && arch::time::ticks() < deadline {
        sched::sleep_ms(1);
    }

    // Acquire, pairing with the receiver's release: seeing the length is what
    // makes everything the receiver decided before storing it visible here.
    let received = ipc_test::RECEIVED.load(Ordering::Acquire);
    if received != IPC_MESSAGE.len() as u64 {
        kprintln!(
            "[test] FAILED: the receiver got {received} bytes, expected {}",
            IPC_MESSAGE.len()
        );
        return;
    }
    if ipc_test::MATCHED.load(Ordering::Relaxed) != 1 {
        kprintln!("[test] FAILED: the message arrived with the wrong contents");
        return;
    }
    if ipc_test::CLOSED.load(Ordering::Relaxed) != 0 {
        kprintln!("[test] FAILED: the receiver was told the channel had closed");
        return;
    }

    let (channels, sent, taken) = ipc::statistics();
    kprintln!(
        "[test] IPC verified: the receiver blocked, then took {received} bytes across a channel \
         ({channels} channels, {sent} sent, {taken} received)"
    );
}

/// State for the TLB shootdown self-test.
mod tlb_test {
    use core::sync::atomic::{AtomicBool, AtomicU64};

    /// Where the test page lives.
    ///
    /// Inside the region reserved for kernel MMIO windows, a megabyte past the
    /// two that are actually used, so nothing else can claim it. The test needs
    /// an address it can repoint at will, which rules out the direct map, the
    /// heap and the stack area.
    pub const PAGE: u64 = nexus_abi::layout::KERNEL_MMIO_BASE + 0x10_0000;

    /// What the first frame holds, and what a stale translation reads back.
    pub const OLD_MARK: u64 = 0x0101_0101_0101_0101;
    /// What the second frame holds.
    pub const NEW_MARK: u64 = 0x0202_0202_0202_0202;

    /// Processors that have read the page while it pointed at the first frame.
    pub static SEEN_OLD: AtomicU64 = AtomicU64::new(0);
    /// Processors that have read the page since it was repointed.
    pub static SEEN_NEW: AtomicU64 = AtomicU64::new(0);
    /// Processors that read the *old* contents after the repoint completed.
    ///
    /// Any bit set here is a shootdown that did not arrive.
    pub static STALE: AtomicU64 = AtomicU64::new(0);
    /// Processors that read something that was neither mark.
    pub static GARBAGE: AtomicU64 = AtomicU64::new(0);

    /// Set once the page points at the second frame.
    pub static REMAPPED: AtomicBool = AtomicBool::new(false);
    /// Tells the readers to finish.
    pub static STOP: AtomicBool = AtomicBool::new(false);
    /// Readers still running.
    pub static RUNNING: AtomicU64 = AtomicU64::new(0);
}

/// A thread that reads the test page and records what it saw, and where.
fn tlb_test_reader(_argument: usize) {
    use core::sync::atomic::Ordering;

    tlb_test::RUNNING.fetch_add(1, Ordering::Relaxed);

    while !tlb_test::STOP.load(Ordering::Relaxed) {
        // Sampled *before* the read, and this order is the whole argument. If
        // the repoint had already completed when this was loaded, then a read
        // that still returns the old contents can only be a translation this
        // processor kept — which is precisely what a shootdown is for.
        let remapped = tlb_test::REMAPPED.load(Ordering::Acquire);

        // SAFETY: the page is mapped for the whole life of the test, and
        // `remap_page` never leaves it absent.
        let value = unsafe { core::ptr::read_volatile(tlb_test::PAGE as *const u64) };
        let bit = 1u64 << arch::percpu::cpu_index();

        match value {
            tlb_test::OLD_MARK => {
                tlb_test::SEEN_OLD.fetch_or(bit, Ordering::Relaxed);
                if remapped {
                    tlb_test::STALE.fetch_or(bit, Ordering::Relaxed);
                }
            }
            tlb_test::NEW_MARK => {
                tlb_test::SEEN_NEW.fetch_or(bit, Ordering::Relaxed);
            }
            _ => {
                tlb_test::GARBAGE.fetch_or(bit, Ordering::Relaxed);
            }
        }

        sched::yield_now();
    }

    tlb_test::RUNNING.fetch_sub(1, Ordering::Relaxed);
}

/// Prove that a mapping change on one processor reaches the others.
///
/// A shootdown that quietly does not happen looks exactly like one that does:
/// the symptom is a processor reading through a translation that should be
/// gone, and nothing faults. Counting broadcasts says the code ran; only
/// reading through the old address on every processor says it worked.
///
/// The test points one page at a frame holding one marker, gets every processor
/// to read it — which is what makes each of them cache the translation —
/// repoints it at a frame holding a different marker, and then fails if any
/// processor ever reads the first marker again.
fn tlb_self_test() {
    use core::sync::atomic::Ordering;

    let online = arch::smp::processor_count();
    if online < 2 {
        kprintln!("[test] only one processor; nothing to shoot down");
        return;
    }

    let (Some(first), Some(second)) = (memory::allocate_frame(), memory::allocate_frame()) else {
        kprintln!("[test] FAILED: could not allocate the TLB test frames");
        return;
    };

    // Written through the direct map, which is a different virtual address, so
    // filling them cannot itself put the test's translation into any TLB.
    // SAFETY: both frames were just allocated and nothing else refers to them.
    unsafe {
        core::ptr::write_volatile(layout::phys_to_virt(first) as *mut u64, tlb_test::OLD_MARK);
        core::ptr::write_volatile(layout::phys_to_virt(second) as *mut u64, tlb_test::NEW_MARK);
    }

    // SAFETY: the frames are owned here and the address is reserved.
    if let Err(error) = unsafe {
        memory::paging::map_page(
            tlb_test::PAGE,
            first,
            memory::paging::WRITABLE | memory::paging::NO_EXECUTE,
        )
    } {
        kprintln!("[test] FAILED: could not map the TLB test page: {error:?}");
        return;
    }

    // More readers than processors, because there is no affinity: the only way
    // to get one onto every processor is to give the scheduler more threads
    // than it has processors to put them on.
    let mut spawned = 0;
    for index in 0..online * 3 {
        if sched::spawn(
            "tlb-reader",
            sched::thread::Priority::Normal,
            tlb_test_reader,
            index,
        )
        .is_ok()
        {
            spawned += 1;
        }
    }
    if spawned == 0 {
        kprintln!("[test] FAILED: could not start any TLB readers");
        return;
    }

    let expected = (1u64 << online) - 1;

    // Wait until every processor has read through the old translation. Until
    // one has, it has nothing cached and the test would pass vacuously.
    let mut deadline = arch::time::ticks() + 2000;
    while tlb_test::SEEN_OLD.load(Ordering::Relaxed) & expected != expected
        && arch::time::ticks() < deadline
    {
        sched::sleep_ms(1);
    }
    let primed = tlb_test::SEEN_OLD.load(Ordering::Relaxed);
    if primed & expected != expected {
        kprintln!(
            "[test] FAILED: only processors {primed:#x} of {expected:#x} cached the test mapping"
        );
        tlb_test::STOP.store(true, Ordering::Relaxed);
        return;
    }

    // The change under test. `remap_page` shoots down before it returns, so by
    // the time the flag below is set, every processor has been told.
    // SAFETY: the page is mapped, and the old frame comes back for disposal.
    let repointed = unsafe {
        memory::paging::remap_page(
            tlb_test::PAGE,
            second,
            memory::paging::WRITABLE | memory::paging::NO_EXECUTE,
        )
    };
    if let Err(error) = repointed {
        kprintln!("[test] FAILED: could not repoint the TLB test page: {error:?}");
        tlb_test::STOP.store(true, Ordering::Relaxed);
        return;
    }
    // Release, to pair with the readers' acquire: a reader that sees this flag
    // must also see everything the remap did.
    tlb_test::REMAPPED.store(true, Ordering::Release);

    // Let every processor read again, now through the new mapping.
    deadline = arch::time::ticks() + 2000;
    while tlb_test::SEEN_NEW.load(Ordering::Relaxed) & expected != expected
        && arch::time::ticks() < deadline
    {
        sched::sleep_ms(1);
    }

    tlb_test::STOP.store(true, Ordering::Relaxed);
    deadline = arch::time::ticks() + 2000;
    while tlb_test::RUNNING.load(Ordering::Relaxed) > 0 && arch::time::ticks() < deadline {
        sched::sleep_ms(1);
    }

    let stale = tlb_test::STALE.load(Ordering::Relaxed);
    let fresh = tlb_test::SEEN_NEW.load(Ordering::Relaxed);
    let garbage = tlb_test::GARBAGE.load(Ordering::Relaxed);

    // SAFETY: nothing refers to the test page any more; the readers have all
    // stopped, which is what the wait above established.
    unsafe {
        let _ = memory::paging::unmap_range(tlb_test::PAGE, 1);
        memory::free_frame(first);
        memory::free_frame(second);
    }

    if garbage != 0 {
        kprintln!("[test] FAILED: processors {garbage:#x} read neither marker from the test page");
        return;
    }
    if stale != 0 {
        kprintln!(
            "[test] FAILED: processors {stale:#x} kept a stale translation after the shootdown"
        );
        return;
    }
    if fresh & expected != expected {
        kprintln!("[test] FAILED: only processors {fresh:#x} of {expected:#x} saw the new mapping");
        return;
    }

    let (broadcasts, timeouts) = arch::tlb::statistics();
    if timeouts != 0 {
        kprintln!("[test] FAILED: {timeouts} shootdowns went unacknowledged");
        return;
    }
    kprintln!(
        "[test] TLB shootdown verified: all {online} processors cached the old mapping and none \
         read through it afterwards ({broadcasts} broadcasts, 0 unacknowledged)"
    );
}

/// Work counters for the scheduler self-test.
mod sched_test {
    use core::sync::atomic::AtomicU64;

    /// Total iterations completed by all worker threads.
    pub static WORK_DONE: AtomicU64 = AtomicU64::new(0);
    /// Number of workers that have run to completion.
    pub static WORKERS_FINISHED: AtomicU64 = AtomicU64::new(0);
    /// Iterations completed by the thread that never yields.
    pub static HOG_ITERATIONS: AtomicU64 = AtomicU64::new(0);
    /// Wake-ups completed by the thread competing with the hog.
    pub static TICKER_WAKEUPS: AtomicU64 = AtomicU64::new(0);
    /// Set to stop the hog once the preemption test has its answer.
    pub static STOP_HOG: AtomicU64 = AtomicU64::new(0);
    /// Bitmap of the processors that ran a worker, one bit per index.
    ///
    /// The point of scheduling on every processor is that work lands on more
    /// than one of them. Nothing else in the log proves that: switch counts and
    /// interrupt counts are consistent with three cores idling politely. Having
    /// the workers record where they ran turns it into something the boot test
    /// can read.
    pub static WORKER_PROCESSORS: AtomicU64 = AtomicU64::new(0);
}

/// Iterations each self-test worker performs.
const WORKER_ITERATIONS: u64 = 500;
/// Number of workers the self-test spawns.
const WORKER_COUNT: u64 = 4;

/// A worker that does a fixed amount of work and exits.
///
/// Half of them yield between iterations and half do not, so the test covers
/// both cooperative hand-off and timer preemption.
fn self_test_worker(argument: usize) {
    use core::sync::atomic::Ordering;

    for iteration in 0..WORKER_ITERATIONS {
        sched_test::WORK_DONE.fetch_add(1, Ordering::Relaxed);
        // Sampled every iteration, not once: a thread can be preempted and
        // resumed on a different processor, and both are worth recording.
        sched_test::WORKER_PROCESSORS.fetch_or(1 << arch::percpu::cpu_index(), Ordering::Relaxed);
        if argument % 2 == 0 {
            sched::yield_now();
        }
        // Sleep occasionally, so the work outlasts the time it takes the other
        // processors to hear about it.
        //
        // Without this the test was a race it sometimes lost: a request to
        // reschedule is noticed at the next timer tick, up to a millisecond
        // away, and five hundred iterations of an atomic increment are over
        // long before that. All four workers finishing on the processor that
        // started them was not a scheduler failure, it was the test measuring
        // wake-up latency and calling it distribution.
        if iteration % 100 == 99 {
            sched::sleep_ms(1);
        }
    }
    sched_test::WORKERS_FINISHED.fetch_add(1, Ordering::Relaxed);
}

/// A thread that never yields and never sleeps.
///
/// The point of the preemption test: if the timer cannot take the processor
/// away from this, nothing else in the system will ever run again.
fn self_test_hog(_argument: usize) {
    use core::sync::atomic::Ordering;

    while sched_test::STOP_HOG.load(Ordering::Relaxed) == 0 {
        sched_test::HOG_ITERATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

/// A thread that sleeps in a loop while the hog spins.
fn self_test_ticker(_argument: usize) {
    use core::sync::atomic::Ordering;

    for _ in 0..5 {
        sched::sleep_ms(20);
        sched_test::TICKER_WAKEUPS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Verify that threads actually run, finish, and can be preempted.
///
/// Two things are checked, and the second is the one that matters. Cooperative
/// scheduling is easy to get right and easy to test. Preemption is neither: it
/// is only real if a thread that never yields can still be taken off the
/// processor, and the only way to know is to run one and see whether anything
/// else makes progress.
fn scheduler_self_test() {
    use core::sync::atomic::Ordering;

    kprintln!("[test] spawning {WORKER_COUNT} worker threads");
    for index in 0..WORKER_COUNT {
        let priority = if index < 2 {
            sched::thread::Priority::Normal
        } else {
            sched::thread::Priority::Background
        };
        if let Err(error) = sched::spawn("worker", priority, self_test_worker, index as usize) {
            kprintln!("[test] FAILED: could not spawn worker {index}: {error}");
            return;
        }
    }

    // Wait for them, but not forever: a scheduler that never runs them must
    // report that rather than hang the boot.
    let deadline = arch::time::ticks() + 5000;
    while sched_test::WORKERS_FINISHED.load(Ordering::Relaxed) < WORKER_COUNT
        && arch::time::ticks() < deadline
    {
        sched::sleep_ms(10);
    }

    let finished = sched_test::WORKERS_FINISHED.load(Ordering::Relaxed);
    let work = sched_test::WORK_DONE.load(Ordering::Relaxed);
    if finished != WORKER_COUNT {
        // A failing test that only says it failed is a test that has to be
        // reproduced before it can be read. Say what was seen.
        kprintln!(
            "[test] FAILED: only {finished} of {WORKER_COUNT} workers finished              ({work} of {} iterations done, {} ticks elapsed of 5000)",
            WORKER_COUNT * WORKER_ITERATIONS,
            arch::time::ticks() + 5000 - deadline
        );
        sched::dump_threads();
        return;
    }
    if work != WORKER_COUNT * WORKER_ITERATIONS {
        kprintln!(
            "[test] FAILED: workers completed {work} iterations, expected {}",
            WORKER_COUNT * WORKER_ITERATIONS
        );
        return;
    }
    kprintln!("[test] {finished} threads ran to completion, {work} iterations total");

    let processors = sched_test::WORKER_PROCESSORS.load(Ordering::Relaxed);
    let used = processors.count_ones();
    let online = arch::smp::processor_count();
    if online > 1 && used < 2 {
        // Reported and carried on. An early return here once hid the TLB
        // shootdown test entirely, so a flake in one check read as a missing
        // marker in another.
        kprintln!(
            "[test] FAILED: {online} processors are online but every worker ran on one of them"
        );
    } else {
        kprintln!(
            "[test] work was spread across {used} of {online} processors (mask {processors:#x})"
        );
    }

    // Now the real question: can a thread that never yields be preempted?
    kprintln!("[test] starting a thread that never yields, at equal priority");
    let spawned = sched::spawn("cpu-hog", sched::thread::Priority::Normal, self_test_hog, 0)
        .and_then(|_| {
            sched::spawn(
                "ticker",
                sched::thread::Priority::Normal,
                self_test_ticker,
                0,
            )
        });
    if let Err(error) = spawned {
        kprintln!("[test] FAILED: could not spawn the preemption test threads: {error}");
        return;
    }

    let deadline = arch::time::ticks() + 3000;
    while sched_test::TICKER_WAKEUPS.load(Ordering::Relaxed) < 5 && arch::time::ticks() < deadline {
        sched::sleep_ms(10);
    }
    sched_test::STOP_HOG.store(1, Ordering::Relaxed);

    let wakeups = sched_test::TICKER_WAKEUPS.load(Ordering::Relaxed);
    let hog = sched_test::HOG_ITERATIONS.load(Ordering::Relaxed);
    if wakeups < 5 {
        kprintln!(
            "[test] FAILED: preemption is not working. The ticker woke {wakeups} of 5 times \
             while a thread that never yields held the processor"
        );
        return;
    }
    if hog == 0 {
        kprintln!("[test] FAILED: the non-yielding thread never ran at all");
        return;
    }

    kprintln!(
        "[test] preemption verified: the ticker woke {wakeups} times while a non-yielding \
         thread completed {hog} iterations"
    );

    // Let the hog observe the stop flag and retire, then reclaim everything.
    sched::sleep_ms(50);
    let reaped = sched::reap_finished();
    kprintln!("[test] reclaimed {reaped} finished threads");
    sched::dump_threads();
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
        let dividend = arch::time::ticks() | 1;
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
