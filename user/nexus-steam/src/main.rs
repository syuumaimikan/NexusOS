//! Steam Client Graphical Application & Gaming Hub for NexusOS.
//!
//! Provides the authentic Valve Steam Client desktop experience on NexusOS:
//! - STORE, LIBRARY, COMMUNITY, PROFILE, and DOWNLOADS tabs
//! - Interactive Game Library with Spacewar (AppID 480), Half-Life 2 (AppID 220), Portal 2 (AppID 620), etc.
//! - Direct launch integration with `BIN/GAME.ELF` via Compositor IPC
//! - Real-time Steamworks API achievements viewer & test unlocker
//! - Steam Cloud sync status, play statistics, and friend status

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use core::panic::PanicInfo;

use nexus_abi::steam::{self, AppId, ToastNotification, APPID_SPACEWAR};
use nexus_ui::{Canvas, Colour, Rect};
use nexus_user::Handle;
use nexus_window::{App, Key, Movement, Window};

#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

const COMPOSITOR: Handle = Handle(1);
const SURFACE_AT: usize = 0x0000_0000_3000_0000;
const HEAP: usize = 2 * 1024 * 1024;

// Steam Theme Colours
const COL_STEAM_DARK_BG: Colour = Colour(0x0017_1A21);
const COL_STEAM_SIDEBAR: Colour = Colour(0x001B_2838);
const COL_STEAM_PANEL: Colour = Colour(0x0021_3244);
const COL_STEAM_BLUE: Colour = Colour(0x0066_C0F4);
const COL_STEAM_LIGHT_BLUE: Colour = Colour(0x00C7_D5E0);
const COL_STEAM_MUTED: Colour = Colour(0x008F_98A0);
const COL_STEAM_GREEN: Colour = Colour(0x005C_7E10);
const COL_STEAM_GOLD: Colour = Colour(0x00F5_9E0B);
const COL_STEAM_ONLINE: Colour = Colour(0x0057_CBDE);
const COL_WHITE: Colour = Colour(0x00FF_FFFF);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Store,
    Library,
    Community,
    Profile,
    Downloads,
}

#[derive(Clone)]
struct GameEntry {
    appid: AppId,
    title: &'static str,
    publisher: &'static str,
    genre: &'static str,
    installed: bool,
    play_time: &'static str,
}

const GAMES: &[GameEntry] = &[
    GameEntry {
        appid: APPID_SPACEWAR,
        title: "Spacewar (Space Vanguard)",
        publisher: "Valve Corporation",
        genre: "Vector Space Action / 60 FPS",
        installed: true,
        play_time: "14.8 hours",
    },
    GameEntry {
        appid: 220,
        title: "Half-Life 2",
        publisher: "Valve Corporation",
        genre: "First-Person Shooter",
        installed: false,
        play_time: "42.1 hours",
    },
    GameEntry {
        appid: 620,
        title: "Portal 2",
        publisher: "Valve Corporation",
        genre: "Puzzle Platformer",
        installed: false,
        play_time: "18.5 hours",
    },
    GameEntry {
        appid: 413150,
        title: "Stardew Valley",
        publisher: "ConcernedApe",
        genre: "Simulation / RPG",
        installed: false,
        play_time: "65.3 hours",
    },
    GameEntry {
        appid: 1091500,
        title: "Cyberpunk 2077",
        publisher: "CD PROJEKT RED",
        genre: "Sci-Fi Action RPG",
        installed: false,
        play_time: "32.0 hours",
    },
];

pub struct SteamClientApp {
    tab: Tab,
    selected_game: usize,
    active_toast: Option<ToastNotification>,
    status_notice: Option<String>,
    notice_ticks: u32,
}

impl SteamClientApp {
    pub fn new() -> Self {
        // Initialize Steamworks API ABI
        steam::steam_context().init_with_appid(APPID_SPACEWAR);

        Self {
            tab: Tab::Library,
            selected_game: 0,
            active_toast: None,
            status_notice: None,
            notice_ticks: 0,
        }
    }

