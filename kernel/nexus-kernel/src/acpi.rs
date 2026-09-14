//! ACPI table discovery.
//!
//! The bootloader hands over the RSDP address; this walks from there to the
//! tables the kernel needs. Right now that is the MADT, which is the only place
//! that says how many processors exist and where their interrupt controllers
//! are — everything about the local APIC and SMP starts here.
//!
//! # Reading firmware tables safely
//!
//! ACPI tables are packed, unaligned, and written by firmware of varying
//! quality. Three rules apply throughout:
//!
//! * every field is read byte-wise, because a `repr(packed)` struct read
//!   through a reference is undefined behaviour and the addresses have no
//!   alignment guarantee anyway;
//! * every table's checksum is verified before its contents are believed;
//! * every length is bounds-checked against the table it came from, so a
//!   malformed table produces a rejection rather than a walk off the end.

use alloc::vec::Vec;

use nexus_abi::layout;

use crate::kprintln;

/// Read `count` bytes at physical address `phys` through the direct map.
///
/// # Safety
///
/// `phys .. phys + count` must lie within the direct map and be readable. ACPI
/// tables live in firmware-reserved RAM, which the bootloader covers.
unsafe fn read_bytes(phys: u64, count: usize) -> &'static [u8] {
    // SAFETY: upheld by the caller. The tables are never written, so a shared
    // reference for the life of the kernel is sound.
    unsafe { core::slice::from_raw_parts(layout::phys_to_virt(phys) as *const u8, count) }
}

/// Read a little-endian `u32` from `bytes` at `offset`.
fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let field = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([field[0], field[1], field[2], field[3]]))
}

/// Read a little-endian `u64` from `bytes` at `offset`.
fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let field = bytes.get(offset..offset + 8)?;
    Some(u64::from_le_bytes([
        field[0], field[1], field[2], field[3], field[4], field[5], field[6], field[7],
    ]))
}

/// Whether `bytes` sums to zero modulo 256, as every ACPI structure must.
fn checksum_is_valid(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) == 0
}

/// Size of the header every system description table starts with.
const SDT_HEADER_LEN: usize = 36;

/// Lengths a table may plausibly claim.
///
/// Shorter than its own header, or larger than a megabyte, and the value came
/// from something other than a table -- which matters because the length is
/// what says how much memory to read.
const PLAUSIBLE_TABLE: core::ops::RangeInclusive<usize> = SDT_HEADER_LEN..=1024 * 1024;

/// A system description table: its signature and its full contents.
struct Table {
    signature: [u8; 4],
    bytes: &'static [u8],
}

/// Map a table at `phys`, validating its header and checksum.
///
/// # Safety
///
/// `phys` must point at a table inside the direct map.
unsafe fn read_table(phys: u64) -> Option<Table> {
    // SAFETY: upheld by the caller. The header is read first so the real length
    // is known before any more of the table is touched.
    let header = unsafe { read_bytes(phys, SDT_HEADER_LEN) };
    let length = read_u32(header, 4)? as usize;

    if !PLAUSIBLE_TABLE.contains(&length) {
        return None;
    }

    // SAFETY: the length came from the header just validated.
    let bytes = unsafe { read_bytes(phys, length) };
    if !checksum_is_valid(bytes) {
        return None;
    }

    Some(Table {
        signature: [bytes[0], bytes[1], bytes[2], bytes[3]],
        bytes,
    })
}

/// One processor, as the MADT describes it.
#[derive(Debug, Clone, Copy)]
pub struct Processor {
    /// The ACPI processor identifier.
    pub acpi_id: u32,
    /// The local APIC identifier, which is how the processor is addressed.
    pub apic_id: u32,
    /// Whether firmware says this processor can be started.
    pub enabled: bool,
}

