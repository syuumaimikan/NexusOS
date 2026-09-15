//! Turning the machine off, and restarting it.
//!
//! # Why this is a service and not a system call
//!
//! The same reason as [`crate::machine`]: a system call cannot be withheld.
//! Every call in [`crate::arch::syscall`] is available to every process that
//! runs. "Stop the machine" is not a power every program should have, and the
//! way to make that true here is to make it arrive in a message — the
//! compositor is lent the channel and hands it to the program with the button
//! on it. Nothing else can ask.
//!
//! # What ACPI gives without an interpreter, and what it does not
//!
//! ACPI has two halves. One is a set of **fixed registers** described in the
//! FADT: a port number in a table, written with a value from a table. The other
//! is **AML** — a bytecode language with a namespace, methods, operation
//! regions and a mutex model — in which everything else is written.
//!
//! Shutdown and reset are in the first half, which is why they are here. The
//! sleep type for S5 is the one piece that lives in the second, and it is a
//! package of constants that can be read without running anything; see
//! [`crate::acpi::read_s5`], which says plainly that it is a shortcut.
//!
//! **The battery is in the second half, and so is not here.** `_BST` is a
//! method, and reading it means executing AML. That is thousands of lines, and
//! it is the honest reason this reports no battery rather than a number. What
//! it reports instead is what the machine actually says — which under QEMU, and
//! on any desktop, is the truth.
//!
//! # Sleep is not here either, and that is a layering decision
//!
//! Real suspend-to-RAM saves the processor state, puts memory into
//! self-refresh, hands firmware a waking vector, and reinitialises every device
//! on the way back. Getting it wrong means a machine that does not come back,
//! which is the worst failure an operating system has. It is not here.
//!
//! What a person means by "sleep" on a machine like this one is mostly: the
//! screen goes dark and stops doing work until I touch it. That needs nothing
//! from the kernel — the scheduler already idles every processor that has
//! nothing to run — and it needs the compositor to stop drawing, which is a
//! decision about what is on the screen and therefore the compositor's. It is
//! implemented there, and this file does not pretend to a power state it does
//! not enter.
//!
//! # Nothing is flushed on the way out
//!
//! Deliberately, and it is worth saying because the absence looks like an
//! oversight. The block cache in [`crate::fs::cache`] is **write-through**:
//! every write reaches the disk before `write` returns, and the cache is
//! updated afterwards so that a failed write can never be served from memory as
//! though it had succeeded. There is no dirty data to lose, so there is nothing
//! for this to flush.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::acpi::{Fadt, Register};
use crate::sync::IrqSpinLock;
use crate::{arch, ipc, kprintln, sched};

/// Bit 13 of the PM1 control register: do the thing the sleep type says.
const SLEEP_ENABLE: u16 = 1 << 13;

/// Bit 0 of the PM1 control register: ACPI is in charge rather than firmware.
const SCI_ENABLED: u16 = 1;

/// What the firmware said about power, read once at boot.
static SETTINGS: IrqSpinLock<Option<Settings>> = IrqSpinLock::new(None);

/// Whether anything has asked for the machine to stop.
static STOPPING: AtomicBool = AtomicBool::new(false);

/// How many times each thing has been asked for, for the monitor.
static SHUTDOWNS: AtomicU64 = AtomicU64::new(0);
static RESTARTS: AtomicU64 = AtomicU64::new(0);

/// What is known about stopping this machine.
#[derive(Debug, Clone, Copy)]
struct Settings {
    fadt: Fadt,
    /// The S5 sleep types, when the DSDT gave them.
    s5: Option<(u8, u8)>,
}

/// Remember what ACPI said. Called once, from the boot path.
pub fn init(info: &crate::acpi::AcpiInfo) {
    let Some(fadt) = info.fadt else {
        kprintln!("[pwr ] no FADT: this machine cannot be turned off from software");
        return;
    };

    *SETTINGS.lock() = Some(Settings { fadt, s5: info.s5 });

    match info.s5 {
        Some((a, b)) => kprintln!(
            "[pwr ] PM1a at {:#06x}, PM1b at {:#06x}, S5 sleep types {a} and {b}",
            fadt.pm1a_control,
            fadt.pm1b_control
        ),
        None => kprintln!(
            "[pwr ] PM1a at {:#06x}, but the DSDT gives no S5 this can read; \
             shutdown will guess and then fall back",
            fadt.pm1a_control
        ),
    }
    match fadt.reset {
        Some((Register::Port(port), value)) => {
            kprintln!("[pwr ] reset by writing {value:#04x} to port {port:#06x}");
        }
        Some((Register::Memory(at), value)) => {
            kprintln!("[pwr ] reset by writing {value:#04x} to {at:#018x}");
        }
        Some((Register::Elsewhere, _)) | None => {
            kprintln!("[pwr ] no usable ACPI reset register; restart will use the 8042");
        }
    }
}

