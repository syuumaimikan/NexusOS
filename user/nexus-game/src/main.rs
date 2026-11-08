//! Steamworks-compatible interactive Spacewar game for NexusOS.
//!
//! Demonstrates:
//! - Full Steamworks API emulation (AppID 480 `Spacewar`, Persona Name, Stats, Achievements, Overlay Toast)
//! - 60 FPS real-time vector graphics with anti-aliasing via `nexus-ui` and `nexus-window`
//! - Fixed-point 256-step physics simulation without floating-point instructions
//! - Interactive keyboard controls, particle system, collision detection, and wave progression.

#![no_std]
#![no_main]
#![allow(
    clippy::precedence,
    clippy::manual_is_multiple_of,
    clippy::needless_range_loop,
    clippy::collapsible_match
)]

extern crate alloc;

mod turn;

use alloc::format;
use core::panic::PanicInfo;

use nexus_abi::steam::{self, ToastNotification, APPID_SPACEWAR};
use nexus_ui::{Canvas, Colour, Rect};
use nexus_user::Handle;
use nexus_window::{App, Key, Movement, Window};

#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

const COMPOSITOR: Handle = Handle(1);
const SURFACE_AT: usize = 0x0000_0000_3000_0000;
const HEAP: usize = 1024 * 1024;

const FP_SHIFT: i32 = 8;
const ONE: i32 = 1 << FP_SHIFT;

const MAX_LASERS: usize = 8;
const MAX_ASTEROIDS: usize = 20;
const MAX_PARTICLES: usize = 48;
const NUM_STARS: usize = 36;

// Palette
const COL_SPACE: Colour = Colour(0x0006_0A12);
const COL_SHIP: Colour = Colour(0x00E0_F2FE);
const COL_SHIP_EDGE: Colour = Colour(0x0038_BDF8);
const COL_THRUST: Colour = Colour(0x00F9_7316);
const COL_LASER: Colour = Colour(0x0022_D3EE);
const COL_ASTEROID: Colour = Colour(0x0094_A3B8);
const COL_ASTEROID_EDGE: Colour = Colour(0x0064_748B);
const COL_HUD_TEXT: Colour = Colour(0x00F1_F5F9);
const COL_HUD_ACCENT: Colour = Colour(0x0038_BDF8);
const COL_STEAM_GOLD: Colour = Colour(0x00F5_9E0B);
const COL_STEAM_BG: Colour = Colour(0x000F_172A);
const COL_STEAM_BORDER: Colour = Colour(0x003B_82F6);

#[derive(Clone, Copy)]
struct Star {
    x: u32,
    y: u32,
    brightness: u8,
    twinkle: u8,
}

#[derive(Clone, Copy)]
struct Ship {
    x: i32,
    y: i32,
    vx: i32,
    vy: i32,
    angle: usize,
    thrust: bool,
    alive: bool,
    invuln: u32,
}

#[derive(Clone, Copy)]
struct Laser {
    x: i32,
    y: i32,
    vx: i32,
    vy: i32,
    life: u32,
    active: bool,
}

#[derive(Clone, Copy)]
struct Asteroid {
    x: i32,
    y: i32,
    vx: i32,
    vy: i32,
    radius: i32,
    stage: u8,
    angle: usize,
    rot_speed: i32,
    seed: u32,
    active: bool,
}

#[derive(Clone, Copy)]
struct Particle {
    x: i32,
    y: i32,
    vx: i32,
    vy: i32,
    life: u32,
    color: Colour,
    active: bool,
}

pub struct SpacewarGame {
    width: u32,
    height: u32,
    ship: Ship,
    lasers: [Laser; MAX_LASERS],
    asteroids: [Asteroid; MAX_ASTEROIDS],
    particles: [Particle; MAX_PARTICLES],
    stars: [Star; NUM_STARS],
    score: u32,
    high_score: u32,
    wave: u32,
    lives: u32,
    game_over: bool,
    paused: bool,
    tick_count: u32,
    active_toast: Option<ToastNotification>,
    toast_anim_y: i32,
    sound: Option<Handle>,
}