    fn launch_selected_game(&mut self) {
        if self.selected_game < GAMES.len() {
            let game = &GAMES[self.selected_game];
            if game.installed {
                // Send launch request to Compositor for BIN/GAME.ELF (tag b"game")
                nexus_user::send(COMPOSITOR, b"game", &[]).ok();
                self.status_notice = Some(format!("Launched {} via Steamworks!", game.title));
                self.notice_ticks = 180;
            } else {
                self.status_notice = Some(format!(
                    "{} (AppID {}) in library. Steamworks Ready.",
                    game.title, game.appid
                ));
                self.notice_ticks = 180;
            }
        }
    }

    fn unlock_next_achievement(&mut self) {
        let ctx = steam::steam_context();
        let achs = [
            ("ACH_FIRST_LAUNCH", "First Flight"),
            ("ACH_SHARPSHOOTER", "Sharpshooter"),
            ("ACH_SURVIVOR", "Space Survivor"),
            ("ACH_SPACE_ACE", "Space Ace"),
        ];

        for (id, name) in &achs {
            if !ctx.is_achievement_unlocked(id) {
                ctx.unlock_achievement(id);
                self.status_notice = Some(format!("Achievement Unlocked: {}!", name));
                self.notice_ticks = 180;
                return;
            }
        }
        self.status_notice = Some(String::from("All 4 Spacewar Achievements Unlocked!"));
        self.notice_ticks = 180;
    }
}

impl Default for SteamClientApp {
    fn default() -> Self {
        Self::new()
    }
}

impl App for SteamClientApp {
    fn draw(&mut self, canvas: &mut Canvas) {
        let w = canvas.width();
        let h = canvas.height();

        // Background
        canvas.fill(Rect::new(0, 0, w, h), COL_STEAM_DARK_BG);

        // 1. Top Navigation Bar (height: 48)
        let nav_rect = Rect::new(0, 0, w, 48);
        canvas.fill(nav_rect, Colour(0x0010_1822));
        canvas.outline(nav_rect, 1, Colour(0x001E_293B));

        // Steam Logo
        canvas.fill_rounded(Rect::new(12, 10, 80, 28), 4, Colour(0x001B_2838));
        canvas.outline(Rect::new(12, 10, 80, 28), 1, COL_STEAM_BLUE);
        canvas.text_centred(Rect::new(12, 14, 80, 20), "STEAM", COL_WHITE);

        // Navigation Tabs
        let tabs = [
            (Tab::Store, "STORE [1]"),
            (Tab::Library, "LIBRARY [2]"),
            (Tab::Community, "COMMUNITY [3]"),
            (Tab::Profile, "PROFILE [4]"),
            (Tab::Downloads, "DOWNLOADS [5]"),
        ];

        let mut tab_x = 108;
        for (t, name) in &tabs {
            let active = self.tab == *t;
            let text_col = if active {
                COL_WHITE
            } else {
                COL_STEAM_LIGHT_BLUE
            };
            let tab_w = 110;
            if active {
                canvas.fill(Rect::new(tab_x, 44, tab_w, 4), COL_STEAM_BLUE);
            }
            canvas.text_centred(Rect::new(tab_x, 16, tab_w, 20), name, text_col);
            tab_x += tab_w + 8;
        }

        // Top-Right Persona Status
        let persona_x = w.saturating_sub(230);
        canvas.text(persona_x, 10, "NexusPlayer", COL_STEAM_ONLINE);
        canvas.text(persona_x, 26, "Online • Level 42 • ¥2,480", COL_STEAM_MUTED);

        // 2. Main Content Area depending on active Tab
        let content_y = 48;
        let content_h = h.saturating_sub(content_y + 30);

        match self.tab {
            Tab::Library => self.draw_library(canvas, content_y, content_h, w),
            Tab::Store => self.draw_store(canvas, content_y, content_h, w),
            Tab::Community => self.draw_community(canvas, content_y, content_h, w),
            Tab::Profile => self.draw_profile(canvas, content_y, content_h, w),
            Tab::Downloads => self.draw_downloads(canvas, content_y, content_h, w),
        }

        // 3. Hotkey Navigation Footer
        let footer_y = h.saturating_sub(26);
        canvas.fill(Rect::new(0, footer_y, w, 26), Colour(0x000F_172A));
        canvas.outline(Rect::new(0, footer_y, w, 26), 1, Colour(0x001E_293B));
        let footer_text = if let Some(ref notice) = self.status_notice {
            notice.as_str()
        } else {
            "Hotkeys: [1-5/F1-F5] Tabs  [Tab] Cycle  [↑/↓] Select Game  [Enter/P] Launch Game  [U] Unlock Achievement"
        };
        let footer_col = if self.status_notice.is_some() {
            COL_STEAM_GOLD
        } else {
            COL_STEAM_MUTED
        };
        canvas.text(16, footer_y + 6, footer_text, footer_col);

        // 4. Achievement Toast Overlay (if triggered)
        if let Some(toast) = self.active_toast {
            let tw = 280;
            let th = 56;
            let tx = w.saturating_sub(tw + 16);
            let ty = h.saturating_sub(th + 36);

            let toast_rect = Rect::new(tx, ty, tw, th);
            canvas.panel(toast_rect, 6, Colour(0x000F_172A), Colour(0x003B_82F6));
            canvas.fill_rounded(Rect::new(tx + 8, ty + 10, 36, 36), 4, Colour(0x001E_3A8A));
            canvas.text(tx + 16, ty + 18, "*", COL_STEAM_GOLD);
            canvas.text(tx + 52, ty + 8, "STEAMWORKS ACHIEVEMENT", COL_STEAM_GOLD);
            canvas.text(tx + 52, ty + 24, toast.name_str(), COL_WHITE);
            canvas.text(tx + 52, ty + 38, toast.desc_str(), COL_STEAM_LIGHT_BLUE);
        }
    }

