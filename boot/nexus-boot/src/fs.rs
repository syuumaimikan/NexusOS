//! Reading the kernel image off the EFI System Partition.

use core::ffi::c_void;

use crate::uefi::{
    self, BootServices, FileInfoHeader, FileProtocol, Handle, LoadedImageProtocol,
    SimpleFileSystemProtocol, Status,
};

/// Where the bootloader expects to find the kernel on the ESP.
///
/// UEFI paths are `CHAR16` and backslash-separated; this is
/// `\nexus\kernel.elf` widened at compile time.
const KERNEL_PATH: [u16; 18] = {
    let ascii = b"\\nexus\\kernel.elf";
    let mut path = [0u16; 18];
    let mut index = 0;
    while index < ascii.len() {
        path[index] = ascii[index] as u16;
        index += 1;
    }
    // The final element stays 0: UEFI paths are NUL terminated.
    path
};

/// Why the kernel image could not be read.
#[derive(Debug, Clone, Copy)]
pub enum FileError {
    /// `HandleProtocol` for `EFI_LOADED_IMAGE_PROTOCOL` failed.
    NoLoadedImage(Status),
    /// The volume the bootloader was loaded from exposes no filesystem.
    NoFileSystem(Status),
    /// `OpenVolume` failed.
    OpenVolume(Status),
    /// The kernel file is missing or unreadable.
    OpenFile(Status),
    /// `GetInfo` did not return the file size.
    FileInfo(Status),
    /// Allocating a buffer for the image failed.
    Allocate(Status),
    /// The read returned an error or came up short.
    Read(Status),
}

impl core::fmt::Display for FileError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (stage, status) = match *self {
            Self::NoLoadedImage(s) => ("HandleProtocol(LoadedImage)", s),
            Self::NoFileSystem(s) => ("HandleProtocol(SimpleFileSystem)", s),
            Self::OpenVolume(s) => ("OpenVolume", s),
            Self::OpenFile(s) => ("Open(\\nexus\\kernel.elf)", s),
            Self::FileInfo(s) => ("GetInfo(FileInfo)", s),
            Self::Allocate(s) => ("AllocatePages", s),
            Self::Read(s) => ("Read", s),
        };
        write!(f, "{stage} failed with status {status:#x}")
    }
}

/// Read `\nexus\kernel.elf` from the volume the bootloader itself came from.
///
/// The image is placed in `LoaderData` pages, which the kernel reclaims as
/// ordinary RAM once it has been copied to its final home.
///
/// # Safety
///
/// `image_handle` must be the handle passed to `efi_main`, and `boot_services`
/// must point at live boot services (that is, before `ExitBootServices`).
pub unsafe fn load_kernel_image(
    image_handle: Handle,
    boot_services: *mut BootServices,
) -> Result<&'static [u8], FileError> {
    // SAFETY: the caller guarantees boot services are live.
    let services = unsafe { &*boot_services };

    // Find the device this bootloader was loaded from, so the kernel is read
    // from the same partition rather than an arbitrary one.
    let mut loaded_image: *mut c_void = core::ptr::null_mut();
    // SAFETY: `image_handle` is our own image handle and the GUID matches the
    // interface type we cast to.
    let status = unsafe {
        (services.handle_protocol)(
            image_handle,
            &uefi::LOADED_IMAGE_PROTOCOL_GUID,
            &mut loaded_image,
        )
    };
    if uefi::is_error(status) {
        return Err(FileError::NoLoadedImage(status));
    }
    // SAFETY: `handle_protocol` succeeded, so this is a `LoadedImageProtocol`.
    let device_handle = unsafe { (*(loaded_image as *mut LoadedImageProtocol)).device_handle };

    let mut file_system: *mut c_void = core::ptr::null_mut();
    // SAFETY: `device_handle` came from the firmware; the GUID matches the cast.
    let status = unsafe {
        (services.handle_protocol)(
            device_handle,
            &uefi::SIMPLE_FILE_SYSTEM_PROTOCOL_GUID,
            &mut file_system,
        )
    };
    if uefi::is_error(status) {
        return Err(FileError::NoFileSystem(status));
    }
    let file_system = file_system as *mut SimpleFileSystemProtocol;

    let mut root: *mut FileProtocol = core::ptr::null_mut();
    // SAFETY: `file_system` is a live protocol instance.
    let status = unsafe { ((*file_system).open_volume)(file_system, &mut root) };
    if uefi::is_error(status) {
        return Err(FileError::OpenVolume(status));
    }

    let mut file: *mut FileProtocol = core::ptr::null_mut();
    // SAFETY: `root` is an open directory handle and `KERNEL_PATH` is a
    // NUL-terminated CHAR16 string.
    let status = unsafe {
        ((*root).open)(
            root,
            &mut file,
            KERNEL_PATH.as_ptr(),
            uefi::FILE_MODE_READ,
            0,
        )
    };
    if uefi::is_error(status) {
        return Err(FileError::OpenFile(status));
    }

    // `EFI_FILE_INFO` ends in a variable-length name; reserve room for both the
    // fixed header and a generous name so a single `GetInfo` call succeeds.
    let mut info_buffer = [0u8; core::mem::size_of::<FileInfoHeader>() + 512];
    let mut info_size = info_buffer.len();
    // SAFETY: `file` is open; the buffer is large enough for the header plus a
    // 256-character name.
    let status = unsafe {
        ((*file).get_info)(
            file,
            &uefi::FILE_INFO_GUID,
            &mut info_size,
            info_buffer.as_mut_ptr() as *mut c_void,
        )
    };
    if uefi::is_error(status) {
        return Err(FileError::FileInfo(status));
    }
    // SAFETY: `GetInfo` succeeded, so the buffer starts with a `FileInfoHeader`.
    // It is read unaligned because `info_buffer` is only byte aligned.
    let file_size =
        unsafe { (info_buffer.as_ptr() as *const FileInfoHeader).read_unaligned() }.file_size;

    let pages = (file_size as usize).div_ceil(4096);
    let mut buffer_phys: u64 = 0;
    // SAFETY: a straightforward page allocation; `buffer_phys` is a valid out
    // pointer.
    let status = unsafe {
        (services.allocate_pages)(
            uefi::AllocateType::AnyPages,
            uefi::MemoryType::LoaderData,
            pages,
            &mut buffer_phys,
        )
    };
    if uefi::is_error(status) {
        return Err(FileError::Allocate(status));
    }

    // Rewind explicitly: `Open` leaves the position at zero, but relying on
    // that is a needless assumption about firmware behaviour.
    // SAFETY: `file` is open for reading.
    let status = unsafe { ((*file).set_position)(file, 0) };
    if uefi::is_error(status) {
        return Err(FileError::Read(status));
    }

    let mut read_size = file_size as usize;
    // SAFETY: the buffer holds `pages * 4096 >= file_size` bytes, and UEFI
    // memory is identity mapped, so `buffer_phys` is a usable pointer.
    let status = unsafe {
        ((*file).read)(file, &mut read_size, buffer_phys as *mut c_void)
    };
    if uefi::is_error(status) || read_size != file_size as usize {
        return Err(FileError::Read(status));
    }

    // SAFETY: closing handles we opened; failures here cannot be acted on and
    // boot services are about to go away regardless.
    unsafe {
        let _ = ((*file).close)(file);
        let _ = ((*root).close)(root);
    }

    // SAFETY: the pages stay allocated for the rest of the bootloader's life
    // and hold exactly `file_size` initialised bytes.
    Ok(unsafe { core::slice::from_raw_parts(buffer_phys as *const u8, file_size as usize) })
}
