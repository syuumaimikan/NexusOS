//! Eager copying of a private user address space, needed by Linux fork.
//!
//! This does not create or schedule a child process. Descriptor state, child
//! reaping and shared mapping ownership must also be inherited before the
//! Linux fork entry point can use it. Unknown shared mappings are rejected.

use super::address_space::AddressSpace;
use super::{allocate_frame, free_frame, paging};
use nexus_abi::layout;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotError {
    OutOfMemory,
    PageLimit,
    SharedMapping,
    LargeMapping,
    SupervisorMapping,
    RestrictedTable,
    NoKernelSpace,
}

/// Copy all private 4 KiB user pages, including inaccessible guard contents.
/// At most `page_limit` pages are copied. Kernel mappings remain shared.
///
/// # Safety
/// Keep the source page tables and private page contents unchanged throughout
/// this call. No concurrent mapper, unmapper or writer may use the source.
pub unsafe fn private_snapshot(
    source: &AddressSpace,
    page_limit: usize,
) -> Result<AddressSpace, SnapshotError> {
    // SAFETY: forwarded from the caller. Allocations are real owned frames.
    unsafe { copy_with(source, page_limit, allocate_frame) }
}

unsafe fn visit(
    table: u64,
    level: usize,
    base: u64,
    user_enabled: bool,
    callback: &mut impl FnMut(u64, u64) -> Result<(), SnapshotError>,
) -> Result<(), SnapshotError> {
    let entries = if level == 0 { 256 } else { 512 };
    let shift = [39, 30, 21, 12][level];
    for index in 0..entries {
        // SAFETY: the source remains live and immutable by the caller's promise.
        let entry = unsafe { paging::read_table_entry(table, index) };
        if entry & paging::PRESENT == 0 {
            continue;
        }
        let address = base | ((index as u64) << shift);
        if level == 3 {
            if entry & paging::USER != 0 && !user_enabled {
                return Err(SnapshotError::SupervisorMapping);
            }
            callback(address, entry)?;
        } else {
            if entry & paging::HUGE != 0 {
                return Err(SnapshotError::LargeMapping);
            }
            if entry & paging::WRITABLE == 0 || entry & paging::NO_EXECUTE != 0 {
                return Err(SnapshotError::RestrictedTable);
            }
            // SAFETY: a present non-large entry identifies a live child table.
            unsafe {
                visit(
                    entry & paging::ADDRESS_MASK,
                    level + 1,
                    address,
                    user_enabled && entry & paging::USER != 0,
                    callback,
                )?
            };
        }
    }
    Ok(())
}

unsafe fn copy_with(
    source: &AddressSpace,
    page_limit: usize,
    mut frame_allocator: impl FnMut() -> Option<u64>,
) -> Result<AddressSpace, SnapshotError> {
    // Reject unsupported ownership and budget before allocating a destination.
    let mut pages = 0;
    unsafe {
        visit(source.root(), 0, 0, true, &mut |_, entry| {
            if entry & paging::SHARED != 0 {
                return Err(SnapshotError::SharedMapping);
            }
            if entry & (paging::USER | paging::GUARD) == 0 {
                return Err(SnapshotError::SupervisorMapping);
            }
            if pages >= page_limit {
                return Err(SnapshotError::PageLimit);
            }
            pages += 1;
            Ok(())
        })?;
    }
    let destination = AddressSpace::new().map_err(|error| match error {
        super::address_space::SpaceError::OutOfMemory => SnapshotError::OutOfMemory,
        super::address_space::SpaceError::NoKernelSpace => SnapshotError::NoKernelSpace,
    })?;
    // RAII frees all previously copied frames and intermediate tables on any
    // failure. The one frame not yet installed needs explicit release.
    unsafe {
        visit(source.root(), 0, 0, true, &mut |address, entry| {
            let frame = frame_allocator().ok_or(SnapshotError::OutOfMemory)?;
            core::ptr::copy_nonoverlapping(
                layout::phys_to_virt(entry & paging::ADDRESS_MASK) as *const u8,
                layout::phys_to_virt(frame) as *mut u8,
                4096,
            );
            // Copy permissions/software guard bits; the physical address is
            // the new owned frame. No source PTE or TLB is changed.
            if destination
                .map(address, frame, entry & !paging::ADDRESS_MASK)
                .is_err()
            {
                free_frame(frame);
                return Err(SnapshotError::OutOfMemory);
            }
            Ok(())
        })?;
    }
    Ok(destination)
}

/// Exercise real page tables and frame ownership before user processes start.
pub fn self_test() {
    let before = super::stats().expect("frame allocator running").free_frames;
    let result = exercise();
    let after = super::stats().expect("frame allocator running").free_frames;
    if let Err(error) = result {
        crate::kprintln!("[test] FAILED: address snapshot: {error}");
    } else if before != after {
        crate::kprintln!("[test] FAILED: address snapshot leaked frames: {before} -> {after}");
    } else {
        crate::kprintln!("[test] private address snapshot verified: bytes, permissions, guards, sparse 32/64-bit addresses, independent writes and unmaps, rollback, no leaked frames");
    }
}

