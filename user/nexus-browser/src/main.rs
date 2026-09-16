//! `browse`: a window that fetches a page and shows what is in it.
//!
//! An ordinary client. It is handed a surface and draws into it, exactly as
//! every other window does, and it is handed one thing no other window is: an
//! end of the kernel's network channel. That handle is the whole of its ability
//! to reach the wire -- there is no socket call it could make instead, and a
//! copy of this program started without it would show an address bar that
//! cannot fetch anything.
//!
//! # What it can show
//!
//! Text. Headings, paragraphs, lists, quotations, preformatted blocks, and
//! links you can follow. No pictures, no styling, no scripts. That is a real
//! ceiling and it is stated rather than worked around: what this is for is
//! reading, and a great deal of the web is readable.
//!
//! # HTTPS
//!
//! Real TLS 1.3, with the certificate chain actually verified against a root
//! store this program is handed at start-up. See `shared/nexus-tls`.
//!
//! The rule it is built under, and the reason it took as long as it did: **a
//! client that does not verify certificates is worse than no client at all.**
//! It produces a padlock and protects against nothing. So there is no way to
//! reach an `https://` page here without a chain that verified, no option to
//! continue anyway, and no retry over `http://` when TLS fails -- that last one
//! being the quiet downgrade that would undo the rest of it.
//!
//! When something does go wrong, the reason is shown in words. "That
//! certificate is for another name" and "nothing here trusts who issued that
//! certificate" are different problems and somebody should be able to tell
//! which one they have.
//!
//! # What it cannot do, and will not pretend to
//!
//! Pictures, styling, scripts.
//!
//! # Keys
//!
//! The address bar has the keyboard until Enter is pressed, and the page has it
//! afterwards. In the page: the arrows and the page keys scroll, a number picks
//! the link with that number, Enter follows it, Backspace goes back, and Escape
//! returns to the address bar.

#![no_std]
#![no_main]

extern crate alloc;

mod fetch;

use alloc::format;
use alloc::string::{String, ToString as _};
use alloc::vec::Vec;
use core::panic::PanicInfo;

use fetch::{Fetch, Stage};
use nexus_html::{Block, Document};
use nexus_netclient as net;
use nexus_ui::{Canvas, Colour, Rect};
use nexus_user::Handle;

/// Where this program's allocations come from.
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// The channel to the compositor that started this program.
const COMPOSITOR: Handle = Handle(1);

/// Where the surface is mapped. This program's own choice, as every mapping is.
const SURFACE_AT: usize = 0x0000_0000_2000_0000;

/// How much heap: a page, the document it becomes, and the lines it lays out.
const HEAP: usize = 8 * 1024 * 1024;

/// The largest root store this will read into memory.
///
/// A hundred and twenty authorities is around two hundred kilobytes. The bound
/// is here because the size comes off a file and this program has eight
/// megabytes of heap for the whole of a page as well.
const MAX_ROOTS_BYTES: usize = 2 * 1024 * 1024;

/// How long the window waits before looking at the network again.
///
/// Only while something is being fetched. An idle window blocks on its channel
/// and costs nothing, which is what a window that is only showing a page should
/// cost.
const POLL_MS: u64 = 20;

/// The page a new window starts on, when this machine has no address.
///
/// It will not load, and what it does is put something in the address bar that
/// can be edited rather than an empty box. A machine with an address starts on
/// its own web server instead, which is the one page certain to be reachable.
const HOME: &str = "http://localhost/";

/// The name a machine calls itself.
///
/// Turned into this machine's own address rather than looked up, because a
/// resolver that was asked would answer about somebody else's idea of
/// `localhost` -- or, on a machine with no resolver, not answer at all.
const MYSELF: &str = "localhost";

/// What the compositor says, and what this program says back.
mod wire {
    pub const SHOWN: &[u8] = b"shown";
    pub const RESIZED: &[u8] = b"size";
    pub const DAMAGED: &[u8] = b"damaged";
}

/// What a key is, as the kernel sends it.
/// F2 saves what is on screen.
///
/// Not F1, which the kernel takes to change the interface language, and not F3,
/// which the desktop takes to open the launcher. A window may only bind what
/// nothing above it has already claimed.
const SAVE_KEY: u32 = 2;

mod key {
    pub const CHARACTER: u8 = 1;
    pub const BACKSPACE: u8 = 2;
    pub const ENTER: u8 = 3;
    pub const ESCAPE: u8 = 4;
    pub const TAB: u8 = 5;
    /// A function key, by its number: `FUNCTION` with value 2 is F2.
    pub const FUNCTION: u8 = 6;
    pub const LANGUAGE: u8 = 7;
    pub const MOVE: u8 = 8;
    pub const SIZE: usize = 5;

    /// Which way a movement key goes, in the order the kernel numbers them.
    pub const UP: u32 = 0;
    pub const DOWN: u32 = 1;
    pub const PAGE_UP: u32 = 4;
    pub const PAGE_DOWN: u32 = 5;
    pub const HOME: u32 = 6;
    pub const END: u32 = 7;
}

/// How tall the address bar is.
const BAR: u32 = 26;
/// And the line along the bottom that says what is happening.
const STATUS: u32 = 20;
/// Space around things.
const PAD: u32 = 8;

