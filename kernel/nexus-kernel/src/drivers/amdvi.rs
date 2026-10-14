//! AMD-Vi v1 DMA domains, command completion and fault records.
//! Activation currently belongs to the explicit native DMA test. Discovery
//! is read-only; no NVIDIA device is enabled or impersonated by this driver.
use crate::{acpi::AcpiInfo, kprintln, memory, sync::IrqSpinLock};
use alloc::vec::Vec;
#[cfg(feature = "amdvi-dma-test")]
use core::sync::atomic::{fence, Ordering};
use nexus_abi::layout;
use nexus_nvidia::amdvi as abi;

#[derive(Debug)]
pub enum Error {
    #[cfg(feature = "amdvi-dma-test")]
    Firmware,
    #[cfg(feature = "amdvi-dma-test")]
    Absent,
    #[cfg(feature = "amdvi-dma-test")]
    Scope,
    #[cfg(feature = "amdvi-dma-test")]
    AlreadyActive,
    Memory,
    Mapping,
    #[cfg(feature = "amdvi-dma-test")]
    Timeout,
    #[cfg(feature = "amdvi-dma-test")]
    Readback,
}
static IVRS: IrqSpinLock<Option<&'static [u8]>> = IrqSpinLock::new(None);
pub fn discover(info: Option<&AcpiInfo>) {
    let Some(bytes) = info.and_then(|info| info.ivrs) else {
        kprintln!("[dma ] no AMD IVRS table; AMD-Vi GPU mapping unavailable");
        return;
    };
    match abi::Ivrs::parse(bytes) {
        Ok(table) => {
            for unit in table.units() {
                // Establish the permanent MMIO mapping before counting the
                // domain's temporary frames. Discovery still performs no writes.
                let control = unsafe { memory::map_device_registers(unit.base(), 0x4000) }.map(
                    |mapped| unsafe {
                        core::ptr::read_volatile((mapped as usize + 0x18) as *const u64)
                    },
                );
                kprintln!(
                    "[dma ] AMD-Vi unit {:04x}:{:04x}, MMIO={:#x}, capability={:#x}, control={:?}; discovery only",
                    unit.segment(),
                    unit.device(),
                    unit.base(),
                    unit.capability(),control
                );
            }
            *IVRS.lock() = Some(bytes);
        }
        Err(error) => kprintln!("[dma ] rejected AMD IVRS table: {:?}", error),
    }
}

struct Block {
    physical: u64,
    order: usize,
}
impl Block {
    fn new(order: usize) -> Result<Self, Error> {
        let physical = memory::allocate_block(order).ok_or(Error::Memory)?;
        let block = Self { physical, order };
        unsafe {
            core::ptr::write_bytes(block.pointer(), 0, block.size());
        }
        Ok(block)
    }
    fn size(&self) -> usize {
        4096usize << self.order
    }
    fn pointer(&self) -> *mut u8 {
        layout::phys_to_virt(self.physical) as *mut u8
    }
    fn read(&self, offset: usize) -> u64 {
        assert!(offset % 8 == 0 && offset + 8 <= self.size());
        unsafe { core::ptr::read_volatile(self.pointer().add(offset) as *const u64) }
    }
    fn write(&mut self, offset: usize, value: u64) {
        assert!(offset % 8 == 0 && offset + 8 <= self.size());
        unsafe {
            core::ptr::write_volatile(self.pointer().add(offset) as *mut u64, value);
        }
    }
}
impl Drop for Block {
    fn drop(&mut self) {
        // This owner is reclaimed only before publication or after the
        // controller has confirmed stopped. It never owns endpoint data.
        unsafe {
            core::ptr::write_bytes(self.pointer(), 0, self.size());
            memory::free_block(self.physical, self.order);
        }
    }
}