fn exercise() -> Result<(), &'static str> {
    let source = AddressSpace::new().map_err(|_| "source allocation")?;
    let addresses = [
        0x0804_8000u64,
        0xbffe_0000,
        0x0040_0000_0000,
        0x2000_0000_0000,
    ];
    let permissions = [
        paging::PRESENT | paging::USER | paging::WRITABLE | paging::NO_EXECUTE,
        paging::PRESENT | paging::USER | paging::NO_EXECUTE,
        paging::PRESENT | paging::USER,
        paging::PRESENT | paging::GUARD | paging::NO_EXECUTE,
    ];
    for (index, address) in addresses.iter().copied().enumerate() {
        let frame = allocate_frame().ok_or("source frame")?;
        unsafe {
            core::ptr::write_bytes(
                layout::phys_to_virt(frame) as *mut u8,
                0x31 + index as u8,
                4096,
            );
            if source.map(address, frame, permissions[index]).is_err() {
                free_frame(frame);
                return Err("source mapping");
            }
        }
    }
    if !matches!(
        unsafe { private_snapshot(&source, 3) },
        Err(SnapshotError::PageLimit)
    ) {
        return Err("budget did not reject four pages");
    }
    // A deterministic leaf allocation failure after two installed pages must
    // reclaim the destination root, all tables and both data frames.
    let rollback_before = super::stats().ok_or("allocator")?.free_frames;
    let mut supplied = 0;
    let failed = unsafe {
        copy_with(&source, 4, || {
            supplied += 1;
            if supplied > 2 {
                None
            } else {
                allocate_frame()
            }
        })
    };
    if !matches!(failed, Err(SnapshotError::OutOfMemory))
        || super::stats().ok_or("allocator")?.free_frames != rollback_before
    {
        return Err("partial snapshot rollback");
    }
    let child = unsafe { private_snapshot(&source, 4) }.map_err(|_| "snapshot")?;
    // Higher-level restrictions are effective CPU permissions too. Do not
    // recreate a reachable writable/executable child from a restricted source.
    let original = unsafe { paging::read_table_entry(source.root(), 0) };
    for (entry, expected) in [
        (original | paging::HUGE, SnapshotError::LargeMapping),
        (original & !paging::USER, SnapshotError::SupervisorMapping),
        (original & !paging::WRITABLE, SnapshotError::RestrictedTable),
        (
            original | paging::NO_EXECUTE,
            SnapshotError::RestrictedTable,
        ),
    ] {
        unsafe { paging::write_table_entry(source.root(), 0, entry) };
        let refused = unsafe { private_snapshot(&source, 4) };
        unsafe { paging::write_table_entry(source.root(), 0, original) };
        if !matches!(refused, Err(error) if error == expected) {
            return Err("restricted table accepted");
        }
    }
    for (index, address) in addresses.iter().copied().enumerate() {
        let parent_frame = source.translate(address).ok_or("source missing")?;
        let child_frame = child.translate(address).ok_or("child missing")?;
        if parent_frame == child_frame
            || paging::flags_in(child.root(), address) != Some(permissions[index])
        {
            return Err("aliased frame or changed permissions");
        }
        let bytes = unsafe {
            core::slice::from_raw_parts(layout::phys_to_virt(child_frame) as *const u8, 4096)
        };
        if !bytes.iter().all(|byte| *byte == 0x31 + index as u8) {
            return Err("copied bytes");
        }
        unsafe { (layout::phys_to_virt(child_frame) as *mut u8).write(0x91) };
        if unsafe { (layout::phys_to_virt(parent_frame) as *const u8).read() } != 0x31 + index as u8
        {
            return Err("child write changed parent");
        }
    }
    for index in 256..512 {
        if unsafe { paging::read_table_entry(child.root(), index) }
            != unsafe { paging::read_table_entry(source.root(), index) }
        {
            return Err("kernel mappings changed");
        }
    }
    if child.translate(addresses[0] + 4096).is_some() {
        return Err("unmapped hole filled");
    }
    let frame = unsafe { child.unmap(addresses[0]) }.map_err(|_| "child unmap")?;
    unsafe { free_frame(frame) };
    if source.translate(addresses[0]).is_none() {
        return Err("child unmap changed parent");
    }
    // SHARED frames belong to MemoryObject, not to either address space. No
    // copy may reinterpret one as owned private data or silently change its
    // sharing semantics. Restoring the private PTE transfers no ownership.
    unsafe { source.protect(addresses[0], permissions[0] | paging::SHARED) }
        .map_err(|_| "shared test flag")?;
    let rejection = unsafe { private_snapshot(&source, 4) };
    unsafe { source.protect(addresses[0], permissions[0]) }.map_err(|_| "restore ownership")?;
    if !matches!(rejection, Err(SnapshotError::SharedMapping)) {
        return Err("shared mapping accepted without owner");
    }
    drop(source);
    if unsafe {
        (layout::phys_to_virt(child.translate(addresses[1]).ok_or("child lifetime")?) as *const u8)
            .read()
    } != 0x91
    {
        return Err("parent teardown damaged child");
    }
    Ok(())
}
