//! Graphical Synthesizer & Music Studio Application for NexusOS.
//!
//! Provides a virtual piano keyboard, melody sequencer with classical and chiptune tracks,
//! 16-band animated audio spectrum visualizer, and AC'97 sound output.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
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
const COL_WHITE_KEY: Colour = Colour(0x00F1_F5F9);
const COL_WHITE_KEY_BORDER: Colour = Colour(0x0064_748B);
const COL_BLACK_KEY: Colour = Colour(0x000F_172A);
const COL_ACTIVE_KEY: Colour = Colour(0x0038_BDF8);
const COL_TEXT_WHITE: Colour = Colour(0x00F8_FAFC);
const COL_TEXT_MUTED: Colour = Colour(0x0094_A3B8);
const COL_CYAN: Colour = Colour(0x0038_BDF8);
const COL_GREEN: Colour = Colour(0x0010_B981);
const COL_GOLD: Colour = Colour(0x00F5_9E0B);
const COL_PURPLE: Colour = Colour(0x00A8_55F7);

#[derive(Clone, Copy)]
struct NoteTone {
    freq: u32,
    duration_ms: u32,
    name: &'static str,
}

const T_C3: u32 = 131;
const T_D3: u32 = 147;
const T_E3: u32 = 165;
const T_F3: u32 = 175;
const T_G3: u32 = 196;
const T_A3: u32 = 220;
const T_B3: u32 = 247;
const T_C4: u32 = 262;
const T_CS4: u32 = 277;
const T_D4: u32 = 294;
const T_DS4: u32 = 311;
const T_E4: u32 = 330;
const T_F4: u32 = 349;
const T_FS4: u32 = 370;
const T_G4: u32 = 392;
const T_GS4: u32 = 415;
const T_A4: u32 = 440;
const T_AS4: u32 = 466;
const T_B4: u32 = 494;
const T_C5: u32 = 523;

struct Song {
    title: &'static str,
    composer: &'static str,
    notes: &'static [NoteTone],
}

// 1. Beethoven - Ode to Joy
static ODE_TO_JOY: &[NoteTone] = &[
    NoteTone {
        freq: T_E4,
        duration_ms: 220,
        name: "E4",
    },
    NoteTone {
        freq: T_E4,
        duration_ms: 220,
        name: "E4",
    },
    NoteTone {
        freq: T_F4,
        duration_ms: 220,
        name: "F4",
    },
    NoteTone {
        freq: T_G4,
        duration_ms: 220,
        name: "G4",
    },
    NoteTone {
        freq: T_G4,
        duration_ms: 220,
        name: "G4",
    },
    NoteTone {
        freq: T_F4,
        duration_ms: 220,
        name: "F4",
    },
    NoteTone {
        freq: T_E4,
        duration_ms: 220,
        name: "E4",
    },
    NoteTone {
        freq: T_D4,
        duration_ms: 220,
        name: "D4",
    },
    NoteTone {
        freq: T_C4,
        duration_ms: 220,
        name: "C4",
    },
    NoteTone {
        freq: T_C4,
        duration_ms: 220,
        name: "C4",
    },
    NoteTone {
        freq: T_D4,
        duration_ms: 220,
        name: "D4",
    },
    NoteTone {
        freq: T_E4,
        duration_ms: 320,
        name: "E4",
    },
    NoteTone {
        freq: T_D4,
        duration_ms: 180,
        name: "D4",
    },
    NoteTone {
        freq: T_D4,
        duration_ms: 360,
        name: "D4",
    },
];

// 2. Bach - Minuet in G
static BACH_MINUET: &[NoteTone] = &[
    NoteTone {
        freq: T_D4,
        duration_ms: 240,
        name: "D4",
    },
    NoteTone {
        freq: T_G3,
        duration_ms: 120,
        name: "G3",
    },
    NoteTone {
        freq: T_A3,
        duration_ms: 120,
        name: "A3",
    },
    NoteTone {
        freq: T_B3,
        duration_ms: 120,
        name: "B3",
    },
    NoteTone {
        freq: T_C4,
        duration_ms: 120,
        name: "C4",
    },
    NoteTone {
        freq: T_D4,
        duration_ms: 240,
        name: "D4",
    },
    NoteTone {
        freq: T_G3,
        duration_ms: 240,
        name: "G3",
    },
    NoteTone {
        freq: T_G3,
        duration_ms: 240,
        name: "G3",
    },
    NoteTone {
        freq: T_E4,
        duration_ms: 240,
        name: "E4",
    },
    NoteTone {
        freq: T_C4,
        duration_ms: 120,
        name: "C4",
    },
    NoteTone {
        freq: T_D4,
        duration_ms: 120,
        name: "D4",
    },
    NoteTone {
        freq: T_E4,
        duration_ms: 120,
        name: "E4",
    },
    NoteTone {
        freq: T_FS4,
        duration_ms: 120,
        name: "F#4",
    },
    NoteTone {
        freq: T_G4,
        duration_ms: 240,
        name: "G4",
    },
];