    fn key(&mut self, key: Key) -> bool {
        match key {
            Key::Function(1) | Key::Character('1') => {
                self.tab = Tab::Store;
                true
            }
            Key::Function(2) | Key::Character('2') => {
                self.tab = Tab::Library;
                true
            }
            Key::Function(3) | Key::Character('3') => {
                self.tab = Tab::Community;
                true
            }
            Key::Function(4) | Key::Character('4') => {
                self.tab = Tab::Profile;
                true
            }
            Key::Function(5) | Key::Character('5') => {
                self.tab = Tab::Downloads;
                true
            }
            Key::Tab => {
                self.tab = match self.tab {
                    Tab::Store => Tab::Library,
                    Tab::Library => Tab::Community,
                    Tab::Community => Tab::Profile,
                    Tab::Profile => Tab::Downloads,
                    Tab::Downloads => Tab::Store,
                };
                true
            }
            Key::Move(Movement::Up) => {
                if self.selected_game > 0 {
                    self.selected_game -= 1;
                    true
                } else {
                    false
                }
            }
            Key::Move(Movement::Down) => {
                if self.selected_game + 1 < GAMES.len() {
                    self.selected_game += 1;
                    true
                } else {
                    false
                }
            }
            Key::Enter | Key::Character('p') | Key::Character('P') | Key::Character(' ') => {
                self.launch_selected_game();
                true
            }
            Key::Character('u')
            | Key::Character('U')
            | Key::Character('a')
            | Key::Character('A') => {
                self.unlock_next_achievement();
                true
            }
            _ => false,
        }
    }

    fn tick_ms(&mut self) -> Option<u64> {
        Some(50)
    }

