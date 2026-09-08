//! The boot display.
//!
//! Owns the framebuffer and paints the screen NexusOS shows while it is coming
//! up. This is not the compositor — there are no surfaces, no damage tracking
//! and no windows, and every repaint redraws whole regions. It exists to make
//! the running system visible, and to be the thing the Nexus Compositor
//! replaces rather than the thing it is built on.
//!
//! The status panel is redrawn by a thread rather than written once at boot,
//! which is the point: a screen that keeps updating is direct evidence that the
//! scheduler, the timer, the heap and the framebuffer are all working together
//! long after boot has finished.
//!
//! Every user-visible string here comes from [`crate::i18n`]. Nothing a person
//! reads is written literally in this file.

use alloc::string::String;

use nexus_abi::FramebufferInfo;

use crate::framebuffer::{font, Color, Framebuffer};
use crate::sync::IrqSpinLock;
use crate::{arch, fs, i18n, input, kprintln, memory, sched};

/// The framebuffer, once the kernel has adopted it.
static DISPLAY: IrqSpinLock<Option<Framebuffer>> = IrqSpinLock::new(None);

/// What the firmware said about the framebuffer, kept so that a process can be
/// handed it.
///
/// A compositor is a process holding a handle to the display's memory, not a
/// thing inside the kernel. This is the kernel's half of that: it knows where
/// the pixels are, and something above it decides what to put in them.
static GEOMETRY: IrqSpinLock<Option<FramebufferInfo>> = IrqSpinLock::new(None);

/// Where the framebuffer is and what shape it has, if there is one.
#[must_use]
pub fn geometry() -> Option<FramebufferInfo> {
    *GEOMETRY.lock()
}

/// The region of the screen the kernel's own chrome never touches.
///
/// Returned as `(x, y, width, height)` in pixels. It is not a window and it is
/// not owned: it is a rectangle the kernel promises to leave alone, until there
/// is a compositor to ask instead of a promise to keep.
///
/// Above the panel band, and that is the whole of why it is where it is. The
/// panel is redrawn twice a second, and clearing it means clearing *whole
/// rows* -- a translated line is a different length from the one it replaces,
/// so anything narrower would leave the tail of the previous language on
/// screen. A rectangle beside the panel is therefore not beside it at all; it
/// is inside the rows the panel wipes, which is what happened to the first
/// version of this and showed up as a fifteen-pixel sliver of somebody's
/// gradient.
#[must_use]
pub fn unclaimed_region() -> Option<(u32, u32, u32, u32)> {
    let info = geometry()?;

    // Where `panel_layout` starts looking for room. Everything from here down
    // belongs to the panel and the bar.
    let panel_band_top = info.height * 2 / 5;

    let width = info.width / 5;
    let margin = 32;
    let height = (panel_band_top - margin * 2).min(info.height / 5);
    let x = info.width - width - margin;
    Some((x, margin, width, height))
}

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
    if font::has_generated_face() {
        kprintln!(
            "[disp] {} glyphs available, including CJK, from {}",
            font::generated_glyph_count(),
            font::generated_source()
        );
    } else {
        // Worth saying plainly: if this line appears, Japanese labels will be
        // placeholder boxes, and the cause is the build rather than the
        // renderer.
        kprintln!("[disp] no generated glyphs; falling back to the built-in ASCII face");
    }

    *DISPLAY.lock() = Some(framebuffer);
    *GEOMETRY.lock() = Some(*info);
    paint_chrome();
    true
}

/// Whether a display is available.
#[must_use]
pub fn is_available() -> bool {
    DISPLAY.lock().is_some()
}

/// Repaint the background across rows `start_y..end_y`, leaving the rectangle
/// that belongs to a process alone.
///
/// Every clear in this module goes through here. A clear that ran from edge to
/// edge would take back the rectangle the kernel gave away, twice a second and
/// again on every language change -- which it did, and looked exactly like a
/// user program that had failed to draw.
fn clear_rows(fb: &mut Framebuffer, start_y: u32, end_y: u32) {
    let surface = fb.height();
    let Some((x, y, width, height)) = unclaimed_region() else {
        fb.vertical_gradient_region(start_y, end_y, surface, BACKGROUND_TOP, BACKGROUND_BOTTOM);
        return;
    };

    // Rows above and below the reserved rectangle: the whole width.
    let above = end_y.min(y);
    if above > start_y {
        fb.vertical_gradient_region(start_y, above, surface, BACKGROUND_TOP, BACKGROUND_BOTTOM);
    }
    let below = start_y.max(y + height);
    if end_y > below {
        fb.vertical_gradient_region(below, end_y, surface, BACKGROUND_TOP, BACKGROUND_BOTTOM);
    }

    // And the rows beside it: everything but the rectangle itself.
    let overlap_start = start_y.max(y);
    let overlap_end = end_y.min(y + height);
    if overlap_end > overlap_start {
        fb.vertical_gradient_span(
            0,
            x,
            overlap_start,
            overlap_end,
            surface,
            BACKGROUND_TOP,
            BACKGROUND_BOTTOM,
        );
        fb.vertical_gradient_span(
            x + width,
            fb.width(),
            overlap_start,
            overlap_end,
            surface,
            BACKGROUND_TOP,
            BACKGROUND_BOTTOM,
        );
    }
}