/// How many links one page will offer by number.
///
/// Two digits' worth. Past that the numbers are longer than some of the link
/// text, and a page with a hundred numbered links is a page nobody is reading
/// by number anyway.
const MAX_NUMBERED: usize = 99;

/// One line, ready to be drawn.
///
/// There is no link on it, deliberately. A line can hold several links and
/// half of another, so a line-wide "this is a link" would be a lie on most of
/// them; what marks a link is the `[7]` in front of it, which is in the text
/// and therefore wraps with it.
struct Line {
    text: String,
    x: u32,
    scale: u32,
    colour: Colour,
}

/// Everything the window knows.
struct Browser {
    service: Handle,
    resolver: net::Address,
    width: u32,
    height: u32,

    /// What is in the address bar.
    typing: String,
    /// Whether the address bar has the keyboard.
    editing: bool,
    /// The digits typed in the page, waiting for Enter.
    picking: String,

    /// This machine's own address, so `localhost` means something.
    myself: Option<net::Address>,
    /// The page being shown.
    document: Document,
    /// Its links, in the order they appear.
    links: Vec<(String, String)>,
    /// Where it came from, so links can be resolved against it.
    here: Option<nexus_http::Url>,
    /// Where this window has been, so Backspace can go back.
    history: Vec<nexus_http::Url>,

    /// What a secure fetch is verified against.
    ///
    /// Loaded once, at start-up, and shared by every fetch this window makes.
    /// `None` on a machine with no root store, which makes `https://` fail with
    /// a sentence rather than fall back to `http://`.
    trust: Option<fetch::Trust>,

    /// The fetch in progress, if any.
    fetch: Option<Fetch>,
    /// What to say along the bottom.
    status: String,

    /// The folder this window may save into, if it was lent one.
    ///
    /// The only thing on the disk this program may write, and not the documents
    /// folder: what a browser saves is somebody else's bytes under somebody
    /// else's name.
    downloads: Option<Handle>,
    /// The bytes of the page on screen, kept so they can be saved.
    ///
    /// The bytes and not the text. What was fetched may not be text at all, and
    /// a save that wrote back what the window happened to render would write a
    /// different file from the one that arrived.
    fetched: Vec<u8>,
    /// Where those bytes came from, for naming the file.
    fetched_from: Option<nexus_http::Url>,

    /// The first line of the page that is on screen.
    scroll: usize,
    /// How many lines the last draw laid out, so scrolling can be bounded.
    laid_out: usize,
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

/// Make a file under `wanted`, or under `wanted` with a number in it.
///
/// Returns the name it settled on and the open file. `None` when every name it
/// tried was taken, which is a hundred of them -- at which point the honest
/// answer is that this is not working rather than to keep counting.
fn free_name(directory: Handle, wanted: &str) -> Option<(String, Handle)> {
    /// How many numbered names to try before giving up.
    const TRIES: u32 = 100;

    if let Ok(file) = nexus_user::create(directory, wanted, nexus_user::Kind::File) {
        return Some((String::from(wanted), file));
    }

    // The number goes before the extension, so `page.html` becomes `page-1.html`
    // and not `page.html-1`: the extension is what says how to open the thing,
    // and a browser that moved it would save files nothing will open.
    let (stem, extension) = match wanted.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, extension),
        _ => (wanted, ""),
    };
    for number in 1..TRIES {
        let candidate = if extension.is_empty() {
            format!("{stem}-{number}")
        } else {
            format!("{stem}-{number}.{extension}")
        };
        if let Ok(file) = nexus_user::create(directory, &candidate, nexus_user::Kind::File) {
            return Some((candidate, file));
        }
    }
    None
}

/// A file name for what a URL fetched.
///
/// The last component of the path, with anything that is not a letter, a digit,
/// a dot, a dash or an underscore replaced. Replaced and not dropped, so two
/// different names cannot collapse into one; and bounded, because a server
/// chooses what its URLs say and a name of four hundred characters is a name
/// the filesystem will refuse in a way that is harder to read than this.
///
/// A path that ends in a separator, or has nothing in it, is a site's front
/// page and gets a name of its own rather than an empty one.
fn file_name(url: &nexus_http::Url) -> String {
    /// Long enough for anything anybody means, short enough for every
    /// filesystem this machine writes.
    const MOST: usize = 64;

    let last = url
        .path
        .rsplit('/')
        .find(|piece| !piece.is_empty())
        .unwrap_or("");
    if last.is_empty() {
        return String::from("index.html");
    }

    let mut name = String::with_capacity(last.len().min(MOST));
    for character in last.chars().take(MOST) {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
            name.push(character);
        } else {
            name.push('_');
        }
    }
    // A name that is entirely dots is `.` or `..`, which name directories that
    // already exist. Given a name of its own rather than refused, because the
    // page is real and somebody asked for it.
    if name.chars().all(|character| character == '.') {
        return String::from("index.html");
    }
    name
}

