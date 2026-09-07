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
use crate::{arch, i18n, kprintln, memory, sched};

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

/// How long each language is shown for by the demonstration thread.
const LOCALE_CYCLE_MS: u64 = 6000;

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
            "[disp] {} glyphs available, including CJK",
            font::generated_glyph_count()
        );
    } else {
        // Worth saying plainly: if this line appears, Japanese labels will be
        // placeholder boxes, and the cause is the build rather than the
        // renderer.
        kprintln!("[disp] no generated glyphs; falling back to the built-in ASCII face");
    }

    *DISPLAY.lock() = Some(framebuffer);
    paint_chrome();
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
        fb.vertical_gradient_region(
            0,
            height - bar_height,
            height,
            BACKGROUND_TOP,
            BACKGROUND_BOTTOM,
        );

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
fn status_rows() -> [StatusRow; 7] {
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
                    ("ready", &scheduler.ready),
                    ("sleeping", &scheduler.sleeping),
                ],
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
        StatusRow {
            label: String::from(i18n::text("status.language")),
            value: String::from(i18n::current().name),
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
        fb.vertical_gradient_region(
            layout.y,
            layout.y + clear_height,
            fb.height(),
            BACKGROUND_TOP,
            BACKGROUND_BOTTOM,
        );

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

/// Cycles the interface language, to demonstrate that switching works at
/// runtime.
///
/// A real system changes language from settings, and only when asked. There is
/// no settings UI yet, and a language selectable only at build time has not
/// really been shown to work — the point of this thread is that every string,
/// and the layout derived from it, is recomputed live.
fn locale_demo_thread(_argument: usize) {
    loop {
        sched::sleep_ms(LOCALE_CYCLE_MS);
        let locale = i18n::next_locale();
        // The tag, not the name. The name is in its own language, and putting
        // UTF-8 on the serial line turns the log into mojibake for anyone whose
        // terminal is not set to it -- which is the whole reason logs here stay
        // ASCII. The tag is also what a developer would grep for.
        kprintln!("[i18n] interface language is now {}", locale.tag);
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

    if i18n::locale_count() > 1 {
        match sched::spawn(
            "locale-demo",
            sched::thread::Priority::Background,
            locale_demo_thread,
            0,
        ) {
            Ok(id) => kprintln!(
                "[i18n] thread {id} cycles {} languages every {} ms",
                i18n::locale_count(),
                LOCALE_CYCLE_MS
            ),
            Err(error) => kprintln!("[i18n] could not start the language demonstration: {error}"),
        }
    }
}