// 3. Spacewar Retro 8-bit Victory Theme
static RETRO_THEME: &[NoteTone] = &[
    NoteTone {
        freq: T_C4,
        duration_ms: 100,
        name: "C4",
    },
    NoteTone {
        freq: T_E4,
        duration_ms: 100,
        name: "E4",
    },
    NoteTone {
        freq: T_G4,
        duration_ms: 100,
        name: "G4",
    },
    NoteTone {
        freq: T_C5,
        duration_ms: 200,
        name: "C5",
    },
    NoteTone {
        freq: T_G4,
        duration_ms: 100,
        name: "G4",
    },
    NoteTone {
        freq: T_C5,
        duration_ms: 300,
        name: "C5",
    },
    NoteTone {
        freq: T_A4,
        duration_ms: 120,
        name: "A4",
    },
    NoteTone {
        freq: T_B4,
        duration_ms: 120,
        name: "B4",
    },
    NoteTone {
        freq: T_C5,
        duration_ms: 350,
        name: "C5",
    },
];

// 4. Pachelbel - Canon in D
static CANON_IN_D: &[NoteTone] = &[
    NoteTone {
        freq: T_FS4,
        duration_ms: 250,
        name: "F#4",
    },
    NoteTone {
        freq: T_E4,
        duration_ms: 250,
        name: "E4",
    },
    NoteTone {
        freq: T_D4,
        duration_ms: 250,
        name: "D4",
    },
    NoteTone {
        freq: T_CS4,
        duration_ms: 250,
        name: "C#4",
    },
    NoteTone {
        freq: T_B3,
        duration_ms: 250,
        name: "B3",
    },
    NoteTone {
        freq: T_A3,
        duration_ms: 250,
        name: "A3",
    },
    NoteTone {
        freq: T_B3,
        duration_ms: 250,
        name: "B3",
    },
    NoteTone {
        freq: T_CS4,
        duration_ms: 250,
        name: "C#4",
    },
];

const SONGS: &[Song] = &[
    Song {
        title: "Ode to Joy",
        composer: "L. v. Beethoven",
        notes: ODE_TO_JOY,
    },
    Song {
        title: "Minuet in G",
        composer: "J. S. Bach",
        notes: BACH_MINUET,
    },
    Song {
        title: "Spacewar 8-Bit Fanfare",
        composer: "NexusOS Retro",
        notes: RETRO_THEME,
    },
    Song {
        title: "Canon in D",
        composer: "J. Pachelbel",
        notes: CANON_IN_D,
    },
];

struct MusicStudioApp {
    sound: Option<Handle>,
    selected_song: usize,
    is_playing: bool,
    note_index: usize,
    note_timer: u32,
    active_freq: u32,
    active_note_name: &'static str,
    viz_bars: [u8; 16],
    tick_count: u64,
}

impl MusicStudioApp {
    pub fn new(sound: Option<Handle>) -> Self {
        Self {
            sound,
            selected_song: 0,
            is_playing: false,
            note_index: 0,
            note_timer: 0,
            active_freq: 0,
            active_note_name: "Ready",
            viz_bars: [12, 24, 45, 60, 80, 95, 75, 65, 55, 40, 30, 20, 15, 10, 8, 5],
            tick_count: 0,
        }
    }

    fn play_tone(&mut self, freq: u32, duration_ms: u32, name: &'static str) {
        self.active_freq = freq;
        self.active_note_name = name;

        // Animate visualizer peaks based on frequency
        let center_band = ((freq.saturating_sub(100)) / 30).min(15) as usize;
        self.viz_bars[center_band] = 100;
        if center_band > 0 {
            self.viz_bars[center_band - 1] = 85;
        }
        if center_band + 1 < 16 {
            self.viz_bars[center_band + 1] = 85;
        }

        let Some(sound) = self.sound else {
            return;
        };

        let mut msg = [0u8; 12];
        msg[0..4].copy_from_slice(b"tone");
        msg[4..8].copy_from_slice(&freq.to_le_bytes());
        msg[8..12].copy_from_slice(&duration_ms.to_le_bytes());
        nexus_user::send(sound, &msg, &[]).ok();
    }