    fn ticked(&mut self) -> bool {
        let mut changed = false;
        // Poll for pending Steamworks toast
        let ctx = steam::steam_context();
        let new_toast = ctx.poll_toast();
        if new_toast != self.active_toast {
            self.active_toast = new_toast;
            changed = true;
        }

        if self.notice_ticks > 0 {
            self.notice_ticks -= 1;
            if self.notice_ticks == 0 {
                self.status_notice = None;
                changed = true;
            }
        }

        changed
    }

    fn damage(&mut self) -> Option<(u32, u32, u32, u32)> {
        None
    }
}

impl SteamClientApp {
    fn draw_library(&self, canvas: &mut Canvas, y: u32, h: u32, w: u32) {
        let sidebar_w = 220;

        // 1. Sidebar (Games List)
        canvas.fill(Rect::new(0, y, sidebar_w, h), COL_STEAM_SIDEBAR);
        canvas.outline(Rect::new(0, y, sidebar_w, h), 1, Colour(0x000F_172A));

        // Search / Filter Header
        canvas.fill_rounded(
            Rect::new(12, y + 10, sidebar_w - 24, 26),
            3,
            Colour(0x0012_1E2A),
        );
        canvas.text(20, y + 16, "Search Library...", COL_STEAM_MUTED);

        // Category header
        canvas.text(16, y + 46, "GAMES (5)", COL_STEAM_LIGHT_BLUE);

        // Game list items
        let item_y_base = y + 70;
        for (i, game) in GAMES.iter().enumerate() {
            let item_y = item_y_base + (i as u32 * 44);
            let is_selected = i == self.selected_game;

            if is_selected {
                canvas.fill(Rect::new(6, item_y, sidebar_w - 12, 38), COL_STEAM_PANEL);
                canvas.outline(Rect::new(6, item_y, sidebar_w - 12, 38), 1, COL_STEAM_BLUE);
            }

            let bullet = if game.installed { "●" } else { "○" };
            let bullet_col = if game.installed {
                COL_STEAM_ONLINE
            } else {
                COL_STEAM_MUTED
            };
            canvas.text(14, item_y + 12, bullet, bullet_col);

            let title_col = if is_selected {
                COL_WHITE
            } else {
                COL_STEAM_LIGHT_BLUE
            };
            canvas.text(28, item_y + 12, game.title, title_col);
        }

        // 2. Right Detail Pane
        let detail_x = sidebar_w + 16;
        let detail_w = w.saturating_sub(detail_x + 16);

        if self.selected_game < GAMES.len() {
            let game = &GAMES[self.selected_game];

            // Banner Hero Header
            let hero_rect = Rect::new(detail_x, y + 12, detail_w, 120);
            canvas.fill_rounded(hero_rect, 6, Colour(0x001E_293B));
            canvas.outline(hero_rect, 1, Colour(0x0033_4155));

            canvas.text(detail_x + 20, y + 28, game.title, COL_WHITE);
            let sub = format!("{} • AppID {} • {}", game.publisher, game.appid, game.genre);
            canvas.text(detail_x + 20, y + 54, &sub, COL_STEAM_LIGHT_BLUE);
            canvas.text(
                detail_x + 20,
                y + 76,
                "Features: Single-player • Steamworks API • Achievements • 60 FPS Vector",
                COL_STEAM_MUTED,
            );

            // Action Bar (Play Button & Play Stats)
            let action_y = y + 144;
            let action_rect = Rect::new(detail_x, action_y, detail_w, 54);
            canvas.fill_rounded(action_rect, 4, Colour(0x0010_1822));
            canvas.outline(action_rect, 1, Colour(0x001F_2937));

            // Green "▶ PLAY" Button
            let btn_rect = Rect::new(detail_x + 12, action_y + 8, 140, 38);
            canvas.fill_rounded(btn_rect, 4, COL_STEAM_GREEN);

            let btn_label = if game.installed {
                "▶  PLAY (Enter)"
            } else {
                "INSTALL"
            };
            canvas.text_centred(
                Rect::new(detail_x + 12, action_y + 18, 140, 20),
                btn_label,
                COL_WHITE,
            );

            // Stats items
            let stat_x = detail_x + 170;
            canvas.text(stat_x, action_y + 12, "LAST PLAYED", COL_STEAM_MUTED);
            canvas.text(stat_x, action_y + 28, "Today", COL_WHITE);

            let stat_x2 = stat_x + 120;
            canvas.text(stat_x2, action_y + 12, "PLAY TIME", COL_STEAM_MUTED);
            canvas.text(stat_x2, action_y + 28, game.play_time, COL_WHITE);

            let stat_x3 = stat_x2 + 130;
            canvas.text(stat_x3, action_y + 12, "CLOUD STATUS", COL_STEAM_MUTED);
            canvas.text(stat_x3, action_y + 28, "Up to date ✓", COL_STEAM_ONLINE);

            // Steam Achievements Section
            let ach_y = action_y + 68;
            let ach_rect = Rect::new(detail_x, ach_y, detail_w, 150);
            canvas.fill_rounded(ach_rect, 4, Colour(0x0014_1F2D));
            canvas.outline(ach_rect, 1, Colour(0x0024_3447));

            let ctx = steam::steam_context();
            let unlocked_count = (0..ctx.achievement_count)
                .filter(|&i| ctx.achievements[i].achieved)
                .count();

            let header_text = format!(
                "ACHIEVEMENTS ({}/{} Unlocked) • Steamworks ABI Integrated",
                unlocked_count, ctx.achievement_count
            );
            canvas.text(detail_x + 16, ach_y + 12, &header_text, COL_WHITE);

            // 4 Achievement Badges
            for i in 0..ctx.achievement_count.min(4) {
                let ach = &ctx.achievements[i];
                let card_x = detail_x + 16 + (i as u32 * (detail_w / 4));
                let card_w = (detail_w / 4).saturating_sub(12);
                let card_rect = Rect::new(card_x, ach_y + 36, card_w, 64);

                let (bg_col, border_col, title_col, status_label) = if ach.achieved {
                    (
                        Colour(0x0017_2A3A),
                        COL_STEAM_GOLD,
                        COL_STEAM_GOLD,
                        "UNLOCKED ★",
                    )
                } else {
                    (
                        Colour(0x000F_172A),
                        Colour(0x0033_4155),
                        COL_STEAM_MUTED,
                        "LOCKED ☆",
                    )
                };

                canvas.fill_rounded(card_rect, 4, bg_col);
                canvas.outline(card_rect, 1, border_col);

                canvas.text(card_x + 8, ach_y + 44, status_label, border_col);
                canvas.text(card_x + 8, ach_y + 60, ach.name_str(), title_col);
                canvas.text(card_x + 8, ach_y + 76, ach.desc_str(), COL_STEAM_LIGHT_BLUE);
            }

            // Test Unlocker Button Indicator
            let unlock_btn = Rect::new(detail_x + 16, ach_y + 110, 260, 28);
            canvas.fill_rounded(unlock_btn, 4, Colour(0x0025_63EB));
            canvas.text_centred(
                Rect::new(detail_x + 16, ach_y + 116, 260, 20),
                "Press [U] to Unlock Next Achievement",
                COL_WHITE,
            );

            // Friends In-Game
            let friend_y = ach_y + 160;
            canvas.text(detail_x, friend_y, "FRIENDS ACTIVITY", COL_STEAM_LIGHT_BLUE);
            canvas.text(
                detail_x,
                friend_y + 20,
                "● NexusPlayer - Playing Spacewar (AppID 480)",
                COL_STEAM_ONLINE,
            );
            canvas.text(
                detail_x,
                friend_y + 38,
                "● GabeN - In-Game: Half-Life 3 (Private Beta)",
                COL_STEAM_ONLINE,
            );
            canvas.text(
                detail_x,
                friend_y + 56,
                "○ Gordon Freeman - Offline (Last online 20 yrs ago)",
                COL_STEAM_MUTED,
            );
        }
    }