/// Read the root certificate store, and say in the log what came of it.
///
/// `None` means `https://` will not work, and every path to it says so: no root
/// store, a store that will not read, a store with nothing usable in it, or a
/// machine with no clock. That last one is the surprising member of the list
/// and belongs there -- a machine that does not know the date cannot tell a
/// current certificate from one withdrawn years ago, so it must not pretend to
/// judge one.
fn load_trust(file: Option<Handle>) -> Option<fetch::Trust> {
    let Some(file) = file else {
        nexus_user::log("browse: no root certificate store; https will not be available").ok();
        return None;
    };

    let size = nexus_user::size(file).ok()?;
    if size == 0 || size > MAX_ROOTS_BYTES {
        nexus_user::log(&format!(
            "browse: the root store is {size} bytes, which is not a size a root store is"
        ))
        .ok();
        return None;
    }
    let mut bytes = alloc::vec![0u8; size];
    let read = nexus_user::read(file, &mut bytes).ok()?;
    bytes.truncate(read);

    let loaded = match nexus_tls::roots::read(&bytes) {
        Ok(loaded) => loaded,
        Err(why) => {
            nexus_user::log(&format!("browse: the root store will not read: {why}")).ok();
            return None;
        }
    };
    if !loaded.skipped.is_empty() {
        // Every certificate in the store was parsed by this same parser when
        // the store was built, so this cannot happen without the tool and the
        // parser having drifted apart -- which is worth a line in the log.
        nexus_user::log(&format!(
            "browse: {} of {} roots would not parse, which should not happen",
            loaded.skipped.len(),
            loaded.held
        ))
        .ok();
    }
    if loaded.roots.is_empty() {
        nexus_user::log("browse: the root store is empty, so nothing can be verified").ok();
        return None;
    }

    // The wall clock, not the uptime. A certificate's validity is a pair of
    // moments and comparing them against how long this boot has lasted would
    // put every certificate in the future.
    let now = match nexus_user::now() {
        Ok(seconds) => Some(seconds as i64),
        Err(_) => {
            nexus_user::log(
                "browse: this machine has no clock, so no certificate can be judged current;                  https will not be available",
            )
            .ok();
            return None;
        }
    };

    nexus_user::log(&format!(
        "browse: {} certificate authorities loaded; https is available",
        loaded.roots.len()
    ))
    .ok();
    Some(fetch::Trust {
        roots: alloc::rc::Rc::new(loaded.roots),
        now,
    })
}

extern "C" fn main() -> ! {
    if !nexus_user::heap::init(HEAP) {
        failed("browse: FAILED: could not get a heap");
        finish();
    }

    // Three handles: the surface to draw on, the network, and the root
    // certificate store. The second is what makes this a browser rather than a
    // window with an address bar in it, and the third is what makes `https://`
    // mean something rather than look like it does.
    let mut buffer = [0u8; 32];
    let mut handles = [Handle(0); 4];
    let Ok(received) = nexus_user::receive(COMPOSITOR, &mut buffer, &mut handles) else {
        failed("browse: FAILED: nothing arrived to draw on");
        finish();
    };
    if received.handles < 1 || received.bytes < 16 {
        failed("browse: FAILED: no surface came with the message");
        finish();
    }

    let width = read_u32(&buffer, 0);
    let height = read_u32(&buffer, 4);
    let surface = handles[0];
    let service = if received.handles >= 2 {
        Some(handles[1])
    } else {
        None
    };
    // Which of the optional handles came, said in the message rather than
    // counted. Counting is what breaks: a machine with no root store but a
    // downloads folder would hand the folder over in the slot where this
    // expects certificates, and this would try to verify a connection against
    // a directory.
    let carrying = read_u32(&buffer, 8);
    let has_roots = carrying & 1 != 0;
    let has_downloads = carrying & 2 != 0;

    // The root certificate store: the file itself, read only. Not the directory
    // it is in and not the disk -- this program is the one on the machine most
    // likely to be handed something hostile, and what it can reach on the store
    // is exactly the one file it has to read to check a certificate.
    // Two, not one. Handle nought is the surface and handle one is the network
    // service; the optional pair begins after those. Starting at one made this
    // read the network service as a certificate store and the certificate file
    // as a folder, and the only sign was `create` being refused -- which is
    // exactly right, because a file is not a directory.
    let mut next = 2usize;
    let roots_file = if has_roots && received.handles > next {
        let handle = handles[next];
        next += 1;
        Some(handle)
    } else {
        None
    };
    // And a folder to save into, which is the only thing on the disk this
    // program may write. Not the documents folder and not the disk: what a
    // browser saves is somebody else's bytes under somebody else's name.
    let downloads = if has_downloads && received.handles > next {
        Some(handles[next])
    } else {
        None
    };

    let Ok(mapped) = nexus_user::memory_map(surface, SURFACE_AT, true) else {
        failed("browse: FAILED: could not map its surface");
        finish();
    };
    if width as usize * height as usize * 4 > mapped {
        failed("browse: FAILED: the surface is smaller than the size it was given");
        finish();
    }

    let Some(service) = service else {
        // A window with no network. Said rather than hidden: an address bar
        // that silently does nothing is worse than one that explains itself.
        nexus_user::log("browse: started without the network; there is nothing it can fetch").ok();
        let mut browser = Browser::new(Handle(0), width, height);
        browser.status = nexus_i18n::text("browse.nonetwork").to_string();
        browser.run(surface);
        finish();
    };

    let mut myself = None;
    let resolver = match net::interface(service) {
        Ok(found) => {
            myself = Some(found.address);
            nexus_user::log(&format!(
                "browse: this machine is {}.{}.{}.{}, resolver {}.{}.{}.{}",
                found.address[0],
                found.address[1],
                found.address[2],
                found.address[3],
                found.resolver[0],
                found.resolver[1],
                found.resolver[2],
                found.resolver[3]
            ))
            .ok();
            found.resolver
        }
        Err(error) => {
            nexus_user::log(&format!("browse: the network is not ready: {error}")).ok();
            [0, 0, 0, 0]
        }
    };

    nexus_user::log(&format!(
        "browse: a window that can fetch a page, {width}x{height}"
    ))
    .ok();
    let mut browser = Browser::new(service, width, height);
    browser.trust = load_trust(roots_file);
    browser.downloads = downloads;
    nexus_user::log(&format!(
        "browse: carrying {carrying:#x}: {} certificates, {} somewhere to save",
        if roots_file.is_some() {
            "with"
        } else {
            "without"
        },
        if downloads.is_some() {
            "with"
        } else {
            "without"
        }
    ))
    .ok();
    browser.resolver = resolver;
    browser.myself = myself;
    // This machine's own server, which is running on this machine and reached
    // through the stack rather than around it. A browser that shows it has
    // proved the whole path: the address bar, the parser, the connection, the
    // request, the server, the loop back and the reader.
    let home = myself.map_or_else(
        || HOME.to_string(),
        |address| {
            format!(
                "http://{}.{}.{}.{}/",
                address[0], address[1], address[2], address[3]
            )
        },
    );
    browser.go(home);
    browser.run(surface);
    finish()
}

