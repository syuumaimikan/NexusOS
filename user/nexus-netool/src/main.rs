//! `netool`: a window for finding out what is on the network.
//!
//! What a person actually wants to know when a network is not behaving: what
//! this machine's own addressing is, whether anything is at an address, and
//! what it is running. Three questions, and a browser answers none of them.
//!
//! # What it does
//!
//! - Says this machine's address, gateway and resolver, which is the first
//!   thing to check and the thing people most often assume.
//! - Connects to a list of ports on a list of addresses and reports each one as
//!   open, refused or silent.
//! - Reads the first thing an open port says, and reports **what it said**
//!   rather than what its number conventionally means. A web server on 22 is
//!   exactly the sort of thing worth finding, and a port-number table would
//!   report it as ssh with total confidence.
//!
//! # What it is not
//!
//! Not a half-open scanner. This machine's network service offers connections,
//! not packets, so every open port found here has had a real connection made to
//! it and closed again -- which anything listening will have noticed and very
//! possibly logged. Said here because somebody will otherwise wonder why their
//! own server logged them.
//!
//! No exploitation of anything, and no credentials. It connects, reads what is
//! offered unprompted, and disconnects. Deciding what to do about what it finds
//! is a person's job.
//!
//! # Why the deciding is in another crate
//!
//! `shared/nexus-netscan` holds the judgements -- what a state means, what a
//! greeting says, what a range of addresses covers -- and has twenty tests on
//! the build machine. What is left here is a window: text, a keyboard, and a
//! loop that keeps one connection in flight at a time.
//!
//! One at a time on purpose. The network service gives out connection
//! identifiers and a scan of a /24 is 254 addresses; opening them all at once
//! would be a program asking a kernel service for two hundred and fifty-four
//! things at once to save a few seconds.

#![no_std]
#![no_main]

extern crate alloc;

use core::panic::PanicInfo;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use nexus_netscan as scan;
use nexus_ui::{Canvas, Colour, Column, Rect};
use nexus_user::Handle;
use nexus_window::{App, Key, Movement, Window};

#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// The channel to the compositor, which is also this program's parent.
const COMPOSITOR: Handle = Handle(1);

/// Where the surface is mapped.
const SURFACE_AT: usize = 0x0000_0000_1000_0000;

/// How long to let a connection sit in `Connecting` before calling it silence.
///
/// Two seconds. Long enough for a round trip and an ARP on a cold cache, short
/// enough that a /24 of dead addresses finishes while somebody is still
/// watching. A scanner's patience is the only number that decides how long it
/// takes, and it is here rather than buried.
const PATIENCE_MS: u64 = 2_000;

/// How often to look at the connection in flight.
const POLL_MS: u64 = 50;

/// How long to let an open port say something before giving up on a greeting.
///
/// Half a second. Plenty of services say nothing until spoken to -- HTTP is one
/// -- and waiting on those is waiting on something that will never come.
const GREETING_MS: u64 = 500;

/// How many findings to keep. A /24 by twenty ports is five thousand lines and
/// nobody reads those; the interesting ones are the ones that answered.
const MOST: usize = 400;

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

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    nexus_user::log(&format!("netool: PANIC: {info}")).ok();
    nexus_user::exit_with(101)
}

/// What the window is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Doing {
    /// Waiting for somebody to type a target.
    Asking,
    /// Working through the list.
    Scanning,
    /// Finished, with the findings on screen.
    Done,
}

/// One connection in flight.
struct InFlight {
    id: u32,
    address: scan::Address,
    port: u16,
    waited: u64,
    /// Set once it opened, so that the greeting wait is separate from the
    /// connection wait: a port that took a second to open still gets its half
    /// second to say something.
    opened_for: Option<u64>,
    greeting: Vec<u8>,
}