/// Run `f` with the framebuffer, if there is one.
fn with<R>(f: impl FnOnce(&mut Framebuffer) -> R) -> Option<R> {
    let mut guard = DISPLAY.lock();
    guard.as_mut().map(f)
}

/// Paint the title block and the bar along the bottom.
///
/// Repainted whenever the language changes, not only at boot: these strings are
/// translated too, and a locale switch has to redraw them or the screen ends up
/// half in each language.
fn paint_chrome() {
    let title = i18n::text("os.name");
    let subtitle = i18n::format("os.subtitle", &[("version", &env!("CARGO_PKG_VERSION"))]);
    let tagline = i18n::text("os.tagline");
    let status = i18n::text("bar.status");

    with(|fb| {
        let width = fb.width();
        let height = fb.height();
        let bar_height = (height / 22).max(28);

        // Clear the whole chrome area before redrawing. A translation is a
        // different length from the one it replaces, so anything left over from
        // the previous language would still be on screen underneath.
        clear_rows(fb, 0, height - bar_height);

        // Sized as a fraction of the surface, so the layout is proportionate at
        // whatever mode the firmware gave us.
        let title_scale = (width / 240).clamp(3, 10);
        let subtitle_scale = (title_scale / 2).max(2);

        let title_y = height / 7;
        fb.draw_text_centered(title_y, title, TEXT, title_scale);

        let subtitle_y = title_y + Framebuffer::line_height(title_scale) + 12;
        fb.draw_text_centered(subtitle_y, &subtitle, ACCENT, subtitle_scale);

        let tagline_y = subtitle_y + Framebuffer::line_height(subtitle_scale) + 8;
        fb.draw_text_centered(tagline_y, tagline, MUTED, (subtitle_scale / 2).max(1));

        // The bar along the bottom, where the Nexus Desktop's dock will go.
        let bar_y = height - bar_height;
        fb.fill_rect(0, bar_y, width, bar_height, BAR);
        fb.fill_rect(0, bar_y, width, 2, ACCENT);

        let label_scale = (bar_height / 22).clamp(1, 2);
        let label_y = bar_y + (bar_height - Framebuffer::line_height(label_scale)) / 2;
        let cursor = fb.draw_text(24, label_y, "Nexus", ACCENT, label_scale);
        fb.draw_text(
            cursor + Framebuffer::text_width("  ", label_scale),
            label_y,
            status,
            MUTED,
            label_scale,
        );
    });
}

/// Geometry of the status panel, in pixels.
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

/// One labelled row of the status panel.
struct StatusRow {
    label: String,
    value: String,
}

/// Lay the panel out around the text it actually has to hold.
///
/// Everything is measured in pixels rather than characters. That is not a
/// refinement, it is required: a Japanese label is full-width and an English one
/// half-width, so the same character count is a different number of pixels.
/// Sizing by character count would draw Japanese values outside the rectangle
/// the next refresh clears, and consecutive frames would overlap into
/// unreadable text.
fn panel_layout(surface_width: u32, surface_height: u32, rows: &[StatusRow]) -> PanelLayout {
    let label_units = rows
        .iter()
        .map(|row| font::measure(&row.label))
        .max()
        .unwrap_or(0);
    let value_units = rows
        .iter()
        .map(|row| font::measure(&row.value))
        .max()
        .unwrap_or(0);
    let gap_units = font::measure("  ");
    let content_units = label_units + gap_units + value_units;

    // The band the panel has to live in: below the title block and above the
    // bar along the bottom.
    let bar_height = (surface_height / 22).max(28);
    let band_top = surface_height * 2 / 5;
    let band_bottom = surface_height - bar_height - 16;
    let band_height = band_bottom.saturating_sub(band_top);

    // Step the scale down until the panel fits the band in *both* directions.
    //
    // Both constraints matter and neither is hypothetical. Japanese lines are
    // wider than their English equivalents, so the horizontal limit binds in
    // one language and not the other; and seven rows at the largest scale is
    // taller than the band, which is how an earlier version ran its panel off
    // the bottom of the screen.
    let margin = surface_width / 12;
    let mut text_scale = (surface_width / 480).clamp(1, 4);
    let (mut padding, mut line_spacing, mut width, mut height);
    loop {
        padding = 12 * text_scale;
        line_spacing = Framebuffer::line_height(text_scale) + text_scale * 3;
        width = content_units * text_scale + padding * 2;
        height = rows.len() as u32 * line_spacing + padding * 2;

        let fits = width + margin <= surface_width && height <= band_height;
        if fits || text_scale == 1 {
            break;
        }
        text_scale -= 1;
    }

    let width = width.min(surface_width);
    // Centre the panel in the band, and never start it above the band.
    let y = band_top + band_height.saturating_sub(height) / 2;

    PanelLayout {
        x: (surface_width - width) / 2,
        y,
        width,
        height,
        padding,
        value_offset: (label_units + gap_units) * text_scale,
        text_scale,
        line_spacing,
    }
}

