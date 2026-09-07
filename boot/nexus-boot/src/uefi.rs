//! Hand-written UEFI 2.x bindings.
//!
//! NexusOS deliberately carries no third-party crates in its boot path, so the
//! subset of the UEFI specification the bootloader needs is declared here.
//!
//! Every structure below must match the layout in the UEFI specification
//! exactly: the firmware hands us these tables and a wrong field offset means
//! calling an arbitrary function pointer. Members the bootloader never calls
//! are declared as [`Unused`] so that the offsets of the ones it does call stay
//! correct without spelling out every signature. The `tests` module at the
//! bottom pins the offsets that matter.

#![allow(dead_code)]

use core::ffi::c_void;

/// A UEFI status code. The high bit marks an error.
pub type Status = usize;
/// An opaque firmware object handle.
pub type Handle = *mut c_void;
/// A function pointer we never call, present only to preserve struct offsets.
pub type Unused = *const c_void;

/// Set in [`Status`] values that represent errors.
pub const ERROR_BIT: usize = 1 << 63;

pub const SUCCESS: Status = 0;
pub const INVALID_PARAMETER: Status = ERROR_BIT | 2;
pub const UNSUPPORTED: Status = ERROR_BIT | 3;
pub const BUFFER_TOO_SMALL: Status = ERROR_BIT | 5;
pub const NOT_FOUND: Status = ERROR_BIT | 14;

/// Whether a status code indicates failure.
#[inline]
#[must_use]
pub fn is_error(status: Status) -> bool {
    status & ERROR_BIT != 0
}

/// A UEFI GUID, laid out as in the specification (mixed endianness).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

pub const GRAPHICS_OUTPUT_PROTOCOL_GUID: Guid = Guid {
    data1: 0x9042_a9de,
    data2: 0x23dc,
    data3: 0x4a38,
    data4: [0x96, 0xfb, 0x7a, 0xde, 0xd0, 0x80, 0x51, 0x6a],
};