struct Netool {
    service: Option<Handle>,
    here: Option<nexus_netclient::Where>,
    doing: Doing,
    /// Which field the typing goes into.
    editing: Field,
    target: String,
    port_text: String,
    /// What is left to try, newest last.
    queue: Vec<(scan::Address, u16)>,
    done: usize,
    total: usize,
    flight: Option<InFlight>,
    found: Vec<(scan::Address, scan::Port)>,
    /// Addresses that answered at all, to report separately from open ports.
    machines: Vec<scan::Address>,
    said: String,
    scroll: usize,
    running: bool,
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Target,
    Ports,
}

/// What the network service said, in this crate's own words.
///
/// The conversion lives here and only here, so that `nexus-netscan` can be
/// tested on a machine where neither the service nor its handle type exists.
fn progress(state: nexus_netclient::State) -> scan::Progress {
    match state {
        nexus_netclient::State::Connecting => scan::Progress::Connecting,
        nexus_netclient::State::Open => scan::Progress::Open,
        nexus_netclient::State::PeerDone => scan::Progress::PeerDone,
        nexus_netclient::State::Closed => scan::Progress::Closed,
        nexus_netclient::State::Unknown => scan::Progress::Unknown,
    }
}

impl Netool {
    fn new(service: Option<Handle>, width: u32, height: u32) -> Self {
        let here = service.and_then(|handle| nexus_netclient::interface(handle).ok());
        let said = match (service, here) {
            (None, _) => nexus_i18n::text("net.nonetwork").to_string(),
            (Some(_), None) => nexus_i18n::text("net.noaddress").to_string(),
            (Some(_), Some(_)) => nexus_i18n::text("net.ready").to_string(),
        };
        // The machine's own network, pre-filled, because it is what somebody
        // wants nine times in ten and typing it out is the boring part.
        let target = match here {
            Some(where_) if where_.address != [0, 0, 0, 0] => {
                format!("{}/24", scan::show(where_.address))
            }
            _ => String::new(),
        };
        Self {
            service,
            here,
            doing: Doing::Asking,
            editing: Field::Target,
            target,
            port_text: String::new(),
            queue: Vec::new(),
            done: 0,
            total: 0,
            flight: None,
            found: Vec::new(),
            machines: Vec::new(),
            said,
            scroll: 0,
            running: true,
            width,
            height,
        }
    }

    /// Turn what was typed into a list of things to try.
    fn begin(&mut self) {
        let Some(_) = self.service else {
            self.said = nexus_i18n::text("net.nonetwork").to_string();
            return;
        };
        let target = self.target.trim();
        // One address, or a network. Both are ordinary ways to say it, and
        // which was meant is decided by whether a prefix is written rather than
        // by a mode somebody has to remember to be in.
        let addresses = if target.contains('/') {
            scan::network(target)
        } else {
            scan::address(target).map(|one| alloc::vec![one])
        };
        let Some(addresses) = addresses else {
            self.said = nexus_i18n::format("net.badtarget", &[("what", &target)]);
            return;
        };
        let Some(ports) = scan::ports(&self.port_text) else {
            self.said = nexus_i18n::format("net.badports", &[("what", &self.port_text.trim())]);
            return;
        };

        self.queue.clear();
        // Built back to front, because the queue is popped from the end and a
        // scan that worked backwards through a network would be a scan nobody
        // could follow.
        for address in addresses.iter().rev() {
            for port in ports.iter().rev() {
                self.queue.push((*address, *port));
            }
        }
        self.total = self.queue.len();
        self.done = 0;
        self.found.clear();
        self.machines.clear();
        self.scroll = 0;
        self.doing = Doing::Scanning;
        self.said = nexus_i18n::format(
            "net.started",
            &[("hosts", &addresses.len()), ("ports", &ports.len())],
        );
        nexus_user::log(&format!(
            "netool: scanning {} address(es) on {} port(s)",
            addresses.len(),
            ports.len()
        ))
        .ok();
    }