impl Browser {
    fn new(service: Handle, width: u32, height: u32) -> Self {
        Self {
            service,
            resolver: [0, 0, 0, 0],
            width,
            height,
            downloads: None,
            fetched: Vec::new(),
            fetched_from: None,
            typing: String::from(HOME),
            editing: false,
            myself: None,
            picking: String::new(),
            document: Document::default(),
            links: Vec::new(),
            here: None,
            history: Vec::new(),
            trust: None,
            fetch: None,
            status: nexus_i18n::text("browse.ready").to_string(),
            scroll: 0,
            laid_out: 0,
        }
    }

    /// Draw, say so, and act on whatever comes back.
    fn run(&mut self, mut surface: Handle) {
        let Ok(set) = nexus_user::wait_set() else {
            failed("browse: FAILED: could not make a wait set");
            return;
        };
        const SAID: u64 = 1;
        if nexus_user::watch(set, COMPOSITOR, SAID).is_err() {
            failed("browse: FAILED: could not watch the compositor");
            return;
        }

        let mut stale = true;
        let mut in_flight = false;
        let mut frames = 0u32;

        // Bounded, so a window whose compositor stops answering cannot spin.
        // Large, because this is a window somebody reads with: at one turn per
        // keystroke and one per network poll it is an afternoon.
        for _ in 0..8_000_000u64 {
            if stale && !in_flight {
                self.draw(surface);
                if nexus_user::send(COMPOSITOR, wire::DAMAGED, &[]).is_err() {
                    break;
                }
                frames += 1;
                stale = false;
                in_flight = true;
            }

            // Only while something is being fetched. A window that is merely
            // being read blocks, which is what it should cost.
            let wait = if self.busy() {
                POLL_MS
            } else {
                nexus_user::FOREVER
            };
            let mut keys = [0u64; 2];
            let Ok(ready) = nexus_user::wait_any_until(set, &mut keys, wait) else {
                break;
            };

            if ready == 0 {
                if self.poll() {
                    stale = true;
                }
                continue;
            }

            let mut message = [0u8; 64];
            let mut incoming = [Handle(0); 1];
            let Ok(received) = nexus_user::receive(COMPOSITOR, &mut message, &mut incoming) else {
                break;
            };
            let bytes = &message[..received.bytes];

            if bytes == wire::SHOWN {
                in_flight = false;
                continue;
            }

            // A new surface, because the window was resized. The old one is
            // unmapped before the new one is mapped: they go to the same
            // address, which is the whole point of being able to unmap.
            if received.bytes >= 12 && bytes.starts_with(wire::RESIZED) && received.handles == 1 {
                nexus_user::memory_unmap(surface, SURFACE_AT).ok();
                nexus_user::close(surface).ok();
                surface = incoming[0];
                self.width = read_u32(bytes, 4);
                self.height = read_u32(bytes, 8);
                let Ok(mapped) = nexus_user::memory_map(surface, SURFACE_AT, true) else {
                    failed("browse: FAILED: could not map the surface it was given");
                    break;
                };
                if self.width as usize * self.height as usize * 4 > mapped {
                    failed("browse: FAILED: the new surface is smaller than its size");
                    break;
                }
                stale = true;
                continue;
            }

            if received.bytes >= key::SIZE && self.key(bytes) {
                stale = true;
            }
        }

        if let Some(fetch) = &mut self.fetch {
            fetch.abandon();
        }
        if frames > 0 {
            nexus_user::log("browse: drew a page for as long as it was asked to").ok();
        }
    }

    /// Whether a fetch is in progress.
    fn busy(&self) -> bool {
        self.fetch
            .as_ref()
            .is_some_and(|fetch| fetch.stage.is_busy())
    }

