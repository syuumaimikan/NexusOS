//! The boot display.
//!
//! Owns the framebuffer and paints the screen NexusOS shows while it is coming
//! up. This is not the compositor — there are no surfaces, no damage tracking
//! and no windows, and every repaint redraws the whole panel. It exists to make
//! the running system visible, and to be the thing the Nexus Compositor
//! replaces rather than the thing it is built on.
//!
//! The status panel is redrawn by a thread rather than written once at boot,
//! which is the point: a screen that keeps updating is direct evidence that the
//! scheduler, the timer, the heap and the framebuffer are all working together
//! long after boot has finished.

use alloc::format;
use alloc::string::String;

use nexus_abi::FramebufferInfo;

use crate::framebuffer::{font, Color, Framebuffer};
use crate::sync::IrqSpinLock;
use crate::{arch, kprintln, memory, sched};

/// The framebuffer, once the kernel has adopted it.
static DISPLAY: IrqSpinLock<Option<Framebuffer>> = IrqSpinLock::new(None);

/// Background at the top of the gradient.
const BACKGROUND_TOP: Color = Color(0x000B_1220);
/// Background at the bottom of the gradient.
const BACKGROUND_BOTTOM: Color = Color(0x0014_2A4A);
/// Panel fill.
const PANEL: Color = Color(0x0010_1C33);
/// Panel border and accents.
const ACCENT: Color = Color(0x0021_8AFF);
/// Primary text.
const TEXT: Color = Color(0x00E6_EDF5);
/// Secondary text.
const MUTED: Color = Color(0x0084_9AB8);
/// The bar along the bottom.
const BAR: Color = Color(0x0008_0E18);

/// Adopt the framebuffer and paint the initial screen.
///
/// # Safety
///
/// `info` must describe a framebuffer mapped writable through the direct map,
/// and this must be called once.
pub unsafe fn init(info: &FramebufferInfo) -> bool {
    // SAFETY: upheld by the caller; this is the only `Framebuffer` constructed.
    let Some(framebuffer) = (unsafe { Framebuffer::new(info) }) else {
        kprintln!("[disp] no usable framebuffer; running headless");
        return false;
    };

    kprintln!(
        "[disp] {}x{} display adopted",
        framebuffer.width(),
        framebuffer.height()
    );
    *DISPLAY.lock() = Some(framebuffer);

    paint_background();
    true
}

/// Whether a display is available.
#[must_use]
pub fn is_available() -> bool {
    DISPLAY.lock().is_some()
}

/// Run `f` with the framebuffer, if there is one.
fn with<R>(f: impl FnOnce(&mut Framebuffer) -> R) -> Option<R> {
    let mut guard = DISPLAY.lock();
    guard.as_mut().map(f)
}

/// Paint the parts of the screen that never change.
fn paint_background() {
    with(|fb| {
        let width = fb.width();
        let height = fb.height();

        fb.vertical_gradient(BACKGROUND_TOP, BACKGROUND_BOTTOM);

        // Title block, sized as a fraction of the surface so it is proportionate
        // at whatever mode the firmware gave us.
        let title_scale = (width / 240).clamp(3, 10);
        let subtitle_scale = (title_scale / 2).max(2);

        let title_y = height / 6;
        fb.draw_text_centered(title_y, "NexusOS", TEXT, title_scale);

        let subtitle_y = title_y + Framebuffer::line_height(title_scale) + 12;
        fb.draw_text_centered(
            subtitle_y,
            concat!("Nexus Kernel v", env!("CARGO_PKG_VERSION")),
            ACCENT,
            subtitle_scale,
        );

        let tagline_y = subtitle_y + Framebuffer::line_height(subtitle_scale) + 8;
        fb.draw_text_centered(
            tagline_y,
            "a high-performance, AI-native, secure desktop operating system",
            MUTED,
            (subtitle_scale / 2).max(1),
        );

        // A bar along the bottom, where the Nexus Desktop's dock will go.
        let bar_height = (height / 22).max(28);
        let bar_y = height - bar_height;
        fb.fill_rect(0, bar_y, width, bar_height, BAR);
        fb.fill_rect(0, bar_y, width, 2, ACCENT);

        let label_scale = (bar_height / 14).clamp(1, 3);
        let label_y = bar_y + (bar_height - Framebuffer::line_height(label_scale)) / 2;
        fb.draw_text(24, label_y, "Nexus", ACCENT, label_scale);
        fb.draw_text(
            24 + Framebuffer::text_width("Nexus  ", label_scale),
            label_y,
            "kernel bring-up",
            MUTED,
            label_scale,
        );
    });
}

/// Geometry of the status panel.
struct PanelLayout {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    padding: u32,
    value_offset: u32,
    text_scale: u32,
    line_spacing: u32,
}