/// Whether something has asked the machine to stop.
///
/// A courtesy for whatever is drawing: the last thing on the screen should not
/// be half a frame. Nothing here waits for anybody.
#[must_use]
pub fn stopping() -> bool {
    STOPPING.load(Ordering::Relaxed)
}

/// Whether this machine can be turned off from software at all.
#[must_use]
pub fn can_shut_down() -> bool {
    SETTINGS
        .lock()
        .is_some_and(|settings| settings.fadt.pm1a_control != 0)
}

/// How many shutdowns and restarts have been asked for.
#[must_use]
pub fn statistics() -> (u64, u64) {
    (
        SHUTDOWNS.load(Ordering::Relaxed),
        RESTARTS.load(Ordering::Relaxed),
    )
}

/// Ask firmware to hand ACPI over, if it has not already.
///
/// On a machine booted through UEFI this has happened before the kernel runs
/// and `SMI_CMD` is zero. On one booted through a legacy BIOS it has not, and
/// writing the sleep registers before the handover does nothing at all.
fn enable_acpi(fadt: &Fadt) {
    if fadt.pm1a_control == 0 {
        return;
    }
    // SAFETY: a port number firmware itself put in the FADT, read as the
    // 16-bit word the specification says this register is.
    let control = unsafe { arch::io::inw(fadt.pm1a_control) };
    if control & SCI_ENABLED != 0 {
        return;
    }
    if fadt.smi_command == 0 || fadt.acpi_enable == 0 {
        // Firmware offers no way to ask. Nothing to do but carry on and let the
        // write below either work or not.
        return;
    }
    let Ok(port) = u16::try_from(fadt.smi_command) else {
        return;
    };

    // SAFETY: the command port and the value both come from the FADT, and this
    // is the exchange the specification defines for them.
    unsafe { arch::io::outb(port, fadt.acpi_enable) };

    // The specification says to poll and does not say for how long. Three
    // milliseconds is far longer than any machine takes, and short enough that
    // one which is never going to answer does not look hung.
    let deadline = arch::time::uptime_ms() + 3;
    while arch::time::uptime_ms() < deadline {
        // SAFETY: as above.
        if unsafe { arch::io::inw(fadt.pm1a_control) } & SCI_ENABLED != 0 {
            return;
        }
        core::hint::spin_loop();
    }
    kprintln!("[pwr ] firmware did not take ACPI within 3 ms; trying anyway");
}

/// Turn the machine off. Never returns.
pub fn shut_down() -> ! {
    SHUTDOWNS.fetch_add(1, Ordering::Relaxed);
    STOPPING.store(true, Ordering::Relaxed);
    kprintln!("[pwr ] shutting down");

    let settings = *SETTINGS.lock();
    if let Some(settings) = settings {
        enable_acpi(&settings.fadt);

        // The sleep type from the DSDT, or five -- which is what S5's type is
        // on the overwhelming majority of machines and is worth one attempt
        // before falling back to something cruder.
        let (a, b) = settings.s5.unwrap_or((5, 5));

        if settings.fadt.pm1a_control != 0 {
            // SAFETY: the port came from the FADT and the value is the sleep
            // type the DSDT gave for it, in the bits the specification puts it.
            unsafe {
                arch::io::outw(
                    settings.fadt.pm1a_control,
                    (u16::from(a) << 10) | SLEEP_ENABLE,
                );
            }
        }
        if settings.fadt.pm1b_control != 0 {
            // SAFETY: as above, for the second block.
            unsafe {
                arch::io::outw(
                    settings.fadt.pm1b_control,
                    (u16::from(b) << 10) | SLEEP_ENABLE,
                );
            }
        }

        // The write above does not return on a machine that obeyed it.
        wait_a_moment();
    }

    // What emulators answer to, on machines where the tables did not work or
    // were not there. Named as what they are -- emulator shutdown ports, not an
    // ACPI mechanism. On hardware that does not implement them, an I/O write to
    // an unclaimed port is discarded.
    kprintln!("[pwr ] ACPI shutdown did not take; trying the emulator ports");
    // SAFETY: three fixed ports, written as words, with no effect on a machine
    // that does not claim them.
    unsafe {
        arch::io::outw(0x604, 0x2000); // QEMU 2.0 and later
        arch::io::outw(0xB004, 0x2000); // Bochs, and older QEMU
        arch::io::outw(0x4004, 0x3400); // VirtualBox
    }
    wait_a_moment();

    kprintln!("[pwr ] this machine will not turn itself off; halting instead");
    kprintln!("[pwr ] it is safe to switch it off now");
    arch::halt_forever()
}