/// A firmware correction to the legacy IRQ-to-interrupt mapping.
///
/// The old 8259 wiring is not what the I/O APIC sees. Firmware describes the
/// difference here, and a kernel that assumes IRQ *n* is global system
/// interrupt *n* will program the wrong pin on many machines — most commonly
/// for the timer, where IRQ 0 is routed to GSI 2.
#[derive(Debug, Clone, Copy)]
pub struct InterruptOverride {
    /// The legacy IRQ number.
    pub source_irq: u8,
    /// The global system interrupt it actually arrives on.
    pub global_system_interrupt: u32,
    /// Raw flags: bits 0..2 polarity, bits 2..4 trigger mode.
    pub flags: u16,
}

impl InterruptOverride {
    /// Whether this interrupt is active low rather than active high.
    #[must_use]
    pub fn is_active_low(&self) -> bool {
        // 0 means "conforms to the bus default", which for ISA is active high.
        self.flags & 0b11 == 0b11
    }

    /// Whether this interrupt is level triggered rather than edge triggered.
    #[must_use]
    pub fn is_level_triggered(&self) -> bool {
        // As above: 0 conforms to the ISA default, which is edge.
        (self.flags >> 2) & 0b11 == 0b11
    }
}

/// One I/O APIC.
#[derive(Debug, Clone, Copy)]
pub struct IoApic {
    pub id: u8,
    /// Physical address of its register window.
    pub address: u32,
    /// First global system interrupt it handles.
    pub gsi_base: u32,
}

/// Where a register is, in whichever address space it lives in.
///
/// ACPI's Generic Address Structure: twelve bytes saying which space, how wide
/// the register is, and where. Only the two spaces that matter here are
/// distinguished, because a reset register in a space this kernel cannot reach
/// is a reset register it must not pretend to have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Register {
    /// An x86 I/O port.
    Port(u16),
    /// A physical memory address.
    Memory(u64),
    /// Somewhere this kernel does not know how to write: PCI configuration
    /// space, the embedded controller, SMBus. Named rather than silently
    /// dropped, so that a failure to reboot can say why.
    Elsewhere,
}

/// Turn twelve bytes of Generic Address Structure into a register.
fn read_address(bytes: &[u8], at: usize) -> Option<Register> {
    let space = *bytes.get(at)?;
    let address = read_u64(bytes, at + 4)?;
    Some(match space {
        0 => Register::Memory(address),
        // An I/O port above 0xFFFF is not one; the field is 64 bits and the
        // space is 16.
        1 if address <= u64::from(u16::MAX) => Register::Port(address as u16),
        _ => Register::Elsewhere,
    })
}

/// What the Fixed ACPI Description Table says about turning the machine off.
///
/// Everything here is a *fixed* register: a port number in a table, not a
/// method to be interpreted. That is the whole reason power management is
/// reachable without an AML interpreter, and it is also the boundary -- the
/// battery, the lid switch and the thermal zones are all AML methods and none
/// of them are here. See `power.rs`.
#[derive(Debug, Clone, Copy)]
pub struct Fadt {
    /// Where to write to enter a sleep state. Zero when the firmware offers no
    /// PM1a control block, which means no ACPI shutdown.
    pub pm1a_control: u16,
    /// A second block, on machines that have one. Zero when there is not.
    pub pm1b_control: u16,
    /// The port to poke to ask firmware to hand ACPI over, and the value.
    /// Both zero on a machine that is in ACPI mode already, which is every
    /// machine booted through UEFI.
    pub smi_command: u32,
    pub acpi_enable: u8,
    /// The reset register and the value to write to it, when the firmware says
    /// it has one.
    pub reset: Option<(Register, u8)>,
    /// Physical address of the DSDT, which is where `\_S5_` lives.
    pub dsdt: u64,
}

/// What the kernel learned from ACPI.
pub struct AcpiInfo {
    /// Physical address of the local APIC register window.
    pub local_apic_address: u64,
    /// Processors the firmware reported.
    pub processors: Vec<Processor>,
    /// I/O APICs the firmware reported.
    pub io_apics: Vec<IoApic>,
    /// Corrections to the legacy IRQ mapping.
    pub interrupt_overrides: Vec<InterruptOverride>,
    /// Whether the firmware says a legacy 8259 PIC is present and must be
    /// masked before the APIC is used.
    pub has_legacy_pic: bool,
    /// What the FADT said, when there was one. A machine without it can still
    /// run; it just cannot be turned off politely.
    pub fadt: Option<Fadt>,
    /// The sleep type values for S5, from the DSDT. See `read_s5`.
    pub s5: Option<(u8, u8)>,
}