    fn draw_store(&self, canvas: &mut Canvas, y: u32, _h: u32, w: u32) {
        let pad = 24;
        let store_w = w.saturating_sub(pad * 2);

        // Featured Event Banner
        let banner_rect = Rect::new(pad, y + 16, store_w, 110);
        canvas.fill_rounded(banner_rect, 6, Colour(0x001B_2838));
        canvas.outline(banner_rect, 1, COL_STEAM_BLUE);

        canvas.text(
            pad + 20,
            y + 32,
            "STEAM SPECIAL EVENT • NEXUSOS GAMING LAUNCH",
            COL_STEAM_GOLD,
        );
        canvas.text(
            pad + 20,
            y + 56,
            "Play Spacewar natively with 60 FPS vector rasterization and Steamworks API!",
            COL_WHITE,
        );
        canvas.text(
            pad + 20,
            y + 78,
            "Architecture: Dual Subsystem (Linux ELF64 + Windows PE32+) & Vulkan 1.2 SwiftShader",
            COL_STEAM_LIGHT_BLUE,
        );

        // Top Sellers Header
        canvas.text(pad, y + 144, "TOP SELLERS & FEATURED", COL_WHITE);

        let items = [
            (
                "Spacewar (AppID 480)",
                "Valve Corporation",
                "Free to Play",
                "Ready to Launch",
            ),
            (
                "Half-Life 2 (AppID 220)",
                "Valve Corporation",
                "¥240 (-80%)",
                "Source Engine",
            ),
            (
                "Portal 2 (AppID 620)",
                "Valve Corporation",
                "¥300 (-75%)",
                "Proton Ready",
            ),
            (
                "Stardew Valley (AppID 413150)",
                "ConcernedApe",
                "¥1,480",
                "Cross-Platform",
            ),
        ];

        for (i, (title, author, price, tag)) in items.iter().enumerate() {
            let row_y = y + 172 + (i as u32 * 54);
            let row_rect = Rect::new(pad, row_y, store_w, 46);

            canvas.fill_rounded(row_rect, 4, Colour(0x0012_1B26));
            canvas.outline(row_rect, 1, Colour(0x001F_2C3B));

            canvas.text(pad + 16, row_y + 14, title, COL_WHITE);
            canvas.text(pad + 240, row_y + 14, author, COL_STEAM_MUTED);
            canvas.text(pad + 420, row_y + 14, tag, COL_STEAM_BLUE);
            canvas.text(
                store_w.saturating_sub(80),
                row_y + 14,
                price,
                COL_STEAM_ONLINE,
            );
        }
    }