impl SpacewarGame {
    pub fn new(sound: Option<Handle>) -> Self {
        let mut stars = [Star {
            x: 0,
            y: 0,
            brightness: 120,
            twinkle: 0,
        }; NUM_STARS];

        let mut seed = 0x1234_5678u32;
        for (i, star) in stars.iter_mut().enumerate() {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            star.x = (seed >> 16) % 800;
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            star.y = (seed >> 16) % 600;
            star.brightness = (100 + ((seed >> 24) % 155)) as u8;
            star.twinkle = (i * 7) as u8;
        }

        let empty_laser = Laser {
            x: 0,
            y: 0,
            vx: 0,
            vy: 0,
            life: 0,
            active: false,
        };
        let empty_asteroid = Asteroid {
            x: 0,
            y: 0,
            vx: 0,
            vy: 0,
            radius: 0,
            stage: 0,
            angle: 0,
            rot_speed: 0,
            seed: 0,
            active: false,
        };
        let empty_particle = Particle {
            x: 0,
            y: 0,
            vx: 0,
            vy: 0,
            life: 0,
            color: Colour(0),
            active: false,
        };

        let mut game = Self {
            width: 640,
            height: 480,
            ship: Ship {
                x: 320 << FP_SHIFT,
                y: 240 << FP_SHIFT,
                vx: 0,
                vy: 0,
                angle: 0,
                thrust: false,
                alive: true,
                invuln: 60,
            },
            lasers: [empty_laser; MAX_LASERS],
            asteroids: [empty_asteroid; MAX_ASTEROIDS],
            particles: [empty_particle; MAX_PARTICLES],
            stars,
            score: 0,
            high_score: 0,
            wave: 1,
            lives: 3,
            game_over: false,
            paused: false,
            tick_count: 0,
            active_toast: None,
            toast_anim_y: -80,
            sound,
        };

        // Initialize Steamworks
        steam::SteamAPI_Init();
        steam::SteamAPI_RestartAppIfNecessary(APPID_SPACEWAR);
        let ctx = steam::steam_context();
        ctx.init_with_appid(APPID_SPACEWAR);

        // Unlock first flight achievement on launch!
        ctx.unlock_achievement("ACH_FIRST_LAUNCH");

        game.spawn_wave(1);
        game
    }
}

impl Default for SpacewarGame {
    fn default() -> Self {
        Self::new(None)
    }
}

impl SpacewarGame {
    pub fn play_tone(&self, hertz: u32, length_ms: u32) {
        if let Some(sound) = self.sound {
            let mut msg = [0u8; 12];
            msg[..4].copy_from_slice(b"tone");
            msg[4..8].copy_from_slice(&hertz.to_le_bytes());
            msg[8..12].copy_from_slice(&length_ms.to_le_bytes());
            nexus_user::send(sound, &msg, &[]).ok();
        }
    }

    fn spawn_wave(&mut self, wave: u32) {
        self.wave = wave;
        let count = (3 + wave as usize).min(6);
        for a in self.asteroids.iter_mut() {
            a.active = false;
        }

        let mut seed = 0x9876_5432u32.wrapping_add(wave * 100);
        for i in 0..count {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            let side = (seed >> 16) % 4;
            let (x, y) = match side {
                0 => (0, (seed % (self.height.max(480))) as i32),
                1 => (
                    (self.width.max(640)) as i32,
                    (seed % (self.height.max(480))) as i32,
                ),
                2 => ((seed % (self.width.max(640))) as i32, 0),
                _ => (
                    (seed % (self.width.max(640))) as i32,
                    (self.height.max(480)) as i32,
                ),
            };

            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            let vx = (((seed >> 16) % 300) as i32 - 150) * ONE / 120;
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            let vy = (((seed >> 16) % 300) as i32 - 150) * ONE / 120;

            self.asteroids[i] = Asteroid {
                x: x << FP_SHIFT,
                y: y << FP_SHIFT,
                vx: if vx == 0 { ONE / 2 } else { vx },
                vy: if vy == 0 { ONE / 2 } else { vy },
                radius: 28,
                stage: 3,
                angle: (i * 40) % 256,
                rot_speed: if i % 2 == 0 { 2 } else { -2 },
                seed: seed ^ (i as u32),
                active: true,
            };
        }
    }

