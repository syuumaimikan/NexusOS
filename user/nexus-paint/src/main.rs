//! Graphical Paint & Pixel Art Studio Application for NexusOS.
//!
//! Provides a multi-tool digital drawing canvas, 16-color palette, brush size controls,
//! shape drawing primitives, and BMP picture saving to the filesystem.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::panic::PanicInfo;

use nexus_ui::{Canvas, Colour, Rect};
use nexus_user::Handle;
use nexus_window::{App, Key, Movement, Window};

#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

const COMPOSITOR: Handle = Handle(1);
const SURFACE_AT: usize = 0x0000_0000_3000_0000;
const HEAP: usize = 4 * 1024 * 1024;

const CANVAS_W: usize = 320;
const CANVAS_H: usize = 200;

const COL_BG: Colour = Colour(0x000F_172A);
const COL_HEADER: Colour = Colour(0x001E_293B);
const COL_PANEL: Colour = Colour(0x001E_293B);
const COL_PANEL_BORDER: Colour = Colour(0x0033_4155);
const COL_TEXT_WHITE: Colour = Colour(0x00F8_FAFC);
const COL_TEXT_MUTED: Colour = Colour(0x0094_A3B8);
const COL_CYAN: Colour = Colour(0x0038_BDF8);
const COL_GOLD: Colour = Colour(0x00F5_9E0B);
const COL_GREEN: Colour = Colour(0x0010_B981);

