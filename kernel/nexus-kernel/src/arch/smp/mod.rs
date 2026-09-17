//! Starting the other processors.
//!
//! A processor other than the one firmware started is an *application
//! processor*, and it sits halted until told otherwise. Waking one is a fixed
//! sequence defined by the architecture: an INIT inter-processor interrupt to
//! reset it, then a startup IPI carrying the page number to begin executing at.
//!
//! Two startup IPIs are sent rather than one. Older processors need the second,
//! and one that has already started ignores it, so sending both is both correct
//! and cheaper than working out which kind of processor this is.

pub mod trampoline;

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use nexus_abi::layout;

use super::{apic, percpu, time};
use crate::kprintln;
use crate::memory::{self, paging};

/// Stack size given to each application processor.
const AP_STACK_SIZE: u64 = 64 * 1024;

/// Processors that have signalled they are running.
static STARTED: AtomicUsize = AtomicUsize::new(0);

/// Set by each application processor once it reaches Rust.
static LAST_STARTED_INDEX: AtomicU64 = AtomicU64::new(u64::MAX);

/// Why a processor could not be started.
#[derive(Debug, Clone, Copy)]
pub enum SmpError {
    /// The local APIC is not running, so no IPI can be sent.
    NoLocalApic,
    /// The trampoline could not be placed at its fixed address.
    TrampolineUnavailable(paging::MapError),
    /// The trampoline is larger than the page reserved for it.
    TrampolineTooLarge { bytes: usize },
    /// No memory for a processor's stack.
    OutOfMemory,
}

impl core::fmt::Display for SmpError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoLocalApic => f.write_str("no local APIC, so no processor can be started"),
            Self::TrampolineUnavailable(error) => {
                write!(f, "could not map the startup trampoline: {error}")
            }
            Self::TrampolineTooLarge { bytes } => {
                write!(
                    f,
                    "the startup trampoline is {bytes} bytes, more than one page"
                )
            }
            Self::OutOfMemory => f.write_str("no memory for an application processor stack"),
        }
    }
}

/// Write a parameter into the trampoline's block.
///
/// # Safety
///
/// The trampoline page must be mapped and writable.
unsafe fn write_parameter(offset: usize, value: u64) {
    let address = layout::phys_to_virt(trampoline::TRAMPOLINE_ADDRESS) + offset as u64;
    // SAFETY: upheld by the caller; the block sits inside the reserved page.
    unsafe { core::ptr::write_volatile(address as *mut u64, value) };
}

/// Read a parameter back.
///
/// # Safety
///
/// See [`write_parameter`].
unsafe fn read_parameter(offset: usize) -> u64 {
    let address = layout::phys_to_virt(trampoline::TRAMPOLINE_ADDRESS) + offset as u64;
    // SAFETY: upheld by the caller.
    unsafe { core::ptr::read_volatile(address as *const u64) }
}

/// Copy the trampoline into its reserved page.
///
/// # Safety
///
/// The page must be reserved for this and reachable through the direct map.
unsafe fn install_trampoline() -> Result<(), SmpError> {
    let code = trampoline::code();
    // The parameter block sits in the same page, so the code must stop short
    // of it.
    if code.len() > trampoline::parameter::PAGE_TABLE {
        return Err(SmpError::TrampolineTooLarge { bytes: code.len() });
    }

    let destination = layout::phys_to_virt(trampoline::TRAMPOLINE_ADDRESS) as *mut u8;
    // SAFETY: the page is excluded from the frame allocator and mapped through
    // the direct map, so nothing else owns it, and no processor is running from
    // it yet.
    unsafe {
        core::ptr::copy_nonoverlapping(code.as_ptr(), destination, code.len());
        trampoline::write_descriptor_tables(destination);
    }
    Ok(())
}

/// Identity-map the low memory the trampoline executes from.
///
/// # Safety
///
/// Undone by [`remove_low_identity_map`] once bring-up finishes.
unsafe fn add_low_identity_map() -> Result<(), SmpError> {
    // The trampoline runs at its physical address from reset until the far jump
    // into the higher half, so those instructions have to be mapped where they
    // physically are. The kernel tore the bootloader's identity map down on
    // purpose, so one page of it comes back just for the duration.
    //
    // SAFETY: the page is reserved for the trampoline; nothing else maps it.
    unsafe {
        paging::map_page(
            trampoline::TRAMPOLINE_ADDRESS,
            trampoline::TRAMPOLINE_ADDRESS,
            paging::WRITABLE,
        )
        .map_err(SmpError::TrampolineUnavailable)
    }
}