    /// Record one answer and let the connection go.
    fn record(
        &mut self,
        address: scan::Address,
        port: u16,
        finding: scan::Finding,
        greeting: Vec<u8>,
    ) {
        if finding.proves_a_machine() && !self.machines.contains(&address) {
            self.machines.push(address);
        }
        if finding != scan::Finding::Silent && self.found.len() < MOST {
            let greeting = if greeting.is_empty() {
                None
            } else {
                // Lossy on purpose: a greeting is bytes from a machine
                // somewhere else and may be anything at all. Refusing to show
                // a banner because it is not UTF-8 would hide the answer.
                Some(String::from_utf8_lossy(&greeting).into_owned())
            };
            self.found.push((
                address,
                scan::Port {
                    number: port,
                    finding,
                    greeting,
                },
            ));
        }
        self.done += 1;
    }

    /// Start the next connection, if there is one and nothing is in flight.
    fn next(&mut self) {
        if self.flight.is_some() {
            return;
        }
        let Some(service) = self.service else {
            self.doing = Doing::Done;
            return;
        };
        let Some((address, port)) = self.queue.pop() else {
            self.finish();
            return;
        };
        match nexus_netclient::open(service, address, port) {
            Ok(id) => {
                self.flight = Some(InFlight {
                    id,
                    address,
                    port,
                    waited: 0,
                    opened_for: None,
                    greeting: Vec::new(),
                });
            }
            Err(_) => {
                // The service would not even start it. Counted as silence
                // rather than dropped, so that the totals still add up -- a
                // progress bar that quietly skips things is a progress bar that
                // lies about how much is left.
                self.record(address, port, scan::Finding::Silent, Vec::new());
            }
        }
    }

    fn finish(&mut self) {
        self.doing = Doing::Done;
        let open = self
            .found
            .iter()
            .filter(|(_, port)| port.finding == scan::Finding::Open)
            .count();
        self.said = nexus_i18n::format(
            "net.finished",
            &[
                ("tried", &self.total),
                ("machines", &self.machines.len()),
                ("open", &open),
            ],
        );
        nexus_user::log(&format!(
            "netool: tried {}, {} machine(s) answered, {} port(s) open",
            self.total,
            self.machines.len(),
            open
        ))
        .ok();
    }

    /// One turn of the scan. Returns whether anything on screen changed.
    fn step(&mut self) -> bool {
        if self.doing != Doing::Scanning {
            return false;
        }
        let Some(service) = self.service else {
            return false;
        };
        self.next();
        let Some(flight) = self.flight.as_mut() else {
            return self.doing == Doing::Done;
        };

        flight.waited += POLL_MS;
        let (state, _, bytes) = match nexus_netclient::read(service, flight.id) {
            Ok(answer) => answer,
            Err(_) => (nexus_netclient::State::Unknown, 0, Vec::new()),
        };
        flight.greeting.extend_from_slice(&bytes);

        // Once it is open, the question changes from "will it answer" to "will
        // it say anything", and those get separate patience.
        if matches!(
            state,
            nexus_netclient::State::Open | nexus_netclient::State::PeerDone
        ) {
            let so_far = flight.opened_for.unwrap_or(0) + POLL_MS;
            flight.opened_for = Some(so_far);
            let heard_enough = !flight.greeting.is_empty() || so_far >= GREETING_MS;
            if !heard_enough && state == nexus_netclient::State::Open {
                return false;
            }
            let (address, port, greeting, id) = (
                flight.address,
                flight.port,
                core::mem::take(&mut flight.greeting),
                flight.id,
            );
            self.flight = None;
            nexus_netclient::close(service, id).ok();
            self.record(address, port, scan::Finding::Open, greeting);
            return true;
        }

        let Some(finding) = scan::conclude(progress(state), flight.waited, PATIENCE_MS) else {
            return false;
        };
        let (address, port, id) = (flight.address, flight.port, flight.id);
        self.flight = None;
        nexus_netclient::close(service, id).ok();
        self.record(address, port, finding, Vec::new());
        true
    }

    fn stop(&mut self) {
        if let (Some(service), Some(flight)) = (self.service, self.flight.as_ref()) {
            nexus_netclient::close(service, flight.id).ok();
        }
        self.flight = None;
        self.queue.clear();
        self.doing = Doing::Done;
        self.said = nexus_i18n::text("net.stopped").to_string();
    }