impl AcpiInfo {
    /// Processors firmware says can be started.
    #[must_use]
    pub fn enabled_processor_count(&self) -> usize {
        self.processors.iter().filter(|cpu| cpu.enabled).count()
    }

    /// The global system interrupt a legacy IRQ actually arrives on.
    ///
    /// Identity unless firmware said otherwise, which is the rule the ACPI
    /// specification gives and the one that is wrong often enough to matter.
    #[must_use]
    pub fn global_system_interrupt_for(&self, irq: u8) -> u32 {
        self.interrupt_overrides
            .iter()
            .find(|override_entry| override_entry.source_irq == irq)
            .map_or(u32::from(irq), |entry| entry.global_system_interrupt)
    }

    /// The override describing `irq`, if firmware supplied one.
    #[must_use]
    pub fn override_for(&self, irq: u8) -> Option<&InterruptOverride> {
        self.interrupt_overrides
            .iter()
            .find(|entry| entry.source_irq == irq)
    }

    /// The I/O APIC that handles `global_system_interrupt`, and the pin index
    /// within it.
    #[must_use]
    pub fn route(&self, global_system_interrupt: u32) -> Option<(&IoApic, u32)> {
        self.io_apics
            .iter()
            .filter(|io_apic| io_apic.gsi_base <= global_system_interrupt)
            .max_by_key(|io_apic| io_apic.gsi_base)
            .map(|io_apic| (io_apic, global_system_interrupt - io_apic.gsi_base))
    }
}

/// Why ACPI could not be read.
#[derive(Debug, Clone, Copy)]
pub enum AcpiError {
    /// The bootloader reported no RSDP.
    NoRsdp,
    /// The RSDP's signature or checksum is wrong.
    BadRsdp,
    /// Neither an RSDT nor an XSDT could be read.
    NoRootTable,
    /// No MADT, so there is nothing to say where the APICs are.
    NoMadt,
}

impl core::fmt::Display for AcpiError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::NoRsdp => "the bootloader reported no ACPI RSDP",
            Self::BadRsdp => "the ACPI RSDP is malformed",
            Self::NoRootTable => "neither the RSDT nor the XSDT could be read",
            Self::NoMadt => "no MADT; the interrupt controllers cannot be located",
        })
    }
}

/// Default local APIC register address, used when the MADT does not override it.
const DEFAULT_LOCAL_APIC_ADDRESS: u64 = 0xFEE0_0000;