    fn fire_laser(&mut self) {
        if !self.ship.alive || self.game_over || self.paused {
            return;
        }

        let Some(slot) = self.lasers.iter_mut().find(|l| !l.active) else {
            return;
        };

        let sin = turn::sine(self.ship.angle);
        let cos = turn::cosine(self.ship.angle);

        // Spawn at ship nose
        let tip_dist = 14 * ONE;
        let lx = self.ship.x + (sin * (tip_dist >> 8) >> 8);
        let ly = self.ship.y - (cos * (tip_dist >> 8) >> 8);

        // Speed: 7 pixels per tick
        let speed = 7 * ONE;
        let lvx = (sin * (speed >> 8) >> 8) + (self.ship.vx / 3);
        let lvy = -(cos * (speed >> 8) >> 8) + (self.ship.vy / 3);

        *slot = Laser {
            x: lx,
            y: ly,
            vx: lvx,
            vy: lvy,
            life: 45, // lasts 45 ticks
            active: true,
        };
        self.play_tone(1200, 25);
    }

    fn spawn_particles(&mut self, x: i32, y: i32, count: usize, base_color: Colour) {
        let mut spawned = 0;
        let mut seed = self.tick_count.wrapping_add(x as u32);
        for p in self.particles.iter_mut() {
            if !p.active {
                seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
                let angle = ((seed >> 16) % 256) as usize;
                seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
                let speed = (((seed >> 16) % 400) + 100) as i32 * ONE / 100;

                let pvx = turn::sine(angle) * (speed >> 8) >> 8;
                let pvy = -(turn::cosine(angle) * (speed >> 8) >> 8);

                *p = Particle {
                    x,
                    y,
                    vx: pvx,
                    vy: pvy,
                    life: 25,
                    color: base_color,
                    active: true,
                };
                spawned += 1;
                if spawned >= count {
                    break;
                }
            }
        }
    }

