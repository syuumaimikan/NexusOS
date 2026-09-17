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
mod compat;
mod crash;
mod display;
mod drivers;
mod framebuffer;
mod fs;
mod i18n;
mod input;
mod ipc;
mod machine;
mod memory;
mod net;
mod panic;
mod pipe;
mod power;
mod removable;

mod process;
mod random;
mod sched;
mod selftest;
mod serial;
mod socket;
mod sound;
mod sync;
mod user;
mod waitset;

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
pub unsafe extern "sysv64" fn _start(boot_info: *const BootInfo) -> ! {
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
    let acpi_info = match unsafe { acpi::init(boot_info.acpi_rsdp) } {
        Ok(info) => {
            acpi::report(&info);
            // Before the APIC, because this only reads what was already parsed
            // and a machine that cannot start its other processors should still
            // be able to turn itself off.
            power::init(&info);
            // Before anything asks for a key. It reads CPUID and nothing
            // else, so it is safe this early and the answer is wanted before
            // the network comes up.
            random::init();
            random::self_test();
            adopt_local_apic(&info);
            bring_up_device_interrupts(&info);
            start_other_processors(&info);
            Some(info)
        }
        Err(error) => {
            // Not fatal. The PIT keeps the system ticking on one processor;
            // what is lost is SMP, MSI and per-core timers.
            kprintln!("[acpi] {error}; continuing on the legacy timer");
            None
        }
    };

    // What the machine has. Everything that is not on the processor is behind
    // PCI, so this is the first thing the kernel does that is about the machine
    // rather than the CPU.
    // SAFETY: called once, before anything drives a device.
    let devices = unsafe { drivers::pci::enumerate() };
    drivers::pci::report(&devices);
    // Before any driver runs. Taking an interrupt is something a driver asks
    // for; leaving one raised by a device nobody listens to is what jams the
    // line everybody else shares.
    // SAFETY: no driver has started.
    unsafe { drivers::pci::silence(&devices) };

    // The disk. Not fatal if there is none: everything else works without it,
    // and saying so beats refusing to boot a machine that has no storage the
    // kernel can drive yet.
    // SAFETY: called once, after enumeration, with the allocators running.
    if let Err(error) = unsafe { drivers::virtio_blk::init(&devices) } {
        kprintln!("[blk ] no block device: {error}");
    } else if let Some(info) = acpi_info.as_ref() {
        // After the device exists, and not with the keyboard: the disk is found
        // by enumerating PCI, which happens later than the interrupt controller
        // comes up. Routing a pin for a device that is not there yet routes
        // nothing, which is what this did on its first attempt.
        bring_up_disk_interrupt(info);
    }

    // The network card, on the same terms: not fatal if there is none, because
    // a machine with no network is a machine that still boots.
    // SAFETY: called once, after enumeration, with the allocators running.
    match unsafe { drivers::virtio_net::init(&devices) } {
        Ok(_) => {
            if let Some(info) = acpi_info.as_ref() {
                bring_up_network_interrupt(info);
            }
        }
        Err(error) => kprintln!("[net ] no network card: {error}"),
    }

    // The GPU, on the same terms. A machine without one keeps the framebuffer
    // the firmware handed over, which is what every boot before this used.
    // SAFETY: called once, after enumeration, with the allocators and the
    // kernel's page tables running.
    if unsafe { drivers::virtio_gpu::init(&devices) } {
        // And prove it displays, rather than merely that it answered. The host
        // can be asked for a picture of what this device is showing, and three
        // bands of known colour are not something that appears by accident.
        drivers::virtio_gpu::self_test();
    } else {
        kprintln!(
            "[gpu ] no virtio GPU on this machine; the firmware's framebuffer is what there is"
        );
    }

    // The sound card, on the same terms. A machine with none keeps the speaker,
    // which is one bit and says so; a machine with one gets sixteen-bit stereo
    // through a codec, and `sound.rs` prefers it without either caller knowing.
    // SAFETY: called once, after enumeration, with the frame allocator running.
    if !unsafe { drivers::ac97::init(&devices) } {
        kprintln!("[snd ] no AC'97 card on this machine; the speaker is what there is");
    }

    // The USB host controller, on the same terms again. A machine with nothing
    // plugged in is the ordinary case and is reported rather than treated as a
    // failure -- and so is a machine whose USB controller is an older kind this
    // does not drive.
    match drivers::xhci::start(&devices) {
        Ok(()) => {
            // SAFETY: the controller was just started, so its window is mapped.
            unsafe { drivers::xhci::survey_ports() };
            // And ask whatever is there what it is.
            // SAFETY: the controller is running and the ports have been reset.
            unsafe { drivers::usb::enumerate() };
        }
        Err(drivers::xhci::Trouble::NotPresent) => {
            kprintln!("[usb ] no xHCI controller on this machine");
        }
        Err(error) => kprintln!("[usb ] the USB controller would not start: {error}"),
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

    // The screen: the firmware's framebuffer where there is one, and the GPU's
    // scanout where there is not.
    //
    // Which it gets is a property of the machine and not a preference. This
    // firmware has no driver for a virtio GPU, so a machine given one and no
    // other display is handed no framebuffer at all -- `no usable framebuffer`
    // from the bootloader, and every pixel after that would have gone nowhere.
    // The kernel's own driver is then the only thing that can put anything on
    // the screen, which is also the tidiest arrangement of the two: there is no
    // handover, and no moment when the firmware and the kernel are both driving
    // one device.
    //
    // Nothing above this line learns which it got. `display::flush` is called
    // either way and does nothing on the firmware's, because a firmware
    // framebuffer *is* the screen.
    let screen = if boot_info.framebuffer.phys_addr == 0 {
        drivers::virtio_gpu::framebuffer().unwrap_or(boot_info.framebuffer)
    } else {
        boot_info.framebuffer
    };
    if screen.phys_addr != boot_info.framebuffer.phys_addr {
        kprintln!(
            "[disp] the firmware gave no framebuffer; taking the GPU's {}x{}",
            screen.width,
            screen.height
        );
    }

    // SAFETY: the bootloader mapped the framebuffer through the direct map, or
    // the GPU driver allocated it out of the same physical memory; this is the
    // only place the kernel adopts one.
    unsafe { display::init(&screen) };

    // The scheduler. The context that got us here becomes thread #0 and keeps
    // running; from this point on it is preemptible like any other thread.
    // SAFETY: called once, from the boot context, with the heap available.
    if let Err(error) = unsafe { sched::init() } {
        kprintln!("FATAL: could not start the scheduler: {error}");
        arch::halt_forever();
    }

    // The line under the logo, moved along as bring-up passes each of its
    // steps. Counted rather than timed, so a machine that is slow because its
    // disk is slow shows a bar that pauses in the same place every time.
    display::progress(1, BOOT_STEPS);

    self_test();
    display::progress(2, BOOT_STEPS);
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

/// How many steps of bring-up the line under the logo is divided into.
///
/// A count of the things that actually happen rather than a guess at how long
/// they take: the scheduler, the self-test, the clock, the keyboard, the
/// network, and the first program that is not the kernel's.
const BOOT_STEPS: u32 = 6;

/// Spawn the long-lived threads and hand the processor over to them.
fn start_system_threads() {
    display::start_thread();
    // The wall clock. After the timer, because what it records is a moment
    // *and* the tick it was read at, and before anything that wants to know
    // what day it is.
    // SAFETY: called once, and nothing else on this system touches the CMOS
    // ports.
    unsafe { drivers::rtc::init() };

    display::progress(3, BOOT_STEPS);

    input::start_thread();
    display::progress(4, BOOT_STEPS);

    // And the network, which has to be a thread: getting an address means
    // sending a broadcast and waiting for an answer, and waiting is something
    // only a thread can do.
    net::start_thread();

    // The first thing NexusOS runs that it does not trust. Not fatal if it
    // fails: the system is less of a system without it, but it is still one.
    // SAFETY: the heap and the scheduler are both running by now, and this is
    // the only call.
    // The speaker, which says the machine is up in the one way a person who
    // is not looking at the screen can hear.
    sound::start_thread();
    machine::start_thread();
    power::start_thread();
    removable::start_thread();

    display::progress(5, BOOT_STEPS);

    if let Err(error) = unsafe { user::start() } {
        kprintln!("[user] could not start user mode: {error}");
    }
    display::progress(BOOT_STEPS, BOOT_STEPS);
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

        // Nothing is worth saying about a machine that is stopping, and the
        // last lines of its log should be why it stopped rather than how many
        // context switches it had got through. Reaping still happens: a thread
        // that finished still has a stack to give back, and the machine may
        // yet be told to carry on.
        if power::stopping() {
            sched::reap_finished();
            continue;
        }

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
                let (raised, blocking) = drivers::virtio_blk::interrupt_statistics();
                kprintln!(
                    "[mon ] disk {} sectors, {read} read, {wrote} written,              {raised} interrupts, requests {}",
                    drivers::virtio_blk::capacity(),
                    if blocking { "block" } else { "spin" }
                );
                // How the requests actually waited, and whether the device is
                // being asked for barriers. Both were counted and neither was
                // printed, which cost a day: "requests block" says blocking is
                // *enabled*, not that any request blocked, and a driver that
                // sends no flush is a driver whose host quietly stops caching.
                let waits = drivers::virtio_blk::wait_statistics();
                let (flushes, takes_flushes) = drivers::virtio_blk::flush_statistics();
                kprintln!(
                    "[mon ] disk {} requests, {} finished on the poll, {} slept,              {flushes} flushes{}",
                    waits.requests,
                    waits.poll_completions,
                    waits.wait_attempts,
                    if takes_flushes {
                        ""
                    } else {
                        " (this disk takes none, so every write waits for the medium)"
                    }
                );
                let pending = fs::store::pending_removals();
                if pending > 0 {
                    // Names taken away from files somebody still holds. A number that
                    // grows and never falls is a handle nobody is closing.
                    kprintln!("[mon ] {pending} removed name(s) still held open");
                }
                let (hits, misses, writes, evictions) = fs::cache::statistics();
                kprintln!(
                    "[mon ] block cache {}% of {} reads served from memory,              {writes} written through, {evictions} evicted",
                    fs::cache::hit_rate(),
                    hits + misses
                );
            }
        }
        if drivers::mouse::is_present() {
            let (bytes, packets, desynchronised, overflows) = drivers::mouse::statistics();
            kprintln!(
                "[mon ] pointer {packets} packets from {bytes} bytes,              {desynchronised} resynchronised, {overflows} overflowed"
            );
        }
        // Anything a fault left behind. Written from here rather than from the
        // handler that recorded it, because writing a file needs the disk and a
        // sleeping lock and the thread that faulted may have been holding
        // either.
        crash::flush();
        let (recorded, written, dropped) = crash::statistics();
        if recorded > 0 {
            kprintln!(
                "[mon ] crash {recorded} faults recorded, {written} written,              {dropped} dropped for want of room"
            );
        }
        if drivers::virtio_net::is_present() {
            let (received, transmitted, dropped, interrupts) = drivers::virtio_net::statistics();
            let (frames_in, frames_out, arp, echoes, unknown) = net::statistics();
            let interface = net::interface();
            if net::is_up() {
                kprintln!(
                    "[mon ] network {}.{}.{}.{} up, {received} frames in, {transmitted} out,              {interrupts} interrupts, {dropped} dropped",
                    interface.ip[0],
                    interface.ip[1],
                    interface.ip[2],
                    interface.ip[3]
                );
            } else {
                kprintln!(
                    "[mon ] network down, {received} frames in, {transmitted} out,              {interrupts} interrupts, {dropped} dropped"
                );
            }
            kprintln!(
                "[mon ] stack {frames_in} frames read, {frames_out} written,              {arp} ARP answered, {echoes} echoes answered, {unknown} ignored"
            );
            let (accepted, answered, resets, retried) = net::tcp::statistics();
            if accepted > 0 || resets > 0 {
                kprintln!(
                    "[mon ] tcp {accepted} connections accepted, {answered} answered,              {resets} reset, {retried} segments resent"
                );
            }
            // The other direction, reported separately because it is a
            // different program: one answers connections and one makes them,
            // and a single line adding the two together would hide which.
            let (opened, out, back, refused) = net::stream::statistics();
            if opened > 0 {
                kprintln!(
                    "[mon ] client {opened} connections opened, {out} bytes out, {back} in,              {refused} came to nothing"
                );
            }
            let (datagrams_out, datagrams_in, datagrams_lost) = net::datagram::statistics();
            if datagrams_out > 0 || datagrams_in > 0 {
                kprintln!(
                    "[mon ] datagrams {datagrams_out} sent, {datagrams_in} received,              {datagrams_lost} dropped for want of room"
                );
            }
            let (looped, loop_dropped) = net::loopback_statistics();
            if looped > 0 {
                kprintln!(
                    "[mon ] loopback {looped} packets this machine sent to itself,              {loop_dropped} dropped for want of room"
                );
            }
            let (notes, hushed) = sound::statistics();
            let (tones, speaker) = drivers::speaker::statistics();
            let (card_tones, card_samples) = drivers::ac97::statistics();
            if notes > 0 || hushed > 0 || tones > 0 || card_tones > 0 {
                kprintln!(
                    "[mon ] sound {notes} notes played, {hushed} refused,              {tones} tones on the speaker{}, {card_tones} through the card ({card_samples} samples)",
                    if speaker { "" } else { " (never used)" }
                );
                let (asked, denied) = machine::statistics();
                if asked + denied > 0 {
                    kprintln!(
                        "[mon ] machine {asked} snapshots answered, {denied} requests refused"
                    );
                }
                let (given, refused) = random::statistics();
                if given + refused > 0 {
                    // Worth having in a log because the failure this reports is
                    // silent otherwise: a machine whose generator stops
                    // answering hands out no keys and says nothing, and the
                    // only symptom is connections that will not start.
                    kprintln!("[mon ] random {given} bytes given out, {refused} requests refused");
                }
                let (offs, reboots) = power::statistics();
                if offs + reboots > 0 {
                    // Only ever seen once, on the last report before the
                    // machine goes -- which is exactly when it is worth having
                    // in the log, because it says the request was heard.
                    kprintln!("[mon ] power {offs} shutdowns and {reboots} restarts asked for");
                }
            }
            let (served, turned_down) = net::service::statistics();
            if served > 0 || turned_down > 0 {
                kprintln!(
                    "[mon ] network service {served} requests answered, {turned_down} refused"
                );
            }
        }
        let (served, turned_away) = removable::statistics();
        if served > 0 || turned_away > 0 {
            kprintln!("[mon ] removable {served} requests answered, {turned_away} refused");
        }
        let (gpu_commands, gpu_flushes) = drivers::virtio_gpu::statistics();
        if gpu_commands > 0 {
            kprintln!(
                "[mon ] gpu {gpu_commands} commands answered, {gpu_flushes} rectangles flushed{}",
                if drivers::virtio_gpu::is_present() {
                    ""
                } else {
                    " (the device has stopped)"
                }
            );
        }
        let (translated, refused, mapped) = compat::linux::statistics();
        let open = compat::linux_files::open_count();
        if translated > 0 || refused > 0 {
            kprintln!(
                "[mon ] linux {translated} calls translated, {refused} answered ENOSYS,                  {mapped} pages mapped, {open} files open"
            );
            let (threads, waits, wakes) = compat::linux_threads::statistics();
            if threads > 0 || waits > 0 {
                kprintln!(
                    "[mon ] linux {threads} threads cloned, {waits} futex waits, {wakes} wakes"
                );
            }
            let (frames, events) = compat::linux_display::statistics();
            if frames > 0 {
                kprintln!("[mon ] linux {frames} frames presented, {events} events delivered");
            }
            let (blocked, immediate) = compat::linux_poll::statistics();
            let (pipes, carried) = pipe::statistics();
            if blocked > 0 || pipes > 0 {
                kprintln!(
                    "[mon ] linux {blocked} waits blocked, {immediate} answered at once,                      {pipes} pipes carrying {carried} bytes"
                );
            }
            let (connections, accepted, passed) = socket::statistics();
            let (sockets, listening) = compat::linux_socket::statistics();
            if sockets > 0 {
                kprintln!(
                    "[mon ] linux {sockets} sockets, {listening} names bound,                      {connections} connections ({accepted} accepted), {passed} handles passed"
                );
            }
            let (translated32, refused32) = compat::linux32::statistics();
            if translated32 > 0 || refused32 > 0 {
                kprintln!(
                    "[mon ] linux32 {translated32} i386 calls translated, {refused32} answered ENOSYS"
                );
            }
            let (handlers, signals) = compat::linux_signal::statistics();
            let replaced = compat::linux_exec::statistics();
            if handlers > 0 || replaced > 0 {
                kprintln!(
                    "[mon ] linux {handlers} signal handlers installed, {signals} delivered,                      {replaced} programs replaced themselves"
                );
            }
        }
        let (calls, unknown) = arch::syscall::statistics();
        let (entered, returned) = arch::syscall::yield_statistics();
        if entered != returned {
            kprintln!("[mon ] {entered} yields begun, {returned} returned");
        }
        let from_user = arch::idt::entries_from_user();
        // And how many of them nobody owned. A spurious interrupt is the local
        // APIC saying a line was raised by a device whose handler it could not
        // find, and a count that climbs is a device holding a pin that nothing
        // releases. It has been counted since the APIC was written and printed
        // nowhere, which is the same blind spot the disk's wait counters were
        // in -- a number that exists and is never looked at is a number that
        // does not exist.
        let spurious = arch::apic::spurious_count();
        kprintln!(
            "[mon ] {calls} system calls ({unknown} unimplemented),              {from_user} interrupts taken from ring 3, {spurious} spurious"
        );
        // And which line they arrived on. Without this the only numbers are a
        // total and the disk's own, and the difference between those two
        // belongs to nobody -- which is exactly where a million and a half
        // interrupts were hiding.
        let mut sources = alloc::string::String::new();
        for (name, count) in arch::interrupts::counts() {
            if count > 0 {
                use core::fmt::Write as _;
                let _ = write!(sources, " {name} {count},");
            }
        }
        kprintln!(
            "[mon ] interrupts by line:{}",
            sources.trim_end_matches(',')
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

                // And the floating-point and vector registers. Nothing in this
                // kernel uses them and nothing built for this system does
                // either -- but a program built for Linux is built against an
                // ABI that *is* SSE, and until this runs its first `xorps`
                // raises invalid opcode. See `arch::fpu`.
                arch::fpu::enable();
            }
            if arch::fpu::enabled() {
                kprintln!(
                    "[fpu ] x87, MMX and SSE enabled for ring 3;                      512 bytes of state saved per thread"
                );
            } else {
                kprintln!(
                    "[fpu ] the vector unit could not be enabled; foreign programs will fault"
                );
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

    // The mouse first, and this ordering is load-bearing. Bringing it up means
    // asking the shared controller questions and reading its answers out of the
    // one output buffer both devices use -- and the moment the keyboard's pin
    // is unmasked, its handler is entitled to take whatever is in there. It did
    // exactly that, and the mouse reported that the controller would not say
    // how it was configured.
    // SAFETY: called once, before any pin on this controller is unmasked.
    unsafe { drivers::mouse::init() };

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

    bring_up_mouse(info);
}

/// Route the mouse's interrupt.
///
/// The device itself was brought up earlier, before the keyboard's pin was
/// unmasked: both are on one controller with one output buffer, and asking the
/// controller a question while another handler is entitled to read its answer
/// is how the answer goes missing.
///
/// Reported and tolerated on failure. A machine with no pointing device is a
/// machine that still boots.
fn bring_up_mouse(info: &acpi::AcpiInfo) {
    if !drivers::mouse::is_present() {
        return;
    }

    let irq = drivers::mouse::MOUSE_IRQ;
    let gsi = info.global_system_interrupt_for(irq);
    if info.route(gsi).is_none() {
        kprintln!("[mouse] no I/O APIC serves global interrupt {gsi}; the pointer is unavailable");
        return;
    }

    let (active_low, level) = info.override_for(irq).map_or((false, false), |entry| {
        (entry.is_active_low(), entry.is_level_triggered())
    });

    // SAFETY: a handler for the vector was registered when the IDT was built,
    // and the device is reporting.
    match unsafe {
        arch::ioapic::route(
            gsi,
            arch::interrupts::MOUSE_VECTOR,
            arch::apic::local_id(),
            active_low,
            level,
        )
    } {
        Ok(()) => kprintln!(
            "[mouse] mouse on IRQ {irq} (global interrupt {gsi}) routed to vector {}",
            arch::interrupts::MOUSE_VECTOR
        ),
        Err(error) => kprintln!("[mouse] could not route the mouse: {error}"),
    }
}

/// Route the disk's interrupt, and let the driver decide whether to trust it.
///
/// The routing is the easy half. The half that matters is that the driver does
/// not switch to blocking because a routing call returned `Ok` -- it makes one
/// request the old way, spinning, and switches only if its handler actually
/// ran. A driver that trusted the routing would hang the machine on the first
/// firmware that had wired the pin somewhere else, and it would look like a
/// disk that stopped answering rather than like an interrupt that never came.
fn bring_up_disk_interrupt(info: &acpi::AcpiInfo) {
    let Some(irq) = drivers::virtio_blk::interrupt_line() else {
        return;
    };
    let gsi = info.global_system_interrupt_for(irq);
    if info.route(gsi).is_none() {
        kprintln!("[blk ] no I/O APIC serves global interrupt {gsi}; the disk will spin");
        return;
    }

    // PCI interrupts are level triggered and active low. That is the bus, not a
    // guess: a pin programmed as edge triggered would deliver the first
    // interrupt and then never another, because the device holds the line down
    // until it is acknowledged.
    let (active_low, level) = info.override_for(irq).map_or((true, true), |entry| {
        (entry.is_active_low(), entry.is_level_triggered())
    });

    // SAFETY: a handler for the vector was registered when the IDT was built.
    match unsafe {
        arch::ioapic::route(
            gsi,
            arch::interrupts::DISK_VECTOR,
            arch::apic::local_id(),
            active_low,
            level,
        )
    } {
        Ok(()) => kprintln!(
            "[blk ] disk on IRQ {irq} (global interrupt {gsi}) routed to vector {}",
            arch::interrupts::DISK_VECTOR
        ),
        Err(error) => {
            kprintln!("[blk ] could not route the disk: {error}; requests will spin");
            return;
        }
    }

    // One request, made the old way, to find out whether the interrupt arrives
    // at all. Sector zero, read and discarded: it is the protective master boot
    // record, it is always there, and nothing is changed by reading it.
    let mut scratch = [0u8; drivers::virtio_blk::SECTOR_SIZE];
    if let Err(error) = drivers::virtio_blk::read_sector(0, &mut scratch) {
        kprintln!("[blk ] the disk did not answer a first request: {error}");
        return;
    }
    drivers::virtio_blk::adopt_interrupt();
}

/// Route the network card's interrupt.
///
/// Unlike the disk there is no request to make to prove the line works: nothing
/// asks a card for a frame. So the proof comes later and from somewhere else --
/// the network thread sends one frame the spinning way, and what comes back is
/// what says the interrupt arrives.
fn bring_up_network_interrupt(info: &acpi::AcpiInfo) {
    let Some(irq) = drivers::virtio_net::interrupt_line() else {
        return;
    };
    let gsi = info.global_system_interrupt_for(irq);
    if info.route(gsi).is_none() {
        kprintln!("[net ] no I/O APIC serves global interrupt {gsi}; the card will spin");
        return;
    }

    // The disk may already be on this pin. Routing it again would not add the
    // card to the line -- it would *move* the line to a different vector, and
    // the disk's completions would arrive at a handler that knows nothing about
    // disks. That is what happened the first time this was written, and it
    // looked like a machine that hung halfway through booting.
    if let Some(disk) = drivers::virtio_blk::interrupt_line() {
        if info.global_system_interrupt_for(disk) == gsi {
            arch::interrupts::share_line_with_disk();
            kprintln!(
                "[net ] card shares IRQ {irq} with the disk; both are asked on vector {}",
                arch::interrupts::DISK_VECTOR
            );
            return;
        }
    }

    // Level triggered and active low, because that is what a PCI pin is. An
    // edge-triggered pin delivers the first interrupt and never another: the
    // device holds the line down until it is acknowledged.
    let (active_low, level) = info.override_for(irq).map_or((true, true), |entry| {
        (entry.is_active_low(), entry.is_level_triggered())
    });

    // SAFETY: a handler for the vector was registered when the IDT was built.
    match unsafe {
        arch::ioapic::route(
            gsi,
            arch::interrupts::NETWORK_VECTOR,
            arch::apic::local_id(),
            active_low,
            level,
        )
    } {
        Ok(()) => kprintln!(
            "[net ] card on IRQ {irq} (global interrupt {gsi}) routed to vector {}",
            arch::interrupts::NETWORK_VECTOR
        ),
        Err(error) => kprintln!("[net ] could not route the card: {error}; sends will spin"),
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

    selftest::run();

    // Named here rather than inside each test, because what this buys is only
    // worth anything when a test does not return: a machine that goes silent
    // between two of them otherwise has no last known position, and the line
    // before the silence belongs to whichever test happened to print last.
    //
    // Every one of these has been seen to stop a boot at some point in this
    // repository's history. One of them did it once in seven boots.
    for (name, run) in [
        ("memory", memory_self_test as fn()),
        ("the heap", heap_self_test),
        ("the scheduler", scheduler_self_test),
        ("TLB shootdown", tlb_self_test),
        ("IPC", ipc_self_test),
        ("processes", process_self_test),
        ("wait sets", waitset_self_test),
        ("the disk", disk_self_test),
        ("the filesystem", filesystem_self_test),
        ("NexusFS", nexusfs_self_test),
    ] {
        // With the clock, because "which self-test is slow" is a question that
        // cannot be answered from a log without one, and a boot nobody has
        // timed is a boot nobody can make faster.
        let started = arch::time::ticks();
        kprintln!("[test] begin {name}");
        run();
        kprintln!(
            "[test] end {name}: {} ms",
            arch::time::ticks().saturating_sub(started)
        );
    }
}

/// Bring up the system's own filesystem, and prove it works.
///
/// Two things at once, because they are the same thing: the filesystem is
/// brought up by using it, and what proves it works is that the count of boots
/// it keeps is right. On the first boot of a fresh disk that count is one and
/// the partition was empty; on the second it is two, and the only place the one
/// could have come from is the disk.
fn nexusfs_self_test() {
    use fs::store;

    let mounted = match store::mount() {
        Ok(mounted) => mounted,
        Err(fs::store::StoreError::NoDisk) => {
            kprintln!("[test] no disk attached; skipping NexusFS");
            return;
        }
        Err(error) => {
            kprintln!("[test] FAILED: could not bring up NexusFS: {error}");
            return;
        }
    };

    // Anything the image brought that a program will have to open. Here rather
    // than with the filesystem checks above, because those run before this
    // point and the store is what is being written to.
    seed_packages();
    crash::report_previous();

    let (total, free) = store::space().unwrap_or((0, 0));
    kprintln!(
        "[fs  ] NexusFS {} at sector {}: {} MiB, {} MiB free, boot {}",
        if mounted.formatted { "made" } else { "mounted" },
        mounted.start_lba,
        total / (1024 * 1024),
        free / (1024 * 1024),
        mounted.boots
    );

    match store::list("/") {
        Ok(entries) => {
            for entry in &entries {
                kprintln!(
                    "[fs  ]   /{}{} {} bytes",
                    entry.name.as_str(),
                    if entry.kind == fs::nexusfs::Kind::Directory {
                        "/"
                    } else {
                        ""
                    },
                    entry.size
                );
            }
        }
        Err(error) => kprintln!("[test] FAILED: could not list the root: {error}"),
    }

    // The last line of the log the previous boot wrote, which is the shortest
    // way to see that what came off the disk is what went onto it.
    match store::boot_log() {
        Ok(log) => {
            let lines = log.lines().count();
            if let Some(last) = log.lines().next_back() {
                kprintln!("[fs  ] the boot log has {lines} lines, ending \"{last}\"");
            }
            // One line per boot, unless the log has been trimmed, so the
            // count cannot exceed the boots and cannot be nothing: this boot
            // wrote a line itself.
            if lines == 0 || lines as u64 > mounted.boots {
                kprintln!(
                    "[test] FAILED: the log has {lines} lines on boot {}",
                    mounted.boots
                );
                return;
            }
        }
        Err(error) => {
            kprintln!("[test] FAILED: could not read the boot log: {error}");
            return;
        }
    }

    deep_filesystem_checks();
}

/// The filesystem's destructive exercises, when this build asks for them.
///
/// Everything above this point checks the filesystem by using it: the store is
/// mounted, the journal is replayed, the root is listed and the log the last
/// boot wrote is read back. That is what an ordinary boot should do, and it is
/// most of what the checks below prove anyway.
///
/// What is here writes: a thousand blocks made, filled, read and emptied, one
/// leaked deliberately and reclaimed, a transaction abandoned and replayed. It
/// is worth doing and it is not worth doing to somebody's disk every time they
/// switch the machine on -- and it cost eleven seconds of a sixteen-second boot
/// while it was.
#[cfg(feature = "deep-selftest")]
fn deep_filesystem_checks() {
    use fs::store;

    match store::self_test() {
        Ok(verdict) => kprintln!("[test] {verdict}"),
        Err(error) => kprintln!("[test] FAILED: NexusFS: {error}"),
    }

    match store::check_self_test() {
        Ok(verdict) => kprintln!("[test] {verdict}"),
        Err(error) => kprintln!("[test] FAILED: the check: {error}"),
    }

    match store::journal_self_test() {
        Ok(verdict) => kprintln!("[test] {verdict}"),
        Err(error) => kprintln!("[test] FAILED: the journal: {error}"),
    }
}

/// Said rather than skipped silently.
///
/// A log that simply lacked those three lines would be a log somebody could
/// read as a machine whose filesystem checks passed quietly. They did not run.
#[cfg(not(feature = "deep-selftest"))]
fn deep_filesystem_checks() {
    kprintln!(
        "[test] the filesystem's destructive checks did not run;          this build was made without deep-selftest"
    );
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

/// Copy the packages in the image onto the store, if they are not there yet.
///
/// The two filesystems have different jobs. The image's FAT partition is what
/// the firmware and the loader read, and nothing on this system writes to it;
/// the store is where programs keep things, and on a fresh disk it is empty
/// because it was made empty. A package that arrived in the image therefore has
/// to be *put* somewhere a program can open it, and this is where.
///
/// It runs after the store is mounted and not with the rest of the filesystem
/// checks, which is where it was first written -- and the store was not up yet,
/// so every copy was refused with "there is no disk to keep a filesystem on".
///
/// Not a special case for one file: every `.NEX` in the image's program
/// directory is copied. A system that hard-coded one package's name would need
/// changing to ship two.
///
/// Pictures travel the same way, into `PICTURES`, for the same reason: a
/// machine whose picture viewer had nothing to show on a fresh disk would be a
/// machine where that program could not be used until somebody had already
/// used it to put a file somewhere.
///
/// And the root certificate store, into `SYSTEM`, where the argument is
/// strongest of all: it is the thing that would have to be fetched securely in
/// order to be able to fetch anything securely.
fn seed_packages() {
    let Ok(partitions) = fs::gpt::read() else {
        return;
    };
    let Some(esp) = partitions.iter().find(|partition| partition.is_esp()) else {
        return;
    };
    let Ok(volume) = fs::fat32::Volume::mount(esp.first_lba) else {
        return;
    };
    let Ok(files) = volume.read_directory_at("BIN") else {
        return;
    };
    for entry in files {
        if entry.is_directory {
            continue;
        }
        let name = entry.name.as_str();
        // Where each kind of thing belongs once it is off the image. By the
        // ending, because the image's directory is flat and the store's is not.
        let into = if name.ends_with(".NEX") {
            "PKG"
        } else if name.ends_with(".PNG")
            || name.ends_with(".BMP")
            || name.ends_with(".JPG")
            || name.ends_with(".AVI")
        {
            "PICTURES"
        } else if name.ends_with(".DEB") || name.ends_with(".TGZ") || name.ends_with(".TAR") {
            // Software written for somewhere else, which arrives the way
            // anything arrives here -- as a file. The downloads folder is where
            // a browser puts what it fetched, and a demonstration package
            // travelling on the image belongs in the same place for the same
            // reason a demonstration picture belongs in PICTURES: it is an
            // example of the thing, sitting where the real thing would sit.
            "DOWNLOAD"
        } else if name.ends_with(".NXR") {
            // The root certificate store. It travels the same way and for the
            // same reason as everything else here: a machine that had to fetch
            // its list of certificate authorities before it could verify a
            // certificate would have nothing to verify the fetch with.
            "SYSTEM"
        } else {
            continue;
        };
        let path = alloc::format!("BIN/{name}");
        let Ok(contents) = volume.read_file(path.as_str()) else {
            kprintln!("[pkg ] {path} is in the image but will not read");
            continue;
        };
        match fs::store::seed(into, name, &contents) {
            Ok(true) => kprintln!(
                "[pkg ] {into}/{name} placed on the store from the image, {} bytes",
                contents.len()
            ),
            Ok(false) => kprintln!("[pkg ] {into}/{name} is already on the store"),
            Err(error) => kprintln!("[pkg ] could not place {path}: {error}"),
        }
    }
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
    // Everything above went straight to the driver, past the block cache. That
    // is the one thing that can leave the cache holding something the disk no
    // longer says -- so it is thrown away rather than reasoned about. It costs
    // a few re-reads once per boot, and reasoning about it is how a cache ends
    // up serving a block that was overwritten behind its back.
    fs::cache::invalidate();

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
/// Shared state for the wait-set self-test.
mod waitset_test {
    use alloc::sync::Arc;
    use core::sync::atomic::AtomicU64;

    use crate::sync::IrqSpinLock;
    use crate::waitset::WaitSet;

    /// The set the waiting thread waits on.
    pub static SET: IrqSpinLock<Option<Arc<WaitSet>>> = IrqSpinLock::new(None);
    /// The key the waiter was given, plus one, so zero means "not yet".
    pub static WOKE_FOR: AtomicU64 = AtomicU64::new(0);
    /// How many keys came back with it, so a set that reported everything as
    /// ready is caught rather than read as a pass.
    pub static WOKE_COUNT: AtomicU64 = AtomicU64::new(0);
}

/// The keys the wait-set test uses.
///
/// Large and unrelated to any handle number, so a set that returned a handle
/// instead of the key it was given fails rather than coincidentally matching.
const KEY_FIRST: u64 = 0x1111_0000;
const KEY_SECOND: u64 = 0x2222_0000;
const KEY_PROCESS: u64 = 0x3333_0000;

/// A thread that blocks on a wait set and records the first thing it hears.
fn waitset_test_waiter(_argument: usize) {
    use core::sync::atomic::Ordering;

    let set = waitset_test::SET.lock().clone();
    let Some(set) = set else {
        return;
    };
    let ready = set.wait();
    waitset_test::WOKE_COUNT.store(ready.len() as u64, Ordering::Relaxed);
    // Published last and with release ordering, because it is what the waiting
    // thread watches: a count seen without the key it belongs to would be read
    // as a wake-up for key zero.
    waitset_test::WOKE_FOR.store(ready.first().copied().unwrap_or(0) + 1, Ordering::Release);
}

/// Exercise waiting for whichever of several things happens first.
///
/// The claim worth making is not that a set can be polled -- that is a loop
/// over a list -- but that a thread blocked on one is woken by *whichever*
/// member becomes ready, having been asleep until it did. So this blocks a
/// thread on a set of three things, checks it left the run queues, makes
/// exactly one of them ready, and requires the key that comes back to be that
/// one and only that one.
///
/// Then the same again for a member of a different kind, because a set whose
/// wake-up path worked for channels and not for processes would pass the first
/// half and hang a real server on the day a client died.
fn waitset_self_test() {
    use alloc::sync::Arc;
    use core::sync::atomic::Ordering;

    let (first_write, first_read) = ipc::Endpoint::pair();
    let (_second_write, second_read) = ipc::Endpoint::pair();

    let space = match memory::address_space::AddressSpace::new() {
        Ok(space) => space,
        Err(error) => {
            kprintln!("[test] FAILED: could not make an address space to watch: {error}");
            return;
        }
    };
    let subject = process::Process::new("watched", Arc::new(space));
    let completion = Arc::clone(&subject.completion);

    let set = Arc::new(waitset::WaitSet::new());
    let members = [
        (
            KEY_FIRST,
            waitset::Watched::Channel(Arc::clone(&first_read)),
        ),
        (
            KEY_SECOND,
            waitset::Watched::Channel(Arc::clone(&second_read)),
        ),
        (
            KEY_PROCESS,
            waitset::Watched::Process(Arc::clone(&completion)),
        ),
    ];
    for (key, what) in members {
        if let Err(error) = set.add(key, what) {
            kprintln!("[test] FAILED: could not add to a wait set: {error}");
            return;
        }
    }

    // The same key twice must be refused. Two members under one name would make
    // the answer ambiguous, which is worse than an error.
    if set.add(
        KEY_FIRST,
        waitset::Watched::Process(Arc::clone(&completion)),
    ) != Err(waitset::WaitSetError::DuplicateKey)
    {
        kprintln!("[test] FAILED: a wait set accepted the same key twice");
        return;
    }
    if set.len() != 3 {
        kprintln!(
            "[test] FAILED: a wait set holds {} members, not 3",
            set.len()
        );
        return;
    }

    // Nothing has happened yet, so nothing is ready. A set that reported a
    // member ready here would make every wait return immediately and turn the
    // whole thing into a spin.
    if !set.poll().is_empty() {
        kprintln!("[test] FAILED: a wait set reported something ready before anything happened");
        return;
    }

    // -- A channel, with a thread already asleep on the set ------------------

    *waitset_test::SET.lock() = Some(Arc::clone(&set));
    waitset_test::WOKE_FOR.store(0, Ordering::Relaxed);
    let blocked_before = sched::stats().blocked;

    if let Err(error) = sched::spawn(
        "waiting",
        sched::thread::Priority::Normal,
        waitset_test_waiter,
        0,
    ) {
        kprintln!("[test] FAILED: could not start a thread to wait on a set: {error}");
        return;
    }

    let mut blocked = false;
    for _ in 0..200 {
        if sched::stats().blocked > blocked_before {
            blocked = true;
            break;
        }
        sched::sleep_ms(1);
    }
    if !blocked {
        kprintln!("[test] FAILED: the thread waiting on a wait set never blocked");
        return;
    }
    if waitset_test::WOKE_FOR.load(Ordering::Acquire) != 0 {
        kprintln!("[test] FAILED: a wait on a set returned before anything was ready");
        return;
    }

    if let Err(error) = first_write.send(b"one", alloc::vec::Vec::new()) {
        kprintln!("[test] FAILED: could not send to a watched channel: {error}");
        return;
    }

    let mut woke = 0;
    for _ in 0..200 {
        woke = waitset_test::WOKE_FOR.load(Ordering::Acquire);
        if woke != 0 {
            break;
        }
        sched::sleep_ms(1);
    }
    if woke == 0 {
        kprintln!("[test] FAILED: a message on a watched channel did not wake the wait set");
        return;
    }
    if woke - 1 != KEY_FIRST {
        kprintln!(
            "[test] FAILED: the wait set woke for key {:#x} and not {KEY_FIRST:#x}",
            woke - 1
        );
        return;
    }
    if waitset_test::WOKE_COUNT.load(Ordering::Relaxed) != 1 {
        kprintln!(
            "[test] FAILED: the wait set reported {} keys ready, not 1",
            waitset_test::WOKE_COUNT.load(Ordering::Relaxed)
        );
        return;
    }

    // -- And a process, which is the other kind of member --------------------

    // The message is read rather than left. Nothing needs its contents, but a
    // message sent and never received is exactly what the boot's own
    // sent-versus-received check exists to catch, and a self-test that trips
    // the system's accounting is a self-test that has to be explained away
    // every time someone reads the numbers.
    if first_read.try_receive().is_none() {
        kprintln!("[test] FAILED: the message a wait set reported was not there to read");
        return;
    }

    // The channel is no longer ready, but it is taken out of the set anyway:
    // what is being tested next is whether a *process* ending wakes the set,
    // and a member that could become ready for any other reason would make the
    // wait return whether it did or not.
    if let Err(error) = set.remove(KEY_FIRST) {
        kprintln!("[test] FAILED: could not remove a key from a wait set: {error}");
        return;
    }
    if set.remove(KEY_FIRST) != Err(waitset::WaitSetError::NoSuchKey) {
        kprintln!("[test] FAILED: removing a key twice was not refused");
        return;
    }

    waitset_test::WOKE_FOR.store(0, Ordering::Relaxed);
    let blocked_before = sched::stats().blocked;
    if let Err(error) = sched::spawn(
        "waiting",
        sched::thread::Priority::Normal,
        waitset_test_waiter,
        0,
    ) {
        kprintln!("[test] FAILED: could not start a second thread to wait: {error}");
        return;
    }
    let mut blocked = false;
    for _ in 0..200 {
        if sched::stats().blocked > blocked_before {
            blocked = true;
            break;
        }
        sched::sleep_ms(1);
    }
    if !blocked {
        kprintln!("[test] FAILED: the second thread waiting on a wait set never blocked");
        return;
    }

    completion.finish(0);

    let mut woke = 0;
    for _ in 0..200 {
        woke = waitset_test::WOKE_FOR.load(Ordering::Acquire);
        if woke != 0 {
            break;
        }
        sched::sleep_ms(1);
    }
    if woke - 1 != KEY_PROCESS {
        kprintln!(
            "[test] FAILED: a process ending woke the wait set for key {:#x}, not {KEY_PROCESS:#x}",
            woke.wrapping_sub(1)
        );
        return;
    }

    // A channel whose peer has gone is ready, and has to be: a holder that was
    // only told about messages would wait forever on a client that died.
    drop(_second_write);
    if !set.poll().contains(&KEY_SECOND) {
        kprintln!("[test] FAILED: a channel whose peer has gone was not reported ready");
        return;
    }

    kprintln!(
        "[test] wait set verified: a thread blocked on three members, was woken by a message \
         for one key and by a process ending for another, and a dead peer reads as ready"
    );
}

/// Shared state for the process-lifetime self-test.
mod process_test {
    use alloc::sync::Arc;
    use core::sync::atomic::AtomicU64;

    use crate::process::Completion;
    use crate::sync::IrqSpinLock;

    /// The completion the waiting thread waits on.
    pub static SUBJECT: IrqSpinLock<Option<Arc<Completion>>> = IrqSpinLock::new(None);
    /// The status the waiter read, plus one, so that zero means "not yet".
    ///
    /// Zero is a legitimate status and this has to distinguish "waiting" from
    /// "finished with zero"; a second flag would be the same thing with a
    /// second chance to publish them out of order.
    pub static OBSERVED: AtomicU64 = AtomicU64::new(0);
}

/// What the process-lifetime test hands back through its completion.
///
/// Not zero, and not one either. A test whose expected value is a number the
/// code would produce by accident is a test that passes when the status is
/// dropped entirely and replaced with a default.
const PROCESS_STATUS: u64 = 0x5A;

/// A thread that blocks until a process ends.
fn process_test_waiter(_argument: usize) {
    use core::sync::atomic::Ordering;

    let subject = process_test::SUBJECT.lock().clone();
    let Some(subject) = subject else {
        return;
    };
    let status = subject.wait();
    process_test::OBSERVED.store(status + 1, Ordering::Release);
}

/// Exercise waiting for a process to end.
///
/// Two claims, and the second is the one a single boot of the real system does
/// not make. `init` waits for the program it asks for, but by then that program
/// has almost always exited already, so what is exercised is the easy path: a
/// wait on something that has finished, which returns without ever blocking.
///
/// This runs the other way round. A thread waits *first*, and is checked to
/// have left the run queues rather than spun, and only then is the completion
/// finished. That is the path with the lost wake-up in it: if the ending were
/// published without waking the queue, or the waiter joined the queue after the
/// ending was published, this thread would wait forever and every real program
/// that ever waits for a child would too.
fn process_self_test() {
    use alloc::sync::Arc;
    use core::sync::atomic::Ordering;

    // A completion with no process behind it. It does not need one: what is
    // being tested is the ending and the waiting, and a real process would add
    // an address space and a program image to something that is about neither.
    let space = match memory::address_space::AddressSpace::new() {
        Ok(space) => space,
        Err(error) => {
            kprintln!("[test] FAILED: could not make an address space to end: {error}");
            return;
        }
    };
    let subject = process::Process::new("ending", Arc::new(space));
    let completion = Arc::clone(&subject.completion);

    if completion.status().is_some() {
        kprintln!("[test] FAILED: a process that has not ended reports a status");
        return;
    }

    *process_test::SUBJECT.lock() = Some(Arc::clone(&completion));
    process_test::OBSERVED.store(0, Ordering::Relaxed);

    // Against a baseline, not against zero. Something else may already be
    // blocked, and a test that read "one thread is blocked" as "our thread is
    // blocked" would pass without the waiter ever having run.
    let blocked_before = sched::stats().blocked;

    if let Err(error) = sched::spawn(
        "waiter",
        sched::thread::Priority::Normal,
        process_test_waiter,
        0,
    ) {
        kprintln!("[test] FAILED: could not start a thread to wait: {error}");
        return;
    }

    // Wait for it to actually block. Sleeping rather than spinning, so this
    // thread is off the processor and the waiter can reach the point where it
    // has nothing left to do.
    let mut blocked = false;
    for _ in 0..200 {
        if sched::stats().blocked > blocked_before {
            blocked = true;
            break;
        }
        sched::sleep_ms(1);
    }
    if !blocked {
        kprintln!("[test] FAILED: the thread waiting for a process never blocked");
        return;
    }
    if process_test::OBSERVED.load(Ordering::Acquire) != 0 {
        kprintln!("[test] FAILED: a wait returned before the process had ended");
        return;
    }

    completion.finish(PROCESS_STATUS);

    let mut observed = 0;
    for _ in 0..200 {
        observed = process_test::OBSERVED.load(Ordering::Acquire);
        if observed != 0 {
            break;
        }
        sched::sleep_ms(1);
    }
    if observed == 0 {
        kprintln!("[test] FAILED: ending a process did not wake the thread waiting for it");
        return;
    }
    if observed - 1 != PROCESS_STATUS {
        kprintln!(
            "[test] FAILED: the waiter read status {} and not {PROCESS_STATUS}",
            observed - 1
        );
        return;
    }

    // And afterwards: the status stays readable, and a second ending does not
    // replace it. A parent that asks twice must not get two different answers,
    // and a process cannot end twice however many threads it comes to have.
    if completion.status() != Some(PROCESS_STATUS) {
        kprintln!("[test] FAILED: the status did not survive being read");
        return;
    }
    completion.finish(PROCESS_STATUS + 1);
    if completion.status() != Some(PROCESS_STATUS) {
        kprintln!("[test] FAILED: a second ending replaced the first one's status");
        return;
    }

    // The completion outlives the process on purpose, which is the whole reason
    // a handle names one rather than naming the process: a parent that keeps a
    // handle must not keep an address space.
    drop(subject);
    if completion.status() != Some(PROCESS_STATUS) {
        kprintln!("[test] FAILED: the status went away with the process");
        return;
    }

    kprintln!(
        "[test] process lifetime verified: a thread blocked waiting for a process, \
         was woken by its ending, and read status {PROCESS_STATUS} back"
    );
}

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

    // A reader that has not stopped is a reader still dereferencing this page.
    // Unmapping it under one would fault on another processor; handing its
    // frames back would give the allocator memory somebody is still reading,
    // which is precisely the corruption this test exists to detect and would be
    // a poor way for the test to end. Two frames are leaked instead, and the
    // machine is told why.
    let reading = tlb_test::RUNNING.load(Ordering::Relaxed);
    if reading > 0 {
        kprintln!(
            "[test] FAILED: {reading} readers were still on the test page; its frames are left              mapped rather than handed back to the allocator"
        );
        return;
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
        if argument.is_multiple_of(2) {
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