/// Parse the ACPI tables reachable from `rsdp_address`.
///
/// # Safety
///
/// `rsdp_address` must be the address the bootloader took from the UEFI
/// configuration table, and the direct map must cover the firmware tables.
pub unsafe fn init(rsdp_address: u64) -> Result<AcpiInfo, AcpiError> {
    if rsdp_address == 0 {
        return Err(AcpiError::NoRsdp);
    }

    // The ACPI 1.0 RSDP is 20 bytes; 2.0 extends it to 36 and adds a second
    // checksum over the whole thing.
    // SAFETY: the bootloader took this address from firmware and the direct map
    // covers firmware memory.
    let rsdp = unsafe { read_bytes(rsdp_address, 36) };
    if &rsdp[0..8] != b"RSD PTR " || !checksum_is_valid(&rsdp[0..20]) {
        return Err(AcpiError::BadRsdp);
    }

    let revision = rsdp[15];
    // Prefer the 64-bit XSDT when the firmware offers one: on a machine with
    // tables above 4 GiB the 32-bit RSDT simply cannot describe them.
    let root = if revision >= 2 && checksum_is_valid(rsdp) {
        let xsdt = read_u64(rsdp, 24).unwrap_or(0);
        if xsdt != 0 {
            // SAFETY: an address from a checksum-validated RSDP.
            unsafe { read_table(xsdt) }.map(|table| (table, 8usize))
        } else {
            None
        }
    } else {
        None
    };

    let (root_table, pointer_width) = match root {
        Some(found) => found,
        None => {
            let rsdt = read_u32(rsdp, 16).unwrap_or(0) as u64;
            // SAFETY: as above.
            let table = unsafe { read_table(rsdt) }.ok_or(AcpiError::NoRootTable)?;
            (table, 4usize)
        }
    };

    kprintln!(
        "[acpi] ACPI {}.0, root table {} with {} entries",
        if pointer_width == 8 { 2 } else { 1 },
        core::str::from_utf8(&root_table.signature).unwrap_or("????"),
        (root_table.bytes.len() - SDT_HEADER_LEN) / pointer_width
    );

    // Walk the root table's pointers looking for the tables that are wanted.
    // Both, in one pass: the walk maps and checksums every table it touches,
    // and doing it twice would do that work twice.
    let entries = &root_table.bytes[SDT_HEADER_LEN..];
    let mut madt: Option<Table> = None;
    let mut fadt: Option<Fadt> = None;

    for chunk in entries.chunks_exact(pointer_width) {
        let address = if pointer_width == 8 {
            u64::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
            ])
        } else {
            u64::from(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        };
        if address == 0 {
            continue;
        }

        // SAFETY: an address from a checksum-validated root table.
        let Some(table) = (unsafe { read_table(address) }) else {
            continue;
        };
        if &table.signature == b"APIC" && madt.is_none() {
            madt = Some(table);
        } else if &table.signature == b"FACP" && fadt.is_none() {
            fadt = parse_fadt(&table);
        }
        if madt.is_some() && fadt.is_some() {
            break;
        }
    }

    let madt = madt.ok_or(AcpiError::NoMadt)?;
    let mut info = parse_madt(&madt);
    // SAFETY: the DSDT address came from a checksum-validated FADT, and the
    // direct map covers firmware memory.
    info.s5 = fadt.and_then(|fadt| unsafe { read_s5(fadt.dsdt) });
    info.fadt = fadt;
    Ok(info)
}

/// Pull the fixed power registers out of a FADT.
///
/// Offsets are from the ACPI specification and are the same in every revision
/// from 1.0 onwards; the 64-bit forms that follow them are ignored, because
/// every one of these registers is an I/O port on every machine that has them
/// and the wide forms exist for architectures this kernel does not run on.
fn parse_fadt(table: &Table) -> Option<Fadt> {
    let bytes = table.bytes;

    // A port number wider than sixteen bits is not one. Zero means "there is
    // no such block", which is a legitimate answer and not an error.
    let port = |at: usize| -> u16 {
        match read_u32(bytes, at) {
            Some(value) if value <= u32::from(u16::MAX) => value as u16,
            _ => 0,
        }
    };

    // The 64-bit DSDT pointer, when the table is long enough to have one and
    // it is not zero. A table that predates it, or has left it empty, still
    // has the 32-bit field at offset 40.
    let dsdt = match read_u64(bytes, 140) {
        Some(wide) if wide != 0 => wide,
        _ => u64::from(read_u32(bytes, 40)?),
    };

    // Bit 10 of the flags is RESET_REG_SUP: the firmware saying it has a reset
    // register at all. Without it the register bytes are meaningless, and a
    // kernel that wrote to them anyway would be writing an arbitrary value to
    // an arbitrary port.
    let flags = read_u32(bytes, 112).unwrap_or(0);
    let reset = if flags & (1 << 10) != 0 {
        match (read_address(bytes, 116), bytes.get(128).copied()) {
            (Some(Register::Elsewhere), _) | (None, _) => None,
            (Some(register), Some(value)) => Some((register, value)),
            (Some(_), None) => None,
        }
    } else {
        None
    };

    Some(Fadt {
        pm1a_control: port(64),
        pm1b_control: port(68),
        smi_command: read_u32(bytes, 48).unwrap_or(0),
        acpi_enable: bytes.get(52).copied().unwrap_or(0),
        reset,
        dsdt,
    })
}