/// Lay the panel out around the text it actually has to hold.
///
/// Sizing the panel from its content is not cosmetic. Each refresh clears the
/// panel and redraws it, so anything drawn *outside* the cleared rectangle is
/// never erased: a value one character wider than the panel leaves the previous
/// frame's glyphs behind it, and the two overlap into gibberish. The panel
/// therefore has to be at least as wide as its widest line, and the text scale
/// drops rather than letting a line escape the surface.
fn panel_layout(surface_width: u32, surface_height: u32, rows: &[StatusRow]) -> PanelLayout {
    // The widest label and the widest value, in characters.
    let label_characters = rows.iter().map(|row| row.label.len()).max().unwrap_or(0) as u32;
    let value_characters = rows.iter().map(|row| row.value.len()).max().unwrap_or(0) as u32;
    // Two spaces between the columns.
    let content_characters = label_characters + 2 + value_characters;

    // Start from a comfortable scale and step down until the widest line fits
    // inside the surface with room for the panel's padding and margins.
    let margin = surface_width / 10;
    let mut text_scale = (surface_width / 480).clamp(2, 4);
    while text_scale > 1 {
        let padding = 12 * text_scale;
        let needed = content_characters * font::GLYPH_WIDTH * text_scale + padding * 2;
        if needed + margin <= surface_width {
            break;
        }
        text_scale -= 1;
    }

    let padding = 12 * text_scale;
    let content_width = content_characters * font::GLYPH_WIDTH * text_scale;
    let width = (content_width + padding * 2).min(surface_width);
    let line_spacing = Framebuffer::line_height(text_scale) + text_scale * 4;
    let height = rows.len() as u32 * line_spacing + padding * 2;

    PanelLayout {
        x: (surface_width - width) / 2,
        y: surface_height / 2,
        width,
        height,
        padding,
        value_offset: (label_characters + 2) * font::GLYPH_WIDTH * text_scale,
        text_scale,
        line_spacing,
    }
}

/// One labelled row of the status panel.
struct StatusRow {
    label: &'static str,
    value: String,
}

/// Redraw the status panel with current system state.
pub fn refresh_status() {
    let uptime_ms = arch::pit::uptime_ms();
    let scheduler = sched::stats();
    let heap = memory::heap::stats();
    let frames = memory::stats();

    let rows = [
        StatusRow {
            label: "uptime",
            value: format!("{}.{:03} s", uptime_ms / 1000, uptime_ms % 1000),
        },
        StatusRow {
            label: "memory",
            value: match frames {
                Some(frames) => format!(
                    "{} MiB free of {} MiB",
                    frames.free_frames * 4096 / (1024 * 1024),
                    frames.managed_frames * 4096 / (1024 * 1024)
                ),
                None => String::from("unavailable"),
            },
        },
        StatusRow {
            label: "heap",
            value: format!("{} KiB used of {} KiB", heap.used / 1024, heap.total / 1024),
        },
        StatusRow {
            label: "threads",
            value: format!(
                "{} ({} ready, {} sleeping)",
                scheduler.threads, scheduler.ready, scheduler.sleeping
            ),
        },
        StatusRow {
            label: "switches",
            value: format!("{}", scheduler.context_switches),
        },
        StatusRow {
            label: "timer",
            value: format!(
                "{} Hz, {} ticks",
                arch::pit::frequency_hz(),
                arch::pit::ticks()
            ),
        },
    ];

    with(|fb| {
        let layout = panel_layout(fb.width(), fb.height(), &rows);

        // Repaint the whole panel each time. Damage tracking is the
        // compositor's job; here it would only be an optimisation of something
        // that already costs a fraction of a frame.
        fb.fill_rect(layout.x, layout.y, layout.width, layout.height, PANEL);
        fb.fill_rect(layout.x, layout.y, layout.width, 2, ACCENT);
        fb.fill_rect(
            layout.x,
            layout.y + layout.height - 2,
            layout.width,
            2,
            ACCENT,
        );
        fb.fill_rect(layout.x, layout.y, 2, layout.height, ACCENT);
        fb.fill_rect(
            layout.x + layout.width - 2,
            layout.y,
            2,
            layout.height,
            ACCENT,
        );

        let label_x = layout.x + layout.padding;
        let value_x = label_x + layout.value_offset;

        for (index, row) in rows.iter().enumerate() {
            let y = layout.y + layout.padding + index as u32 * layout.line_spacing;
            fb.draw_text(label_x, y, row.label, MUTED, layout.text_scale);
            fb.draw_text(value_x, y, &row.value, TEXT, layout.text_scale);
        }
    });
}

/// The thread that keeps the status panel current.
///
/// Two updates a second: fast enough that the screen is visibly alive, slow
/// enough to cost nothing measurable.
fn display_thread(_argument: usize) {
    loop {
        refresh_status();
        sched::sleep_ms(500);
    }
}

/// Start the display thread. Does nothing when there is no framebuffer.
pub fn start_thread() {
    if !is_available() {
        return;
    }
    match sched::spawn(
        "display",
        sched::thread::Priority::Interactive,
        display_thread,
        0,
    ) {
        Ok(id) => kprintln!("[disp] display thread {id} started"),
        Err(error) => kprintln!("[disp] could not start the display thread: {error}"),
    }
}
