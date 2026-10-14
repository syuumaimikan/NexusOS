//! Window-management policy. Decorations have no close/minimize/maximize
//! buttons; these operations are compositor-owned keyboard actions.
use super::{CLIENTS, MIN_SIZE, Screen, Tile, clamp, glass_window, raise, resize};
pub const TOP_RESERVED: u32 = 40;
#[derive(Clone, Copy)]
pub enum Action {
    Maximize,
    Minimize,
    Restore,
    Tile,
    Move(i32, i32),
    Resize(i32, i32),
    Focus(bool),
}
pub fn chord(chord: u32) -> Option<Action> {
    let shifted = chord & 0x10000 != 0;
    Some(match (chord & 0xffff, shifted) {
        (0x66, _) => Action::Maximize,
        (0x6d, false) => Action::Minimize,
        (0x6d, true) => Action::Restore,
        (0x74, _) => Action::Tile,
        (0x110, true) => Action::Move(0, -24),
        (0x111, true) => Action::Move(0, 24),
        (0x112, true) => Action::Move(-24, 0),
        (0x113, true) => Action::Move(24, 0),
        (0x68, true) => Action::Resize(-24, 0),
        (0x6c, true) => Action::Resize(24, 0),
        (0x6b, true) => Action::Resize(0, -24),
        (0x6a, true) => Action::Resize(0, 24),
        (0x68 | 0x6b, false) => Action::Focus(true),
        (0x6a | 0x6c, false) => Action::Focus(false),
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
    match action {
        Action::Focus(backwards) => {
            for step in 1..=CLIENTS {
                let next = if backwards {
                    (*focus + CLIENTS - step) % CLIENTS
                } else {
                    (*focus + step) % CLIENTS
                };
                if tiles[next]
                    .as_ref()
                    .is_some_and(|tile| tile.live && !tile.minimised)
                {
                    *focus = next;
                    raise(order, next);
                    nexus_user::log("wm: keyboard focus changed").ok();
                    break;
                }
            }
        }
        Action::Restore => {
            for tile in tiles.iter_mut().flatten().filter(|tile| tile.live) {
                tile.minimised = false;
            }
            nexus_user::log("wm: restored hidden windows").ok();
        }
        Action::Tile => {
            let mut slots = [0; CLIENTS];
            let mut count = 0;
            for (index, tile) in tiles.iter().enumerate() {
                if tile
                    .as_ref()
                    .is_some_and(|tile| tile.live && !tile.minimised && !glass_window(tile))
                {
                    slots[count] = index;
                    count += 1;
                }
            }
            if count == 0 {
                return;
            }
            let gap = 12;
            let columns = if count == 1 {
                1
            } else if count <= 4 {
                2
            } else {
                3
            };
            let rows = count.div_ceil(columns);
            let width = screen.width.saturating_sub(gap * (columns as u32 + 1)) / columns as u32;
            let height = work_height.saturating_sub(gap * (rows as u32 + 1)) / rows as u32;
            if width < MIN_SIZE || height < MIN_SIZE {
                return;
            }
            let mut complete = true;
            for (position, &index) in slots[..count].iter().enumerate() {
                if let Some(tile) = tiles[index].as_mut() {
                    let x = screen.x + gap + (position % columns) as u32 * (width + gap);
                    let y = work_y + gap + (position / columns) as u32 * (height + gap);
                    complete &= fit(index, tile, x, y, width, height);
                    tile.restored = None;
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