    /// Let the fetch make what progress it can, and say whether to redraw.
    fn poll(&mut self) -> bool {
        let Some(fetch) = &mut self.fetch else {
            return false;
        };
        if !fetch.step() {
            return false;
        }

        match fetch.stage.clone() {
            Stage::Done => {
                // Taken out of the fetch before anything is done with it. What
                // follows either shows the page or starts another fetch, and
                // both of those want the whole of `self`.
                let Some(mut fetch) = self.fetch.take() else {
                    return false;
                };
                let url = fetch.url.clone();
                let redirects = fetch.redirects;
                let response = core::mem::take(&mut fetch.response);

                // Somewhere else to go. Followed rather than shown, bounded so
                // that a server pointing at itself ends in a message instead of
                // a loop.
                if let Some(location) = response.redirect() {
                    let location = location.to_string();
                    if redirects >= fetch::MAX_REDIRECTS {
                        self.status = nexus_i18n::text("browse.toomany").to_string();
                        return true;
                    }
                    match nexus_http::resolve(&url, &location) {
                        Ok(next) => {
                            self.status =
                                nexus_i18n::format("browse.moved", &[("url", &next.to_text())]);
                            self.typing = next.to_text();
                            self.fetch = Some(Fetch::start(
                                self.service,
                                self.resolver,
                                next,
                                redirects + 1,
                                self.trust.clone(),
                            ));
                            return true;
                        }
                        Err(error) => {
                            self.status = format!("{error}");
                            return true;
                        }
                    }
                }

                // Kept before it is shown, because showing it turns it into
                // laid-out text and the bytes are what a save has to write.
                self.fetched = response.body.clone();
                self.fetched_from = Some(url.clone());
                self.show(&url, response.status, response.is_html(), &response.body);
                true
            }
            Stage::Failed(why) => {
                self.status = why;
                self.fetch = None;
                true
            }
            _ => true,
        }
    }

    /// Show what came back.
    fn show(&mut self, url: &nexus_http::Url, status: u16, html: bool, body: &[u8]) {
        // Everything is read as UTF-8, replacing what is not. A page in another
        // encoding comes out with some characters wrong rather than not at all,
        // which for a machine with no transcoding tables is the better half of
        // a bad choice -- and it is said in the status line rather than hidden.
        let text = String::from_utf8_lossy(body).to_string();
        self.document = if html {
            nexus_html::parse_with_preformatted(&text)
        } else {
            // Not HTML: shown as it is, which is what `text/plain` means.
            Document {
                title: None,
                blocks: text
                    .split('\n')
                    .take(nexus_html::MAX_BLOCKS)
                    .map(|line| Block::Preformatted(line.trim_end().to_string()))
                    .collect(),
            }
        };
        self.links = self.document.links();
        self.scroll = 0;

        if let Some(previous) = self.here.take() {
            if previous != *url {
                self.history.push(previous);
            }
        }
        self.here = Some(url.clone());
        self.typing = url.to_text();

        let title = self
            .document
            .title
            .clone()
            .unwrap_or_else(|| url.host.clone());
        self.status = nexus_i18n::format(
            "browse.shown",
            &[
                ("title", &title),
                ("status", &status),
                ("links", &self.links.len()),
            ],
        );
        // Said only when it is true, and true only because a chain verified --
        // there is no path to a shown `https://` page that did not. A browser
        // that showed this for any address beginning with the right five
        // letters would be teaching people to read a word that means nothing.
        if url.secure {
            self.status = format!(
                "{}  |  {}",
                nexus_i18n::format("browse.https.safe", &[("host", &url.host)]),
                self.status
            );
        }
        nexus_user::log(&format!(
            "browse: showed {} ({status}), {} blocks, {} links",
            url.to_text(),
            self.document.blocks.len(),
            self.links.len()
        ))
        .ok();
    }

    /// Start fetching what is in the address bar.
    fn go(&mut self, address: String) {
        if let Some(fetch) = &mut self.fetch {
            fetch.abandon();
        }
        match nexus_http::parse(&address) {
            Ok(mut url) => {
                // `localhost` is this machine, and asking a resolver about it
                // would be asking somebody else what this machine is called.
                if url.host == MYSELF || url.host == "127.0.0.1" {
                    if let Some(address) = self.myself {
                        url.host = format!(
                            "{}.{}.{}.{}",
                            address[0], address[1], address[2], address[3]
                        );
                    }
                }
                self.status = nexus_i18n::format("browse.fetching", &[("url", &url.to_text())]);
                self.typing = url.to_text();
                self.fetch = Some(Fetch::start(
                    self.service,
                    self.resolver,
                    url,
                    0,
                    self.trust.clone(),
                ));
                self.editing = false;
            }
            Err(error) => {
                self.status = format!("{error}");
                self.fetch = None;
            }
        }
    }

    /// Follow a link by its number.
    fn follow(&mut self, number: usize) {
        let Some(here) = self.here.clone() else {
            return;
        };
        let Some((href, _)) = self.links.get(number.wrapping_sub(1)) else {
            self.status = nexus_i18n::format("browse.nolink", &[("number", &number)]);
            return;
        };
        match nexus_http::resolve(&here, href) {
            Ok(url) => self.go(url.to_text()),
            Err(error) => self.status = format!("{error}"),
        }
    }

