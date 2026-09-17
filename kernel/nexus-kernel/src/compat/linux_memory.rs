//! The address space of a program built for Linux.
//!
//! `mmap` used to be a bump pointer that handed out zeroed anonymous pages and
//! refused everything else. That is enough for a language runtime's heap, and
//! it is not enough for a *dynamically linked* program, because the first thing
//! a dynamic linker does is map somebody else's file into its own address space
//! at an address it chose. Three refusals stood between here and that:
//!
//! - **`MAP_FIXED`.** The linker reserves a library's whole span, then maps each
//!   of its segments over that span at the exact addresses the file names. It
//!   has no other way of doing it, and an address other than the one it asked
//!   for is not a smaller version of the answer -- it is a library whose every
//!   internal address is wrong.
//! - **A file.** `mmap(fd, offset)` is how a library's text gets into memory.
//!   Reading the file into anonymous pages instead would be a different thing
//!   wearing the same name and would show up as a program whose code is zeros.
//! - **`PROT_EXEC`.** A library's text is mapped executable, and a page this
//!   layer refused to make executable is a library that cannot be called.
//!
//! All three are here now. What is *not* here is a page that is writable and
//! executable at once: this system maps no such page for its own programs and
//! does not make an exception for a foreign one. A just-in-time compiler needs
//! one, so a just-in-time compiler does not run here yet, and that is written
//! down rather than discovered.
//!
//! # What is tracked, and what is not
//!
//! Nothing is tracked. There is no list of regions: `munmap` and `mprotect`
//! walk the page tables themselves and act on what is actually mapped, which is
//! the only description that cannot drift out of step with the truth. The one
//! piece of per-process state is where the next mapping without an address
//! should go, and that is a number.
//!
//! # What a mapping is made of
//!
//! Ordinary frames from this machine's page allocator, mapped into the calling
//! process's own address space through the same `AddressSpace::map` every Nexus
//! program's memory goes through. A translated program's memory is not a
//! separate kind of memory, and there is no operation here a Nexus program's
//! memory does not already have.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::memory::paging;
use crate::sync::IrqSpinLock;

use super::linux::error;

/// What `mmap` may be asked for.
pub mod flag {
    /// Changes are private to this process. Every mapping here is.
    pub const PRIVATE: u64 = 0x02;
    /// Changes are to be visible to everyone else mapping the file. Refused.
    pub const SHARED: u64 = 0x01;
    /// The mapping is not backed by a file.
    pub const ANONYMOUS: u64 = 0x20;
    /// The caller insists on the address, and means it.
    pub const FIXED: u64 = 0x10;
    /// The same, but refusing rather than replacing what is already there.
    pub const FIXED_NOREPLACE: u64 = 0x10_0000;

    pub const PROT_NONE: u64 = 0;
    pub const PROT_READ: u64 = 1;
    pub const PROT_WRITE: u64 = 2;
    pub const PROT_EXEC: u64 = 4;
}

/// Where a mapping goes when the caller did not say.
///
/// Well above where an executable is loaded and its heap would grow, well below
/// where a dynamic linker is put, and nowhere near the region Nexus programs
/// map surfaces into.
const MMAP_BASE: u64 = 0x0000_2000_0000_0000;

/// And where one goes for a program that is thirty-two bit.
///
/// One gigabyte, and nothing above three: a thirty-two bit program's `mmap2`
/// hands the address back in `eax`, so an address above four gigabytes would
/// arrive truncated -- which for the region above is *zero*, and a program that
/// checked its mapping for zero would think the call had failed while the
/// mapping was made.
///
/// Above where an i386 executable is linked (`0x08048000`) and below where its
/// stack is, so that neither can grow into these.
const MMAP_BASE32: u64 = 0x4000_0000;
/// The highest address a thirty-two bit mapping may reach.
const MMAP_LIMIT32: u64 = 0xB000_0000;

/// The most one `mmap` will give out.
///
/// Two hundred and fifty-six mebibytes, up from sixty-four: a C library's span
/// is a single mapping, a thread stack with its guard region is another, and
/// the old cap refused things a dynamic linker asks for as a matter of course.
/// Small enough that a program asking for a terabyte is refused rather than
/// spending the machine's memory finding out.
///
/// Every page is really allocated when it is mapped. Linux would hand back the
/// address and find the memory when it was touched; this does not, so a program
/// that maps a large region it never uses pays for all of it. That difference is
/// the reason there is a cap at all.
const MAX_MMAP: u64 = 256 * 1024 * 1024;