    pub fn tick(&mut self) {
        if self.paused {
            return;
        }

        self.tick_count = self.tick_count.wrapping_add(1);

        // Poll Steam achievements / toasts
        let ctx = steam::steam_context();
        if let Some(toast) = ctx.poll_toast() {
            self.active_toast = Some(toast);
        } else {
            self.active_toast = None;
        }

        // Animate toast banner
        if self.active_toast.is_some() {
            if self.toast_anim_y < 16 {
                self.toast_anim_y += 4;
            }
        } else if self.toast_anim_y > -80 {
            self.toast_anim_y -= 4;
        }

        let w_fp = (self.width as i32) << FP_SHIFT;
        let h_fp = (self.height as i32) << FP_SHIFT;

        // Ship physics
        if self.ship.alive {
            if self.ship.invuln > 0 {
                self.ship.invuln -= 1;
            }

            if self.ship.thrust {
                let sin = turn::sine(self.ship.angle);
                let cos = turn::cosine(self.ship.angle);
                // Thrust acceleration
                let accel = ONE / 4;
                self.ship.vx += sin * (accel >> 8) >> 8;
                self.ship.vy -= cos * (accel >> 8) >> 8;

                // Spawn thruster particle flame
                if self.tick_count % 2 == 0 {
                    let back_dist = 8 * ONE;
                    let px = self.ship.x - (sin * (back_dist >> 8) >> 8);
                    let py = self.ship.y + (cos * (back_dist >> 8) >> 8);
                    self.spawn_particles(px, py, 1, COL_THRUST);
                }
            }

            // Drag / dampening
            self.ship.vx = (self.ship.vx * 252) / 256;
            self.ship.vy = (self.ship.vy * 252) / 256;

            self.ship.x += self.ship.vx;
            self.ship.y += self.ship.vy;

            // Toroidal wrap
            if self.ship.x < 0 {
                self.ship.x += w_fp;
            } else if self.ship.x >= w_fp {
                self.ship.x -= w_fp;
            }
            if self.ship.y < 0 {
                self.ship.y += h_fp;
            } else if self.ship.y >= h_fp {
                self.ship.y -= h_fp;
            }
        }

        // Lasers
        for laser in self.lasers.iter_mut() {
            if !laser.active {
                continue;
            }
            laser.x += laser.vx;
            laser.y += laser.vy;

            // Screen wrap
            if laser.x < 0 {
                laser.x += w_fp;
            } else if laser.x >= w_fp {
                laser.x -= w_fp;
            }
            if laser.y < 0 {
                laser.y += h_fp;
            } else if laser.y >= h_fp {
                laser.y -= h_fp;
            }

            if laser.life == 0 {
                laser.active = false;
            } else {
                laser.life -= 1;
            }
        }

        // Asteroids
        let mut active_count = 0;
        for i in 0..MAX_ASTEROIDS {
            if !self.asteroids[i].active {
                continue;
            }
            active_count += 1;

            self.asteroids[i].x += self.asteroids[i].vx;
            self.asteroids[i].y += self.asteroids[i].vy;
            let rot = (self.asteroids[i].angle as i32 + self.asteroids[i].rot_speed + 256) % 256;
            self.asteroids[i].angle = rot as usize;

            if self.asteroids[i].x < 0 {
                self.asteroids[i].x += w_fp;
            } else if self.asteroids[i].x >= w_fp {
                self.asteroids[i].x -= w_fp;
            }
            if self.asteroids[i].y < 0 {
                self.asteroids[i].y += h_fp;
            } else if self.asteroids[i].y >= h_fp {
                self.asteroids[i].y -= h_fp;
            }

            // Laser vs Asteroid collision
            let ax = self.asteroids[i].x >> FP_SHIFT;
            let ay = self.asteroids[i].y >> FP_SHIFT;
            let rad = self.asteroids[i].radius;

            for laser in self.lasers.iter_mut() {
                if !laser.active {
                    continue;
                }
                let lx = laser.x >> FP_SHIFT;
                let ly = laser.y >> FP_SHIFT;
                let dx = ax - lx;
                let dy = ay - ly;
                if dx * dx + dy * dy <= rad * rad {
                    laser.active = false;
                    self.hit_asteroid(i);
                    break;
                }
            }

            // Ship vs Asteroid collision
            if self.ship.alive && self.ship.invuln == 0 && self.asteroids[i].active {
                let sx = self.ship.x >> FP_SHIFT;
                let sy = self.ship.y >> FP_SHIFT;
                let dx = ax - sx;
                let dy = ay - sy;
                if dx * dx + dy * dy <= (rad + 8) * (rad + 8) {
                    self.ship_hit();
                }
            }
        }

        // Next wave if all asteroids cleared
        if active_count == 0 && !self.game_over {
            self.spawn_wave(self.wave + 1);
            let ctx = steam::steam_context();
            ctx.unlock_achievement("ACH_SURVIVOR");
        }

        // Particles
        for p in self.particles.iter_mut() {
            if !p.active {
                continue;
            }
            p.x += p.vx;
            p.y += p.vy;
            if p.life == 0 {
                p.active = false;
            } else {
                p.life -= 1;
            }
        }
    }