    fn hush(&mut self) {
        self.is_playing = false;
        self.active_freq = 0;
        self.active_note_name = "Stopped";

        let Some(sound) = self.sound else {
            return;
        };

        nexus_user::send(sound, b"hush", &[]).ok();
    }

    fn start_song(&mut self, song_idx: usize) {
        self.selected_song = song_idx % SONGS.len();
        self.note_index = 0;
        self.note_timer = 0;
        self.is_playing = true;
    }
}

impl Default for MusicStudioApp {
    fn default() -> Self {
        Self::new(None)
    }
}

impl App for MusicStudioApp {
    fn draw(&mut self, canvas: &mut Canvas) {
        let w = canvas.width();
        let h = canvas.height();

        // 1. Background & Header
        canvas.fill(Rect::new(0, 0, w, h), COL_BG);

        let header_rect = Rect::new(0, 0, w, 48);
        canvas.fill(header_rect, COL_HEADER);
        canvas.outline(header_rect, 1, COL_PANEL_BORDER);

        canvas.text(16, 14, "NEXUS SYNTHESIZER & MUSIC STUDIO", COL_TEXT_WHITE);

        let status_str = if self.is_playing {
            format!(
                "PLAYING: {} • {}",
                SONGS[self.selected_song].title, self.active_note_name
            )
        } else {
            format!(
                "STATUS: {} (Freq: {} Hz)",
                self.active_note_name, self.active_freq
            )
        };
        let status_col = if self.is_playing { COL_GREEN } else { COL_CYAN };
        canvas.text(w.saturating_sub(340), 14, &status_str, status_col);

        // 2. Audio Spectrum Visualizer Panel (Top Left)
        let viz_x = 16;
        let viz_y = 60;
        let viz_w = (w / 2).saturating_sub(24);
        let viz_h = 130;

        let viz_rect = Rect::new(viz_x, viz_y, viz_w, viz_h);
        canvas.fill_rounded(viz_rect, 6, COL_PANEL);
        canvas.outline(viz_rect, 1, COL_PANEL_BORDER);

        canvas.text(
            viz_x + 14,
            viz_y + 12,
            "REAL-TIME SPECTRUM VISUALIZER (AC'97)",
            COL_PURPLE,
        );

        // Render 16 Equalizer Bars
        let bar_area_w = viz_w.saturating_sub(28);
        let bar_step = bar_area_w / 16;
        let bar_w = (bar_step.saturating_sub(4)).max(2);
        let max_bar_h = viz_h.saturating_sub(45);

        for i in 0..16 {
            let val = self.viz_bars[i] as u32;
            let bh = (val * max_bar_h) / 100;
            let bx = viz_x + 14 + (i as u32 * bar_step);
            let by = viz_y + viz_h - 12 - bh;

            let col = if val > 80 {
                COL_GOLD
            } else if val > 50 {
                COL_CYAN
            } else {
                Colour(0x0025_63EB)
            };

            canvas.fill(Rect::new(bx, by, bar_w, bh), col);
            // Peak cap
            canvas.fill(
                Rect::new(bx, by.saturating_sub(3), bar_w, 2),
                COL_TEXT_WHITE,
            );
        }

        // 3. Song Playlist & Info Panel (Top Right)
        let list_x = viz_x + viz_w + 16;
        let list_w = w.saturating_sub(list_x + 16);
        let list_h = 130;

        let list_rect = Rect::new(list_x, viz_y, list_w, list_h);
        canvas.fill_rounded(list_rect, 6, COL_PANEL);
        canvas.outline(list_rect, 1, COL_PANEL_BORDER);

        canvas.text(
            list_x + 14,
            viz_y + 12,
            "BUILT-IN TRACKS ([1-4] to select)",
            COL_GOLD,
        );

        for (i, song) in SONGS.iter().enumerate() {
            let item_y = viz_y + 34 + (i as u32 * 22);
            let is_sel = i == self.selected_song;

            if is_sel {
                let sel_rect = Rect::new(list_x + 8, item_y - 2, list_w.saturating_sub(16), 20);
                canvas.fill_rounded(sel_rect, 3, Colour(0x001E_3A8A));
                canvas.outline(sel_rect, 1, COL_CYAN);
            }

            let prefix = if is_sel && self.is_playing {
                "▶ "
            } else {
                "  "
            };
            let title_str = format!("{}[{}] {} - {}", prefix, i + 1, song.title, song.composer);
            let col = if is_sel {
                COL_TEXT_WHITE
            } else {
                COL_TEXT_MUTED
            };
            canvas.text(list_x + 12, item_y + 2, &title_str, col);
        }

        // 4. Virtual Piano Keyboard (Bottom)
        let piano_y = 205;
        let piano_h = h.saturating_sub(piano_y + 36);
        let piano_w = w.saturating_sub(32);

        let piano_rect = Rect::new(16, piano_y, piano_w, piano_h);
        canvas.fill_rounded(piano_rect, 8, COL_PANEL);
        canvas.outline(piano_rect, 1, COL_PANEL_BORDER);

        canvas.text(
            32,
            piano_y + 12,
            "VIRTUAL PIANO KEYBOARD (Z X C V B N M , / Play Keys)",
            COL_TEXT_WHITE,
        );

        // Draw 14 White Keys
        let key_area_y = piano_y + 36;
        let key_area_h = piano_h.saturating_sub(48);
        let white_key_count = 14;
        let white_key_w = piano_w.saturating_sub(32) / (white_key_count as u32);

        let white_notes = [
            ("C3", T_C3),
            ("D3", T_D3),
            ("E3", T_E3),
            ("F3", T_F3),
            ("G3", T_G3),
            ("A3", T_A3),
            ("B3", T_B3),
            ("C4", T_C4),
            ("D4", T_D4),
            ("E4", T_E4),
            ("F4", T_F4),
            ("G4", T_G4),
            ("A4", T_A4),
            ("B4", T_B4),
        ];

        for (i, (name, freq)) in white_notes.iter().enumerate() {
            let kx = 32 + (i as u32 * white_key_w);
            let k_rect = Rect::new(kx, key_area_y, white_key_w.saturating_sub(2), key_area_h);

            let is_active = self.active_freq == *freq;
            let bg_col = if is_active {
                COL_ACTIVE_KEY
            } else {
                COL_WHITE_KEY
            };

            canvas.fill_rounded(k_rect, 4, bg_col);
            canvas.outline(k_rect, 1, COL_WHITE_KEY_BORDER);

            let label_col = if is_active {
                COL_TEXT_WHITE
            } else {
                Colour(0x000F_172A)
            };
            canvas.text(kx + 6, key_area_y + key_area_h - 22, name, label_col);
        }

        // Draw Black Keys overlay
        let black_key_w = (white_key_w * 6) / 10;
        let black_key_h = (key_area_h * 6) / 10;

        let black_offsets = [
            (0, "C#3", T_CS4 / 2),
            (1, "D#3", T_DS4 / 2),
            (3, "F#3", T_FS4 / 2),
            (4, "G#3", T_GS4 / 2),
            (5, "A#3", T_AS4 / 2),
            (7, "C#4", T_CS4),
            (8, "D#4", T_DS4),
            (10, "F#4", T_FS4),
            (11, "G#4", T_GS4),
            (12, "A#4", T_AS4),
        ];

        for (white_idx, name, freq) in black_offsets {
            let bk_x = 32 + (white_idx as u32 * white_key_w) + (white_key_w * 7 / 10);
            let bk_rect = Rect::new(bk_x, key_area_y, black_key_w, black_key_h);

            let is_active = self.active_freq == freq;
            let bg_col = if is_active { COL_GOLD } else { COL_BLACK_KEY };

            canvas.fill_rounded(bk_rect, 3, bg_col);
            canvas.outline(bk_rect, 1, Colour(0x0033_4155));

            canvas.text(
                bk_x + 4,
                key_area_y + black_key_h - 18,
                name,
                COL_TEXT_WHITE,
            );
        }

        // 5. Footer Controls
        let footer_y = h.saturating_sub(26);
        canvas.fill(Rect::new(0, footer_y, w, 26), Colour(0x000A_0F1D));
        canvas.outline(Rect::new(0, footer_y, w, 26), 1, COL_PANEL_BORDER);

        canvas.text(
            16,
            footer_y + 6,
            "Shortcuts: [Space] Play/Pause  [1-4] Select Track  [H/Esc] Silence  [Z-M] Virtual Keyboard",
            COL_TEXT_MUTED,
        );
    }