    /// How many lines of findings fit below the header.
    fn visible(&self) -> usize {
        let top = 150;
        if self.height <= top + 20 {
            return 1;
        }
        ((self.height - top) / 18).max(1) as usize
    }
}

mod paint {
    use super::*;

    pub const BACKGROUND: Colour = Colour(0x0008_0F1C);
    pub const PANEL: Colour = Colour(0x0011_1B2E);
    pub const EDGE: Colour = Colour(0x001E_2F4A);
    pub const TEXT: Colour = Colour(0x00D8_E2F0);
    pub const DIM: Colour = Colour(0x0078_8AA4);
    pub const OPEN: Colour = Colour(0x005C_E86A);
    pub const REFUSED: Colour = Colour(0x00E8_B45C);
    pub const FOCUS: Colour = Colour(0x005C_A8E8);
}

impl App for Netool {
    fn draw(&mut self, canvas: &mut Canvas) {
        let width = canvas.width();
        let height = canvas.height();
        canvas.fill(Rect::new(0, 0, width, height), paint::BACKGROUND);

        // What this machine is, which is the question people skip and then
        // spend an hour on.
        let header = Rect::new(8, 8, width.saturating_sub(16), 60);
        canvas.panel(header, 6, paint::PANEL, paint::EDGE);
        let mine = match self.here {
            Some(where_) => nexus_i18n::format(
                "net.here",
                &[
                    ("address", &scan::show(where_.address)),
                    ("gateway", &scan::show(where_.gateway)),
                    ("resolver", &scan::show(where_.resolver)),
                ],
            ),
            None => nexus_i18n::text("net.nowhere").to_string(),
        };
        canvas.text(18, 18, nexus_i18n::text("net.title"), paint::TEXT);
        canvas.text(18, 40, &mine, paint::DIM);

        // What to scan.
        let fields = Rect::new(8, 76, width.saturating_sub(16), 62);
        canvas.panel(fields, 6, paint::PANEL, paint::EDGE);
        let target_colour = if self.editing == Field::Target {
            paint::FOCUS
        } else {
            paint::DIM
        };
        let ports_colour = if self.editing == Field::Ports {
            paint::FOCUS
        } else {
            paint::DIM
        };
        canvas.text(18, 84, nexus_i18n::text("net.target"), target_colour);
        let shown_target = if self.target.is_empty() {
            nexus_i18n::text("net.anytarget")
        } else {
            &self.target
        };
        canvas.text(130, 84, shown_target, paint::TEXT);
        canvas.text(18, 104, nexus_i18n::text("net.ports"), ports_colour);
        let shown_ports = if self.port_text.trim().is_empty() {
            nexus_i18n::text("net.usual")
        } else {
            &self.port_text
        };
        canvas.text(130, 104, shown_ports, paint::TEXT);
        canvas.text(18, 122, &self.said, paint::DIM);

        // The findings.
        let mut column = Column::new(
            Rect::new(8, 150, width.saturating_sub(16), height.saturating_sub(158)),
            0,
        );
        let visible = self.visible();
        if self.doing == Doing::Scanning {
            let left = self.total.saturating_sub(self.done);
            let row = column.row(18);
            canvas.text(
                row.x,
                row.y,
                &nexus_i18n::format("net.progress", &[("left", &left), ("of", &self.total)]),
                paint::DIM,
            );
        }
        for (address, port) in self.found.iter().skip(self.scroll).take(visible) {
            let row = column.row(18);
            if row.height == 0 {
                break;
            }
            let colour = match port.finding {
                scan::Finding::Open => paint::OPEN,
                scan::Finding::Refused => paint::REFUSED,
                scan::Finding::Silent => paint::DIM,
            };
            let text = format!("{}  {}", scan::show(*address), scan::line(port));
            canvas.text(row.x + 10, row.y, &text, colour);
        }
        if self.found.is_empty() && self.doing != Doing::Scanning {
            let row = column.row(18);
            canvas.text(
                row.x + 10,
                row.y,
                nexus_i18n::text("net.nothing"),
                paint::DIM,
            );
        }

        // The keys, at the bottom, because a window whose keys are not written
        // on it is a window with a manual somewhere else.
        if height > 26 {
            canvas.text(18, height - 20, nexus_i18n::text("net.keys"), paint::DIM);
        }
    }