    /// Act on a key. Returns whether anything on screen changed.
    fn key(&mut self, message: &[u8]) -> bool {
        let kind = message[0];
        let value = read_u32(message, 1);

        if kind == key::LANGUAGE {
            if let Some(locale) = nexus_i18n::LOCALES.get(value as usize) {
                nexus_i18n::set_locale(locale.tag);
            }
            return true;
        }

        if kind == key::TAB {
            self.editing = !self.editing;
            self.picking.clear();
            return true;
        }

        // Save what was fetched. Before the split between the address bar and
        // the page, because it is about the window rather than about the
        // caret -- and because the moment somebody wants it is right after a
        // page has loaded, which is exactly when the bar still has the
        // keyboard. Putting it in the page handler was the first attempt, and
        // it meant the key did nothing at the only moment anybody would press
        // it.
        if kind == key::FUNCTION && value == SAVE_KEY {
            self.save();
            return true;
        }

        if self.editing {
            return self.key_in_bar(kind, value);
        }
        self.key_in_page(kind, value)
    }

    /// A key while the address bar has the keyboard.
    /// Write what was fetched into the downloads folder.
    ///
    /// The name comes from the URL's last path component, and everything about
    /// turning that into a filename is refusing rather than repairing. A server
    /// chooses what a URL says, so the name is somebody else's text: it may
    /// have separators in it, it may be `..`, it may be four hundred characters
    /// of nothing. A browser that cleaned such a name up and saved it anyway
    /// would be a browser that occasionally wrote where it was not asked to.
    fn save(&mut self) {
        // Every ending of this says so on the log, and not only the one that
        // worked. What the window shows is for the person in front of it; the
        // log is the only way anything outside the machine can tell that a key
        // arrived and what came of it, and a save that quietly did nothing is
        // exactly the failure worth being able to see.
        let Some(directory) = self.downloads else {
            nexus_user::log("browse: asked to save with nowhere to save to").ok();
            self.status = nexus_i18n::text("browse.nosaving").to_string();
            return;
        };
        if self.fetched.is_empty() {
            nexus_user::log("browse: asked to save with nothing fetched").ok();
            self.status = nexus_i18n::text("browse.nothingtosave").to_string();
            return;
        }
        let Some(url) = self.fetched_from.clone() else {
            nexus_user::log("browse: asked to save with no address for what was fetched").ok();
            self.status = nexus_i18n::text("browse.nothingtosave").to_string();
            return;
        };

        let wanted = file_name(&url);
        // `create` and not `open`, and a number when the name is taken. A save
        // that silently replaced a file would be a browser overwriting what
        // somebody downloaded earlier because two servers both said `index`;
        // one that refused would be a browser that cannot save the same page
        // twice. Numbering is what every other browser does and is what a
        // person expects.
        let Some((name, file)) = free_name(directory, &wanted) else {
            nexus_user::log(&format!("browse: no free name near {wanted}")).ok();
            self.status = nexus_i18n::format("browse.notsaved", &[("why", &wanted)]);
            return;
        };
        match Ok::<_, nexus_user::Error>(file) {
            Ok(file) => {
                let written = nexus_user::write(file, &self.fetched);
                nexus_user::close(file).ok();
                match written {
                    Ok(count) if count == self.fetched.len() => {
                        nexus_user::log(&format!("browse: saved {name}, {count} bytes")).ok();
                        self.status = nexus_i18n::format(
                            "browse.saved",
                            &[("name", &name), ("bytes", &count)],
                        );
                    }
                    // A short write is the failure that must not look like
                    // success: what is on the disk is then neither the whole
                    // file nor nothing.
                    Ok(count) => {
                        self.status = nexus_i18n::format(
                            "browse.savedpart",
                            &[
                                ("name", &name),
                                ("bytes", &count),
                                ("total", &self.fetched.len()),
                            ],
                        );
                    }
                    Err(error) => {
                        nexus_user::log(&format!("browse: could not write {name}: {error}")).ok();
                        self.status = nexus_i18n::format("browse.notsaved", &[("why", &error)]);
                    }
                }
            }
            Err(error) => {
                nexus_user::log(&format!("browse: could not make {name}: {error}")).ok();
                self.status = nexus_i18n::format("browse.notsaved", &[("why", &error)]);
            }
        }
    }

    fn key_in_bar(&mut self, kind: u8, value: u32) -> bool {
        match kind {
            key::CHARACTER => {
                if let Some(character) = char::from_u32(value) {
                    self.typing.push(character);
                    return true;
                }
                false
            }
            key::BACKSPACE => {
                self.typing.pop();
                true
            }
            key::ENTER => {
                let address = self.typing.clone();
                self.go(address);
                true
            }
            key::ESCAPE => {
                self.editing = false;
                self.typing = self
                    .here
                    .as_ref()
                    .map_or_else(|| String::from(HOME), nexus_http::Url::to_text);
                true
            }
            _ => false,
        }
    }

