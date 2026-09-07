//! Bringing up the linear framebuffer through the UEFI Graphics Output Protocol.
//!
//! The firmware is the only thing that can set a display mode before the kernel
//! has a GPU driver, so the bootloader picks the mode NexusOS will run in and
//! hands the resulting framebuffer to the kernel. The compositor draws into it
//! directly until a real driver takes over.

use core::ffi::c_void;

use nexus_abi::{FramebufferInfo, PixelFormat};

use crate::uefi::{
    self, BootServices, GraphicsOutputProtocol, GraphicsOutputModeInformation,
    GraphicsPixelFormat,
};

/// Widest mode the bootloader will select.
///
/// Larger panels are perfectly usable, but an unnecessarily high boot
/// resolution costs real time in the software compositor before GPU
/// acceleration exists, so the loader stops here.
const MAX_WIDTH: u32 = 1920;
/// Tallest mode the bootloader will select.
const MAX_HEIGHT: u32 = 1200;

/// A framebuffer report meaning "the firmware gave us nothing to draw on".
const NO_FRAMEBUFFER: FramebufferInfo = FramebufferInfo {
    phys_addr: 0,
    size: 0,
    width: 0,
    height: 0,
    stride: 0,
    bytes_per_pixel: 0,
    format: PixelFormat::Unknown,
    _reserved: 0,
};

/// Map a UEFI pixel format onto the kernel's, or `None` if it is one the
/// software compositor cannot render into.
fn translate_format(format: GraphicsPixelFormat) -> Option<PixelFormat> {
    match format {
        GraphicsPixelFormat::RedGreenBlueReserved8BitPerColor => Some(PixelFormat::Rgbx8888),
        GraphicsPixelFormat::BlueGreenRedReserved8BitPerColor => Some(PixelFormat::Bgrx8888),
        // `BitMask` needs per-channel shifts and `BltOnly` has no linear
        // framebuffer at all; neither is worth supporting before the GPU stack.
        GraphicsPixelFormat::BitMask | GraphicsPixelFormat::BltOnly => None,
    }
}

/// Rank a candidate mode. Higher is better; `None` means unusable.
fn score(info: &GraphicsOutputModeInformation) -> Option<u64> {
    translate_format(info.pixel_format)?;
    if info.horizontal_resolution == 0 || info.vertical_resolution == 0 {
        return None;
    }
    if info.horizontal_resolution > MAX_WIDTH || info.vertical_resolution > MAX_HEIGHT {
        return None;
    }
    Some(u64::from(info.horizontal_resolution) * u64::from(info.vertical_resolution))
}

/// Select the best available display mode and return the resulting framebuffer.
///
/// If no Graphics Output Protocol is present, or every mode it offers is one
/// the kernel cannot draw into, this returns an invalid [`FramebufferInfo`] and
/// the system boots headless with serial output only.
///
/// # Safety
///
/// `boot_services` must point at live boot services.
pub unsafe fn init(boot_services: *mut BootServices) -> FramebufferInfo {
    // SAFETY: upheld by the caller.
    let services = unsafe { &*boot_services };

    let mut gop: *mut c_void = core::ptr::null_mut();
    // SAFETY: the GUID matches the interface type the result is cast to.
    let status = unsafe {
        (services.locate_protocol)(
            &uefi::GRAPHICS_OUTPUT_PROTOCOL_GUID,
            core::ptr::null_mut(),
            &mut gop,
        )
    };
    if uefi::is_error(status) || gop.is_null() {
        crate::log!("no Graphics Output Protocol; continuing headless");
        return NO_FRAMEBUFFER;
    }
    let gop = gop as *mut GraphicsOutputProtocol;

    // SAFETY: `locate_protocol` succeeded, so `gop` and its `mode` are live.
    let max_mode = unsafe { (*(*gop).mode).max_mode };

    let mut best_mode: Option<u32> = None;
    let mut best_score = 0u64;

    for mode_number in 0..max_mode {
        let mut info: *mut GraphicsOutputModeInformation = core::ptr::null_mut();
        let mut info_size: usize = 0;
        // SAFETY: `gop` is live; `mode_number` is below `max_mode`.
        let status =
            unsafe { ((*gop).query_mode)(gop, mode_number, &mut info_size, &mut info) };
        if uefi::is_error(status) || info.is_null() {
            continue;
        }
        // SAFETY: `query_mode` succeeded, so `info` points at mode information
        // the firmware owns and keeps alive.
        let info = unsafe { &*info };

        if let Some(candidate) = score(info) {
            if candidate > best_score {
                best_score = candidate;
                best_mode = Some(mode_number);
            }
        }
    }

    if let Some(mode_number) = best_mode {
        // SAFETY: `mode_number` was returned by a successful `query_mode`.
        let status = unsafe { ((*gop).set_mode)(gop, mode_number) };
        if uefi::is_error(status) {
            crate::log!(
                "SetMode({mode_number}) failed with status {status:#x}; keeping current mode"
            );
        }
    }

    // Re-read the mode block: it reflects whichever mode is actually active,
    // whether or not `SetMode` above succeeded.
    // SAFETY: `gop` is live and its `mode` pointer is firmware-owned.
    let mode = unsafe { &*(*gop).mode };
    // SAFETY: as above.
    let info = unsafe { &*mode.info };

    let Some(format) = translate_format(info.pixel_format) else {
        crate::log!(
            "active mode uses an unsupported pixel format ({:?}); continuing headless",
            info.pixel_format
        );
        return NO_FRAMEBUFFER;
    };

    FramebufferInfo {
        phys_addr: mode.frame_buffer_base,
        size: mode.frame_buffer_size as u64,
        width: info.horizontal_resolution,
        height: info.vertical_resolution,
        stride: info.pixels_per_scan_line,
        bytes_per_pixel: 4,
        format,
        _reserved: 0,
    }
}