const PAGE: u64 = 4096;

/// No such device: what a `MAP_SHARED` file mapping gets.
const ENODEV: u64 = (-19i64) as u64;
/// File exists: what `MAP_FIXED_NOREPLACE` gets when something already is.
const EEXIST: u64 = (-17i64) as u64;
/// The medium failed while a file mapping was being filled.
const EIO: u64 = (-5i64) as u64;

/// Pages handed out, pages given back, and mappings placed where the caller
/// insisted.
static MAPPED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static UNMAPPED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static PLACED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// What each process has mapped that belongs to a memory object.
///
/// The mapping has to keep the object alive, and that is not bookkeeping for
/// its own sake -- it is what `mmap` means. A program maps a `memfd`, closes
/// the descriptor, and goes on using the memory; a Wayland compositor does
/// exactly that with every buffer a client hands it. Without this the object
/// went when the last *handle* went, its frames went back to the allocator, and
/// the program was left reading pages that had been given to something else.
///
/// Keyed by process, with the address and length so that `munmap` can let go of
/// the right one.
static SHARED: IrqSpinLock<BTreeMap<u64, Vec<Mapped>>> = IrqSpinLock::new(BTreeMap::new());

/// One view of a memory object that a process is holding open.
struct Mapped {
    at: u64,
    pages: u64,
    object: Arc<crate::ipc::MemoryObject>,
}

/// Where each process's next unplaced mapping goes.
///
/// Per process rather than one number for the machine. A single counter would
/// mean a program's address space depended on what every other program had ever
/// mapped, which is both surprising and, after enough programs, exhausted.
static NEXT: IrqSpinLock<BTreeMap<u64, u64>> = IrqSpinLock::new(BTreeMap::new());

/// Pages mapped, pages unmapped, and mappings placed where the caller insisted.
#[must_use]
pub fn statistics() -> (u64, u64, u64) {
    use core::sync::atomic::Ordering;
    (
        MAPPED.load(Ordering::Relaxed),
        UNMAPPED.load(Ordering::Relaxed),
        PLACED.load(Ordering::Relaxed),
    )
}

/// Forget a process's placement cursor and let go of what it had mapped.
///
/// Called when it ends, and by `execve` -- which is the same thing from the
/// memory's point of view.
pub fn forget(process: u64) {
    NEXT.lock().remove(&process);
    // Dropped outside the lock: dropping the last reference to a memory object
    // frees its frames, and doing that while holding this would put the page
    // allocator's lock underneath it.
    let held = SHARED.lock().remove(&process);
    drop(held);
}

/// The page-table flags a `PROT_*` set asks for, or `None` if it asks for
/// something this system does not make.
///
/// `PROT_NONE` is refused rather than approximated. An allocator puts a
/// `PROT_NONE` page between its arenas as a guard, and a "guard page" that could
/// be read and written without faulting would be a guard that guards nothing
/// while looking like it does.
///
/// Writable and executable together is refused for the reason in the module
/// note: this system maps no such page for its own programs.
fn page_flags(protection: u64) -> Option<u64> {
    use flag as f;
    if protection == f::PROT_NONE || protection & f::PROT_READ == 0 {
        return None;
    }
    if protection & f::PROT_WRITE != 0 && protection & f::PROT_EXEC != 0 {
        return None;
    }
    let mut flags = paging::PRESENT | paging::USER;
    if protection & f::PROT_WRITE != 0 {
        flags |= paging::WRITABLE;
    }
    if protection & f::PROT_EXEC == 0 {
        flags |= paging::NO_EXECUTE;
    }
    Some(flags)
}

/// Whether every page of a range is absent from this space.
fn is_free(space: &crate::memory::address_space::AddressSpace, at: u64, pages: u64) -> bool {
    (0..pages).all(|page| space.translate(at + page * PAGE).is_none())
}

/// Find somewhere for `pages` pages in this process's space.
///
/// A cursor that only goes up, and a check that what it points at is actually
/// free -- because a `MAP_FIXED` mapping may have been put anywhere, including
/// in front of the cursor. Without the check the cursor would eventually hand
/// out an address a library is already living at, and the mapping would fail
/// halfway through with half of it in place.
fn place(process: &crate::process::Process, pages: u64) -> Option<u64> {
    /// How many times to step past something already mapped before giving up.
    /// A program needing more than this has an address space so fragmented that
    /// the honest answer is that there is no room.
    const ATTEMPTS: u32 = 64;

    // Where this program's mappings live depends on how wide it is. See
    // [`MMAP_BASE32`].
    let thirty_two_bit = process.personality == crate::process::Personality::Linux32;
    let (base, limit) = if thirty_two_bit {
        (MMAP_BASE32, MMAP_LIMIT32)
    } else {
        (MMAP_BASE, nexus_abi::layout::USER_SPACE_END)
    };

    let bytes = pages * PAGE;
    let mut cursor = NEXT.lock();
    let at = cursor.entry(process.id.0).or_insert(base);

    for _ in 0..ATTEMPTS {
        let candidate = *at;
        let end = candidate.checked_add(bytes)?;
        if end >= limit {
            return None;
        }
        *at = end;
        if is_free(&process.address_space, candidate, pages) {
            return Some(candidate);
        }
    }
    None
}