/// Find the sleep type values for S5 in the DSDT.
///
/// # What this is, honestly
///
/// `\_S5_` is an AML object, and reading AML properly means writing an
/// interpreter: a bytecode machine with a namespace, method invocation,
/// operation regions and a mutex model. That is thousands of lines and it is
/// what would be needed for the battery, the lid switch and the thermal zones.
/// It is not here.
///
/// What *is* here is the narrow thing that does not need it. `\_S5_` is a
/// package of constants -- it has to be, because firmware evaluates it during
/// shutdown when almost nothing else is running -- so it can be found by
/// looking for its name and read by parsing the handful of bytes after it.
/// This is a well-worn shortcut and it is a shortcut; it is written down as one
/// rather than dressed up as an ACPI implementation.
///
/// Every step is checked and any surprise gives `None`, because the thing being
/// parsed is bytecode that this does not otherwise understand. A wrong answer
/// here writes a wrong value to a hardware register.
///
/// # Safety
///
/// `dsdt` must be a DSDT address from a checksum-validated FADT.
unsafe fn read_s5(dsdt: u64) -> Option<(u8, u8)> {
    if dsdt == 0 {
        return None;
    }
    // SAFETY: upheld by the caller.
    let table = unsafe { read_table(dsdt) }?;
    if &table.signature != b"DSDT" {
        return None;
    }
    let bytes = table.bytes;

    // The name, anywhere in the block. There is only ever one.
    let at = bytes.windows(4).position(|window| window == b"_S5_")?;
    let mut at = at + 4;

    // A PackageOp, possibly after the NameOp's own bookkeeping. Two bytes of
    // slack covers both spellings seen in the wild without turning this into a
    // scan that could find a package belonging to something else.
    let package = (0..3).find(|skip| bytes.get(at + skip) == Some(&0x12))?;
    at += package + 1;

    // PkgLength: the top two bits say how many extra bytes follow. Its value is
    // not needed -- what follows is read positionally -- but its width is,
    // because it says where the element count is.
    let lead = *bytes.get(at)?;
    at += 1 + usize::from(lead >> 6);

    let elements = *bytes.get(at)?;
    if elements < 2 {
        return None;
    }
    at += 1;

    // Two integers. AML spells a small one three ways, and all three appear in
    // real firmware.
    let integer = |at: &mut usize| -> Option<u8> {
        let value = match *bytes.get(*at)? {
            0x00 => {
                *at += 1;
                0
            }
            0x01 => {
                *at += 1;
                1
            }
            0x0A => {
                let byte = *bytes.get(*at + 1)?;
                *at += 2;
                byte
            }
            // Anything else is an expression rather than a constant, which
            // means this is not the simple package this can read.
            _ => return None,
        };
        // SLP_TYP is three bits wide. A value that does not fit is a value this
        // has misread, and writing it would set bits in the control register
        // that mean something else entirely.
        (value <= 0b111).then_some(value)
    };

    let a = integer(&mut at)?;
    let b = integer(&mut at)?;
    Some((a, b))
}