    fn key(&mut self, key: Key) -> bool {
        match key {
            Key::Enter => {
                if self.doing == Doing::Scanning {
                    self.stop();
                } else {
                    self.begin();
                }
                true
            }
            Key::Escape => {
                if self.doing == Doing::Scanning {
                    self.stop();
                } else {
                    self.running = false;
                }
                true
            }
            // Left and right, and **not Tab**, which never arrives: the
            // compositor takes Tab for moving between windows, so a client that
            // bound it would find its keys going to a different window from the
            // moment it was pressed. That is what happened here, and from
            // inside this program it looked like the keyboard had stopped
            // working halfway through a line.
            Key::Move(Movement::Left) | Key::Move(Movement::Right) => {
                self.editing = match self.editing {
                    Field::Target => Field::Ports,
                    Field::Ports => Field::Target,
                };
                true
            }
            Key::Character(character) if !character.is_control() => {
                let field = match self.editing {
                    Field::Target => &mut self.target,
                    Field::Ports => &mut self.port_text,
                };
                // Bounded, because a field is a line on screen and an unbounded
                // one is a window a held-down key can push off the edge of.
                if field.chars().count() < 40 {
                    field.push(character);
                }
                true
            }
            Key::Backspace => {
                let field = match self.editing {
                    Field::Target => &mut self.target,
                    Field::Ports => &mut self.port_text,
                };
                field.pop();
                true
            }
            Key::Move(Movement::Down) => {
                if self.scroll + self.visible() < self.found.len() {
                    self.scroll += 1;
                    return true;
                }
                false
            }
            Key::Move(Movement::Up) => {
                if self.scroll > 0 {
                    self.scroll -= 1;
                    return true;
                }
                false
            }
            Key::Language => true,
            _ => false,
        }
    }

    fn tick_ms(&mut self) -> Option<u64> {
        // A clock only while there is something to do. A window that asked to
        // be woken fifty times a second for ever would cost more asleep than
        // the scan costs running.
        (self.doing == Doing::Scanning).then_some(POLL_MS)
    }

    fn ticked(&mut self) -> bool {
        self.step()
    }

    fn resized(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
    }

    fn running(&self) -> bool {
        self.running
    }
}

extern "C" fn main() -> ! {
    if !nexus_user::heap::init(nexus_user::heap::DEFAULT_SIZE) {
        nexus_user::log("netool: FAILED: could not get a heap").ok();
        nexus_user::exit_with(1);
    }

    // The surface, and the network if it was lent. A window with no network is
    // still a window: it says so, and it can still report that there is none,
    // which is a more useful answer than refusing to start.
    let mut lent = [Handle(0); 1];
    let (window, _) = match Window::open(COMPOSITOR, SURFACE_AT, &mut lent) {
        Ok(opened) => opened,
        Err(why) => {
            nexus_user::log(&format!("netool: FAILED: {why}")).ok();
            nexus_user::exit_with(1);
        }
    };
    let service = (lent[0] != Handle(0)).then_some(lent[0]);

    let (width, height) = (window.width(), window.height());
    let mut tool = Netool::new(service, width, height);
    nexus_user::log(&format!(
        "netool: a window for finding out what is on the network, {}",
        if service.is_some() {
            "with the network lent to it"
        } else {
            "with no network lent to it"
        }
    ))
    .ok();

    let outcome = window.run(&mut tool);
    nexus_user::log(&format!(
        "netool: {} frame(s), {}",
        outcome.frames, outcome.ended
    ))
    .ok();
    nexus_user::exit_with(0)
}