/// Remove the temporary identity mapping.
///
/// # Safety
///
/// No processor may still be executing from the trampoline.
unsafe fn remove_low_identity_map() {
    // SAFETY: every started processor has acknowledged reaching the higher
    // half before this runs.
    unsafe {
        let _ = paging::unmap_page(trampoline::TRAMPOLINE_ADDRESS);
    }
}

/// Entry point every application processor reaches from the trampoline.
///
/// # Safety
///
/// Called only by the trampoline, once per processor, on its own stack.
extern "sysv64" fn application_processor_entry(cpu_index: u64) -> ! {
    let index = cpu_index as usize;

    // SAFETY: this processor is executing on the stack the boot processor gave
    // it, and `index` is unique to it.
    unsafe {
        let stack_top = read_parameter(trampoline::parameter::STACK_TOP);
        percpu::install(index, apic::local_id(), stack_top);

        // Descriptor tables are per-processor: the GDT because the task
        // register and the TSS are, and the IDT because the register that holds
        // it is. Sharing the tables is fine; each core still has to load them.
        super::gdt::init(index);
        super::interrupts::load_on_this_processor();

        // This core's own APIC and timer. The frequency is already known from
        // the boot processor's calibration, so nothing is measured again.
        apic::init_processor(super::interrupts::APIC_TIMER_VECTOR, time::frequency_hz());

        // Per processor, not once: `EFER`, `STAR`, `LSTAR` and `FMASK` are
        // per-processor registers, so a core that skipped this would take an
        // invalid opcode the first time a user thread migrated onto it and made
        // a call.
        super::syscall::init();
        // As on the boot processor: the vector registers have to be turned on
        // wherever a foreign program might be scheduled, which is everywhere.
        super::fpu::enable();
    }

    STARTED.fetch_add(1, Ordering::Release);
    LAST_STARTED_INDEX.store(cpu_index, Ordering::Release);

    // Interrupts on, then into the scheduler, which this core enters as its own
    // idle thread and never leaves.
    super::interrupts::enable();
    crate::sched::run_idle_on_this_processor(index)
}

/// Start every processor the firmware reported, other than this one.
///
/// Returns the number brought online. Failure to start one processor is
/// reported and the rest are still attempted: a system with three of four cores
/// is far better than one that gives up.
///
/// # Safety
///
/// Call once, from the boot processor, after the local APIC is running and the
/// heap is available.
pub unsafe fn start_processors(processors: &[crate::acpi::Processor]) -> Result<usize, SmpError> {
    if !apic::is_active() {
        return Err(SmpError::NoLocalApic);
    }

    // SAFETY: the trampoline page is reserved and the caller guarantees the
    // memory manager is up.
    unsafe {
        add_low_identity_map()?;
        install_trampoline()?;
        write_parameter(trampoline::parameter::PAGE_TABLE, paging::active_root());
        write_parameter(
            trampoline::parameter::ENTRY,
            application_processor_entry as *const () as u64,
        );
    }

    let boot_apic_id = apic::local_id();
    let mut index = 1usize;

    for processor in processors {
        if !processor.enabled || processor.apic_id == boot_apic_id {
            continue;
        }
        if index >= percpu::MAX_PROCESSORS {
            kprintln!(
                "[smp ] more processors than the kernel supports; stopping at {}",
                percpu::MAX_PROCESSORS
            );
            break;
        }

        match start_one(processor.apic_id, index) {
            Ok(()) => index += 1,
            Err(error) => {
                kprintln!(
                    "[smp ] processor with APIC {} did not start: {error}",
                    processor.apic_id
                );
            }
        }
    }

    // SAFETY: every processor that started has acknowledged reaching the higher
    // half, so none is still executing from the trampoline.
    unsafe { remove_low_identity_map() };

    Ok(STARTED.load(Ordering::Acquire))
}