pub const LOADED_IMAGE_PROTOCOL_GUID: Guid = Guid {
    data1: 0x5b1b_31a1,
    data2: 0x9562,
    data3: 0x11d2,
    data4: [0x8e, 0x3f, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
};

pub const SIMPLE_FILE_SYSTEM_PROTOCOL_GUID: Guid = Guid {
    data1: 0x964e_5b22,
    data2: 0x6459,
    data3: 0x11d2,
    data4: [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
};

pub const FILE_INFO_GUID: Guid = Guid {
    data1: 0x0957_6e92,
    data2: 0x6d3f,
    data3: 0x11d2,
    data4: [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
};

pub const ACPI_20_TABLE_GUID: Guid = Guid {
    data1: 0x8868_e871,
    data2: 0xe4f1,
    data3: 0x11d3,
    data4: [0xbc, 0x22, 0x00, 0x80, 0xc7, 0x3c, 0x88, 0x81],
};

pub const ACPI_10_TABLE_GUID: Guid = Guid {
    data1: 0xeb9d_2d30,
    data2: 0x2d88,
    data3: 0x11d3,
    data4: [0x9a, 0x16, 0x00, 0x90, 0x27, 0x3f, 0xc1, 0x4d],
};

/// Common header on every UEFI table.
#[repr(C)]
pub struct TableHeader {
    pub signature: u64,
    pub revision: u32,
    pub header_size: u32,
    pub crc32: u32,
    pub reserved: u32,
}

/// `EFI_MEMORY_TYPE` values.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryType {
    ReservedMemoryType = 0,
    LoaderCode = 1,
    LoaderData = 2,
    BootServicesCode = 3,
    BootServicesData = 4,
    RuntimeServicesCode = 5,
    RuntimeServicesData = 6,
    ConventionalMemory = 7,
    UnusableMemory = 8,
    AcpiReclaimMemory = 9,
    AcpiMemoryNvs = 10,
    MemoryMappedIo = 11,
    MemoryMappedIoPortSpace = 12,
    PalCode = 13,
    PersistentMemory = 14,
}

/// `EFI_ALLOCATE_TYPE`.
#[repr(u32)]
#[derive(Debug, Clone, Copy)]
pub enum AllocateType {
    AnyPages = 0,
    MaxAddress = 1,
    Address = 2,
}

/// One entry of the UEFI memory map.
///
/// The firmware may report a descriptor *larger* than this structure, so the
/// map must always be walked using the `descriptor_size` returned by
/// `GetMemoryMap` rather than `size_of::<MemoryDescriptor>()`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MemoryDescriptor {
    pub kind: u32,
    pub _pad: u32,
    pub physical_start: u64,
    pub virtual_start: u64,
    pub number_of_pages: u64,
    pub attribute: u64,
}

/// `EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL`.
#[repr(C)]
pub struct SimpleTextOutputProtocol {
    pub reset: Unused,
    pub output_string:
        unsafe extern "efiapi" fn(this: *mut SimpleTextOutputProtocol, s: *const u16) -> Status,
    pub test_string: Unused,
    pub query_mode: Unused,
    pub set_mode: Unused,
    pub set_attribute: Unused,
    pub clear_screen: unsafe extern "efiapi" fn(this: *mut SimpleTextOutputProtocol) -> Status,
    pub set_cursor_position: Unused,
    pub enable_cursor: Unused,
    pub mode: Unused,
}

/// `EFI_BOOT_SERVICES`. Field order is load-bearing.
#[repr(C)]
pub struct BootServices {
    pub hdr: TableHeader,

    pub raise_tpl: Unused,
    pub restore_tpl: Unused,

    pub allocate_pages: unsafe extern "efiapi" fn(
        alloc_type: AllocateType,
        memory_type: MemoryType,
        pages: usize,
        memory: *mut u64,
    ) -> Status,
    pub free_pages: unsafe extern "efiapi" fn(memory: u64, pages: usize) -> Status,
    pub get_memory_map: unsafe extern "efiapi" fn(
        map_size: *mut usize,
        map: *mut MemoryDescriptor,
        map_key: *mut usize,
        descriptor_size: *mut usize,
        descriptor_version: *mut u32,
    ) -> Status,
    pub allocate_pool: unsafe extern "efiapi" fn(
        pool_type: MemoryType,
        size: usize,
        buffer: *mut *mut c_void,
    ) -> Status,
    pub free_pool: unsafe extern "efiapi" fn(buffer: *mut c_void) -> Status,

    pub create_event: Unused,
    pub set_timer: Unused,
    pub wait_for_event: Unused,
    pub signal_event: Unused,
    pub close_event: Unused,
    pub check_event: Unused,

    pub install_protocol_interface: Unused,
    pub reinstall_protocol_interface: Unused,
    pub uninstall_protocol_interface: Unused,
    pub handle_protocol: unsafe extern "efiapi" fn(
        handle: Handle,
        protocol: *const Guid,
        interface: *mut *mut c_void,
    ) -> Status,
    pub reserved: Unused,
    pub register_protocol_notify: Unused,
    pub locate_handle: Unused,
    pub locate_device_path: Unused,
    pub install_configuration_table: Unused,

    pub load_image: Unused,
    pub start_image: Unused,
    pub exit: Unused,
    pub unload_image: Unused,
    pub exit_boot_services:
        unsafe extern "efiapi" fn(image_handle: Handle, map_key: usize) -> Status,

    pub get_next_monotonic_count: Unused,
    pub stall: unsafe extern "efiapi" fn(microseconds: usize) -> Status,
    pub set_watchdog_timer: unsafe extern "efiapi" fn(
        timeout: usize,
        watchdog_code: u64,
        data_size: usize,
        watchdog_data: *mut u16,
    ) -> Status,

    pub connect_controller: Unused,
    pub disconnect_controller: Unused,

    pub open_protocol: Unused,
    pub close_protocol: Unused,
    pub open_protocol_information: Unused,

    pub protocols_per_handle: Unused,
    pub locate_handle_buffer: Unused,
    pub locate_protocol: unsafe extern "efiapi" fn(
        protocol: *const Guid,
        registration: *mut c_void,
        interface: *mut *mut c_void,
    ) -> Status,
    pub install_multiple_protocol_interfaces: Unused,
    pub uninstall_multiple_protocol_interfaces: Unused,

    pub calculate_crc32: Unused,
    pub copy_mem: Unused,
    pub set_mem: Unused,
    pub create_event_ex: Unused,
}

/// One `EFI_CONFIGURATION_TABLE` entry.
#[repr(C)]
pub struct ConfigurationTable {
    pub vendor_guid: Guid,
    pub vendor_table: *mut c_void,
}

/// `EFI_SYSTEM_TABLE`.
#[repr(C)]
pub struct SystemTable {
    pub hdr: TableHeader,
    pub firmware_vendor: *const u16,
    pub firmware_revision: u32,
    pub _pad: u32,
    pub console_in_handle: Handle,
    pub con_in: *mut c_void,
    pub console_out_handle: Handle,
    pub con_out: *mut SimpleTextOutputProtocol,
    pub standard_error_handle: Handle,
    pub std_err: *mut SimpleTextOutputProtocol,
    pub runtime_services: *mut c_void,
    pub boot_services: *mut BootServices,
    pub number_of_table_entries: usize,
    pub configuration_table: *mut ConfigurationTable,
}

/// `EFI_GRAPHICS_PIXEL_FORMAT`.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicsPixelFormat {
    RedGreenBlueReserved8BitPerColor = 0,
    BlueGreenRedReserved8BitPerColor = 1,
    BitMask = 2,
    BltOnly = 3,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PixelBitmask {
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub reserved_mask: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GraphicsOutputModeInformation {
    pub version: u32,
    pub horizontal_resolution: u32,
    pub vertical_resolution: u32,
    pub pixel_format: GraphicsPixelFormat,
    pub pixel_information: PixelBitmask,
    pub pixels_per_scan_line: u32,
}

#[repr(C)]
pub struct GraphicsOutputProtocolMode {
    pub max_mode: u32,
    pub mode: u32,
    pub info: *mut GraphicsOutputModeInformation,
    pub size_of_info: usize,
    pub frame_buffer_base: u64,
    pub frame_buffer_size: usize,
}

/// `EFI_GRAPHICS_OUTPUT_PROTOCOL`.
#[repr(C)]
pub struct GraphicsOutputProtocol {
    pub query_mode: unsafe extern "efiapi" fn(
        this: *mut GraphicsOutputProtocol,
        mode_number: u32,
        size_of_info: *mut usize,
        info: *mut *mut GraphicsOutputModeInformation,
    ) -> Status,
    pub set_mode:
        unsafe extern "efiapi" fn(this: *mut GraphicsOutputProtocol, mode_number: u32) -> Status,
    pub blt: Unused,
    pub mode: *mut GraphicsOutputProtocolMode,
}

/// `EFI_LOADED_IMAGE_PROTOCOL`.
#[repr(C)]
pub struct LoadedImageProtocol {
    pub revision: u32,
    pub _pad: u32,
    pub parent_handle: Handle,
    pub system_table: *mut SystemTable,
    pub device_handle: Handle,
    pub file_path: *mut c_void,
    pub reserved: *mut c_void,
    pub load_options_size: u32,
    pub _pad2: u32,
    pub load_options: *mut c_void,
    pub image_base: *mut c_void,
    pub image_size: u64,
    pub image_code_type: MemoryType,
    pub image_data_type: MemoryType,
    pub unload: Unused,
}

/// `EFI_SIMPLE_FILE_SYSTEM_PROTOCOL`.
#[repr(C)]
pub struct SimpleFileSystemProtocol {
    pub revision: u64,
    pub open_volume: unsafe extern "efiapi" fn(
        this: *mut SimpleFileSystemProtocol,
        root: *mut *mut FileProtocol,
    ) -> Status,
}

pub const FILE_MODE_READ: u64 = 0x0000_0000_0000_0001;

/// `EFI_FILE_PROTOCOL`.
#[repr(C)]
pub struct FileProtocol {
    pub revision: u64,
    pub open: unsafe extern "efiapi" fn(
        this: *mut FileProtocol,
        new_handle: *mut *mut FileProtocol,
        file_name: *const u16,
        open_mode: u64,
        attributes: u64,
    ) -> Status,
    pub close: unsafe extern "efiapi" fn(this: *mut FileProtocol) -> Status,
    pub delete: Unused,
    pub read: unsafe extern "efiapi" fn(
        this: *mut FileProtocol,
        buffer_size: *mut usize,
        buffer: *mut c_void,
    ) -> Status,
    pub write: Unused,
    pub get_position: Unused,
    pub set_position: unsafe extern "efiapi" fn(this: *mut FileProtocol, position: u64) -> Status,
    pub get_info: unsafe extern "efiapi" fn(
        this: *mut FileProtocol,
        information_type: *const Guid,
        buffer_size: *mut usize,
        buffer: *mut c_void,
    ) -> Status,
    pub set_info: Unused,
    pub flush: Unused,
}

/// Leading fixed-size part of `EFI_FILE_INFO`.
///
/// The real structure ends with a variable-length `CHAR16` name, which the
/// bootloader does not read.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FileInfoHeader {
    pub size: u64,
    pub file_size: u64,
    pub physical_size: u64,
    pub create_time: [u8; 16],
    pub last_access_time: [u8; 16],
    pub modification_time: [u8; 16],
    pub attribute: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;

    /// The bootloader reaches firmware routines through these offsets; if any
    /// of them drift it would call into the wrong function entirely.
    #[test]
    fn boot_services_offsets_match_the_uefi_specification() {
        assert_eq!(offset_of!(BootServices, allocate_pages), 40);
        assert_eq!(offset_of!(BootServices, get_memory_map), 56);
        assert_eq!(offset_of!(BootServices, allocate_pool), 64);
        assert_eq!(offset_of!(BootServices, handle_protocol), 152);
        assert_eq!(offset_of!(BootServices, exit_boot_services), 232);
        assert_eq!(offset_of!(BootServices, stall), 248);
        assert_eq!(offset_of!(BootServices, locate_protocol), 320);
    }

    #[test]
    fn system_table_offsets_match_the_uefi_specification() {
        assert_eq!(offset_of!(SystemTable, con_out), 64);
        assert_eq!(offset_of!(SystemTable, boot_services), 96);
        assert_eq!(offset_of!(SystemTable, configuration_table), 112);
    }

    #[test]
    fn memory_descriptor_is_forty_bytes() {
        assert_eq!(core::mem::size_of::<MemoryDescriptor>(), 40);
        assert_eq!(offset_of!(MemoryDescriptor, physical_start), 8);
        assert_eq!(offset_of!(MemoryDescriptor, number_of_pages), 24);
    }

    #[test]
    fn file_info_file_size_is_at_offset_eight() {
        assert_eq!(offset_of!(FileInfoHeader, file_size), 8);
        assert_eq!(core::mem::size_of::<FileInfoHeader>(), 80);
    }

    #[test]
    fn loaded_image_image_base_is_at_offset_sixty_four() {
        assert_eq!(offset_of!(LoadedImageProtocol, device_handle), 24);
        assert_eq!(offset_of!(LoadedImageProtocol, image_base), 64);
    }
}