pub struct Domain {
    pages: Vec<Block>,
    published: bool,
}
impl Domain {
    pub fn new() -> Result<Self, Error> {
        let mut pages = Vec::new();
        pages.try_reserve(1).map_err(|_| Error::Memory)?;
        pages.push(Block::new(0)?);
        Ok(Self {
            pages,
            published: false,
        })
    }
    pub fn root(&self) -> u64 {
        self.pages[0].physical
    }
    /// Walk the stored tables, checking presence, expected level and rights.
    pub fn resolve(&self, iova: u64, access: u64) -> Option<u64> {
        if iova >= abi::IOVA_LIMIT || access == 0 || access & !abi::RW != 0 {
            return None;
        }
        let mut page = 0;
        let mut rights = abi::RW;
        for level in (0..=2).rev() {
            let slot = ((iova >> (12 + 9 * level)) & 511) as usize;
            let entry = self.pages[page].read(slot * 8);
            rights &= entry & abi::RW;
            if entry & 1 == 0 || rights & access != access || (entry >> 9) & 7 != level {
                return None;
            }
            let physical = entry & abi::ADDRESS_MASK;
            if level == 0 {
                return Some(physical | (iova & 4095));
            }
            page = self
                .pages
                .iter()
                .position(|block| block.physical == physical)?;
        }
        None
    }
    /// Map exclusively owned, pinned RAM. The caller must keep the data alive
    /// until this domain has been detached and invalidation has completed.
    /// No live mapping edits are allowed without a controller cache fence.
    pub unsafe fn map(&mut self, iova: u64, physical: u64, permissions: u64) -> Result<(), Error> {
        if self.published || iova >= abi::IOVA_LIMIT || iova % 4096 != 0 {
            return Err(Error::Mapping);
        }
        abi::page_address(physical).map_err(|_| Error::Mapping)?;
        let mut page = 0;
        for level in (1..=2).rev() {
            let slot = ((iova >> (12 + 9 * level)) & 511) as usize;
            let entry = self.pages[page].read(slot * 8);
            if entry == 0 {
                self.pages.try_reserve(1).map_err(|_| Error::Memory)?;
                let child = Block::new(0)?;
                let pointer = abi::pointer(child.physical, level as u8, abi::RW)
                    .map_err(|_| Error::Mapping)?;
                self.pages.push(child);
                let next = self.pages.len() - 1;
                self.pages[page].write(slot * 8, pointer);
                page = next;
            } else {
                page = self
                    .pages
                    .iter()
                    .position(|block| block.physical == entry & abi::ADDRESS_MASK)
                    .ok_or(Error::Mapping)?;
            }
        }
        let slot = ((iova >> 12) & 511) as usize;
        if self.pages[page].read(slot * 8) != 0 {
            return Err(Error::Mapping);
        }
        self.pages[page].write(
            slot * 8,
            abi::pointer(physical, 0, permissions).map_err(|_| Error::Mapping)?,
        );
        Ok(())
    }
}