    /// A key while the page has the keyboard.
    fn key_in_page(&mut self, kind: u8, value: u32) -> bool {
        let page = self.rows().max(1) as usize;
        match kind {
            key::ESCAPE => {
                self.editing = true;
                self.picking.clear();
                true
            }
            key::CHARACTER => {
                let Some(character) = char::from_u32(value) else {
                    return false;
                };
                if character.is_ascii_digit() {
                    if self.picking.len() < 2 {
                        self.picking.push(character);
                    }
                    return true;
                }
                match character {
                    // The keys somebody who has used a terminal pager already
                    // knows, so that a machine with no arrow keys is still
                    // navigable -- and this machine had none until last week.
                    'j' | ' ' => self.scroll_by(page as i64 / 2),
                    'k' => self.scroll_by(-(page as i64) / 2),
                    'g' => self.scroll = 0,
                    'G' => self.scroll = self.laid_out.saturating_sub(page),
                    _ => return false,
                }
                true
            }
            key::BACKSPACE => {
                if !self.picking.is_empty() {
                    self.picking.pop();
                    return true;
                }
                // Back, which is what Backspace has meant in a browser for
                // thirty years.
                let Some(previous) = self.history.pop() else {
                    self.status = nexus_i18n::text("browse.noback").to_string();
                    return true;
                };
                // Taken off the history rather than pushed onto it again: going
                // back and then back again should reach the page before, not
                // bounce between two.
                self.here = None;
                self.go(previous.to_text());
                true
            }
            key::ENTER => {
                if self.picking.is_empty() {
                    self.editing = true;
                    return true;
                }
                let number = self.picking.parse::<usize>().unwrap_or(0);
                self.picking.clear();
                self.follow(number);
                true
            }
            key::MOVE => {
                match value {
                    key::UP => self.scroll_by(-1),
                    key::DOWN => self.scroll_by(1),
                    key::PAGE_UP => self.scroll_by(-(page as i64)),
                    key::PAGE_DOWN => self.scroll_by(page as i64),
                    key::HOME => self.scroll = 0,
                    key::END => self.scroll = self.laid_out.saturating_sub(page),
                    _ => return false,
                }
                true
            }
            _ => false,
        }
    }

    /// Move the view, without running off either end.
    fn scroll_by(&mut self, lines: i64) {
        let page = self.rows().max(1) as usize;
        let last = self.laid_out.saturating_sub(page);
        let at = self.scroll as i64 + lines;
        self.scroll = at.clamp(0, last as i64) as usize;
    }

    /// How many lines of page fit on screen.
    fn rows(&self) -> u32 {
        self.height
            .saturating_sub(BAR + STATUS + PAD * 2)
            .max(nexus_ui::LINE_HEIGHT)
            / nexus_ui::LINE_HEIGHT
    }

    /// Turn the document into lines, and draw the ones that are on screen.
    fn draw(&mut self, _surface: Handle) {
        // SAFETY: the surface is mapped here, writable, and at least
        // `width * height * 4` bytes -- checked when it was taken and again
        // after every replacement.
        let mut canvas = unsafe { Canvas::packed(SURFACE_AT, self.width, self.height) };

        let ground = Colour::rgb(0x0A, 0x0F, 0x18);
        let ink = Colour::rgb(0xDC, 0xE4, 0xF0);
        let dim = Colour::rgb(0x7C, 0x8A, 0xA4);
        let accent = Colour::rgb(0x58, 0xA6, 0xFF);
        let heading = Colour::rgb(0xF0, 0xF4, 0xFF);

        canvas.fill(canvas.bounds(), ground);

        // -- the address bar
        let bar = Rect::new(PAD, PAD, self.width.saturating_sub(PAD * 2), BAR);
        canvas.fill(bar, Colour::rgb(0x14, 0x1C, 0x2C));
        canvas.outline(
            bar,
            1,
            if self.editing {
                accent
            } else {
                dim.blend(ground, 120)
            },
        );
        let shown = if self.editing {
            format!("{}_", self.typing)
        } else {
            self.typing.clone()
        };
        // From the right when it does not fit, because the end of an address is
        // what somebody is typing and the start is what they already know.
        let room = bar.width.saturating_sub(PAD * 2);
        let text = trim_to_width(&shown, room);
        canvas.text(
            bar.x + PAD,
            bar.y + (BAR - nexus_ui::LINE_HEIGHT) / 2,
            text,
            ink,
        );

        // -- the page
        let area = Rect::new(
            PAD,
            PAD + BAR + PAD / 2,
            self.width.saturating_sub(PAD * 2),
            self.height.saturating_sub(PAD * 2 + BAR + STATUS + PAD / 2),
        );
        let lines = self.lay_out(area.width);
        self.laid_out = lines.len();

        let rows = (area.height / nexus_ui::LINE_HEIGHT) as usize;
        let mut y = area.y;
        for line in lines.iter().skip(self.scroll).take(rows) {
            let colour = line.colour;
            let x = area.x + line.x;
            if line.scale > 1 {
                canvas.text_scaled(x, y, &line.text, heading, line.scale);
                y += nexus_ui::LINE_HEIGHT * line.scale;
            } else {
                canvas.text(x, y, &line.text, colour);
                y += nexus_ui::LINE_HEIGHT;
            }
            if y + nexus_ui::LINE_HEIGHT > area.y + area.height {
                break;
            }
        }

        // -- the status line
        let status = Rect::new(
            PAD,
            self.height.saturating_sub(STATUS + PAD / 2),
            self.width.saturating_sub(PAD * 2),
            STATUS,
        );
        let mut text = self.status.clone();
        if !self.picking.is_empty() {
            text = nexus_i18n::format("browse.picking", &[("number", &self.picking)]);
        } else if self.laid_out > rows {
            let of = nexus_i18n::format(
                "browse.position",
                &[("line", &(self.scroll + 1)), ("total", &self.laid_out)],
            );
            text = format!("{text}  {of}");
        }
        canvas.text(
            status.x,
            status.y + 2,
            trim_to_width(&text, status.width),
            dim,
        );
    }