    fn hit_asteroid(&mut self, index: usize) {
        let a = self.asteroids[index];
        self.asteroids[index].active = false;

        self.spawn_particles(a.x, a.y, 8, COL_ASTEROID);
        self.play_tone(160, 60);

        let points = match a.stage {
            3 => 20,
            2 => 50,
            _ => 100,
        };
        self.score += points;
        if self.score > self.high_score {
            self.high_score = self.score;
        }

        // Steam stats and achievements
        let ctx = steam::steam_context();
        let cur_hit = ctx.get_stat("AsteroidsDestroyed").unwrap_or(0);
        ctx.set_stat("AsteroidsDestroyed", cur_hit + 1);
        ctx.set_stat("HighScore", self.high_score as i32);

        // Sharpshooter unlock
        ctx.unlock_achievement("ACH_SHARPSHOOTER");

        // Space Ace unlock (1,000 points)
        if self.score >= 1000 {
            ctx.unlock_achievement("ACH_SPACE_ACE");
        }

        // Split into 2 smaller asteroids if stage > 1
        if a.stage > 1 {
            let next_stage = a.stage - 1;
            let next_radius = if next_stage == 2 { 16 } else { 9 };
            let mut spawned = 0;
            for slot in self.asteroids.iter_mut() {
                if !slot.active {
                    let dir = if spawned == 0 { 64 } else { 192 };
                    let rot_ang = (a.angle + dir) % 256;
                    let speed = 2 * ONE;
                    let nvx = a.vx / 2 + (turn::sine(rot_ang) * (speed >> 8) >> 8);
                    let nvy = a.vy / 2 - (turn::cosine(rot_ang) * (speed >> 8) >> 8);

                    *slot = Asteroid {
                        x: a.x,
                        y: a.y,
                        vx: nvx,
                        vy: nvy,
                        radius: next_radius,
                        stage: next_stage,
                        angle: rot_ang,
                        rot_speed: if spawned == 0 { 3 } else { -3 },
                        seed: a.seed.wrapping_add((spawned + 1) * 31),
                        active: true,
                    };
                    spawned += 1;
                    if spawned >= 2 {
                        break;
                    }
                }
            }
        }
    }

    fn ship_hit(&mut self) {
        self.spawn_particles(self.ship.x, self.ship.y, 24, COL_THRUST);
        self.play_tone(220, 100);
        if self.lives > 1 {
            self.lives -= 1;
            self.ship.x = (self.width as i32 / 2) << FP_SHIFT;
            self.ship.y = (self.height as i32 / 2) << FP_SHIFT;
            self.ship.vx = 0;
            self.ship.vy = 0;
            self.ship.invuln = 90;
        } else {
            self.lives = 0;
            self.ship.alive = false;
            self.game_over = true;
        }
    }

    pub fn restart(&mut self) {
        self.score = 0;
        self.lives = 3;
        self.game_over = false;
        self.ship.alive = true;
        self.ship.x = (self.width as i32 / 2) << FP_SHIFT;
        self.ship.y = (self.height as i32 / 2) << FP_SHIFT;
        self.ship.vx = 0;
        self.ship.vy = 0;
        self.ship.invuln = 60;
        self.spawn_wave(1);
    }
}