/// Restart the machine. Never returns.
pub fn restart() -> ! {
    RESTARTS.fetch_add(1, Ordering::Relaxed);
    STOPPING.store(true, Ordering::Relaxed);
    kprintln!("[pwr ] restarting");

    let settings = *SETTINGS.lock();
    if let Some(settings) = settings {
        match settings.fadt.reset {
            Some((Register::Port(port), value)) => {
                // SAFETY: the port and the value are both what firmware put in
                // its own reset register description.
                unsafe { arch::io::outb(port, value) };
                wait_a_moment();
            }
            Some((Register::Memory(at), value)) => {
                // SAFETY: a physical address from the FADT, inside the direct
                // map, written as the single byte the specification says.
                unsafe {
                    core::ptr::write_volatile(
                        nexus_abi::layout::phys_to_virt(at) as *mut u8,
                        value,
                    );
                }
                wait_a_moment();
            }
            Some((Register::Elsewhere, _)) | None => {}
        }
    }

    // The keyboard controller's reset line, which predates ACPI and is still
    // wired on essentially every x86 machine. Bit 1 of the status port is the
    // input buffer; a command written while it is full is lost.
    kprintln!("[pwr ] ACPI reset did not take; pulsing the 8042");
    for _ in 0..100_000 {
        // SAFETY: reading the 8042 status port has no effect.
        if unsafe { arch::io::inb(0x64) } & 0x02 == 0 {
            break;
        }
        core::hint::spin_loop();
    }
    // SAFETY: 0xFE on the 8042 command port pulses the processor's reset line.
    // This is the oldest way to restart a PC and is why the A20 gate exists.
    unsafe { arch::io::outb(0x64, 0xFE) };
    wait_a_moment();

    // A triple fault. With no interrupt descriptor table the next interrupt
    // becomes a double fault, and with nothing to handle that either the
    // processor resets. It always works, and it is last because it gives the
    // machine no chance to do anything tidy on the way out.
    kprintln!("[pwr ] the 8042 did not reset it either; faulting");
    // SAFETY: this is the documented last resort, it is the last thing this
    // function does, and the function never returns.
    unsafe { arch::triple_fault() }
}

/// Long enough for a machine that obeyed a power register to act on it.
///
/// A shutdown write is not instantaneous -- the chipset sequences it -- and a
/// kernel that gave up after a handful of instructions would report failure on
/// a machine that was halfway through succeeding.
fn wait_a_moment() {
    let deadline = arch::time::uptime_ms() + 500;
    while arch::time::uptime_ms() < deadline {
        arch::wait_for_interrupt();
    }
}

// ---------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------

/// Ask what the machine can do about power, and what it is running on.
const ASK: &[u8] = b"pwr?";
/// Turn it off.
const OFF: &[u8] = b"off ";
/// Restart it.
const BOOT: &[u8] = b"boot";
/// It worked, and a payload follows.
const GOOD: &[u8] = b"ok  ";
/// It did not, and a two-byte reason follows.
const BAD: &[u8] = b"err!";

/// The layout of the `pwr?` reply. Bumped when a field moves or changes meaning.
const VERSION: u16 = 1;

/// How many bytes version 1 of the reply payload is: four flags and nothing
/// else. Fixed, so that a reader knows before it reads.
const PAYLOAD: usize = 4;

const MALFORMED: u16 = 1;
const UNKNOWN_TAG: u16 = 2;
const NO_SUCH_VERSION: u16 = 3;
const CANNOT: u16 = 4;

/// Channels programs have been lent.
static SERVICES: IrqSpinLock<Vec<Arc<ipc::Endpoint>>> = IrqSpinLock::new(Vec::new());

/// What was asked for, and has not happened yet.
///
/// Acting on a request inside the service thread would stop the machine while a
/// program was waiting for a reply it would never get. So the reply goes first
/// and the machine stops afterwards, from the same thread, once the message is
/// on its way.
static WANTED: IrqSpinLock<Option<Wanted>> = IrqSpinLock::new(None);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wanted {
    Off,
    Restart,
}

/// Make a channel to this service, and return the end a program should hold.
///
/// Read *and* write: a request-and-reply service is asked as well as answered,
/// and a handle without write cannot carry the ask.
pub fn endpoint() -> Arc<ipc::Endpoint> {
    let (service, client) = ipc::Endpoint::pair();
    SERVICES.lock().push(service);
    client
}

/// Start the thread that answers.
pub fn start_thread() {
    match sched::spawn(
        "power",
        // Above background: somebody pressing a button is waiting, and the
        // reply is what tells the screen to stop.
        sched::thread::Priority::Normal,
        power_thread,
        0,
    ) {
        Ok(id) => kprintln!("[pwr ] power thread {id} started"),
        Err(error) => kprintln!("[pwr ] could not start the power thread: {error}"),
    }
}