    /// Turn the document into lines that fit `width`.
    ///
    /// Done on every frame rather than kept, because the width changes when the
    /// window is resized and a layout that was cached would be a page laid out
    /// for a window that is no longer that shape. A page is a few thousand
    /// lines at the outside and this is a loop over its text; the frame that
    /// costs is the one somebody asked for by scrolling.
    fn lay_out(&self, width: u32) -> Vec<Line> {
        let ink = Colour::rgb(0xDC, 0xE4, 0xF0);
        let dim = Colour::rgb(0x7C, 0x8A, 0xA4);
        let mut lines: Vec<Line> = Vec::new();
        let mut link_number = 0usize;

        if self.document.blocks.is_empty() {
            return lines;
        }

        for block in &self.document.blocks {
            match block {
                Block::Rule => {
                    lines.push(Line {
                        text: "─".repeat((width / 8).clamp(8, 80) as usize),
                        x: 0,
                        scale: 1,
                        colour: dim,
                    });
                }
                Block::Preformatted(text) => {
                    for row in text.split('\n') {
                        lines.push(Line {
                            text: row.to_string(),
                            x: 16,
                            scale: 1,
                            colour: dim.blend(ink, 160),
                        });
                    }
                    lines.push(blank());
                }
                Block::Heading(level, spans) => {
                    // Only the top two levels are drawn larger. Six sizes on a
                    // sixteen-pixel face is five sizes nobody can tell apart.
                    let scale = if *level <= 2 { 2 } else { 1 };
                    let text = join(spans);
                    for piece in nexus_ui::wrap(&text, width / scale) {
                        lines.push(Line {
                            text: piece.to_string(),
                            x: 0,
                            scale,
                            colour: ink,
                        });
                    }
                    lines.push(blank());
                }
                Block::Item {
                    depth,
                    marker,
                    spans,
                } => {
                    let indent = 16 + *depth as u32 * 20;
                    let text = format!("{marker} {}", self.number(spans, &mut link_number));
                    for (index, piece) in nexus_ui::wrap(&text, width.saturating_sub(indent))
                        .iter()
                        .enumerate()
                    {
                        lines.push(Line {
                            text: (*piece).to_string(),
                            x: if index == 0 { indent } else { indent + 16 },
                            scale: 1,
                            colour: ink,
                        });
                    }
                }
                Block::Quote(depth, spans) => {
                    let indent = 16 + (*depth as u32).min(4) * 16;
                    let text = self.number(spans, &mut link_number);
                    for piece in nexus_ui::wrap(&text, width.saturating_sub(indent)) {
                        lines.push(Line {
                            text: format!("│ {piece}"),
                            x: indent,
                            scale: 1,
                            colour: dim.blend(ink, 120),
                        });
                    }
                    lines.push(blank());
                }
                Block::Paragraph(spans) => {
                    let text = self.number(spans, &mut link_number);
                    for piece in nexus_ui::wrap(&text, width) {
                        lines.push(Line {
                            text: piece.to_string(),
                            x: 0,
                            scale: 1,
                            colour: ink,
                        });
                    }
                    lines.push(blank());
                }
            }
        }
        lines
    }

    /// Put a number in front of each link, so it can be followed by typing one.
    ///
    /// The number goes in the text rather than beside it because the text is
    /// what gets wrapped: a marker drawn separately would end up on a different
    /// line from the link it marks the moment the window narrowed.
    fn number(&self, spans: &[nexus_html::Span], next: &mut usize) -> String {
        let mut out = String::new();
        let mut last: Option<&String> = None;
        for span in spans {
            match &span.link {
                Some(href) => {
                    let same = last.is_some_and(|previous| previous == href);
                    if !same {
                        *next += 1;
                        if *next <= MAX_NUMBERED {
                            out.push_str(&format!("[{next}]"));
                        }
                    }
                    last = Some(href);
                }
                None => last = None,
            }
            out.push_str(&span.text);
        }
        out
    }
}

/// An empty line, which is how blocks are kept apart.
fn blank() -> Line {
    Line {
        text: String::new(),
        x: 0,
        scale: 1,
        colour: Colour::rgb(0, 0, 0),
    }
}

/// The text of every run, joined.
fn join(spans: &[nexus_html::Span]) -> String {
    spans.iter().map(|span| span.text.as_str()).collect()
}

/// As much of the end of a piece of text as fits.
///
/// The *end*, because this is used for an address being typed and for a status
/// line, and in both the newest part is the part somebody is looking at.
fn trim_to_width(text: &str, width: u32) -> &str {
    if nexus_ui::measure(text) <= width {
        return text;
    }
    let mut start = 0;
    for (index, _) in text.char_indices() {
        if nexus_ui::measure(&text[index..]) <= width {
            start = index;
            break;
        }
    }
    &text[start..]
}

/// Read a little-endian `u32` out of a message.
fn read_u32(buffer: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        buffer[offset],
        buffer[offset + 1],
        buffer[offset + 2],
        buffer[offset + 3],
    ])
}

/// Whether anything has gone wrong, for the status this program exits with.
static FAILED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Say what happened and stop. Never returns.
fn finish() -> ! {
    if FAILED.load(core::sync::atomic::Ordering::Relaxed) {
        nexus_user::exit_with(1)
    } else {
        nexus_user::exit()
    }
}

/// Log a failure and remember it.
fn failed(what: &str) {
    FAILED.store(true, core::sync::atomic::Ordering::Relaxed);
    nexus_user::log(what).ok();
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    nexus_user::log("browse: PANIC").ok();
    nexus_user::exit_with(2)
}