impl App for SpacewarGame {
    fn draw(&mut self, canvas: &mut Canvas) {
        let (w, h) = (canvas.width(), canvas.height());
        self.width = w;
        self.height = h;

        // Clear deep space background
        canvas.fill(Rect::new(0, 0, w, h), COL_SPACE);

        // Draw twinkling starfield
        for (i, star) in self.stars.iter().enumerate() {
            let sx = star.x % w;
            let sy = star.y % h;
            let twinkle_val = ((self.tick_count + i as u32 * 5) % 30) as u8;
            let b = (star.brightness as u32 + twinkle_val as u32).min(255) as u8;
            let col = (b as u32) | ((b as u32) << 8) | ((b as u32) << 16);
            canvas.set(sx, sy, Colour(col));
        }

        // Draw Asteroids
        for a in self.asteroids.iter() {
            if !a.active {
                continue;
            }
            let cx = a.x >> FP_SHIFT;
            let cy = a.y >> FP_SHIFT;
            let rad = a.radius;

            // Draw jagged polygon points
            const NUM_PTS: usize = 8;
            let mut pts = [(0i32, 0i32); NUM_PTS];
            for k in 0..NUM_PTS {
                let ang = (a.angle + (k * 256 / NUM_PTS)) % 256;
                let var = if (k ^ (a.seed as usize)) % 2 == 0 {
                    rad * 4 / 5
                } else {
                    rad * 6 / 5
                };
                let px = cx + (turn::sine(ang) * var >> 16);
                let py = cy - (turn::cosine(ang) * var >> 16);
                pts[k] = (px, py);
            }

            for k in 0..NUM_PTS {
                let (x0, y0) = pts[k];
                let (x1, y1) = pts[(k + 1) % NUM_PTS];
                canvas.line_aa(x0, y0, x1, y1, COL_ASTEROID);
            }
            canvas.circle_aa(cx, cy, (rad / 2) as u32, COL_ASTEROID_EDGE);
        }

        // Draw Lasers
        for laser in self.lasers.iter() {
            if !laser.active {
                continue;
            }
            let lx = laser.x >> FP_SHIFT;
            let ly = laser.y >> FP_SHIFT;
            let tail_x = (laser.x - laser.vx * 2) >> FP_SHIFT;
            let tail_y = (laser.y - laser.vy * 2) >> FP_SHIFT;
            canvas.line_aa(tail_x, tail_y, lx, ly, COL_LASER);
        }

        // Draw Particles
        for p in self.particles.iter() {
            if !p.active {
                continue;
            }
            let px = p.x >> FP_SHIFT;
            let py = p.y >> FP_SHIFT;
            canvas.set(px as u32, py as u32, p.color);
        }

        // Draw Player Ship
        if self.ship.alive {
            let flicker = (self.ship.invuln > 0) && (self.tick_count % 4 < 2);
            if !flicker {
                let sx = self.ship.x >> FP_SHIFT;
                let sy = self.ship.y >> FP_SHIFT;
                let ang = self.ship.angle;

                let sin = turn::sine(ang);
                let cos = turn::cosine(ang);

                // Nose tip
                let nx = sx + (sin * 14 >> 16);
                let ny = sy - (cos * 14 >> 16);

                // Left wing
                let l_ang = (ang + 105) % 256;
                let lx = sx + (turn::sine(l_ang) * 11 >> 16);
                let ly = sy - (turn::cosine(l_ang) * 11 >> 16);

                // Right wing
                let r_ang = (ang + 151) % 256;
                let rx = sx + (turn::sine(r_ang) * 11 >> 16);
                let ry = sy - (turn::cosine(r_ang) * 11 >> 16);

                // Ship center hull
                canvas.line_aa(nx, ny, lx, ly, COL_SHIP);
                canvas.line_aa(lx, ly, sx, sy, COL_SHIP_EDGE);
                canvas.line_aa(sx, sy, rx, ry, COL_SHIP_EDGE);
                canvas.line_aa(rx, ry, nx, ny, COL_SHIP);

                // Thruster flame
                if self.ship.thrust && self.tick_count % 4 < 3 {
                    let fx = sx - (sin * 10 >> 16);
                    let fy = sy + (cos * 10 >> 16);
                    canvas.line_aa(lx, ly, fx, fy, COL_THRUST);
                    canvas.line_aa(rx, ry, fx, fy, COL_THRUST);
                }
            }
        }

        // Draw HUD Bar
        let hud_bg = Rect::new(0, 0, w, 28);
        canvas.fill(hud_bg, Colour(0x000B_1120));
        canvas.outline(hud_bg, 1, Colour(0x001E_293B));

        let score_txt = format!("SCORE: {:06}", self.score);
        canvas.text(12, 6, &score_txt, COL_HUD_ACCENT);

        let high_txt = format!("HIGH: {:06}", self.high_score);
        canvas.text(150, 6, &high_txt, COL_HUD_TEXT);

        let wave_txt = format!("WAVE: {}", self.wave);
        canvas.text(280, 6, &wave_txt, COL_STEAM_GOLD);

        let lives_txt = format!("SHIPS: {}", self.lives);
        canvas.text(370, 6, &lives_txt, COL_HUD_TEXT);

        let steam_txt = "STEAM: ONLINE [AppID 480]";
        canvas.text(w.saturating_sub(230), 6, steam_txt, Colour(0x0010_B981));

        // Bottom Controls Hint
        let footer_bg = Rect::new(0, h.saturating_sub(22), w, 22);
        canvas.fill(footer_bg, Colour(0x000B_1120));
        canvas.text(
            12,
            h.saturating_sub(18),
            "A/D: Turn  |  W: Thrust  |  Space: Fire  |  P: Pause  |  R: Restart  |  Esc: Exit",
            Colour(0x0064_748B),
        );

        // Game Over or Paused Overlay
        if self.game_over {
            let panel_w = 320;
            let panel_h = 130;
            let px = (w.saturating_sub(panel_w)) / 2;
            let py = (h.saturating_sub(panel_h)) / 2;
            let r = Rect::new(px, py, panel_w, panel_h);
            canvas.panel(r, 8, Colour(0x000F_172A), Colour(0x00EF_4444));

            canvas.text_centred(
                Rect::new(px, py + 16, panel_w, 20),
                "MISSION FAILED",
                Colour(0x00EF_4444),
            );
            let final_txt = format!("Final Score: {}", self.score);
            canvas.text_centred(
                Rect::new(px, py + 46, panel_w, 20),
                &final_txt,
                COL_HUD_TEXT,
            );
            canvas.text_centred(
                Rect::new(px, py + 78, panel_w, 20),
                "Press [R] to Respawn  |  [Esc] to Exit",
                Colour(0x0038_BDF8),
            );
        } else if self.paused {
            let px = (w.saturating_sub(220)) / 2;
            let py = (h.saturating_sub(70)) / 2;
            let r = Rect::new(px, py, 220, 70);
            canvas.panel(r, 6, Colour(0x000F_172A), COL_HUD_ACCENT);
            canvas.text_centred(Rect::new(px, py + 14, 220, 20), "PAUSED", COL_HUD_TEXT);
            canvas.text_centred(
                Rect::new(px, py + 38, 220, 20),
                "Press [P] to Resume",
                Colour(0x0094_A3B8),
            );
        }

        // Draw Steam Toast Notification Overlay Banner
        if let Some(toast) = self.active_toast {
            let tw = 270u32;
            let th = 56u32;
            let tx = w.saturating_sub(tw + 16);
            let ty = (h as i32 - 40 - th as i32 + self.toast_anim_y).max(10) as u32;

            let toast_rect = Rect::new(tx, ty, tw, th);
            canvas.panel(toast_rect, 6, COL_STEAM_BG, COL_STEAM_BORDER);

            // Steam Icon Badge
            canvas.fill_rounded(Rect::new(tx + 8, ty + 10, 36, 36), 4, Colour(0x001E_3A8A));
            canvas.text(tx + 16, ty + 18, "*", COL_STEAM_GOLD);

            canvas.text(tx + 52, ty + 8, "ACHIEVEMENT UNLOCKED!", COL_STEAM_GOLD);
            canvas.text(tx + 52, ty + 24, toast.name_str(), COL_HUD_TEXT);
            canvas.text(tx + 52, ty + 38, toast.desc_str(), Colour(0x0094_A3B8));
        }
    }

