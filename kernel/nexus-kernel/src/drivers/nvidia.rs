//! Native NVIDIA discovery and read-only Blackwell identity registers.
//!
//! Register definitions: NVIDIA/open-gpu-kernel-modules, commit
//! ca2d03e6fafd4d4b56b383497d931c25b283bfcf,
//! src/common/inc/swref/published/nv_ref.h (NV_PMC_BOOT_0 / BOOT_42).
//! This does not boot GSP or submit graphics commands. Firmware scanout remains
//! in use until the native resource manager and rendering driver are ready.

use super::pci::{self, BaseAddress, Device};
use crate::{kprintln, memory};
pub mod bootstrap;
#[derive(Clone, Copy)]
struct Adapter {
    address: pci::Address,
    bar0: u64,
    chip: u32,
}
static DETECTED: crate::sync::IrqSpinLock<Option<Adapter>> = crate::sync::IrqSpinLock::new(None);
static PREPARED: crate::sync::IrqSpinLock<Option<bootstrap::Prepared>> =
    crate::sync::IrqSpinLock::new(None);

/// Called after storage is available. Preserve scanout while preparing the
/// pinned firmware; this does not report acceleration or enable DMA engines.
pub fn prepare_firmware() {
    let adapter = *DETECTED.lock();
    if let Some(adapter) = adapter {
        kprintln!(
            "[nv  ] preparing bootstrap for {} chip={:#x} BAR0={:#x}",
            adapter.address,
            adapter.chip,
            adapter.bar0
        );
        match bootstrap::prepare() {
            Ok(prepared) => {
                prepared.report();
                *PREPARED.lock() = Some(prepared);
            }
            Err(error) => kprintln!(
                "[nv  ] bootstrap not prepared: {}; firmware display retained",
                error
            ),
        }
    }
    #[cfg(feature = "nvidia-bootstrap-test")]
    bootstrap::self_test();
}

const NVIDIA: u16 = 0x10de;
const RTX_5060: u16 = 0x2d05;
const BOOT_42: usize = 0xa00;
const GB200_ARCHITECTURE: u32 = 0x1b;

fn boot0_architecture(value: u32) -> u32 {
    ((value >> 24) & 0x1f) | (((value >> 8) & 1) << 5)
}

/// Called once at boot, before graphics drivers claim device resources.
///
/// # Safety
/// Devices must come from PCI enumeration; no other driver may own their BAR0.
/// No PCI command, power state, engine or bus-master register is changed here.
pub unsafe fn probe(devices: &[Device]) {
    let mut found = false;
    for device in devices
        .iter()
        .filter(|d| d.vendor == NVIDIA && d.class == 3)
    {
        found = true;
        let name = if device.device == RTX_5060 {
            "GeForce RTX 5060"
        } else {
            "NVIDIA display adapter"
        };
        kprintln!(
            "[nv  ] {}: {} ({:04x}:{:04x})",
            device.address,
            name,
            device.vendor,
            device.device
        );
        // SAFETY: enumerated configuration space, read-only access.
        let command = unsafe { pci::read_config(device.address, 4) };
        let power = unsafe {
            device.capabilities(|offset, id| {
                if id != 1 || offset > 0xf8 {
                    return None;
                }
                Some(pci::read_config(device.address, offset + 4) & 3)
            })
        };
        if command & 2 == 0 || power.is_some_and(|state| state != 0) {
            kprintln!("[nv  ] MMIO unavailable: memory decoding={}, power={:?}; retaining firmware display", command & 2 != 0, power);
            continue;
        }
        // Only BAR0 is interpreted. A 64-bit BAR consumes the next slot;
        // indiscriminately reading all six as independent BARs is incorrect.
        let BaseAddress::Memory { base, .. } = (unsafe { device.base_address(0) }) else {
            kprintln!("[nv  ] BAR0 is not a memory aperture");
            continue;
        };
        if base == 0 || base & 0xfff != 0 || device.device != RTX_5060 {
            kprintln!(
                "[nv  ] no register probe for this device/BAR0 ({:#x})",
                base
            );
            continue;
        }
        // RTX 5060 BAR0 contains both published identity registers within its
        // first page. Map only this page; never map VRAM as device registers.
        // SAFETY: firmware allocated BAR0, decoding is enabled and device D0.
        let Some(mapped) = (unsafe { memory::map_device_registers(base, 4096) }) else {
            kprintln!("[nv  ] cannot map identity registers at {:#x}", base);
            continue;
        };
        // SAFETY: aligned, published read-only registers within mapped BAR0.
        let (boot0, boot42) = unsafe {
            (
                core::ptr::read_volatile(mapped as *const u32),
                core::ptr::read_volatile((mapped as usize + BOOT_42) as *const u32),
            )
        };
        if boot0 == u32::MAX || boot42 == u32::MAX || boot0 == 0 || boot42 == 0 {
            kprintln!(
                "[nv  ] identity registers did not answer: {:#010x}/{:#010x}",
                boot0,
                boot42
            );
            continue;
        }
        let architecture = (boot42 >> 24) & 0x3f;
        let chip = (boot42 >> 20) & 0x3ff;
        if architecture != GB200_ARCHITECTURE || boot0_architecture(boot0) != architecture {
            kprintln!(
                "[nv  ] unexpected architecture: BOOT0={:#010x}, BOOT42={:#010x}",
                boot0,
                boot42
            );
            continue;
        }
        kprintln!(
            "[nv  ] Blackwell chip={:#x}, revision={}.{}.{}; BAR0={:#x}",
            chip,
            (boot42 >> 16) & 15,
            (boot42 >> 12) & 15,
            (boot42 >> 8) & 15,
            base
        );
        kprintln!(
            "[nv  ] native discovery complete; GSP/VRAM/command submission are not initialised"
        );
        // This first-stage port owns one selected adapter. Do not associate
        // its bootstrap memory with whichever second card enumerates last.
        let mut selected = DETECTED.lock();
        if selected.is_none() {
            *selected = Some(Adapter {
                address: device.address,
                bar0: base,
                chip,
            });
        }
    }
    if !found {
        kprintln!("[nv  ] no physical NVIDIA display adapter found");
    }
}
