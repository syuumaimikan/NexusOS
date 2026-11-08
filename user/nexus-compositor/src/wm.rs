//! Window-management policy. Komorebi-inspired tiling window manager
//! with multiple layout modes (MasterStack, Grid, Columns), auto-tiling,
//! and native window controls.

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use super::{clamp, glass_window, raise, resize, Screen, Tile, CLIENTS, MIN_SIZE};

pub const TOP_RESERVED: u32 = 40;

/// Available Komorebi-style tiling layout modes.
#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum LayoutMode {
    Grid,
    MasterStack,
    Columns,
}

static CURRENT_LAYOUT: AtomicU8 = AtomicU8::new(0); // 0 = Grid, 1 = MasterStack, 2 = Columns
static AUTO_TILE: AtomicBool = AtomicBool::new(false);
static CURRENT_WORKSPACE: AtomicU8 = AtomicU8::new(0); // 0 = WS1, 1 = WS2, 2 = WS3, 3 = WS4
static DOCK_AUTOHIDE: AtomicBool = AtomicBool::new(false);

/// Current active workspace index (0..=3).
pub fn current_workspace() -> u8 {
    CURRENT_WORKSPACE.load(Ordering::Relaxed)
}

/// Set active workspace index clamped to 0..=3.
pub fn set_workspace(ws: u8) {
    CURRENT_WORKSPACE.store(ws.min(3), Ordering::Relaxed);
}

/// Whether dock auto-hide is enabled.
pub fn is_dock_autohide_enabled() -> bool {
    DOCK_AUTOHIDE.load(Ordering::Relaxed)
}

/// Set dock auto-hide mode explicitly.
pub fn set_dock_autohide(enabled: bool) {
    DOCK_AUTOHIDE.store(enabled, Ordering::Relaxed);
}

/// Toggle dock auto-hide mode and return new state.
pub fn toggle_dock_autohide() -> bool {
    let next = !DOCK_AUTOHIDE.load(Ordering::Relaxed);
    DOCK_AUTOHIDE.store(next, Ordering::Relaxed);
    next
}

/// Current active layout name for status bar display.
#[allow(dead_code)]
pub fn current_layout_name() -> &'static str {
    match CURRENT_LAYOUT.load(Ordering::Relaxed) {
        1 => "Master",
        2 => "Columns",
        _ => "Grid",
    }
}

/// Whether automatic tiling on window lifecycle events is active.
pub fn is_auto_tile_enabled() -> bool {
    AUTO_TILE.load(Ordering::Relaxed)
}

#[derive(Clone, Copy)]
pub enum Action {
    Maximize,
    Minimize,
    Restore,
    Tile,
    ToggleAutoTile,
    Swap,
    Move(i32, i32),
    Resize(i32, i32),
    Focus(bool),
    SwitchWorkspace(u8),
    SendToWorkspace(u8),
    ToggleDockAutoHide,
}

pub fn chord(chord: u32) -> Option<Action> {
    let shifted = chord & 0x10000 != 0;
    Some(match (chord & 0xffff, shifted) {
        (0x66, _) => Action::Maximize,           // Super+F
        (0x6d, false) => Action::Minimize,       // Super+M
        (0x6d, true) => Action::Restore,         // Super+Shift+M
        (0x74, false) => Action::Tile,           // Super+T (Cycle layouts / tile)
        (0x74, true) => Action::ToggleAutoTile,  // Super+Shift+T
        (0x77, _) => Action::Swap,               // Super+W
        (0x64, _) => Action::ToggleDockAutoHide, // Super+D / Super+Shift+D
        (0x31..=0x34, false) => Action::SwitchWorkspace(((chord & 0xffff) - 0x31) as u8), // Super+1..4
        (0x31..=0x34, true) => Action::SendToWorkspace(((chord & 0xffff) - 0x31) as u8), // Super+Shift+1..4
        (0x110, true) => Action::Move(0, -24), // Super+Shift+Up
        (0x111, true) => Action::Move(0, 24),  // Super+Shift+Down
        (0x112, true) => Action::Move(-24, 0), // Super+Shift+Left
        (0x113, true) => Action::Move(24, 0),  // Super+Shift+Right
        (0x68, true) => Action::Resize(-24, 0), // Super+Shift+H
        (0x6c, true) => Action::Resize(24, 0), // Super+Shift+L
        (0x6b, true) => Action::Resize(0, -24), // Super+Shift+K
        (0x6a, true) => Action::Resize(0, 24), // Super+Shift+J
        (0x68 | 0x6b, false) => Action::Focus(true), // Super+H / Super+K
        (0x6a | 0x6c, false) => Action::Focus(false), // Super+J / Super+L
        _ => return None,
    })
}

fn fit(index: usize, tile: &mut Tile, x: u32, y: u32, width: u32, height: u32) -> bool {
    if resize(index, tile, width, height).is_err() || tile.width != width || tile.height != height {
        return false;
    }
    tile.x = x;
    tile.y = y;
    true
}