const PALETTE: [Colour; 16] = [
    Colour(0x0000_0000), // Black
    Colour(0x001E_293B), // Dark Slate
    Colour(0x0047_5569), // Slate
    Colour(0x0094_A3B8), // Light Gray
    Colour(0x00F8_FAFC), // White
    Colour(0x00EF_4444), // Bright Red
    Colour(0x0099_1B1B), // Dark Crimson
    Colour(0x00F9_7316), // Orange
    Colour(0x00F5_9E0B), // Amber
    Colour(0x00EA_580C), // Deep Orange
    Colour(0x0010_B981), // Emerald
    Colour(0x0006_5F46), // Dark Green
    Colour(0x0006_B6D4), // Cyan
    Colour(0x003B_82F6), // Royal Blue
    Colour(0x001E_3A8A), // Navy
    Colour(0x00A8_55F7), // Purple
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum PaintTool {
    Pen,
    Eraser,
    Line,
    Rectangle,
    FilledRect,
    Circle,
}

fn isqrt(val: usize) -> usize {
    let mut x0 = val / 2;
    if x0 == 0 {
        return val;
    }
    let mut x1 = (x0 + val / x0) / 2;
    while x1 < x0 {
        x0 = x1;
        x1 = (x0 + val / x0) / 2;
    }
    x0
}

struct PaintApp {
    files: Option<Handle>,
    canvas_pixels: Vec<Colour>,
    tool: PaintTool,
    color_idx: usize,
    brush_size: u32,
    cursor_x: usize,
    cursor_y: usize,
    drag_start: Option<(usize, usize)>,
    status_msg: Option<String>,
    msg_ticks: u32,
}

impl PaintApp {
    pub fn new(files: Option<Handle>) -> Self {
        let default_canvas = vec![Colour(0x00F8_FAFC); CANVAS_W * CANVAS_H];
        Self {
            files,
            canvas_pixels: default_canvas,
            tool: PaintTool::Pen,
            color_idx: 0, // Black
            brush_size: 2,
            cursor_x: CANVAS_W / 2,
            cursor_y: CANVAS_H / 2,
            drag_start: None,
            status_msg: None,
            msg_ticks: 0,
        }
    }

    fn draw_dot(&mut self, cx: usize, cy: usize, col: Colour, radius: u32) {
        let r = radius as isize;
        for dy in -r..=r {
            for dx in -r..=r {
                if dx * dx + dy * dy <= r * r {
                    let px = cx as isize + dx;
                    let py = cy as isize + dy;
                    if px >= 0 && px < CANVAS_W as isize && py >= 0 && py < CANVAS_H as isize {
                        let idx = (py as usize) * CANVAS_W + (px as usize);
                        self.canvas_pixels[idx] = col;
                    }
                }
            }
        }
    }

    fn apply_current_tool(&mut self) {
        let col = if self.tool == PaintTool::Eraser {
            Colour(0x00F8_FAFC) // White eraser
        } else {
            PALETTE[self.color_idx]
        };

        match self.tool {
            PaintTool::Pen | PaintTool::Eraser => {
                self.draw_dot(self.cursor_x, self.cursor_y, col, self.brush_size);
            }
            PaintTool::Line => {
                if let Some((sx, sy)) = self.drag_start.take() {
                    self.render_line(sx, sy, self.cursor_x, self.cursor_y, col);
                    self.status_msg = Some(String::from("Line drawn"));
                    self.msg_ticks = 40;
                } else {
                    self.drag_start = Some((self.cursor_x, self.cursor_y));
                    self.status_msg = Some(String::from(
                        "Line anchor set: move cursor and press [Enter] again",
                    ));
                    self.msg_ticks = 60;
                }
            }
            PaintTool::Rectangle => {
                if let Some((sx, sy)) = self.drag_start.take() {
                    let (min_x, max_x) = (sx.min(self.cursor_x), sx.max(self.cursor_x));
                    let (min_y, max_y) = (sy.min(self.cursor_y), sy.max(self.cursor_y));
                    for x in min_x..=max_x {
                        self.draw_dot(x, min_y, col, 1);
                        self.draw_dot(x, max_y, col, 1);
                    }
                    for y in min_y..=max_y {
                        self.draw_dot(min_x, y, col, 1);
                        self.draw_dot(max_x, y, col, 1);
                    }
                    self.status_msg = Some(String::from("Rectangle drawn"));
                    self.msg_ticks = 40;
                } else {
                    self.drag_start = Some((self.cursor_x, self.cursor_y));
                    self.status_msg = Some(String::from(
                        "Corner set: move cursor and press [Enter] again",
                    ));
                    self.msg_ticks = 60;
                }
            }
            PaintTool::FilledRect => {
                if let Some((sx, sy)) = self.drag_start.take() {
                    let (min_x, max_x) = (sx.min(self.cursor_x), sx.max(self.cursor_x));
                    let (min_y, max_y) = (sy.min(self.cursor_y), sy.max(self.cursor_y));
                    for y in min_y..=max_y {
                        for x in min_x..=max_x {
                            let idx = y * CANVAS_W + x;
                            self.canvas_pixels[idx] = col;
                        }
                    }
                    self.status_msg = Some(String::from("Filled rectangle drawn"));
                    self.msg_ticks = 40;
                } else {
                    self.drag_start = Some((self.cursor_x, self.cursor_y));
                    self.status_msg = Some(String::from(
                        "Corner set: move cursor and press [Enter] again",
                    ));
                    self.msg_ticks = 60;
                }
            }
            PaintTool::Circle => {
                if let Some((sx, sy)) = self.drag_start.take() {
                    let dx = (self.cursor_x as isize) - (sx as isize);
                    let dy = (self.cursor_y as isize) - (sy as isize);
                    let radius = isqrt((dx * dx + dy * dy) as usize);
                    self.render_circle(sx, sy, radius, col);
                    self.status_msg = Some(String::from("Circle drawn"));
                    self.msg_ticks = 40;
                } else {
                    self.drag_start = Some((self.cursor_x, self.cursor_y));
                    self.status_msg = Some(String::from(
                        "Center set: move cursor and press [Enter] again",
                    ));
                    self.msg_ticks = 60;
                }
            }
        }
    }

    fn render_line(&mut self, mut x0: usize, mut y0: usize, x1: usize, y1: usize, col: Colour) {
        let dx = (x1 as isize - x0 as isize).abs();
        let sx = if x0 < x1 { 1isize } else { -1isize };
        let dy = -(y1 as isize - y0 as isize).abs();
        let sy = if y0 < y1 { 1isize } else { -1isize };
        let mut err = dx + dy;

        loop {
            self.draw_dot(x0, y0, col, self.brush_size);
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 = (x0 as isize + sx) as usize;
            }
            if e2 <= dx {
                err += dx;
                y0 = (y0 as isize + sy) as usize;
            }
        }
    }

    fn render_circle(&mut self, cx: usize, cy: usize, radius: usize, col: Colour) {
        let mut x = radius as isize;
        let mut y = 0isize;
        let mut err = 0isize;

        while x >= y {
            let pts = [
                (cx as isize + x, cy as isize + y),
                (cx as isize + y, cy as isize + x),
                (cx as isize - y, cy as isize + x),
                (cx as isize - x, cy as isize + y),
                (cx as isize - x, cy as isize - y),
                (cx as isize - y, cy as isize - x),
                (cx as isize + y, cy as isize - x),
                (cx as isize + x, cy as isize - y),
            ];
            for (px, py) in pts {
                if px >= 0 && px < CANVAS_W as isize && py >= 0 && py < CANVAS_H as isize {
                    self.draw_dot(px as usize, py as usize, col, self.brush_size);
                }
            }
            y += 1;
            err += 1 + 2 * y;
            if 2 * (err - x) + 1 > 0 {
                x -= 1;
                err += 1 - 2 * x;
            }
        }
    }

    fn save_bmp(&mut self) {
        let Some(files) = self.files else {
            self.status_msg = Some(String::from("Error: No filesystem handle lent"));
            self.msg_ticks = 60;
            return;
        };

        // Construct 24-bit BMP image
        let row_bytes = (CANVAS_W * 3 + 3) & !3; // 4-byte aligned
        let image_size = row_bytes * CANVAS_H;
        let file_size = 54 + image_size;

        let mut bmp = Vec::with_capacity(file_size);

        // 14-byte File Header
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&(file_size as u32).to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes()); // Reserved
        bmp.extend_from_slice(&54u32.to_le_bytes()); // Offset

        // 40-byte DIB Header (BITMAPINFOHEADER)
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&(CANVAS_W as i32).to_le_bytes());
        bmp.extend_from_slice(&(CANVAS_H as i32).to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes()); // Planes
        bmp.extend_from_slice(&24u16.to_le_bytes()); // Bits per pixel
        bmp.extend_from_slice(&0u32.to_le_bytes()); // Compression
        bmp.extend_from_slice(&(image_size as u32).to_le_bytes());
        bmp.extend_from_slice(&2835u32.to_le_bytes()); // X ppm (~72 DPI)
        bmp.extend_from_slice(&2835u32.to_le_bytes()); // Y ppm
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());

        // Pixel Array (Bottom-up BGR)
        for y in (0..CANVAS_H).rev() {
            for x in 0..CANVAS_W {
                let col = self.canvas_pixels[y * CANVAS_W + x];
                let b = (col.0 & 0xFF) as u8;
                let g = ((col.0 >> 8) & 0xFF) as u8;
                let r = ((col.0 >> 16) & 0xFF) as u8;
                bmp.push(b);
                bmp.push(g);
                bmp.push(r);
            }
            // Row padding
            let pad_len = row_bytes - CANVAS_W * 3;
            bmp.resize(bmp.len() + pad_len, 0);
        }

        let filename = "PICTURES/PAINT.BMP";
        let handle = match nexus_user::open(files, filename) {
            Ok(h) => h,
            Err(_) => match nexus_user::create(files, filename, nexus_user::Kind::File) {
                Ok(h) => h,
                Err(_) => {
                    self.status_msg =
                        Some(String::from("Error: Could not create PICTURES/PAINT.BMP"));
                    self.msg_ticks = 60;
                    return;
                }
            },
        };

        if nexus_user::write(handle, &bmp).is_ok() {
            nexus_user::close(handle).ok();
            self.status_msg = Some(format!(
                "SUCCESS: Saved {} bytes to {}",
                bmp.len(),
                filename
            ));
            self.msg_ticks = 80;
        } else {
            nexus_user::close(handle).ok();
            self.status_msg = Some(String::from("Error: Failed writing BMP data to file"));
            self.msg_ticks = 60;
        }
    }
}