#[cfg(feature = "amdvi-dma-test")]
const CONTROL: usize = 0x18;
#[cfg(feature = "amdvi-dma-test")]
const HEAD: usize = 0x2000;
#[cfg(feature = "amdvi-dma-test")]
const TAIL: usize = 0x2008;
#[cfg(feature = "amdvi-dma-test")]
const EVENT_TAIL: usize = 0x2018;
#[cfg(feature = "amdvi-dma-test")]
const STATUS: usize = 0x2020;
/// Controller state owns all published tables and command/event storage.
/// Quarantine it (forget the owner) if stop fails; never free live DMA memory.
#[cfg(feature = "amdvi-dma-test")]
struct Controller {
    registers: usize,
    old: [u64; 7],
    device_table: Block,
    commands: Block,
    events: Block,
    completion: Block,
    domain: Domain,
    tail: usize,
    active: bool,
}
#[cfg(feature = "amdvi-dma-test")]
impl Controller {
    fn read(&self, at: usize) -> u64 {
        unsafe { core::ptr::read_volatile((self.registers + at) as *const u64) }
    }
    fn write(&self, at: usize, value: u64) {
        unsafe {
            core::ptr::write_volatile((self.registers + at) as *mut u64, value);
        }
    }
    fn new(device: u16, domain: Domain) -> Result<Self, Error> {
        let bytes = (*IVRS.lock()).ok_or(Error::Absent)?;
        let table = abi::Ivrs::parse(bytes).map_err(|_| Error::Firmware)?;
        let mut selected = None;
        for unit in table.units().filter(|unit| unit.segment() == 0) {
            let requester = unit.requester(device).map_err(|_| Error::Scope)?;
            if requester == Some(device) {
                if selected.is_some_and(|base| base != unit.base()) {
                    return Err(Error::Scope);
                }
                selected = Some(unit.base());
            } else if requester.is_some() {
                return Err(Error::Scope);
            }
        }
        let base = selected.ok_or(Error::Scope)?;
        let registers =
            unsafe { memory::map_device_registers(base, 0x4000) }.ok_or(Error::Memory)? as usize;
        let read =
            |offset: usize| unsafe { core::ptr::read_volatile((registers + offset) as *const u64) };
        let control = read(CONTROL);
        if control == u64::MAX || control & ((1 << 12) | 7) != 0 {
            return Err(Error::AlreadyActive);
        }
        let old = [
            read(0),
            read(8),
            read(16),
            control,
            read(HEAD),
            read(TAIL),
            read(EVENT_TAIL),
        ];
        // A full 16-bit requester table; other entries remain invalid/bypass.
        // Existing firmware-owned active translations are always refused.
        let mut device_table = Block::new(9)?;
        let entry = abi::device_entry(domain.root(), 1).map_err(|_| Error::Mapping)?;
        for (word, &value) in entry.iter().enumerate() {
            device_table.write(device as usize * 32 + word * 8, value);
        }
        Ok(Self {
            registers,
            old,
            device_table,
            commands: Block::new(0)?,
            events: Block::new(0)?,
            completion: Block::new(0)?,
            domain,
            tail: 0,
            active: false,
        })
    }
    fn start(&mut self, device: u16) -> Result<(), Error> {
        self.domain.published = true;
        fence(Ordering::SeqCst);
        self.write(0, self.device_table.physical | 511);
        self.write(8, self.commands.physical | (8 << 56)); // 256 commands * 16 bytes.
        self.write(16, self.events.physical | (8 << 56));
        self.write(HEAD, 0);
        self.write(TAIL, 0);
        self.write(0x2010, 0);
        self.write(EVENT_TAIL, 0);
        // Mark active before the first enabling write: failures must stop or
        // quarantine all published allocations, even on a readback failure.
        self.active = true;
        self.write(CONTROL, self.old[3] | 1 | 4 | (1 << 12));
        if self.read(CONTROL) & (1 | 4 | (1 << 12)) != (1 | 4 | (1 << 12)) {
            return Err(Error::Readback);
        }
        self.fence(device)?;
        Ok(())
    }
    fn fence(&mut self, device: u16) -> Result<(), Error> {
        self.completion.write(0, 0);
        let commands = [
            abi::invalidate_device(device),
            abi::invalidate_domain(1),
            abi::completion(self.completion.physical, 0xc0de_cafe).map_err(|_| Error::Mapping)?,
        ];
        for command in commands {
            let next = (self.tail + 16) % 4096;
            if next == self.read(HEAD) as usize {
                return Err(Error::Timeout);
            }
            self.commands.write(self.tail, command[0]);
            self.commands.write(self.tail + 8, command[1]);
            self.tail = next;
        }
        fence(Ordering::SeqCst);
        self.write(TAIL, self.tail as u64);
        for _ in 0..5_000_000 {
            if self.completion.read(0) == 0xc0de_cafe {
                fence(Ordering::SeqCst);
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(Error::Timeout)
    }
    fn fault(&self, requester: u16) -> bool {
        let tail = self.read(EVENT_TAIL) as usize;
        if tail > 4096 || tail % 16 != 0 {
            return false;
        }
        (0..tail)
            .step_by(16)
            .any(|at| self.events.read(at) >> 60 == 2 && self.events.read(at) as u16 == requester)
    }
    fn stop(&mut self) -> Result<(), Error> {
        if !self.active {
            return Ok(());
        }
        self.write(CONTROL, self.old[3]);
        for _ in 0..5_000_000 {
            if self.read(CONTROL) & (1 | 4 | (1 << 12)) == 0 && self.read(STATUS) & 24 == 0 {
                fence(Ordering::SeqCst);
                self.write(STATUS, 7); // acknowledge only faults/completions we generated.
                self.write(0, self.old[0]);
                self.write(8, self.old[1]);
                self.write(16, self.old[2]);
                self.write(HEAD, self.old[4]);
                self.write(TAIL, self.old[5]);
                self.write(EVENT_TAIL, self.old[6]);
                self.domain.published = false;
                self.active = false;
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(Error::Timeout)
    }
}
#[cfg(feature = "amdvi-dma-test")]
impl Drop for Controller {
    fn drop(&mut self) {
        // All callers must explicitly stop before dropping. Fail closed:
        // freeing published DMA storage is never an acceptable recovery.
        assert!(
            !self.active,
            "AMD-Vi owner dropped while hardware is active"
        );
    }
}

#[cfg(feature = "amdvi-dma-test")]
pub fn self_test(devices: &[super::pci::Device]) {
    use super::pci::{self, BaseAddress};
    const SRC: u64 = 0x1_0000_0000;
    const DST: u64 = SRC + 4096;
    let result = (|| {
        let device = devices
            .iter()
            .find(|d| d.vendor == 0x1234 && d.device == 0x11e8)
            .ok_or(Error::Absent)?;
        let BaseAddress::Memory { base, .. } = (unsafe { device.base_address(0) }) else {
            return Err(Error::Mapping);
        };
        let mapped =
            unsafe { memory::map_device_registers(base, 4096) }.ok_or(Error::Memory)? as usize;
        let read = |at: usize| unsafe { core::ptr::read_volatile((mapped + at) as *const u64) };
        let write = |at: usize, value: u64| unsafe {
            core::ptr::write_volatile((mapped + at) as *mut u64, value);
        };
        let transfer = |source: u64, destination: u64, command: u64| -> Result<(), Error> {
            write(0x80, source);
            write(0x88, destination);
            write(0x90, 4096);
            fence(Ordering::SeqCst);
            write(0x98, command);
            for _ in 0..20_000_000 {
                if read(0x98) & 1 == 0 {
                    fence(Ordering::SeqCst);
                    return Ok(());
                }
                core::hint::spin_loop();
            }
            Err(Error::Timeout)
        };
        let before = memory::stats().ok_or(Error::Memory)?.free_frames;
        let data = Block::new(1)?;
        let source = unsafe { core::slice::from_raw_parts_mut(data.pointer(), 4096) };
        let destination =
            unsafe { core::slice::from_raw_parts_mut(data.pointer().add(4096), 4096) };
        for (i, byte) in source.iter_mut().enumerate() {
            *byte = (i.wrapping_mul(37) ^ (i >> 4) ^ 0xa7) as u8;
        }
        destination.fill(0x5a);
        let mut domain = Domain::new()?;
        unsafe {
            domain.map(SRC, data.physical, abi::READ)?;
            domain.map(DST, data.physical + 4096, abi::WRITE)?;
        }
        if domain.resolve(SRC + 4095, abi::READ) != Some(data.physical + 4095)
            || domain.resolve(SRC, abi::WRITE).is_some()
            || domain.resolve(DST, abi::READ).is_some()
            || domain.resolve(DST + 4096, abi::WRITE).is_some()
        {
            return Err(Error::Mapping);
        }
        let requester = ((device.address.bus as u16) << 8)
            | ((device.address.device as u16) << 3)
            | device.address.function as u16;
        let mut controller = Controller::new(requester, domain)?;
        let command = unsafe { pci::read_config(device.address, 4) } & 0xffff;
        let checked = (|| {
            controller.start(requester)?;
            unsafe {
                device.enable();
            }
            kprintln!("[dma ] EDU BAR={:#x}, id={:#x}, PCI command={:#x}, AMD control={:#x}, DTE base={:#x}",base,unsafe {core::ptr::read_volatile(mapped as *const u32)},unsafe {pci::read_config(device.address,4)},controller.read(CONTROL),controller.read(0));
            transfer(SRC, 0x40000, 1)?;
            transfer(0x40000, DST, 3)?;
            if source != destination {
                return Err(Error::Readback);
            }
            destination.fill(0x5a);
            transfer(0x40000, DST + 4096, 3)?;
            if !controller.fault(requester) || destination.iter().any(|&byte| byte != 0x5a) {
                return Err(Error::Mapping);
            }
            // Drain fault records, then prove the READ-only source mapping
            // also refuses device writes. Change CPU bytes first so a wrongly
            // permitted write of the old device buffer cannot pass unnoticed.
            controller.write(0x2010, controller.read(EVENT_TAIL));
            controller.write(STATUS, 3);
            for byte in source.iter_mut() {
                *byte ^= 0xff;
            }
            transfer(0x40000, SRC, 3)?;
            if !controller.fault(requester)
                || source
                    .iter()
                    .enumerate()
                    .any(|(i, &byte)| byte != ((i.wrapping_mul(37) ^ (i >> 4) ^ 0xa7) as u8 ^ 0xff))
            {
                return Err(Error::Mapping);
            }
            for byte in source.iter_mut() {
                *byte ^= 0xff;
            }
            transfer(0x40000, DST, 3)?;
            if source != destination {
                return Err(Error::Readback);
            }
            // More commands than fit in one ring: completion must survive wrap.
            for _ in 0..100 {
                controller.fence(requester)?;
            }
            Ok::<(), Error>(())
        })();
        unsafe {
            pci::write_config(device.address, 4, command);
        }
        if read(0x98) & 1 != 0 || controller.stop().is_err() {
            core::mem::forget(controller);
            core::mem::forget(data);
            kprintln!("[dma ] stop not confirmed; endpoint data and tables quarantined");
            return Err(Error::Timeout);
        }
        drop(controller);
        drop(data);
        checked?;
        if memory::stats().ok_or(Error::Memory)?.free_frames != before {
            return Err(Error::Memory);
        }
        Ok::<(), Error>(())
    })();
    match result {
        Ok(())=>kprintln!("[test] AMD-Vi DMA PASS: real IVRS, >4GiB nonidentity read/write DMA, exact bytes, unmapped/read-only faults, recovery, ring wrap, completion fences, controller stop and reclamation; no RTX execution"),
        Err(error)=>kprintln!("[test] AMD-Vi DMA FAILED: {:?}",error),
    }
}