/// Take a range out of the address space, freeing what was this layer's.
///
/// A page carrying `SHARED` belongs to a memory object somebody else also maps,
/// so it is unmapped and not freed: giving that frame back to the allocator
/// would take it away from everyone else holding it. That distinction is the
/// system's own and is not something this layer invented for Linux.
fn release(process: &crate::process::Process, at: u64, pages: u64) -> u64 {
    let root = process.address_space.root();
    let mut removed = 0;
    for page in 0..pages {
        let virt = at + page * PAGE;
        if virt >= nexus_abi::layout::USER_SPACE_END {
            break;
        }
        let shared = paging::flags_in(root, virt).is_some_and(|flags| flags & paging::SHARED != 0);
        // SAFETY: the caller is unmapping the address, so nothing in this
        // process may still be using it.
        if let Ok(frame) = unsafe { process.address_space.unmap(virt) } {
            let _ = frame;
            if !shared {
                // SAFETY: the frame came from this layer or from the image, and
                // is no longer mapped in this space.
                unsafe { crate::memory::free_frame(frame) };
            }
            removed += 1;
        }
    }
    UNMAPPED.fetch_add(removed, core::sync::atomic::Ordering::Relaxed);

    // And let go of any memory object this range was a view of. Taken out under
    // the lock and dropped outside it, for the reason `forget` gives.
    let released: Vec<Arc<crate::ipc::MemoryObject>> = {
        let mut shared = SHARED.lock();
        let Some(mappings) = shared.get_mut(&process.id.0) else {
            return removed;
        };
        // Only a mapping the range covers *whole*. A `munmap` of part of one is
        // a program keeping the rest, and the object has to stay alive for it.
        let mut taken = Vec::new();
        mappings.retain(|mapping| {
            let covered =
                mapping.at >= at && mapping.at + mapping.pages * PAGE <= at + pages * PAGE;
            if covered {
                taken.push(Arc::clone(&mapping.object));
            }
            !covered
        });
        taken
    };
    drop(released);
    removed
}