    fn draw_community(&self, canvas: &mut Canvas, y: u32, _h: u32, w: u32) {
        let pad = 24;
        canvas.text(pad, y + 20, "STEAM COMMUNITY HUB", COL_WHITE);
        canvas.text(
            pad,
            y + 42,
            "Welcome to the NexusOS Steam Community! Share screenshots, artwork, and guides.",
            COL_STEAM_LIGHT_BLUE,
        );

        let box_rect = Rect::new(pad, y + 74, w.saturating_sub(pad * 2), 160);
        canvas.fill_rounded(box_rect, 6, Colour(0x0014_1F2D));
        canvas.outline(box_rect, 1, Colour(0x0024_3447));

        canvas.text(pad + 16, y + 90, "LATEST WORKSHOP MODS", COL_WHITE);
        canvas.text(
            pad + 16,
            y + 114,
            "• Neon Starfighter Skin Pack (Spacewar) - By NexusPilot [Subscribed]",
            COL_STEAM_ONLINE,
        );
        canvas.text(
            pad + 16,
            y + 134,
            "• High-Precision Asteroid Trajectory Shader - By VulkanDev [Subscribed]",
            COL_STEAM_LIGHT_BLUE,
        );
        canvas.text(
            pad + 16,
            y + 154,
            "• Sound Pack: Retro Synthesizer 8-bit Audio - By ChiptuneKing [Installed]",
            COL_STEAM_LIGHT_BLUE,
        );
        canvas.text(
            pad + 16,
            y + 174,
            "• Dual ABI Calling Convention Thunk Debugger v1.2 - By ArchitectureTeam",
            COL_STEAM_MUTED,
        );
    }

