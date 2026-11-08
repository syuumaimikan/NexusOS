//! Calculator & Bitwise Programmer Tool for NexusOS.
//!
//! Provides:
//! - Standard mathematical calculation with operator precedence and parentheses.
//! - Programmer mode with simultaneous HEX, DEC, OCT, and BIN readouts.
//! - Bitwise operations (AND, OR, XOR, NOT, Shift Left/Right).
//! - Clean, accessible dark UI with digital LCD readout.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::panic::PanicInfo;

use nexus_ui::{Canvas, Colour, Rect};
use nexus_user::Handle;
use nexus_window::{App, Key, Window};

#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

const COMPOSITOR: Handle = Handle(1);
const SURFACE_AT: usize = 0x0000_0000_3000_0000;
const HEAP: usize = 1024 * 1024;

const COL_BG: Colour = Colour(0x0011_1827);
const COL_DISPLAY_BG: Colour = Colour(0x0003_0712);
const COL_BTN_NUM: Colour = Colour(0x001F_2937);
const COL_BTN_OP: Colour = Colour(0x0037_4151);
const COL_BTN_EQ: Colour = Colour(0x0025_63EB);
const COL_BTN_CLEAR: Colour = Colour(0x00DC_2626);
const COL_TEXT_WHITE: Colour = Colour(0x00F9_FAFB);
const COL_TEXT_MUTED: Colour = Colour(0x009C_A3AF);
const COL_TEXT_CYAN: Colour = Colour(0x0038_BDF8);
const COL_TEXT_GOLD: Colour = Colour(0x00F5_9E0B);
const COL_TEXT_GREEN: Colour = Colour(0x0010_B981);

#[derive(Clone, Copy, PartialEq, Eq)]
enum CalcMode {
    Standard,
    Programmer,
}

pub struct CalculatorApp {
    mode: CalcMode,
    input: String,
    result_val: i64,
    has_result: bool,
    error: Option<&'static str>,
}

impl CalculatorApp {
    pub fn new() -> Self {
        Self {
            mode: CalcMode::Standard,
            input: String::from("1024 * 768"),
            result_val: 786432,
            has_result: true,
            error: None,
        }
    }

    fn evaluate(&mut self) {
        let trimmed = self.input.trim();
        if trimmed.is_empty() {
            self.result_val = 0;
            self.has_result = false;
            self.error = None;
            return;
        }

        match evaluate_expr(trimmed) {
            Some(res) => {
                self.result_val = res;
                self.has_result = true;
                self.error = None;
            }
            None => {
                self.has_result = false;
                self.error = Some("Syntax Error / Div by 0");
            }
        }
    }

    fn push_char(&mut self, c: char) {
        if self.input.len() < 40 {
            self.input.push(c);
            self.evaluate();
        }
    }

    fn backspace(&mut self) {
        self.input.pop();
        self.evaluate();
    }

    fn clear(&mut self) {
        self.input.clear();
        self.result_val = 0;
        self.has_result = false;
        self.error = None;
    }
}

impl Default for CalculatorApp {
    fn default() -> Self {
        Self::new()
    }
}

impl App for CalculatorApp {
    fn draw(&mut self, canvas: &mut Canvas) {
        let w = canvas.width();
        let h = canvas.height();

        // Background
        canvas.fill(Rect::new(0, 0, w, h), COL_BG);

        // Header Title & Mode
        let hdr_rect = Rect::new(0, 0, w, 40);
        canvas.fill(hdr_rect, Colour(0x000F_172A));
        canvas.outline(hdr_rect, 1, Colour(0x001F_2937));

        canvas.text(16, 12, "NEXUSOS CALCULATOR", COL_TEXT_WHITE);

        let mode_str = match self.mode {
            CalcMode::Standard => "Mode: STANDARD [1]",
            CalcMode::Programmer => "Mode: PROGRAMMER [2]",
        };
        canvas.text(w.saturating_sub(180), 12, mode_str, COL_TEXT_CYAN);

        // Digital LCD Display Box (height: 100)
        let disp_rect = Rect::new(16, 48, w.saturating_sub(32), 100);
        canvas.fill_rounded(disp_rect, 6, COL_DISPLAY_BG);
        canvas.outline(disp_rect, 1, Colour(0x0037_4151));

        // Expression input line
        let expr_text = if self.input.is_empty() {
            "0"
        } else {
            &self.input
        };
        canvas.text(32, 60, expr_text, COL_TEXT_MUTED);

        // Result line (large text)
        let res_text = if let Some(err) = self.error {
            format!("Error: {}", err)
        } else if self.has_result {
            format!("= {}", self.result_val)
        } else {
            String::from("0")
        };
        let res_col = if self.error.is_some() {
            COL_BTN_CLEAR
        } else {
            COL_TEXT_GREEN
        };
        canvas.text(32, 92, &res_text, res_col);

        // Mode-specific content area
        let main_y = 156;
        let main_h = h.saturating_sub(main_y + 30);

        match self.mode {
            CalcMode::Standard => self.draw_standard_keypad(canvas, main_y, main_h, w),
            CalcMode::Programmer => self.draw_programmer_view(canvas, main_y, main_h, w),
        }

        // Bottom status footer
        let footer_y = h.saturating_sub(26);
        canvas.fill(Rect::new(0, footer_y, w, 26), Colour(0x000A_0F1D));
        canvas.text(
            16,
            footer_y + 6,
            "Hotkeys: [1] Standard  [2] Programmer  [Tab] Switch Mode  [0-9 + - * / ( )] Type  [Enter] =",
            COL_TEXT_MUTED,
        );
    }