impl Default for PaintApp {
    fn default() -> Self {
        Self::new(None)
    }
}

impl App for PaintApp {
    fn draw(&mut self, canvas: &mut Canvas) {
        let w = canvas.width();
        let h = canvas.height();

        // Background
        canvas.fill(Rect::new(0, 0, w, h), COL_BG);

        // Header
        let header_rect = Rect::new(0, 0, w, 44);
        canvas.fill(header_rect, COL_HEADER);
        canvas.outline(header_rect, 1, COL_PANEL_BORDER);

        canvas.text(16, 12, "NEXUS PAINT & PIXEL ART STUDIO", COL_TEXT_WHITE);

        let tool_name = match self.tool {
            PaintTool::Pen => "Brush/Pen [P]",
            PaintTool::Eraser => "Eraser [E]",
            PaintTool::Line => "Line Tool [L]",
            PaintTool::Rectangle => "Rect Tool [R]",
            PaintTool::FilledRect => "Filled Rect [F]",
            PaintTool::Circle => "Circle Tool [C]",
        };
        let tool_str = format!(
            "Tool: {} • Size: {}px • (X:{}, Y:{})",
            tool_name, self.brush_size, self.cursor_x, self.cursor_y
        );
        canvas.text(w.saturating_sub(440), 12, &tool_str, COL_CYAN);

        // Toolbar Panel (Left)
        let tool_panel_w = 140;
        let tool_panel_h = h.saturating_sub(74);
        let tool_panel_rect = Rect::new(16, 52, tool_panel_w, tool_panel_h);
        canvas.fill_rounded(tool_panel_rect, 6, COL_PANEL);
        canvas.outline(tool_panel_rect, 1, COL_PANEL_BORDER);

        canvas.text(26, 62, "COLOR PALETTE", COL_GOLD);

        // 16-color swatches (2 columns of 8)
        for (i, &col) in PALETTE.iter().enumerate() {
            let col_x = 26 + ((i % 2) as u32 * 46);
            let col_y = 86 + ((i / 2) as u32 * 26);
            let swatch_rect = Rect::new(col_x, col_y, 40, 20);

            canvas.fill_rounded(swatch_rect, 3, col);
            let border_col = if i == self.color_idx {
                COL_CYAN
            } else {
                Colour(0x0047_5569)
            };
            canvas.outline(
                swatch_rect,
                if i == self.color_idx { 2 } else { 1 },
                border_col,
            );
        }

        // Tools shortcuts list
        let info_y = 310;
        canvas.text(26, info_y, "TOOL SHORTCUTS:", COL_TEXT_WHITE);
        canvas.text(26, info_y + 22, "[P] Pen / Brush", COL_TEXT_MUTED);
        canvas.text(26, info_y + 40, "[E] Eraser", COL_TEXT_MUTED);
        canvas.text(26, info_y + 58, "[L] Straight Line", COL_TEXT_MUTED);
        canvas.text(26, info_y + 76, "[R] Rectangle", COL_TEXT_MUTED);
        canvas.text(26, info_y + 94, "[F] Filled Rect", COL_TEXT_MUTED);
        canvas.text(26, info_y + 112, "[C] Circle", COL_TEXT_MUTED);
        canvas.text(26, info_y + 130, "[S] Save BMP", COL_GREEN);
        canvas.text(26, info_y + 148, "[X] Clear", COL_GOLD);

        // Drawing Canvas Viewport (Right)
        let canvas_view_x = 172;
        let canvas_view_y = 52;
        let canvas_view_w = w.saturating_sub(canvas_view_x + 16);
        let canvas_view_h = h.saturating_sub(canvas_view_y + 36);

        let frame_rect = Rect::new(canvas_view_x, canvas_view_y, canvas_view_w, canvas_view_h);
        canvas.fill_rounded(frame_rect, 6, COL_PANEL);
        canvas.outline(frame_rect, 1, COL_PANEL_BORDER);

        // Draw virtual canvas pixels (scale factor 2x or 1x)
        let draw_x = canvas_view_x + 12;
        let draw_y = canvas_view_y + 12;

        let scale = if canvas_view_w >= (CANVAS_W as u32 * 2 + 24)
            && canvas_view_h >= (CANVAS_H as u32 * 2 + 24)
        {
            2
        } else {
            1
        };

        for y in 0..CANVAS_H {
            for x in 0..CANVAS_W {
                let col = self.canvas_pixels[y * CANVAS_W + x];
                let px = draw_x + (x as u32 * scale);
                let py = draw_y + (y as u32 * scale);
                if scale == 1 {
                    canvas.set(px, py, col);
                } else {
                    canvas.fill(Rect::new(px, py, scale, scale), col);
                }
            }
        }

        // Draw Brush Cursor crosshair / ring
        let cur_px = draw_x + (self.cursor_x as u32 * scale);
        let cur_py = draw_y + (self.cursor_y as u32 * scale);
        canvas.fill(
            Rect::new(cur_px.saturating_sub(4), cur_py, 9, 1),
            Colour(0x00FF_0000),
        );
        canvas.fill(
            Rect::new(cur_px, cur_py.saturating_sub(4), 1, 9),
            Colour(0x00FF_0000),
        );

        // Footer Status
        let footer_y = h.saturating_sub(26);
        canvas.fill(Rect::new(0, footer_y, w, 26), Colour(0x000A_0F1D));
        canvas.outline(Rect::new(0, footer_y, w, 26), 1, COL_PANEL_BORDER);

        let status_msg = if let Some(ref msg) = self.status_msg {
            msg.as_str()
        } else {
            "Use [Arrows] to move cursor • [Enter/Space] to Draw • [0-9] Palette • [S] Export to PICTURES/PAINT.BMP"
        };
        let status_col = if self.status_msg.is_some() {
            COL_GOLD
        } else {
            COL_TEXT_MUTED
        };
        canvas.text(16, footer_y + 6, status_msg, status_col);
    }