    fn key(&mut self, key: Key) -> bool {
        match key {
            Key::Character('a') | Key::Character('A') | Key::Move(Movement::Left) => {
                self.ship.angle = (self.ship.angle + 256 - 8) % 256;
                true
            }
            Key::Character('d') | Key::Character('D') | Key::Move(Movement::Right) => {
                self.ship.angle = (self.ship.angle + 8) % 256;
                true
            }
            Key::Character('w') | Key::Character('W') | Key::Move(Movement::Up) => {
                self.ship.thrust = true;
                true
            }
            Key::Character(' ') | Key::Enter => {
                self.fire_laser();
                true
            }
            Key::Character('p') | Key::Character('P') => {
                self.paused = !self.paused;
                true
            }
            Key::Character('r') | Key::Character('R') => {
                if self.game_over {
                    self.restart();
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    fn message(&mut self, bytes: &[u8]) -> bool {
        if bytes.len() >= 12 && &bytes[..4] == b"mclk" {
            let click_x = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as i32;
            let click_y = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as i32;

            if self.game_over {
                self.restart();
                return true;
            }
            if self.paused {
                self.paused = false;
                return true;
            }
            if self.ship.alive {
                let sx = self.ship.x >> FP_SHIFT;
                let sy = self.ship.y >> FP_SHIFT;
                let dx = click_x - sx;
                let dy = click_y - sy;

                // Find angle matching direction vector (dx, dy)
                let mut best_ang = 0;
                let mut best_dot = i64::MIN;
                for a in 0..256 {
                    let sin = turn::sine(a) as i64;
                    let cos = turn::cosine(a) as i64;
                    let dot = (sin * dx as i64) - (cos * dy as i64);
                    if dot > best_dot {
                        best_dot = dot;
                        best_ang = a;
                    }
                }
                self.ship.angle = best_ang;
                self.fire_laser();
                return true;
            }
        }
        false
    }

    fn tick_ms(&mut self) -> Option<u64> {
        Some(16) // ~60 FPS
    }

    fn ticked(&mut self) -> bool {
        self.tick();
        // Turn off continuous thrust key so it requires holding or re-press
        if self.tick_count % 3 == 0 {
            self.ship.thrust = false;
        }
        true
    }

    fn damage(&mut self) -> Option<(u32, u32, u32, u32)> {
        None // Full window damage for smooth 60fps action
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
    nexus_user::log(&format!("game: PANIC: {info}")).ok();
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
        failed("game: FAILED: could not allocate heap");
        finish();
    }

    nexus_user::log("game: Nexus Spacewar starting (Steam AppID 480)").ok();

    // Check if started under compositor window
    let mut lent = [Handle(0); 1];
    match Window::open(COMPOSITOR, SURFACE_AT, &mut lent) {
        Ok((window, carried)) => {
            let sound = (carried >= 1).then(|| lent[0]);
            let mut game = SpacewarGame::new(sound);
            nexus_user::log("game: window opened with compositor, running 60 FPS loop").ok();
            window.run(&mut game);
            if let Some(s) = sound {
                nexus_user::close(s).ok();
            }
        }
        Err(_) => {
            let mut game = SpacewarGame::new(None);
            // Headless / diagnostic simulation mode
            nexus_user::log("game: running headless diagnostic verification").ok();

            // Run 60 frames of simulation
            for _ in 0..60 {
                game.ship.thrust = true;
                game.tick();
            }
            game.fire_laser();
            for _ in 0..20 {
                game.tick();
            }

            // Simulate destroying an asteroid
            game.hit_asteroid(0);

            let ctx = steam::steam_context();
            let destroyed = ctx.get_stat("AsteroidsDestroyed").unwrap_or(0);
            let first_flight = ctx.is_achievement_unlocked("ACH_FIRST_LAUNCH");
            let sharpshooter = ctx.is_achievement_unlocked("ACH_SHARPSHOOTER");

            nexus_user::log(&format!(
                "game: diagnostic complete! Score: {}, Destroyed: {}, FirstFlight: {}, Sharpshooter: {}",
                game.score, destroyed, first_flight, sharpshooter
            )).ok();
        }
    }

    finish()
}