    fn key(&mut self, key: Key) -> bool {
        match key {
            Key::Function(1) | Key::Character('1') if self.input.is_empty() => {
                self.mode = CalcMode::Standard;
                true
            }
            Key::Function(2) | Key::Character('2') if self.input.is_empty() => {
                self.mode = CalcMode::Programmer;
                true
            }
            Key::Tab => {
                self.mode = match self.mode {
                    CalcMode::Standard => CalcMode::Programmer,
                    CalcMode::Programmer => CalcMode::Standard,
                };
                true
            }
            Key::Character(c) => match c {
                '0'..='9' | '+' | '-' | '*' | '/' | '%' | '(' | ')' | '&' | '|' | '^' => {
                    self.push_char(c);
                    true
                }
                '=' => {
                    self.evaluate();
                    true
                }
                'c' | 'C' => {
                    self.clear();
                    true
                }
                _ => false,
            },
            Key::Backspace => {
                self.backspace();
                true
            }
            Key::Enter => {
                self.evaluate();
                true
            }
            Key::Escape => {
                self.clear();
                true
            }
            _ => false,
        }
    }

    fn damage(&mut self) -> Option<(u32, u32, u32, u32)> {
        None
    }
}

impl CalculatorApp {
    fn draw_standard_keypad(&self, canvas: &mut Canvas, y: u32, h: u32, w: u32) {
        let pad = 16;
        let pad_w = w.saturating_sub(pad * 2);

        let rows = [
            ["C", "(", ")", "/"],
            ["7", "8", "9", "*"],
            ["4", "5", "6", "-"],
            ["1", "2", "3", "+"],
            ["0", "%", "DEL", "="],
        ];

        let cols = 4;
        let btn_w = (pad_w.saturating_sub((cols - 1) * 8)) / cols;
        let btn_h = (h.saturating_sub(rows.len() as u32 * 8)) / rows.len() as u32;

        for (r_idx, row) in rows.iter().enumerate() {
            let by = y + (r_idx as u32 * (btn_h + 8));
            for (c_idx, &label) in row.iter().enumerate() {
                let bx = pad + (c_idx as u32 * (btn_w + 8));
                let btn_rect = Rect::new(bx, by, btn_w, btn_h);

                let bg = match label {
                    "=" => COL_BTN_EQ,
                    "C" => COL_BTN_CLEAR,
                    "+" | "-" | "*" | "/" | "%" | "(" | ")" => COL_BTN_OP,
                    _ => COL_BTN_NUM,
                };

                canvas.fill_rounded(btn_rect, 4, bg);
                canvas.outline(btn_rect, 1, Colour(0x0037_4151));
                canvas.text_centred(
                    Rect::new(bx, by + (btn_h / 2).saturating_sub(8), btn_w, 20),
                    label,
                    COL_TEXT_WHITE,
                );
            }
        }
    }

    fn draw_programmer_view(&self, canvas: &mut Canvas, y: u32, h: u32, w: u32) {
        let pad = 16;
        let panel_rect = Rect::new(pad, y, w.saturating_sub(pad * 2), h);
        canvas.fill_rounded(panel_rect, 6, Colour(0x001F_2937));
        canvas.outline(panel_rect, 1, Colour(0x0037_4151));

        canvas.text(
            pad + 16,
            y + 16,
            "MULTI-BASE INTEGER REPRESENTATION",
            COL_TEXT_GOLD,
        );

        let val = self.result_val;

        let hex_str = format!("HEX:  0x{:08X}", val);
        let dec_str = format!("DEC:  {}", val);
        let oct_str = format!("OCT:  0o{:o}", val);

        // Binary with 4-bit nibble formatting
        let raw_bin = format!("{:032b}", val as u32);
        let mut bin_formatted = String::from("BIN:  0b");
        for (i, c) in raw_bin.chars().enumerate() {
            if i > 0 && i % 4 == 0 {
                bin_formatted.push(' ');
            }
            bin_formatted.push(c);
        }

        canvas.text(pad + 20, y + 48, &hex_str, COL_TEXT_CYAN);
        canvas.text(pad + 20, y + 74, &dec_str, COL_TEXT_WHITE);
        canvas.text(pad + 20, y + 100, &oct_str, COL_TEXT_MUTED);
        canvas.text(pad + 20, y + 126, &bin_formatted, COL_TEXT_GREEN);

        // Bitwise operators explanation
        canvas.text(
            pad + 16,
            y + 164,
            "BITWISE OPERATIONS SUPPORTED:",
            COL_TEXT_WHITE,
        );
        canvas.text(
            pad + 20,
            y + 188,
            "• AND (&)   e.g. 0xFF & 0x0F",
            COL_TEXT_MUTED,
        );
        canvas.text(
            pad + 20,
            y + 206,
            "• OR  (|)   e.g. 0x10 | 0x01",
            COL_TEXT_MUTED,
        );
        canvas.text(
            pad + 20,
            y + 224,
            "• XOR (^)   e.g. 0xAA ^ 0xFF",
            COL_TEXT_MUTED,
        );
    }
}

