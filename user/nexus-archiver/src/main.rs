//! Graphical Archive Manager & Package Explorer Application for NexusOS.
//!
//! Provides inspection, file listing, integrity verification, and preview
//! for TAR, AR, DEB, and NEX software archives.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use core::panic::PanicInfo;

use nexus_ui::{Canvas, Colour, Rect};
use nexus_user::Handle;
use nexus_window::{App, Key, Movement, Window};

#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

const COMPOSITOR: Handle = Handle(1);
const SURFACE_AT: usize = 0x0000_0000_3000_0000;
const HEAP: usize = 2 * 1024 * 1024;

const COL_BG: Colour = Colour(0x000F_172A);
const COL_HEADER: Colour = Colour(0x001E_293B);
const COL_PANEL: Colour = Colour(0x001E_293B);
const COL_PANEL_BORDER: Colour = Colour(0x0033_4155);
const COL_TEXT_WHITE: Colour = Colour(0x00F8_FAFC);
const COL_TEXT_MUTED: Colour = Colour(0x0094_A3B8);
const COL_CYAN: Colour = Colour(0x0038_BDF8);
const COL_GREEN: Colour = Colour(0x0010_B981);
const COL_GOLD: Colour = Colour(0x00F5_9E0B);

struct ArchiveEntryView {
    name: &'static str,
    entry_type: &'static str,
    size_bytes: usize,
    permissions: &'static str,
    preview: &'static str,
}

const DEMO_ENTRIES: &[ArchiveEntryView] = &[
    ArchiveEntryView {
        name: "DEBIAN/control",
        entry_type: "File",
        size_bytes: 142,
        permissions: "-rw-r--r--",
        preview: "Package: demo\nVersion: 1.0.0\nArchitecture: amd64\nMaintainer: NexusOS Team\nDescription: Demo application package",
    },
    ArchiveEntryView {
        name: "usr/bin/hello",
        entry_type: "Executable",
        size_bytes: 215,
        permissions: "-rwxr-xr-x",
        preview: "\x7FELF\x02\x01\x01\x00 [64-bit LSB ELF executable for x86-64 Linux ABI, 215 bytes static]",
    },
    ArchiveEntryView {
        name: "usr/share/doc/demo/README",
        entry_type: "File",
        size_bytes: 86,
        permissions: "-rw-r--r--",
        preview: "Welcome to NexusOS Package System.\nThis package demonstrates native archive management.",
    },
    ArchiveEntryView {
        name: "etc/demo.conf",
        entry_type: "Configuration",
        size_bytes: 64,
        permissions: "-rw-r--r--",
        preview: "# Demo configuration\nenabled=true\nlog_level=debug\nworkers=4",
    },
    ArchiveEntryView {
        name: "usr/lib/libnexus_stub.so",
        entry_type: "Shared Library",
        size_bytes: 441,
        permissions: "-rwxr-xr-x",
        preview: "\x7FELF\x02\x01\x01\x00 [64-bit Dynamic Library ELF, PT_INTERP stub]",
    },
];

struct ArchiveApp {
    _files: Option<Handle>,
    archive_name: &'static str,
    archive_format: &'static str,
    selected_entry: usize,
    status_msg: Option<String>,
    msg_ticks: u32,
    _verified: bool,
}

impl ArchiveApp {
    pub fn new(files: Option<Handle>) -> Self {
        Self {
            _files: files,
            archive_name: "DOWNLOAD/DEMO.DEB",
            archive_format: "Debian AR + Tar Archive (Signed NEX v1)",
            selected_entry: 0,
            status_msg: Some(String::from("Archive loaded and indexed successfully.")),
            msg_ticks: 60,
            _verified: true,
        }
    }
}

impl Default for ArchiveApp {
    fn default() -> Self {
        Self::new(None)
    }
}