    fn key(&mut self, key: Key) -> bool {
        match key {
            Key::Character(' ') => {
                if self.is_playing {
                    self.hush();
                } else {
                    self.start_song(self.selected_song);
                }
                true
            }
            Key::Character('1') => {
                self.start_song(0);
                true
            }
            Key::Character('2') => {
                self.start_song(1);
                true
            }
            Key::Character('3') => {
                self.start_song(2);
                true
            }
            Key::Character('4') => {
                self.start_song(3);
                true
            }
            Key::Character('0') | Key::Escape => {
                self.hush();
                true
            }
            // Keyboard Piano Mapping:
            Key::Character('z') | Key::Character('Z') => {
                self.play_tone(T_C4, 180, "C4");
                true
            }
            Key::Character('s') | Key::Character('S') => {
                self.play_tone(T_CS4, 180, "C#4");
                true
            }
            Key::Character('x') | Key::Character('X') => {
                self.play_tone(T_D4, 180, "D4");
                true
            }
            Key::Character('d') | Key::Character('D') => {
                self.play_tone(T_DS4, 180, "D#4");
                true
            }
            Key::Character('c') | Key::Character('C') => {
                self.play_tone(T_E4, 180, "E4");
                true
            }
            Key::Character('v') | Key::Character('V') => {
                self.play_tone(T_F4, 180, "F4");
                true
            }
            Key::Character('g') | Key::Character('G') => {
                self.play_tone(T_FS4, 180, "F#4");
                true
            }
            Key::Character('b') | Key::Character('B') => {
                self.play_tone(T_G4, 180, "G4");
                true
            }
            Key::Character('h') | Key::Character('H') => {
                self.play_tone(T_GS4, 180, "G#4");
                true
            }
            Key::Character('n') | Key::Character('N') => {
                self.play_tone(T_A4, 180, "A4");
                true
            }
            Key::Character('j') | Key::Character('J') => {
                self.play_tone(T_AS4, 180, "A#4");
                true
            }
            Key::Character('m') | Key::Character('M') => {
                self.play_tone(T_B4, 180, "B4");
                true
            }
            Key::Character(',') => {
                self.play_tone(T_C5, 180, "C5");
                true
            }
            Key::Move(Movement::Up) if self.selected_song > 0 => {
                self.selected_song -= 1;
                true
            }
            Key::Move(Movement::Down) if self.selected_song + 1 < SONGS.len() => {
                self.selected_song += 1;
                true
            }
            _ => false,
        }
    }

