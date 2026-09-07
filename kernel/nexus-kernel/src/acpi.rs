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

    if length < SDT_HEADER_LEN || length > 1024 * 1024 {
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

/// One I/O APIC.
#[derive(Debug, Clone, Copy)]
pub struct IoApic {
    pub id: u8,
    /// Physical address of its register window.
    pub address: u32,
    /// First global system interrupt it handles.
    pub gsi_base: u32,
}

/// What the kernel learned from ACPI.
pub struct AcpiInfo {
    /// Physical address of the local APIC register window.
    pub local_apic_address: u64,
    /// Processors the firmware reported.
    pub processors: Vec<Processor>,
    /// I/O APICs the firmware reported.
    pub io_apics: Vec<IoApic>,
    /// Whether the firmware says a legacy 8259 PIC is present and must be
    /// masked before the APIC is used.
    pub has_legacy_pic: bool,
}

impl AcpiInfo {
    /// Processors firmware says can be started.
    #[must_use]
    pub fn enabled_processor_count(&self) -> usize {
        self.processors.iter().filter(|cpu| cpu.enabled).count()
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

    // Walk the root table's pointers looking for the MADT.
    let entries = &root_table.bytes[SDT_HEADER_LEN..];
    let mut madt: Option<Table> = None;

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
        if &table.signature == b"APIC" {
            madt = Some(table);
            break;
        }
    }

    let madt = madt.ok_or(AcpiError::NoMadt)?;
    Ok(parse_madt(&madt))
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
        has_legacy_pic,
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
    if info.has_legacy_pic {
        kprintln!("[acpi] a legacy 8259 is present and must be masked");
    }
}