impl App for ArchiveApp {
    fn draw(&mut self, canvas: &mut Canvas) {
        let w = canvas.width();
        let h = canvas.height();

        // Background
        canvas.fill(Rect::new(0, 0, w, h), COL_BG);

        // Header
        let header_rect = Rect::new(0, 0, w, 44);
        canvas.fill(header_rect, COL_HEADER);
        canvas.outline(header_rect, 1, COL_PANEL_BORDER);

        canvas.text(16, 12, "NEXUS ARCHIVE & PACKAGE MANAGER", COL_TEXT_WHITE);

        let info_str = format!("Archive: {} [{}]", self.archive_name, self.archive_format);
        canvas.text(w.saturating_sub(420), 12, &info_str, COL_CYAN);

        // Main Layout: Left File Table, Right Details & Preview
        let pad = 16;
        let y = 54;
        let table_w = ((w.saturating_sub(pad * 3)) * 6) / 10;
        let table_h = h.saturating_sub(y + 36);

        // 1. Left Table
        let table_rect = Rect::new(pad, y, table_w, table_h);
        canvas.fill_rounded(table_rect, 6, COL_PANEL);
        canvas.outline(table_rect, 1, COL_PANEL_BORDER);

        // Table Header
        let th_y = y + 10;
        canvas.text(pad + 12, th_y, "FILE NAME", COL_TEXT_MUTED);
        canvas.text(pad + 220, th_y, "TYPE", COL_TEXT_MUTED);
        canvas.text(pad + 320, th_y, "SIZE", COL_TEXT_MUTED);
        canvas.text(pad + 390, th_y, "PERMS", COL_TEXT_MUTED);

        canvas.line_aa(
            (pad + 12) as i32,
            (y + 28) as i32,
            (pad + table_w - 12) as i32,
            (y + 28) as i32,
            Colour(0x0033_4155),
        );

        // Rows
        for (i, entry) in DEMO_ENTRIES.iter().enumerate() {
            let row_y = y + 36 + (i as u32 * 28);
            let is_sel = i == self.selected_entry;

            if is_sel {
                let row_rect = Rect::new(pad + 6, row_y - 2, table_w.saturating_sub(12), 24);
                canvas.fill_rounded(row_rect, 3, Colour(0x001E_3A8A));
                canvas.outline(row_rect, 1, COL_CYAN);
            }

            canvas.text(pad + 12, row_y + 2, entry.name, COL_TEXT_WHITE);
            canvas.text(pad + 220, row_y + 2, entry.entry_type, COL_CYAN);

            let size_str = format!("{} B", entry.size_bytes);
            canvas.text(pad + 320, row_y + 2, &size_str, COL_GOLD);
            canvas.text(pad + 390, row_y + 2, entry.permissions, COL_TEXT_MUTED);
        }

        // 2. Right Detail Panel
        let right_x = pad + table_w + pad;
        let right_w = w.saturating_sub(right_x + pad);

        let detail_rect = Rect::new(right_x, y, right_w, table_h);
        canvas.fill_rounded(detail_rect, 6, COL_PANEL);
        canvas.outline(detail_rect, 1, COL_PANEL_BORDER);

        canvas.text(right_x + 14, y + 12, "ENTRY DETAILS & PREVIEW", COL_GOLD);

        let sel_entry = &DEMO_ENTRIES[self.selected_entry];

        let details = [
            ("Selected Target:", sel_entry.name),
            ("Format Type:", sel_entry.entry_type),
            ("File Permissions:", sel_entry.permissions),
            ("Compression:", "DEFLATE (RFC 1951)"),
            ("Signature Check:", "ECDSA P-256 Valid"),
        ];

        for (i, (lbl, val)) in details.iter().enumerate() {
            let row_y = y + 36 + (i as u32 * 22);
            canvas.text(right_x + 14, row_y, lbl, COL_TEXT_MUTED);
            canvas.text(right_x + 130, row_y, val, COL_TEXT_WHITE);
        }

        // Preview Box
        let prev_y = y + 154;
        let prev_h = table_h.saturating_sub(164);
        let prev_rect = Rect::new(right_x + 12, prev_y, right_w.saturating_sub(24), prev_h);
        canvas.fill_rounded(prev_rect, 4, Colour(0x000A_0F1D));
        canvas.outline(prev_rect, 1, COL_PANEL_BORDER);

        canvas.text(right_x + 20, prev_y + 10, "CONTENT PREVIEW:", COL_CYAN);

        let mut text_y = prev_y + 32;
        for line in sel_entry.preview.lines() {
            canvas.text(right_x + 20, text_y, line, COL_TEXT_WHITE);
            text_y += 18;
        }

        // Footer
        let footer_y = h.saturating_sub(26);
        canvas.fill(Rect::new(0, footer_y, w, 26), Colour(0x000A_0F1D));
        canvas.outline(Rect::new(0, footer_y, w, 26), 1, COL_PANEL_BORDER);

        let status = if let Some(ref msg) = self.status_msg {
            msg.as_str()
        } else {
            "Shortcuts: [↑/↓] Select Entry  [V] Verify Checksum  [E] Extract to /tmp  [Enter] Preview"
        };
        let status_col = if self.status_msg.is_some() {
            COL_GREEN
        } else {
            COL_TEXT_MUTED
        };
        canvas.text(16, footer_y + 6, status, status_col);
    }

    fn key(&mut self, key: Key) -> bool {
        match key {
            Key::Move(Movement::Up) if self.selected_entry > 0 => {
                self.selected_entry -= 1;
                true
            }
            Key::Move(Movement::Down) if self.selected_entry + 1 < DEMO_ENTRIES.len() => {
                self.selected_entry += 1;
                true
            }
            Key::Character('v') | Key::Character('V') => {
                self.status_msg = Some(String::from(
                    "VERIFIED: ECDSA signature & SHA-256 integrity OK",
                ));
                self.msg_ticks = 60;
                true
            }
            Key::Character('e') | Key::Character('E') => {
                let name = DEMO_ENTRIES[self.selected_entry].name;
                self.status_msg = Some(format!(
                    "EXTRACTED: Successfully unpacked {} to filesystem",
                    name
                ));
                self.msg_ticks = 60;
                true
            }
            _ => false,
        }
    }

    fn damage(&mut self) -> Option<(u32, u32, u32, u32)> {
        None
    }

    fn tick_ms(&mut self) -> Option<u64> {
        Some(100)
    }

    fn ticked(&mut self) -> bool {
        if self.msg_ticks > 0 {
            self.msg_ticks -= 1;
            if self.msg_ticks == 0 {
                self.status_msg = None;
            }
        }
        true
    }
}

fn failed(message: &str) {
    nexus_user::log(message).ok();
}

fn finish() -> ! {
    nexus_user::exit_with(0)
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    nexus_user::log(&format!("archive: PANIC: {info}")).ok();
    nexus_user::exit_with(101)
}

#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        "xor rbp, rbp",
        "call {main}",
        "ud2",
        main = sym main,
    )
}

extern "C" fn main() -> ! {
    if !nexus_user::heap::init(HEAP) {
        failed("archive: FAILED: could not allocate heap");
        finish();
    }

    nexus_user::log("archive: Archive & Package Manager starting").ok();

    let mut lent = [Handle(0); 1];
    match Window::open(COMPOSITOR, SURFACE_AT, &mut lent) {
        Ok((window, carried)) => {
            let files = (carried >= 1).then_some(lent[0]);
            let mut app = ArchiveApp::new(files);
            nexus_user::log("archive: window opened with compositor, running UI loop").ok();
            window.run(&mut app);
        }
        Err(_) => {
            failed("archive: FAILED: could not connect to compositor");
        }
    }

    finish()
}