    fn draw_profile(&self, canvas: &mut Canvas, y: u32, _h: u32, w: u32) {
        let pad = 24;
        let card = Rect::new(pad, y + 20, w.saturating_sub(pad * 2), 180);
        canvas.fill_rounded(card, 6, Colour(0x0018_2434));
        canvas.outline(card, 1, COL_STEAM_BLUE);

        // Avatar placeholder
        canvas.fill_rounded(Rect::new(pad + 20, y + 40, 64, 64), 6, Colour(0x0025_63EB));
        canvas.text_centred(
            Rect::new(pad + 20, y + 60, 64, 24),
            "NP",
            Colour(0x00F8_FAFC),
        );

        canvas.text(pad + 100, y + 40, "NexusPlayer", COL_WHITE);
        canvas.text(
            pad + 100,
            y + 60,
            "SteamID: 76561198000010001 • Universe: Public",
            COL_STEAM_LIGHT_BLUE,
        );
        canvas.text(
            pad + 100,
            y + 80,
            "Steam Level: 42 • Years of Service: 10 Years",
            COL_STEAM_GOLD,
        );
        canvas.text(
            pad + 100,
            y + 100,
            "Status: Online • Dual Subsystem Running",
            COL_STEAM_ONLINE,
        );

        canvas.text(pad + 20, y + 140, "BADGES & SHOWCASE", COL_WHITE);
        canvas.text(
            pad + 20,
            y + 160,
            "★ Kernel Architect  |  ★ 60 FPS Ace Pilot  |  ★ ABI Master  |  ★ Vulkan Explorer",
            COL_STEAM_GOLD,
        );
    }

    fn draw_downloads(&self, canvas: &mut Canvas, y: u32, _h: u32, w: u32) {
        let pad = 24;
        canvas.text(pad, y + 20, "DOWNLOADS & UPDATES", COL_WHITE);

        let dl_rect = Rect::new(pad, y + 50, w.saturating_sub(pad * 2), 100);
        canvas.fill_rounded(dl_rect, 4, Colour(0x0012_1B26));
        canvas.outline(dl_rect, 1, Colour(0x0025_63EB));

        canvas.text(
            pad + 16,
            y + 64,
            "Spacewar (AppID 480) - Patch v1.4.0",
            COL_WHITE,
        );
        canvas.text(
            pad + 16,
            y + 82,
            "Download Complete: 342.8 KiB / 342.8 KiB (100%)",
            COL_STEAM_ONLINE,
        );
        canvas.text(
            pad + 16,
            y + 100,
            "Peak Speed: 48.5 MB/s • Disk Write: Complete • Shaders Cached",
            COL_STEAM_MUTED,
        );

        // Progress bar
        let bar_w = w.saturating_sub(pad * 2 + 32);
        canvas.fill_rounded(Rect::new(pad + 16, y + 124, bar_w, 8), 4, COL_STEAM_BLUE);
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
    nexus_user::log(&format!("steam: PANIC: {info}")).ok();
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
        failed("steam: FAILED: could not allocate heap");
        finish();
    }

    nexus_user::log("steam: Steam Client starting on NexusOS").ok();

    let mut lent = [Handle(0); 2];
    match Window::open(COMPOSITOR, SURFACE_AT, &mut lent) {
        Ok((window, _carried)) => {
            let mut steam_app = SteamClientApp::new();
            nexus_user::log("steam: window opened with compositor, running UI loop").ok();
            window.run(&mut steam_app);
        }
        Err(_) => {
            failed("steam: FAILED: could not connect to compositor");
        }
    }

    finish()
}