    fn damage(&mut self) -> Option<(u32, u32, u32, u32)> {
        None
    }

    fn tick_ms(&mut self) -> Option<u64> {
        Some(50) // 20 ticks per second
    }

    fn ticked(&mut self) -> bool {
        self.tick_count = self.tick_count.wrapping_add(1);

        // Smooth decay of visualizer bars
        for bar in self.viz_bars.iter_mut() {
            *bar = bar.saturating_sub(4).max(6);
        }

        if !self.is_playing {
            return true;
        }

        // Song Sequencer step
        let current_song = &SONGS[self.selected_song];
        if self.note_index >= current_song.notes.len() {
            // Loop or finish
            self.note_index = 0;
            self.note_timer = 0;
        }

        if self.note_timer == 0 {
            let note = current_song.notes[self.note_index];
            self.play_tone(note.freq, note.duration_ms, note.name);
            // Convert ms to 50ms ticks
            self.note_timer = (note.duration_ms / 50).max(1);
            self.note_index += 1;
        } else {
            self.note_timer -= 1;
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
    nexus_user::log(&format!("music: PANIC: {info}")).ok();
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
        failed("music: FAILED: could not allocate heap");
        finish();
    }

    nexus_user::log("music: Music Studio & Synthesizer starting").ok();

    let mut lent = [Handle(0); 1];
    match Window::open(COMPOSITOR, SURFACE_AT, &mut lent) {
        Ok((window, carried)) => {
            let sound = (carried >= 1).then_some(lent[0]);
            let mut app = MusicStudioApp::new(sound);
            nexus_user::log("music: window opened with compositor, running synthesizer loop").ok();
            window.run(&mut app);
        }
        Err(_) => {
            failed("music: FAILED: could not connect to compositor");
        }
    }

    finish()
}