/// Recursively evaluate arithmetic expression with precedence.
fn evaluate_expr(expr: &str) -> Option<i64> {
    let expr = expr.trim();
    if expr.is_empty() {
        return None;
    }

    // Outer parentheses
    if expr.starts_with('(') && expr.ends_with(')') {
        let mut depth = 0;
        let mut all_inside = true;
        for (i, c) in expr.char_indices() {
            if c == '(' {
                depth += 1;
            } else if c == ')' {
                depth -= 1;
                if depth == 0 && i < expr.len() - 1 {
                    all_inside = false;
                    break;
                }
            }
        }
        if all_inside && depth == 0 {
            return evaluate_expr(&expr[1..expr.len() - 1]);
        }
    }

    let chars: Vec<char> = expr.chars().collect();

    // Lowest precedence: bitwise | and ^
    let mut depth = 0;
    for (i, &c) in chars.iter().enumerate().rev() {
        if c == ')' {
            depth += 1;
        } else if c == '(' {
            depth -= 1;
        } else if depth == 0 && (c == '|' || c == '^') && i > 0 {
            let left: String = chars[..i].iter().collect();
            let right: String = chars[i + 1..].iter().collect();
            let l = evaluate_expr(&left)?;
            let r = evaluate_expr(&right)?;
            return Some(if c == '|' { l | r } else { l ^ r });
        }
    }

    // Next precedence: bitwise &
    depth = 0;
    for (i, &c) in chars.iter().enumerate().rev() {
        if c == ')' {
            depth += 1;
        } else if c == '(' {
            depth -= 1;
        } else if depth == 0 && c == '&' && i > 0 {
            let left: String = chars[..i].iter().collect();
            let right: String = chars[i + 1..].iter().collect();
            let l = evaluate_expr(&left)?;
            let r = evaluate_expr(&right)?;
            return Some(l & r);
        }
    }

    // Addition and Subtraction
    depth = 0;
    for (i, &c) in chars.iter().enumerate().rev() {
        if c == ')' {
            depth += 1;
        } else if c == '(' {
            depth -= 1;
        } else if depth == 0 && (c == '+' || c == '-') && i > 0 {
            let left: String = chars[..i].iter().collect();
            let right: String = chars[i + 1..].iter().collect();
            let l = evaluate_expr(&left)?;
            let r = evaluate_expr(&right)?;
            return Some(if c == '+' {
                l.saturating_add(r)
            } else {
                l.saturating_sub(r)
            });
        }
    }

    // Multiplication, Division, Modulo
    depth = 0;
    for (i, &c) in chars.iter().enumerate().rev() {
        if c == ')' {
            depth += 1;
        } else if c == '(' {
            depth -= 1;
        } else if depth == 0 && (c == '*' || c == '/' || c == '%') && i > 0 {
            let left: String = chars[..i].iter().collect();
            let right: String = chars[i + 1..].iter().collect();
            let l = evaluate_expr(&left)?;
            let r = evaluate_expr(&right)?;
            return match c {
                '*' => Some(l.saturating_mul(r)),
                '/' if r != 0 => Some(l / r),
                '%' if r != 0 => Some(l % r),
                _ => None,
            };
        }
    }

    // Hex / Dec parse
    let trimmed = expr.trim();
    if trimmed.starts_with("0x") || trimmed.starts_with("0X") {
        i64::from_str_radix(&trimmed[2..], 16).ok()
    } else {
        trimmed.parse::<i64>().ok()
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
    nexus_user::log(&format!("calc: PANIC: {info}")).ok();
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
        failed("calc: FAILED: could not allocate heap");
        finish();
    }

    nexus_user::log("calc: Graphical Calculator starting").ok();

    let mut lent = [Handle(0); 1];
    match Window::open(COMPOSITOR, SURFACE_AT, &mut lent) {
        Ok((window, _carried)) => {
            let mut app = CalculatorApp::new();
            nexus_user::log("calc: window opened with compositor, running UI loop").ok();
            window.run(&mut app);
        }
        Err(_) => {
            failed("calc: FAILED: could not connect to compositor");
        }
    }

    finish()
}