/// The tallest panel painted so far, so the cleared band never shrinks below
/// what a previous frame drew.
///
/// A locale switch changes every string at once, and the layout with them.
/// Without this, switching to a language that needs a shorter panel would clear
/// less than the last frame painted and leave the old panel's lower edge behind.
static PAINTED_HEIGHT: IrqSpinLock<u32> = IrqSpinLock::new(0);

/// The language last painted, so a switch can be noticed.
static PAINTED_LOCALE: IrqSpinLock<usize> = IrqSpinLock::new(usize::MAX);

/// Gather the current system state as translated label and value pairs.
fn status_rows() -> [StatusRow; 10] {
    let uptime_ms = arch::time::uptime_ms();
    let scheduler = sched::stats();
    let heap = memory::heap::stats();
    let frames = memory::stats();

    let memory_value = match frames {
        Some(frames) => i18n::format(
            "value.memory",
            &[
                ("free", &(frames.free_frames * 4096 / (1024 * 1024))),
                ("total", &(frames.managed_frames * 4096 / (1024 * 1024))),
            ],
        ),
        None => String::from(i18n::text("value.unavailable")),
    };

    [
        StatusRow {
            label: String::from(i18n::text("status.uptime")),
            value: i18n::format(
                "value.uptime",
                &[
                    ("seconds", &(uptime_ms / 1000)),
                    ("millis", &format_args!("{:03}", uptime_ms % 1000)),
                ],
            ),
        },
        StatusRow {
            label: String::from(i18n::text("status.memory")),
            value: memory_value,
        },
        StatusRow {
            label: String::from(i18n::text("status.heap")),
            value: i18n::format(
                "value.heap",
                &[
                    ("used", &(heap.used / 1024)),
                    ("total", &(heap.total / 1024)),
                ],
            ),
        },
        StatusRow {
            label: String::from(i18n::text("status.threads")),
            value: i18n::format(
                "value.threads",
                &[
                    ("total", &scheduler.threads),
                    ("running", &scheduler.running),
                    ("ready", &scheduler.ready),
                    ("sleeping", &scheduler.sleeping),
                ],
            ),
        },
        // On screen because it is the visible difference between a system that
        // brought its processors up and one that is scheduling on all of them.
        StatusRow {
            label: String::from(i18n::text("status.processors")),
            value: i18n::format(
                "value.processors",
                &[("online", &arch::smp::processor_count())],
            ),
        },
        StatusRow {
            label: String::from(i18n::text("status.switches")),
            value: i18n::format("value.switches", &[("count", &scheduler.context_switches)]),
        },
        StatusRow {
            label: String::from(i18n::text("status.timer")),
            value: i18n::format(
                "value.timer",
                &[
                    ("hz", &arch::time::frequency_hz()),
                    ("ticks", &arch::time::ticks()),
                    (
                        "source",
                        &match arch::time::source() {
                            arch::time::TimerSource::LocalApic => "APIC",
                            arch::time::TimerSource::Pit => "PIT",
                            arch::time::TimerSource::None => "-",
                        },
                    ),
                ],
            ),
        },
        // The filesystem the system keeps its own things in, which is on
        // screen for the same reason memory is: it is a resource that runs out,
        // and a number nobody can see is a number nobody notices moving.
        StatusRow {
            label: String::from(i18n::text("status.storage")),
            value: match fs::store::space() {
                Some((total, free)) => i18n::format(
                    "value.storage",
                    &[
                        ("free", &(free / (1024 * 1024))),
                        ("total", &(total / (1024 * 1024))),
                    ],
                ),
                None => String::from(i18n::text("value.unavailable")),
            },
        },
        StatusRow {
            label: String::from(i18n::text("status.language")),
            value: String::from(i18n::current().name),
        },
        StatusRow {
            label: String::from(i18n::text("status.input")),
            value: {
                let typed = input::line();
                if typed.is_empty() {
                    String::from(i18n::text("value.input_empty"))
                } else {
                    i18n::format("value.input", &[("text", &typed)])
                }
            },
        },
    ]
}

/// Redraw the status panel with current system state.
pub fn refresh_status() {
    let rows = status_rows();

    with(|fb| {
        let layout = panel_layout(fb.width(), fb.height(), &rows);

        let clear_height = {
            let mut painted = PAINTED_HEIGHT.lock();
            *painted = (*painted).max(layout.height);
            *painted
        };
        clear_rows(fb, layout.y, layout.y + clear_height);

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
            fb.draw_text(label_x, y, &row.label, MUTED, layout.text_scale);
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
        let locale = i18n::current_index();
        let changed = {
            let mut painted = PAINTED_LOCALE.lock();
            let changed = *painted != locale;
            *painted = locale;
            changed
        };
        if changed {
            paint_chrome();
        }

        refresh_status();
        sched::sleep_ms(500);
    }
}

/// Start the display threads. Does nothing when there is no framebuffer.
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