/// Wait to be asked, answer, and then do what was asked.
fn power_thread(_argument: usize) {
    let set = Arc::new(crate::waitset::WaitSet::new());
    let mut watched = 0usize;

    loop {
        let services: Vec<Arc<ipc::Endpoint>> = SERVICES.lock().clone();
        if services.len() != watched {
            for key in 0..watched as u64 {
                set.remove(key).ok();
            }
            for (key, service) in services.iter().enumerate() {
                set.add(
                    key as u64,
                    crate::waitset::Watched::Channel(Arc::clone(service)),
                )
                .ok();
            }
            watched = services.len();
        }

        let seen = set.change_count();
        let answered = serve(&services);

        // After the replies have been sent, not before. A program that asked
        // for shutdown gets its answer; then the machine stops.
        //
        // The pause is not decoration. The reply has to cross a channel and be
        // read, and the compositor has to notice `stopping()` and put the last
        // frame up. Half a second is long enough for both and short enough that
        // nobody wonders whether the button worked.
        let wanted = WANTED.lock().take();
        if let Some(wanted) = wanted {
            sched::sleep_ms(500);
            match wanted {
                Wanted::Off => shut_down(),
                Wanted::Restart => restart(),
            }
        }

        if !answered {
            // Read the counter, test, then block only if nothing has changed --
            // the discipline every wait in this kernel follows. With a deadline
            // as well, because a service lent after this thread last looked is
            // not in the set and cannot wake it.
            set.wait_since(seen, Some(arch::time::ticks() + 500));
        }
    }
}

/// Answer everything that has been asked. Returns whether anything was.
fn serve(services: &[Arc<ipc::Endpoint>]) -> bool {
    let mut did = false;
    let mut gone = false;

    for service in services {
        // Bounded, so one program asking in a tight loop cannot stop the others
        // being answered.
        for _ in 0..16 {
            let Some(request) = service.try_receive() else {
                break;
            };
            did = true;
            let reply = answer(&request.bytes);
            if service.send(&reply, Vec::new()).is_err() {
                gone = true;
                break;
            }
        }
        if !service.peer_open() {
            gone = true;
        }
    }

    if gone {
        SERVICES.lock().retain(|service| service.peer_open());
    }
    did
}

/// Do one request.
fn answer(request: &[u8]) -> Vec<u8> {
    let refuse = |code: u16| {
        let mut reply = Vec::with_capacity(6);
        reply.extend_from_slice(BAD);
        reply.extend_from_slice(&code.to_le_bytes());
        reply
    };

    let Some(tag) = request.get(..4) else {
        return refuse(MALFORMED);
    };

    // Exactly the tag and the version, and nothing after. A request with
    // trailing bytes comes from something that believes this protocol is a
    // different shape, and answering it would confirm the belief.
    if request.len() != 6 {
        return refuse(MALFORMED);
    }
    if u16::from_le_bytes([request[4], request[5]]) != VERSION {
        return refuse(NO_SUCH_VERSION);
    }

    match tag {
        _ if tag == ASK => {
            let mut reply = Vec::with_capacity(8 + PAYLOAD);
            reply.extend_from_slice(GOOD);
            reply.extend_from_slice(&VERSION.to_le_bytes());
            reply.extend_from_slice(&(PAYLOAD as u16).to_le_bytes());
            reply.extend_from_slice(&flags().to_le_bytes());
            reply
        }
        _ if tag == OFF || tag == BOOT => {
            if tag == OFF && !can_shut_down() {
                // Said rather than accepted and then silently not done. A
                // button that appears to work and does not is worse than one
                // that says it cannot.
                return refuse(CANNOT);
            }
            *WANTED.lock() = Some(if tag == OFF {
                Wanted::Off
            } else {
                Wanted::Restart
            });
            let mut reply = Vec::with_capacity(8);
            reply.extend_from_slice(GOOD);
            reply.extend_from_slice(&VERSION.to_le_bytes());
            reply.extend_from_slice(&0u16.to_le_bytes());
            reply
        }
        _ => refuse(UNKNOWN_TAG),
    }
}

/// What this machine can do about power, as bits.
///
/// Four of them, and three are false on every machine this has run on so far.
/// They are here because the alternative is a program guessing, and a program
/// that guesses wrong offers a person a button that does nothing.
fn flags() -> u32 {
    let mut flags = 0u32;
    if can_shut_down() {
        flags |= 1 << 0;
    }
    // Restart always works: the 8042 and the triple fault are always there.
    flags |= 1 << 1;
    // There is no battery, because reading one needs an AML interpreter. This
    // bit exists so that a program can tell "no battery" from "did not ask",
    // and it is the honest answer on a desktop as well as here.
    // flags |= 1 << 2;  // on battery
    // flags |= 1 << 3;  // battery level known
    flags
}