    fn key(&mut self, key: Key) -> bool {
        match key {
            Key::Move(Movement::Up) => {
                self.cursor_y = self.cursor_y.saturating_sub(2);
                true
            }
            Key::Move(Movement::Down) => {
                self.cursor_y = (self.cursor_y + 2).min(CANVAS_H - 1);
                true
            }
            Key::Move(Movement::Left) => {
                self.cursor_x = self.cursor_x.saturating_sub(2);
                true
            }
            Key::Move(Movement::Right) => {
                self.cursor_x = (self.cursor_x + 2).min(CANVAS_W - 1);
                true
            }
            Key::Enter | Key::Character(' ') => {
                self.apply_current_tool();
                true
            }
            Key::Character('p') | Key::Character('P') => {
                self.tool = PaintTool::Pen;
                self.drag_start = None;
                self.status_msg = Some(String::from("Selected: Brush / Pen"));
                self.msg_ticks = 40;
                true
            }
            Key::Character('e') | Key::Character('E') => {
                self.tool = PaintTool::Eraser;
                self.drag_start = None;
                self.status_msg = Some(String::from("Selected: Eraser"));
                self.msg_ticks = 40;
                true
            }
            Key::Character('l') | Key::Character('L') => {
                self.tool = PaintTool::Line;
                self.drag_start = None;
                self.status_msg = Some(String::from("Selected: Line Tool"));
                self.msg_ticks = 40;
                true
            }
            Key::Character('r') | Key::Character('R') => {
                self.tool = PaintTool::Rectangle;
                self.drag_start = None;
                self.status_msg = Some(String::from("Selected: Rectangle Tool"));
                self.msg_ticks = 40;
                true
            }
            Key::Character('f') | Key::Character('F') => {
                self.tool = PaintTool::FilledRect;
                self.drag_start = None;
                self.status_msg = Some(String::from("Selected: Filled Rectangle"));
                self.msg_ticks = 40;
                true
            }
            Key::Character('c') | Key::Character('C') => {
                self.tool = PaintTool::Circle;
                self.drag_start = None;
                self.status_msg = Some(String::from("Selected: Circle Tool"));
                self.msg_ticks = 40;
                true
            }
            Key::Character('x') | Key::Character('X') => {
                self.canvas_pixels.fill(Colour(0x00F8_FAFC));
                self.drag_start = None;
                self.status_msg = Some(String::from("Canvas cleared"));
                self.msg_ticks = 40;
                true
            }
            Key::Character('s') | Key::Character('S') => {
                self.save_bmp();
                true
            }
            Key::Character('[') => {
                self.brush_size = self.brush_size.saturating_sub(1).max(1);
                true
            }
            Key::Character(']') => {
                self.brush_size = (self.brush_size + 1).min(8);
                true
            }
            Key::Character(ch @ '0'..='9') => {
                let digit = (ch as usize) - ('0' as usize);
                self.color_idx = digit % PALETTE.len();
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
    nexus_user::log(&format!("paint: PANIC: {info}")).ok();
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
        failed("paint: FAILED: could not allocate heap");
        finish();
    }

    nexus_user::log("paint: Paint & Pixel Art Studio starting").ok();

    let mut lent = [Handle(0); 1];
    match Window::open(COMPOSITOR, SURFACE_AT, &mut lent) {
        Ok((window, carried)) => {
            let files = (carried >= 1).then_some(lent[0]);
            let mut app = PaintApp::new(files);
            nexus_user::log("paint: window opened with compositor, running UI loop").ok();
            window.run(&mut app);
        }
        Err(_) => {
            failed("paint: FAILED: could not connect to compositor");
        }
    }

    finish()
}