/// `mmap(address, length, protection, flags, fd, offset)`.
///
/// Anonymous or file-backed, private, at an address of the caller's choosing or
/// of this layer's.
///
/// A file mapping is a *copy* of the file's bytes into fresh pages, which is
/// exactly what `MAP_PRIVATE` means -- and is why `MAP_SHARED` is refused. A
/// shared file mapping promises that a write reaches the file and every other
/// mapping of it, and this cannot keep that promise. Answering `ENODEV` says so;
/// mapping it privately instead would be a program's writes silently going
/// nowhere.
pub fn mmap(
    address: u64,
    length: u64,
    protection: u64,
    flags: u64,
    descriptor: u64,
    offset: u64,
) -> u64 {
    use flag as f;

    if length == 0 || length > MAX_MMAP {
        return error::EINVAL;
    }
    let Some(page_flags) = page_flags(protection) else {
        return error::EINVAL;
    };
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };

    // The window, which is the one thing here that really is shared and so the
    // one mapping where `MAP_SHARED` is not only allowed but required: the
    // compositor reads the same frames the program writes. Handled before the
    // checks below, because those are about the mappings this layer *makes*,
    // and this one is a view of memory that already exists.
    if flags & f::ANONYMOUS == 0 {
        if let Some(surface) = super::linux_display::surface_of(process.id.0, descriptor) {
            return share(&process, &surface, address, length, offset, protection);
        }
        // Memory with a descriptor on it: a `memfd`, or one that arrived over a
        // socket from the program that made it. Mapped shared, because that is
        // the whole point of it -- two programs looking at the same bytes is
        // how every graphics protocol moves a frame.
        if let Ok(crate::ipc::Object::Memory(memory)) = u32::try_from(descriptor)
            .map_err(|_| ())
            .and_then(|handle| {
                process
                    .handles
                    .object(handle, crate::ipc::Rights::READ)
                    .map_err(|_| ())
            })
        {
            return share(&process, &memory, address, length, offset, protection);
        }
    }

    if flags & f::SHARED != 0 {
        return ENODEV;
    }
    if flags & f::PRIVATE == 0 || !offset.is_multiple_of(PAGE) {
        return error::EINVAL;
    }

    let pages = length.div_ceil(PAGE);
    let fixed = flags & (f::FIXED | f::FIXED_NOREPLACE) != 0;

    // The file, if there is one. Looked up before a single page is mapped, so a
    // bad descriptor is a refusal and not a half-built mapping.
    let node = if flags & f::ANONYMOUS != 0 {
        None
    } else {
        match super::linux_files::node_of(descriptor) {
            Ok(node) => Some(node),
            Err(reason) => {
                crate::kprintln!(
                    "[linux] mmap of descriptor {descriptor} refused: error {}",
                    reason as i64
                );
                return reason;
            }
        }
    };

    let at = if fixed {
        if address == 0 || !address.is_multiple_of(PAGE) {
            return error::EINVAL;
        }
        if address.saturating_add(pages * PAGE) >= nexus_abi::layout::USER_SPACE_END {
            return error::ENOMEM;
        }
        // `MAP_FIXED` replaces whatever is there; `MAP_FIXED_NOREPLACE` is the
        // same request with "and tell me if something already is", which is the
        // safer one and the one a careful linker uses to reserve a span.
        if flags & f::FIXED_NOREPLACE != 0 && !is_free(&process.address_space, address, pages) {
            return EEXIST;
        }
        release(&process, address, pages);
        PLACED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        address
    } else {
        match place(&process, pages) {
            Some(at) => at,
            None => return error::ENOMEM,
        }
    };

    for page in 0..pages {
        let virt = at + page * PAGE;
        let Some(frame) = crate::memory::allocate_frame() else {
            // What was mapped stays mapped until the process ends. Unwinding
            // backwards through a half-built mapping is the kind of cleanup
            // that goes wrong under a fault handler that may already be looking
            // at one of these pages.
            return error::ENOMEM;
        };
        let kernel = nexus_abi::layout::phys_to_virt(frame) as *mut u8;

        // Zeroed before anything can see it. A page handed over with the last
        // program's data in it is the oldest information leak there is -- and
        // for a file mapping it is also what makes the tail of the last page
        // read as zero, which is what Linux guarantees.
        //
        // SAFETY: the frame came from the allocator and nothing else holds it.
        unsafe { core::ptr::write_bytes(kernel, 0, PAGE as usize) };

        if let Some(node) = node.as_ref() {
            // SAFETY: as above -- the frame is this mapping's and reachable
            // through the direct map for the length of one page.
            let buffer = unsafe { core::slice::from_raw_parts_mut(kernel, PAGE as usize) };
            // Past the end of the file reads nothing and leaves zeros, which is
            // what a mapping longer than its file is.
            if let Err(trouble) = crate::fs::store::read_node_at(node, offset + page * PAGE, buffer)
            {
                crate::kprintln!("[linux] a file mapping could not be filled: {trouble}");
                // SAFETY: never mapped, so nothing refers to it.
                unsafe { crate::memory::free_frame(frame) };
                return EIO;
            }
        }

        // SAFETY: the frame is this mapping's, and `virt` is in the user half --
        // checked above against `USER_SPACE_END`. Anything that was at this
        // address was released first.
        if unsafe { process.address_space.map(virt, frame, page_flags) }.is_err() {
            // SAFETY: never mapped, so nothing refers to it.
            unsafe { crate::memory::free_frame(frame) };
            return error::ENOMEM;
        }
    }

    MAPPED.fetch_add(pages, core::sync::atomic::Ordering::Relaxed);
    at
}