/// Start one processor and wait for it to report in.
fn start_one(apic_id: u32, index: usize) -> Result<(), SmpError> {
    // Each processor gets its own stack, in the same region as thread stacks,
    // with a guard page below it for the same reason.
    let stack_top = allocate_stack(index)?;

    let expected = STARTED.load(Ordering::Acquire) + 1;

    // SAFETY: the trampoline page is mapped and this processor is not running.
    unsafe {
        write_parameter(trampoline::parameter::STACK_TOP, stack_top);
        write_parameter(trampoline::parameter::CPU_INDEX, index as u64);
        write_parameter(trampoline::parameter::ACKNOWLEDGE, 0);
    }

    // SAFETY: the local APIC is running; `apic_id` came from the MADT.
    unsafe {
        apic::send_init(apic_id);
        // The architecture asks for 10 ms after INIT before the first startup
        // IPI. Waiting on the tick rather than a spin count keeps this correct
        // whatever the processor's speed.
        time::busy_wait_ms(10);

        apic::send_startup(apic_id, trampoline::TRAMPOLINE_VECTOR);
        time::busy_wait_ms(1);

        // A second startup IPI: older processors need it, and one that is
        // already running ignores it.
        if !acknowledged() {
            apic::send_startup(apic_id, trampoline::TRAMPOLINE_VECTOR);
        }
    }

    // Wait for the processor to reach Rust and count itself in. Bounded, so a
    // core that never starts costs a fixed delay rather than the whole boot.
    let deadline = time::ticks() + time::ms_to_ticks(200);
    while STARTED.load(Ordering::Acquire) < expected && time::ticks() < deadline {
        core::hint::spin_loop();
    }

    if STARTED.load(Ordering::Acquire) < expected {
        // SAFETY: reading the trampoline's parameter block.
        let reached_long_mode = unsafe { acknowledged() };
        kprintln!(
            "[smp ] processor with APIC {apic_id} timed out ({})",
            if reached_long_mode {
                "reached long mode but not Rust"
            } else {
                "never left the trampoline"
            }
        );
    }

    Ok(())
}

/// Whether the processor being started has reached long mode.
///
/// # Safety
///
/// The trampoline page must be mapped.
unsafe fn acknowledged() -> bool {
    // SAFETY: upheld by the caller.
    unsafe { read_parameter(trampoline::parameter::ACKNOWLEDGE) != 0 }
}

/// Map a stack for processor `index`.
///
/// Placed in the kernel stack area, well above the slots threads use, so the
/// two allocators cannot collide.
fn allocate_stack(index: usize) -> Result<u64, SmpError> {
    /// First stack slot reserved for application processors.
    const AP_STACK_SLOT_BASE: u64 = 1024;

    let frames = AP_STACK_SIZE / 4096;
    let order = nexus_mm::order_for_frames(frames).ok_or(SmpError::OutOfMemory)?;
    let physical = memory::allocate_block(order).ok_or(SmpError::OutOfMemory)?;

    let slot = AP_STACK_SLOT_BASE + index as u64;
    let base = layout::KERNEL_STACK_AREA_BASE + slot * layout::KERNEL_STACK_STRIDE;
    let bottom = base + (layout::KERNEL_STACK_STRIDE - AP_STACK_SIZE);

    // SAFETY: the slot is inside the kernel stack area and is used by nothing
    // else; the block was just allocated.
    let mapped = unsafe {
        paging::map_range(
            bottom,
            physical,
            AP_STACK_SIZE,
            paging::WRITABLE | paging::NO_EXECUTE | paging::GLOBAL,
        )
    };
    if mapped.is_err() {
        // SAFETY: the block was allocated above and never used.
        unsafe { memory::free_block(physical, order) };
        return Err(SmpError::OutOfMemory);
    }

    // 16-byte aligned, as the ABI requires at a function's entry.
    Ok((bottom + AP_STACK_SIZE) & !0xF)
}

/// Processors running, including the boot processor.
#[must_use]
pub fn processor_count() -> usize {
    percpu::online_count()
}

/// Log each processor and the interrupts it has taken.
///
/// The interrupt counts are the useful part: a processor that started but is no
/// longer taking its timer interrupt has wedged, and nothing else about it
/// would say so.
pub fn report() {
    kprintln!("[smp ] {} processors online", percpu::online_count());
    for index in 0..percpu::MAX_PROCESSORS {
        if let Some((apic_id, interrupts, _)) = percpu::snapshot(index) {
            kprintln!(
                "[smp ]   processor {index} (APIC {apic_id}) has taken {interrupts} interrupts"
            );
        }
    }
}

/// A one-line summary: processors online, and the fewest interrupts any of them
/// has taken.
///
/// The minimum rather than the total, because a total hides a stalled core
/// behind three healthy ones.
#[must_use]
pub fn summary() -> (usize, u64) {
    let mut fewest = u64::MAX;
    for index in 0..percpu::MAX_PROCESSORS {
        if let Some((_, interrupts, _)) = percpu::snapshot(index) {
            fewest = fewest.min(interrupts);
        }
    }
    (
        percpu::online_count(),
        if fewest == u64::MAX { 0 } else { fewest },
    )
}