pub fn apply(
    action: Action,
    screen: &Screen,
    tiles: &mut [Option<Tile>; CLIENTS],
    focus: &mut usize,
    order: &mut [usize; CLIENTS],
) {
    let work_y = screen.y + TOP_RESERVED;
    let work_height = screen.usable_height().saturating_sub(TOP_RESERVED);
    let cur_ws = current_workspace();
    match action {
        Action::ToggleDockAutoHide => {
            let enabled = toggle_dock_autohide();
            if enabled {
                nexus_user::log("wm: dock auto-hide enabled").ok();
            } else {
                nexus_user::log("wm: dock auto-hide disabled").ok();
            }
        }
        Action::SwitchWorkspace(ws) => {
            let target_ws = ws.min(3);
            set_workspace(target_ws);
            // Switch focus to front-most visible window in target workspace, if any
            for &idx in order.iter().rev() {
                if let Some(Some(t)) = tiles.get(idx) {
                    if t.live && !t.minimised && t.workspace == target_ws {
                        *focus = idx;
                        break;
                    }
                }
            }
            nexus_user::log("wm: switched workspace").ok();
        }
        Action::SendToWorkspace(ws) => {
            let target_ws = ws.min(3);
            if let Some(tile) = tiles
                .get_mut(*focus)
                .and_then(Option::as_mut)
                .filter(|t| t.live)
            {
                tile.workspace = target_ws;
                nexus_user::log("wm: sent window to workspace").ok();
            }
            // If the moved window left current workspace, switch focus to another visible window here
            let cur = current_workspace();
            if tiles
                .get(*focus)
                .and_then(Option::as_ref)
                .is_some_and(|t| t.workspace != cur)
            {
                for &idx in order.iter().rev() {
                    if let Some(Some(t)) = tiles.get(idx) {
                        if t.live && !t.minimised && t.workspace == cur {
                            *focus = idx;
                            break;
                        }
                    }
                }
            }
        }
        Action::Focus(backwards) => {
            for step in 1..=CLIENTS {
                let next = if backwards {
                    (*focus + CLIENTS - step) % CLIENTS
                } else {
                    (*focus + step) % CLIENTS
                };
                if tiles[next]
                    .as_ref()
                    .is_some_and(|tile| tile.live && !tile.minimised && tile.workspace == cur_ws)
                {
                    *focus = next;
                    raise(order, next);
                    nexus_user::log("wm: keyboard focus changed").ok();
                    break;
                }
            }
        }
        Action::Restore => {
            for tile in tiles
                .iter_mut()
                .flatten()
                .filter(|tile| tile.live && tile.workspace == cur_ws)
            {
                tile.minimised = false;
            }
            nexus_user::log("wm: restored hidden windows").ok();
        }
        Action::ToggleAutoTile => {
            let next = !AUTO_TILE.load(Ordering::Relaxed);
            AUTO_TILE.store(next, Ordering::Relaxed);
            if next {
                nexus_user::log("wm: automatic tiling enabled").ok();
                apply(Action::Tile, screen, tiles, focus, order);
            } else {
                nexus_user::log("wm: automatic tiling disabled").ok();
            }
        }
        Action::Swap => {
            // Find another visible window and swap positions
            let current = *focus;
            let other = tiles.iter().enumerate().position(|(idx, tile)| {
                idx != current
                    && tile.as_ref().is_some_and(|t| {
                        t.live && !t.minimised && t.workspace == cur_ws && !glass_window(t)
                    })
            });
            if let Some(other_idx) = other {
                if let (Some(t1), Some(t2)) = (&tiles[current], &tiles[other_idx]) {
                    let (x1, y1, w1, h1) = (t1.x, t1.y, t1.width, t1.height);
                    let (x2, y2, w2, h2) = (t2.x, t2.y, t2.width, t2.height);
                    if let Some(t1) = tiles[current].as_mut() {
                        fit(current, t1, x2, y2, w2, h2);
                    }
                    if let Some(t2) = tiles[other_idx].as_mut() {
                        fit(other_idx, t2, x1, y1, w1, h1);
                    }
                    nexus_user::log("wm: swapped window positions").ok();
                }
            }
        }
        Action::Tile => {
            let mut slots = [0; CLIENTS];
            let mut count = 0;
            for (index, tile) in tiles.iter().enumerate() {
                if tile.as_ref().is_some_and(|tile| {
                    tile.live && !tile.minimised && tile.workspace == cur_ws && !glass_window(tile)
                }) {
                    slots[count] = index;
                    count += 1;
                }
            }
            if count == 0 {
                return;
            }

            let mode_val = CURRENT_LAYOUT.load(Ordering::Relaxed);
            let gap = 12;

            let mut complete = true;

            match mode_val {
                1 => {
                    // Master-Stack Layout: primary window on left (58%), remaining on right
                    if count == 1 {
                        let width = screen.width.saturating_sub(gap * 2);
                        let height = work_height.saturating_sub(gap * 2);
                        if let Some(tile) = tiles[slots[0]].as_mut() {
                            complete &=
                                fit(slots[0], tile, screen.x + gap, work_y + gap, width, height);
                            tile.restored = None;
                        }
                    } else {
                        let master_width = ((screen.width.saturating_sub(gap * 3)) * 58) / 100;
                        let stack_width = screen
                            .width
                            .saturating_sub(gap * 3)
                            .saturating_sub(master_width);
                        let stack_count = count - 1;
                        let stack_height = (work_height
                            .saturating_sub(gap * (stack_count as u32 + 1)))
                            / stack_count as u32;

                        // Master window
                        if let Some(tile) = tiles[slots[0]].as_mut() {
                            let master_height = work_height.saturating_sub(gap * 2);
                            complete &= fit(
                                slots[0],
                                tile,
                                screen.x + gap,
                                work_y + gap,
                                master_width,
                                master_height,
                            );
                            tile.restored = None;
                        }

                        // Stack windows
                        for (pos, &idx) in slots[1..count].iter().enumerate() {
                            if let Some(tile) = tiles[idx].as_mut() {
                                let x = screen.x + gap * 2 + master_width;
                                let y = work_y + gap + pos as u32 * (stack_height + gap);
                                complete &= fit(idx, tile, x, y, stack_width, stack_height);
                                tile.restored = None;
                            }
                        }
                    }
                }
                2 => {
                    // Columns layout: side-by-side equal columns
                    let width =
                        screen.width.saturating_sub(gap * (count as u32 + 1)) / count as u32;
                    let height = work_height.saturating_sub(gap * 2);
                    for (pos, &idx) in slots[..count].iter().enumerate() {
                        if let Some(tile) = tiles[idx].as_mut() {
                            let x = screen.x + gap + pos as u32 * (width + gap);
                            let y = work_y + gap;
                            complete &= fit(idx, tile, x, y, width, height);
                            tile.restored = None;
                        }
                    }
                }
                _ => {
                    // Grid Layout: balanced row/column matrix
                    let columns = if count == 1 {
                        1
                    } else if count <= 4 {
                        2
                    } else {
                        3
                    };
                    let rows = count.div_ceil(columns);
                    let width =
                        screen.width.saturating_sub(gap * (columns as u32 + 1)) / columns as u32;
                    let height = work_height.saturating_sub(gap * (rows as u32 + 1)) / rows as u32;
                    if width >= MIN_SIZE && height >= MIN_SIZE {
                        for (position, &index) in slots[..count].iter().enumerate() {
                            if let Some(tile) = tiles[index].as_mut() {
                                let x =
                                    screen.x + gap + (position % columns) as u32 * (width + gap);
                                let y = work_y + gap + (position / columns) as u32 * (height + gap);
                                complete &= fit(index, tile, x, y, width, height);
                                tile.restored = None;
                            }
                        }
                    }
                }
            }

            if complete {
                nexus_user::log("wm: tiled visible windows").ok();
            }
        }
        action => {
            let Some(tile) = tiles
                .get_mut(*focus)
                .and_then(Option::as_mut)
                .filter(|tile| tile.live && !tile.minimised)
            else {
                return;
            };
            match action {
                Action::Minimize => {
                    tile.minimised = true;
                    nexus_user::log("wm: minimized focused window").ok();
                }
                Action::Maximize => {
                    if let Some((x, y, w, h)) = tile.restored {
                        if fit(*focus, tile, x, y, w, h) {
                            tile.restored = None;
                            nexus_user::log("wm: restored window size").ok();
                        }
                    } else {
                        let previous = (tile.x, tile.y, tile.width, tile.height);
                        if fit(
                            *focus,
                            tile,
                            screen.x + 12,
                            work_y + 12,
                            screen.width.saturating_sub(24),
                            work_height.saturating_sub(24),
                        ) {
                            tile.restored = Some(previous);
                            nexus_user::log("wm: maximized focused window").ok();
                        }
                    }
                }
                Action::Move(dx, dy) => {
                    tile.x = clamp(
                        tile.x as i64 + dx as i64,
                        screen.x,
                        screen.x + screen.width.saturating_sub(tile.width),
                    );
                    tile.y = clamp(
                        tile.y as i64 + dy as i64,
                        work_y,
                        work_y + work_height.saturating_sub(tile.height),
                    );
                    tile.restored = None;
                    nexus_user::log("wm: moved focused window").ok();
                }
                Action::Resize(dx, dy) => {
                    let width = clamp(
                        tile.width as i64 + dx as i64,
                        MIN_SIZE,
                        screen.width.saturating_sub(tile.x - screen.x).max(MIN_SIZE),
                    );
                    let height = clamp(
                        tile.height as i64 + dy as i64,
                        MIN_SIZE,
                        screen
                            .usable_height()
                            .saturating_sub(tile.y - screen.y)
                            .max(MIN_SIZE),
                    );
                    if fit(*focus, tile, tile.x, tile.y, width, height) {
                        tile.restored = None;
                        nexus_user::log("wm: resized focused window").ok();
                    }
                }
                _ => {}
            }
        }
    }
}