/// Map a memory object into the calling process, shared rather than copied.
///
/// The frames belong to the object and not to this address space, which is what
/// `paging::SHARED` records -- without it, the second space to be dropped would
/// free frames the first had already returned. It is the same bit and the same
/// reason the Nexus `memory_map` call uses, because this *is* that operation
/// reached by a different name.
fn share(
    process: &crate::process::Process,
    memory: &Arc<crate::ipc::MemoryObject>,
    address: u64,
    length: u64,
    offset: u64,
    protection: u64,
) -> u64 {
    use flag as f;

    // An offset has to be a whole number of pages, because what is mapped is
    // frames: there is no way to start a mapping halfway through one.
    if !offset.is_multiple_of(PAGE) {
        return error::EINVAL;
    }
    let first = (offset / PAGE) as usize;
    let pages = length.div_ceil(PAGE);
    if first.saturating_add(pages as usize) > memory.pages() {
        return error::EINVAL;
    }

    let mut flags = paging::PRESENT | paging::USER | paging::NO_EXECUTE | paging::SHARED;
    if protection & f::PROT_WRITE != 0 {
        flags |= paging::WRITABLE;
    }

    let Some(at) = place(process, pages) else {
        return error::ENOMEM;
    };
    // The address is this layer's choice, as every unplaced mapping is. A
    // caller that insisted on one would be asking for something the window has
    // no reason to honour -- and `MAP_FIXED` over a surface would mean
    // releasing pages that belong to the compositor.
    let _ = address;

    for page in 0..pages {
        let Some(frame) = memory.frame(first + page as usize) else {
            return error::EINVAL;
        };
        // SAFETY: the frame belongs to a memory object this process holds
        // through its window descriptor, and `at` is inside the user half --
        // `place` never returns an address that is not.
        if unsafe { process.address_space.map(at + page * PAGE, frame, flags) }.is_err() {
            return error::ENOMEM;
        }
    }

    // The mapping keeps the object alive. See [`SHARED`].
    SHARED.lock().entry(process.id.0).or_default().push(Mapped {
        at,
        pages,
        object: Arc::clone(memory),
    });

    MAPPED.fetch_add(pages, core::sync::atomic::Ordering::Relaxed);
    at
}

/// `munmap(address, length)`.
pub fn munmap(address: u64, length: u64) -> u64 {
    if length == 0 || !address.is_multiple_of(PAGE) {
        return error::EINVAL;
    }
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    release(&process, address, length.div_ceil(PAGE));
    0
}

/// `mprotect(address, length, protection)`.
///
/// This used to be accepted and ignored, on the grounds that every mapping was
/// already readable and writable and not executable -- so there was nothing a
/// caller could ask for that was not either already true or impossible. That
/// stopped being true the moment a page could be executable.
///
/// A dynamic linker maps a library writable, applies its relocations, and then
/// takes the write permission away; it maps text without execute permission and
/// then grants it. A kernel that agreed to both and did neither would run a
/// program whose code pages stayed writable for its whole life and whose
/// read-only relocations never became read-only. So this changes the page
/// tables, or fails.
pub fn mprotect(address: u64, length: u64, protection: u64) -> u64 {
    if !address.is_multiple_of(PAGE) {
        return error::EINVAL;
    }
    if length == 0 {
        return 0;
    }
    let Some(flags) = page_flags(protection) else {
        return error::EINVAL;
    };
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };

    for page in 0..length.div_ceil(PAGE) {
        let virt = address + page * PAGE;
        if virt >= nexus_abi::layout::USER_SPACE_END {
            return error::ENOMEM;
        }
        // A page that is not mapped is what `ENOMEM` means here, and it is what
        // Linux answers: the range has a hole, and a caller that wanted the
        // whole range protected has not got it.
        //
        // SAFETY: the address is in the user half and the flags include `USER`,
        // which `page_flags` always sets. The frame does not change hands.
        if unsafe { process.address_space.protect(virt, flags) }.is_err() {
            return error::ENOMEM;
        }
    }
    0
}

/// Write into a process's memory before it runs.
///
/// Used by the loader to build the stack a System V program starts on. That
/// stack spans more than one page, the frames behind it are not contiguous, and
/// "the frame at the top" is therefore not a description of it -- which is why
/// this walks the page tables for each piece rather than taking one frame and
/// an offset.
///
/// # Safety
///
/// The range must be mapped in `space`, and nothing else may be writing it.
pub unsafe fn write_into(
    space: &Arc<crate::memory::address_space::AddressSpace>,
    at: u64,
    bytes: &[u8],
) -> bool {
    let mut written = 0usize;
    while written < bytes.len() {
        let address = at + written as u64;
        let Some(frame) = space.translate(address & !(PAGE - 1)) else {
            return false;
        };
        let into_page = (address & (PAGE - 1)) as usize;
        let take = (PAGE as usize - into_page).min(bytes.len() - written);
        // SAFETY: `frame` is mapped in `space` and reachable through the direct
        // map; `take` stays inside the one page it starts in.
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr().add(written),
                (nexus_abi::layout::phys_to_virt(frame) + into_page as u64) as *mut u8,
                take,
            );
        }
        written += take;
    }
    true
}