/// Parse the Multiple APIC Description Table.
fn parse_madt(madt: &Table) -> AcpiInfo {
    let bytes = madt.bytes;

    let mut local_apic_address =
        u64::from(read_u32(bytes, SDT_HEADER_LEN).unwrap_or(DEFAULT_LOCAL_APIC_ADDRESS as u32));
    let flags = read_u32(bytes, SDT_HEADER_LEN + 4).unwrap_or(0);
    // Bit 0 of the MADT flags: the machine has an 8259 that must be masked
    // before the APIC is trusted with interrupt delivery.
    let has_legacy_pic = flags & 1 != 0;

    let mut processors = Vec::new();
    let mut io_apics = Vec::new();
    let mut interrupt_overrides = Vec::new();

    // Entries follow the 8 bytes of MADT-specific header. Each is
    // `type, length, payload`, and the length is what advances the cursor —
    // walking by a fixed stride would desynchronise on the first entry type
    // this kernel does not know about.
    let mut offset = SDT_HEADER_LEN + 8;
    while offset + 2 <= bytes.len() {
        let entry_type = bytes[offset];
        let entry_length = bytes[offset + 1] as usize;

        // A zero or oversized length would loop forever or read past the end.
        if entry_length < 2 || offset + entry_length > bytes.len() {
            break;
        }
        let entry = &bytes[offset..offset + entry_length];

        match entry_type {
            // Processor Local APIC.
            0 if entry_length >= 8 => {
                let flags = read_u32(entry, 4).unwrap_or(0);
                processors.push(Processor {
                    acpi_id: u32::from(entry[2]),
                    apic_id: u32::from(entry[3]),
                    // Bit 0 is "enabled"; bit 1 is "can be enabled online".
                    enabled: flags & 0b11 != 0,
                });
            }
            // I/O APIC.
            1 if entry_length >= 12 => {
                io_apics.push(IoApic {
                    id: entry[2],
                    address: read_u32(entry, 4).unwrap_or(0),
                    gsi_base: read_u32(entry, 8).unwrap_or(0),
                });
            }
            // Interrupt Source Override: the legacy IRQ wiring as the I/O APIC
            // actually sees it.
            2 if entry_length >= 10 => {
                interrupt_overrides.push(InterruptOverride {
                    source_irq: entry[3],
                    global_system_interrupt: read_u32(entry, 4).unwrap_or(0),
                    flags: u16::from(entry[8]) | (u16::from(entry[9]) << 8),
                });
            }
            // Local APIC Address Override: a 64-bit address replacing the
            // 32-bit one in the header.
            5 if entry_length >= 12 => {
                if let Some(address) = read_u64(entry, 4) {
                    if address != 0 {
                        local_apic_address = address;
                    }
                }
            }
            // Processor Local x2APIC, for machines with more than 255 CPUs.
            9 if entry_length >= 16 => {
                let apic_id = read_u32(entry, 4).unwrap_or(0);
                let flags = read_u32(entry, 8).unwrap_or(0);
                let acpi_id = read_u32(entry, 12).unwrap_or(0);
                processors.push(Processor {
                    acpi_id,
                    apic_id,
                    enabled: flags & 0b11 != 0,
                });
            }
            _ => {}
        }

        offset += entry_length;
    }

    if local_apic_address == 0 {
        local_apic_address = DEFAULT_LOCAL_APIC_ADDRESS;
    }

    AcpiInfo {
        local_apic_address,
        processors,
        io_apics,
        interrupt_overrides,
        has_legacy_pic,
        // Filled in by the caller, which is the only thing that has read the
        // FADT. Here rather than threaded through `parse_madt`, which is about
        // interrupt controllers and has no business knowing about sleep.
        fadt: None,
        s5: None,
    }
}

/// Log what ACPI reported.
pub fn report(info: &AcpiInfo) {
    kprintln!(
        "[acpi] local APIC at {:#010x}, {} processors ({} enabled), {} I/O APICs",
        info.local_apic_address,
        info.processors.len(),
        info.enabled_processor_count(),
        info.io_apics.len()
    );
    for processor in &info.processors {
        kprintln!(
            "[acpi]   processor {} has local APIC {}{}",
            processor.acpi_id,
            processor.apic_id,
            if processor.enabled {
                ""
            } else {
                " (disabled by firmware)"
            }
        );
    }
    for io_apic in &info.io_apics {
        kprintln!(
            "[acpi]   I/O APIC {} at {:#010x}, global system interrupts from {}",
            io_apic.id,
            io_apic.address,
            io_apic.gsi_base
        );
    }
    for entry in &info.interrupt_overrides {
        kprintln!(
            "[acpi]   IRQ {} is really global system interrupt {}{}{}",
            entry.source_irq,
            entry.global_system_interrupt,
            if entry.is_active_low() {
                ", active low"
            } else {
                ""
            },
            if entry.is_level_triggered() {
                ", level triggered"
            } else {
                ""
            }
        );
    }
    if info.has_legacy_pic {
        kprintln!("[acpi] a legacy 8259 is present and must be masked");
    }
}
