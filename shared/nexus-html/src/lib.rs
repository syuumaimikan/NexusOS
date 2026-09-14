//! HTML, reduced to what can be read.
//!
//! This is not a browser engine. It has no CSS, no boxes, no floats and no idea
//! what a `<div>` is for. What it does is turn a page into a short list of
//! things that can be drawn one under another -- headings, paragraphs, list
//! items, preformatted blocks, rules -- each made of runs of text that may be
//! bold, or a link.
//!
//! That is a deliberate ceiling and not a stopping point on the way to a real
//! engine. A machine that can *read* the web is a different and much smaller
//! problem than a machine that can render it as designed, and this is the first
//! one. Everything it drops -- the styling, the layout, the scripting -- it
//! drops on purpose and says so here rather than pretending to support it.
//!
//! # Being told lies
//!
//! Every byte here came off a wire from a stranger. So there is no place where
//! a malformed page is an error: an unclosed tag, a tag that closes something
//! that was never open, an attribute with no value, a `<` that is not a tag at
//! all, a comment that never ends. Each of them has a defined outcome and none
//! of them is a panic or an unbounded loop, which is the property that makes
//! this safe to point at the internet.
//!
//! # What is skipped entirely
//!
//! The contents of `<script>` and `<style>`, because they are not text for a
//! person and showing them would fill the window with somebody's minified
//! JavaScript. They are skipped by *scanning for the closing tag* rather than
//! by parsing, which is what the specification requires and what stops a `<` in
//! a string literal inside a script from being read as markup.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::string::{String, ToString as _};
use alloc::vec::Vec;

/// The most blocks one page will produce.
///
/// A bound rather than a trust. A page is read into memory a program has to
/// draw from, and a document with a million empty paragraphs in it is a page
/// somebody can send.
pub const MAX_BLOCKS: usize = 4_000;

/// The most characters one run of text will hold.
pub const MAX_RUN: usize = 8 * 1024;

/// How deeply lists may nest before the indent stops growing.
pub const MAX_DEPTH: usize = 6;

/// How a run of text is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    /// Inside `<code>`, `<tt>` or `<kbd>`.
    pub fixed: bool,
}

/// One run of text, with how it is drawn and where it leads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
    /// The `href` of the innermost `<a>` this run is inside, if any.
    pub link: Option<String>,
}

/// One thing that is drawn on a line of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// A heading, one to six, with one being the largest.
    Heading(u8, Vec<Span>),
    /// A run of ordinary text.
    Paragraph(Vec<Span>),
    /// An item in a list, at this depth, with the marker it was given.
    Item {
        depth: usize,
        marker: String,
        spans: Vec<Span>,
    },
    /// A quotation, at this depth.
    Quote(usize, Vec<Span>),
    /// Text whose spacing matters, kept exactly as it was.
    Preformatted(String),
    /// A horizontal line.
    Rule,
}

impl Block {
    /// Every run of text in it.
    #[must_use]
    pub fn spans(&self) -> &[Span] {
        match self {
            Self::Heading(_, spans) | Self::Paragraph(spans) => spans,
            Self::Item { spans, .. } | Self::Quote(_, spans) => spans,
            Self::Preformatted(_) | Self::Rule => &[],
        }
    }

    /// The whole of its text, with the runs joined.
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::Preformatted(text) => text.clone(),
            Self::Rule => String::new(),
            other => other
                .spans()
                .iter()
                .map(|span| span.text.as_str())
                .collect(),
        }
    }
}

/// A page, as much of it as could be read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Document {
    /// What the page called itself, if it said.
    pub title: Option<String>,
    pub blocks: Vec<Block>,
}

impl Document {
    /// Every link in the page, in the order they appear.
    #[must_use]
    pub fn links(&self) -> Vec<(String, String)> {
        let mut links: Vec<(String, String)> = Vec::new();
        for block in &self.blocks {
            for span in block.spans() {
                let Some(href) = &span.link else { continue };
                // Runs of one link are joined, because a link whose middle word
                // is bold is three runs and one link.
                match links.last_mut() {
                    Some((last, text)) if last == href => text.push_str(&span.text),
                    _ => links.push((href.clone(), span.text.clone())),
                }
            }
        }
        links
    }

    /// Whether nothing readable came out of the page.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

// -- Entities ------------------------------------------------------------------

/// The named entities worth knowing.
///
/// Not the full list, which runs to two and a half thousand names. These are
/// the ones that appear in ordinary prose, plus the five that have to be
/// escaped to write HTML at all. A name that is not here is left as it was
/// written, which is the honest outcome: showing `&hellip;` is worse than
/// showing an ellipsis and much better than showing nothing.
const NAMED: &[(&str, char)] = &[
    ("amp", '&'),
    ("lt", '<'),
    ("gt", '>'),
    ("quot", '"'),
    ("apos", '\''),
    ("nbsp", '\u{00A0}'),
    ("copy", '©'),
    ("reg", '®'),
    ("trade", '™'),
    ("hellip", '…'),
    ("mdash", '—'),
    ("ndash", '–'),
    ("lsquo", '\u{2018}'),
    ("rsquo", '\u{2019}'),
    ("ldquo", '\u{201C}'),
    ("rdquo", '\u{201D}'),
    ("bull", '•'),
    ("middot", '·'),
    ("deg", '°'),
    ("plusmn", '±'),
    ("times", '×'),
    ("divide", '÷'),
    ("frac12", '½'),
    ("laquo", '«'),
    ("raquo", '»'),
    ("euro", '€'),
    ("pound", '£'),
    ("yen", '¥'),
    ("sect", '§'),
    ("para", '¶'),
    ("dagger", '†'),
    ("larr", '←'),
    ("uarr", '↑'),
    ("rarr", '→'),
    ("darr", '↓'),
];

/// Turn `&...;` into what it stands for.
///
/// An entity that is not recognised is left exactly as it was written. That is
/// the specification's own behaviour for an unterminated one and the kindest
/// reading of a misspelled one: the alternative is silently deleting text.
#[must_use]
pub fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        // An entity's name runs to the semicolon, and a long stretch with no
        // semicolon is an ampersand somebody typed rather than an entity.
        let end = after
            .char_indices()
            .take(12)
            .find(|(_, character)| *character == ';')
            .map(|(index, _)| index);

        let Some(end) = end else {
            out.push('&');
            rest = after;
            continue;
        };
        let name = &after[..end];

        let replacement = if let Some(digits) = name.strip_prefix("#x").or(name.strip_prefix("#X"))
        {
            u32::from_str_radix(digits, 16)
                .ok()
                .and_then(char::from_u32)
        } else if let Some(digits) = name.strip_prefix('#') {
            digits.parse::<u32>().ok().and_then(char::from_u32)
        } else {
            NAMED
                .iter()
                .find(|(known, _)| *known == name)
                .map(|(_, character)| *character)
        };

        match replacement {
            Some(character) => out.push(character),
            None => {
                out.push('&');
                out.push_str(name);
                out.push(';');
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

// -- Tokenising ----------------------------------------------------------------

/// One tag, taken apart.
struct Tag {
    name: String,
    closing: bool,
    attributes: Vec<(String, String)>,
}

impl Tag {
    fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Read a tag starting at `<`, and say where it ended.
///
/// Returns `None` when what follows the `<` is not a tag at all, which is
/// ordinary: `a < b` in running text is text.
fn read_tag(bytes: &[u8], start: usize) -> Option<(Tag, usize)> {
    let mut at = start + 1;
    let closing = bytes.get(at) == Some(&b'/');
    if closing {
        at += 1;
    }
    // A tag name begins with a letter. Anything else after `<` -- a space, a
    // digit, a comparison -- is text.
    if !bytes.get(at)?.is_ascii_alphabetic() {
        return None;
    }

    let name_start = at;
    while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'-') {
        at += 1;
    }
    let name = String::from_utf8_lossy(&bytes[name_start..at]).to_ascii_lowercase();

    let mut attributes = Vec::new();
    loop {
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        match bytes.get(at) {
            None => {
                return Some((
                    Tag {
                        name,
                        closing,
                        attributes,
                    },
                    bytes.len(),
                ))
            }
            Some(b'>') => {
                return Some((
                    Tag {
                        name,
                        closing,
                        attributes,
                    },
                    at + 1,
                ))
            }
            Some(b'/') => {
                at += 1;
                continue;
            }
            _ => {}
        }

        let key_start = at;
        while at < bytes.len()
            && !bytes[at].is_ascii_whitespace()
            && bytes[at] != b'='
            && bytes[at] != b'>'
        {
            at += 1;
        }
        if at == key_start {
            // No progress: a byte that is neither a name nor a delimiter. Step
            // over it rather than loop for ever on it.
            at += 1;
            continue;
        }
        let key = String::from_utf8_lossy(&bytes[key_start..at]).to_ascii_lowercase();

        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if bytes.get(at) != Some(&b'=') {
            // An attribute with no value, which is how `disabled` is written.
            attributes.push((key, String::new()));
            continue;
        }
        at += 1;
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }

        let value = match bytes.get(at) {
            Some(quote @ (b'"' | b'\'')) => {
                let quote = *quote;
                at += 1;
                let value_start = at;
                while at < bytes.len() && bytes[at] != quote {
                    at += 1;
                }
                let value = String::from_utf8_lossy(&bytes[value_start..at]).to_string();
                // Past the closing quote, if there was one. An unclosed quote
                // swallows the rest of the document, which is what every
                // browser does with it.
                at = (at + 1).min(bytes.len());
                value
            }
            _ => {
                let value_start = at;
                while at < bytes.len() && !bytes[at].is_ascii_whitespace() && bytes[at] != b'>' {
                    at += 1;
                }
                String::from_utf8_lossy(&bytes[value_start..at]).to_string()
            }
        };
        attributes.push((key, unescape(&value)));
    }
}

/// Where a run of raw text ends: the closing tag for `name`, case-insensitively.
fn find_close(bytes: &[u8], from: usize, name: &str) -> usize {
    let mut at = from;
    while at + 2 < bytes.len() {
        if bytes[at] == b'<' && bytes[at + 1] == b'/' {
            let after = at + 2;
            let end = (after + name.len()).min(bytes.len());
            if bytes[after..end].eq_ignore_ascii_case(name.as_bytes()) {
                return at;
            }
        }
        at += 1;
    }
    bytes.len()
}

/// Skip past `<!-- ... -->`, `<!doctype ...>` and the like.
///
/// Returns where the document carries on. A comment that never ends takes the
/// rest of the document with it, which is what browsers do and what stops an
/// unterminated comment from being read as markup.
fn skip_declaration(bytes: &[u8], start: usize) -> usize {
    if bytes[start..].starts_with(b"<!--") {
        let mut at = start + 4;
        while at + 2 < bytes.len() {
            if &bytes[at..at + 3] == b"-->" {
                return at + 3;
            }
            at += 1;
        }
        return bytes.len();
    }
    let mut at = start + 1;
    while at < bytes.len() && bytes[at] != b'>' {
        at += 1;
    }
    (at + 1).min(bytes.len())
}

// -- Reading a document --------------------------------------------------------

/// Tags that end the block they are in and start nothing.
fn is_break(name: &str) -> bool {
    matches!(name, "br")
}

/// Tags whose content is not markup.
fn is_raw(name: &str) -> bool {
    matches!(name, "script" | "style" | "noscript" | "template" | "svg")
}

/// Tags after which a block ends, whatever else is going on.
fn starts_block(name: &str) -> bool {
    matches!(
        name,
        "p" | "div"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "main"
            | "aside"
            | "nav"
            | "form"
            | "table"
            | "tr"
            | "ul"
            | "ol"
            | "dl"
            | "dt"
            | "dd"
            | "figure"
            | "figcaption"
            | "address"
            | "fieldset"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "li"
            | "blockquote"
            | "pre"
            | "hr"
    )
}

/// What is being built while the page is read.
struct Builder {
    document: Document,
    /// The runs of the block being built.
    spans: Vec<Span>,
    /// How that block should come out.
    kind: Pending,
    style: Style,
    /// The stack of links, so that nesting closes in order.
    links: Vec<String>,
    /// The stack of lists, each with whether it is numbered and its count.
    lists: Vec<(bool, usize)>,
    /// How deep inside `<blockquote>`.
    quote: usize,
    /// Whether the last thing added was a space, so runs of space collapse.
    space: bool,
}

/// What the block being built will become.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pending {
    Paragraph,
    Heading(u8),
    Item,
}

impl Builder {
    fn new() -> Self {
        Self {
            document: Document::default(),
            spans: Vec::new(),
            kind: Pending::Paragraph,
            style: Style::default(),
            links: Vec::new(),
            lists: Vec::new(),
            quote: 0,
            space: true,
        }
    }

    /// Add text to the block being built, collapsing runs of whitespace.
    fn text(&mut self, raw: &str) {
        let text = unescape(raw);
        for character in text.chars() {
            if character.is_whitespace() {
                if !self.space {
                    self.push(' ');
                    self.space = true;
                }
                continue;
            }
            self.push(character);
            self.space = false;
        }
    }

    /// Add one character to the run being built, starting a new run when the
    /// style or the link has changed.
    fn push(&mut self, character: char) {
        // The innermost link that actually goes somewhere. An `<a>` with no
        // href, or one pointing at the page itself, is on the stack so that its
        // `</a>` pops the right number of times -- it is not a link, and a run
        // inside it must not be marked as one.
        let link = self
            .links
            .iter()
            .rev()
            .find(|href| !href.is_empty())
            .cloned();
        let matches = self
            .spans
            .last()
            .is_some_and(|span| span.style == self.style && span.link == link);
        if !matches {
            // A leading space in a new run is a space between two runs, which
            // the run before it already carries.
            if character == ' ' && self.spans.is_empty() {
                return;
            }
            self.spans.push(Span {
                text: String::new(),
                style: self.style,
                link,
            });
        }
        if let Some(span) = self.spans.last_mut() {
            if span.text.len() < MAX_RUN {
                span.text.push(character);
            }
        }
    }

    /// Finish the block being built, if there is anything in it.
    fn flush(&mut self) {
        // Trailing space belongs to no word.
        if let Some(last) = self.spans.last_mut() {
            while last.text.ends_with(' ') {
                last.text.pop();
            }
        }
        self.spans.retain(|span| !span.text.is_empty());
        self.space = true;

        if self.spans.is_empty() {
            self.kind = Pending::Paragraph;
            return;
        }
        if self.document.blocks.len() >= MAX_BLOCKS {
            self.spans.clear();
            return;
        }

        let spans = core::mem::take(&mut self.spans);
        let block = match self.kind {
            Pending::Heading(level) => Block::Heading(level, spans),
            Pending::Item => Block::Item {
                depth: self.lists.len().saturating_sub(1).min(MAX_DEPTH),
                marker: self.marker(),
                spans,
            },
            Pending::Paragraph if self.quote > 0 => Block::Quote(self.quote.min(MAX_DEPTH), spans),
            Pending::Paragraph => Block::Paragraph(spans),
        };
        self.document.blocks.push(block);
        self.kind = Pending::Paragraph;
    }

    /// The bullet or number for the item being built.
    fn marker(&self) -> String {
        match self.lists.last() {
            Some((true, count)) => alloc::format!("{count}."),
            _ => String::from("•"),
        }
    }

    /// Add a block that has no text of its own.
    fn rule(&mut self) {
        self.flush();
        if self.document.blocks.len() < MAX_BLOCKS {
            self.document.blocks.push(Block::Rule);
        }
    }
}

/// Read a page.
///
/// Never fails. Everything that could be an error is a decision instead, and
/// what comes out of a page that makes no sense is whatever text was in it.
#[must_use]
pub fn parse(html: &str) -> Document {
    let bytes = html.as_bytes();
    let mut builder = Builder::new();
    let mut at = 0;
    // Inside `<title>`, whose text names the page rather than appearing in it.
    let mut in_title = false;
    let mut title = String::new();
    // Inside `<head>`, where text is not content.
    let mut head_depth = 0usize;

    while at < bytes.len() {
        let Some(next) = bytes[at..].iter().position(|byte| *byte == b'<') else {
            let text = String::from_utf8_lossy(&bytes[at..]);
            if in_title {
                title.push_str(&text);
            } else if head_depth == 0 {
                builder.text(&text);
            }
            break;
        };

        if next > 0 {
            let text = String::from_utf8_lossy(&bytes[at..at + next]);
            if in_title {
                title.push_str(&text);
            } else if head_depth == 0 {
                builder.text(&text);
            }
        }
        let tag_at = at + next;

        // A declaration or a comment.
        if bytes.get(tag_at + 1) == Some(&b'!') || bytes.get(tag_at + 1) == Some(&b'?') {
            at = skip_declaration(bytes, tag_at);
            continue;
        }

        let Some((tag, after)) = read_tag(bytes, tag_at) else {
            // Not a tag: a `<` in running text. Kept, because it is text.
            if head_depth == 0 && !in_title {
                builder.text("<");
            }
            at = tag_at + 1;
            continue;
        };
        at = after;

        // Raw text: scanned for its closing tag rather than parsed, so that a
        // `<` inside a script is not markup.
        if !tag.closing && is_raw(&tag.name) {
            let close = find_close(bytes, at, &tag.name);
            at = if close >= bytes.len() {
                bytes.len()
            } else {
                // Past `</name` and whatever follows to the `>`.
                let mut end = close + 2 + tag.name.len();
                while end < bytes.len() && bytes[end] != b'>' {
                    end += 1;
                }
                (end + 1).min(bytes.len())
            };
            continue;
        }

        match tag.name.as_str() {
            "head" => {
                head_depth = if tag.closing {
                    head_depth.saturating_sub(1)
                } else {
                    head_depth + 1
                };
            }
            "title" => {
                if tag.closing {
                    in_title = false;
                    let text = unescape(title.trim());
                    if !text.is_empty() {
                        builder.document.title = Some(collapse(&text));
                    }
                } else {
                    in_title = true;
                    title.clear();
                }
            }
            "body" => {
                // Anything before `<body>` that was not in `<head>` is still
                // content; this only ends the head if one was left open.
                head_depth = 0;
                builder.flush();
            }
            _ => apply(&mut builder, &tag),
        }
    }

    builder.flush();
    builder.document
}

/// Collapse runs of whitespace in a piece of text.
fn collapse(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = true;
    for character in text.chars() {
        if character.is_whitespace() {
            if !space {
                out.push(' ');
                space = true;
            }
            continue;
        }
        out.push(character);
        space = false;
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Act on one tag.
fn apply(builder: &mut Builder, tag: &Tag) {
    let name = tag.name.as_str();

    if is_break(name) {
        builder.flush();
        return;
    }
    if name == "hr" {
        builder.rule();
        return;
    }

    match name {
        "b" | "strong" => builder.style.bold = !tag.closing,
        "i" | "em" | "cite" | "var" => builder.style.italic = !tag.closing,
        "code" | "tt" | "kbd" | "samp" => builder.style.fixed = !tag.closing,
        "a" => {
            if tag.closing {
                builder.links.pop();
            } else if let Some(href) = tag.attribute("href") {
                let href = href.trim();
                // A link with nowhere to go is not a link. `href="#"` is how a
                // page says "this is a button", and drawing it as somewhere to
                // follow would be a promise nothing keeps.
                if !href.is_empty()
                    && !href.starts_with('#')
                    && !starts_with_ignore_case(href, "javascript:")
                {
                    builder.links.push(href.to_string());
                } else {
                    // Pushed anyway, so that the matching `</a>` pops the right
                    // number of times. An empty string is never a link, and the
                    // run-building treats it as one more thing to compare.
                    builder.links.push(String::new());
                }
            } else {
                builder.links.push(String::new());
            }
        }
        "img" => {
            // No pictures, but the words that stand in for one. A page whose
            // navigation is images would otherwise have no navigation at all.
            if let Some(alt) = tag.attribute("alt") {
                if !alt.trim().is_empty() {
                    builder.text(alt);
                }
            }
        }
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            builder.flush();
            if !tag.closing {
                let level = name.as_bytes()[1] - b'0';
                builder.kind = Pending::Heading(level.clamp(1, 6));
            }
        }
        "ul" | "ol" => {
            builder.flush();
            if tag.closing {
                builder.lists.pop();
            } else if builder.lists.len() < MAX_DEPTH {
                builder.lists.push((name == "ol", 0));
            }
        }
        "li" => {
            builder.flush();
            if !tag.closing {
                // A list item outside any list is still an item, and pages
                // written by hand are full of them.
                if builder.lists.is_empty() {
                    builder.lists.push((false, 0));
                }
                if let Some(list) = builder.lists.last_mut() {
                    list.1 += 1;
                }
                builder.kind = Pending::Item;
            }
        }
        "blockquote" => {
            builder.flush();
            builder.quote = if tag.closing {
                builder.quote.saturating_sub(1)
            } else {
                builder.quote + 1
            };
        }
        "td" | "th" => {
            // A cell is a paragraph. Tables are not laid out here, and a table
            // whose cells ran together would be unreadable in a way that one
            // cell per line is not.
            builder.flush();
            builder.style.bold = !tag.closing && name == "th";
        }
        _ if starts_block(name) => builder.flush(),
        _ => {}
    }

    // A link is closed by the tag that closes it and also by the block ending,
    // because a page with an unclosed `<a>` would otherwise make the rest of
    // itself one enormous link.
    if tag.closing && starts_block(name) {
        builder.links.clear();
    }
}

/// Whether `text` starts with `prefix`, ignoring case.
fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len() && text[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// Read a page whose `<pre>` blocks should keep their spacing.
///
/// Done as a second pass over the source rather than inside the reader above,
/// because preformatted text is the one place where the whitespace collapsing
/// that makes everything else readable is exactly wrong -- and threading "do
/// not collapse" through every path would make every path carry a flag for the
/// sake of one tag.
#[must_use]
pub fn parse_with_preformatted(html: &str) -> Document {
    let bytes = html.as_bytes();
    let mut pieces: Vec<(bool, String)> = Vec::new();
    let mut at = 0;

    while at < bytes.len() {
        let Some(found) = find_tag(bytes, at, "pre") else {
            pieces.push((false, String::from_utf8_lossy(&bytes[at..]).to_string()));
            break;
        };
        pieces.push((
            false,
            String::from_utf8_lossy(&bytes[at..found.0]).to_string(),
        ));
        let close = find_close(bytes, found.1, "pre");
        let inner = String::from_utf8_lossy(&bytes[found.1..close.min(bytes.len())]).to_string();
        pieces.push((true, inner));
        at = if close >= bytes.len() {
            bytes.len()
        } else {
            let mut end = close + 2 + 3;
            while end < bytes.len() && bytes[end] != b'>' {
                end += 1;
            }
            (end + 1).min(bytes.len())
        };
    }

    let mut document = Document::default();
    for (preformatted, piece) in pieces {
        if preformatted {
            // Tags inside are dropped and their text kept: a `<pre>` full of
            // `<span class=...>` is highlighted source, and the highlighting is
            // not something this can show.
            let text = unescape(&strip_tags(&piece));
            let text = text.trim_matches('\n');
            if !text.is_empty() && document.blocks.len() < MAX_BLOCKS {
                document.blocks.push(Block::Preformatted(text.to_string()));
            }
            continue;
        }
        let mut part = parse(&piece);
        if document.title.is_none() {
            document.title = part.title.take();
        }
        document.blocks.append(&mut part.blocks);
    }
    document
}

/// Where an opening tag with this name is: where it starts, and where its
/// content does.
fn find_tag(bytes: &[u8], from: usize, name: &str) -> Option<(usize, usize)> {
    let mut at = from;
    while at < bytes.len() {
        if bytes[at] != b'<' {
            at += 1;
            continue;
        }
        let Some((tag, after)) = read_tag(bytes, at) else {
            at += 1;
            continue;
        };
        if !tag.closing && tag.name == name {
            return Some((at, after));
        }
        at = after.max(at + 1);
    }
    None
}

/// Everything that is not a tag.
fn strip_tags(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'<' {
            out.push(bytes[at] as char);
            at += 1;
            continue;
        }
        match read_tag(bytes, at) {
            Some((_, after)) => at = after.max(at + 1),
            None => {
                out.push('<');
                at += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn texts(document: &Document) -> Vec<String> {
        document.blocks.iter().map(Block::text).collect()
    }

    #[test]
    fn text_between_tags_becomes_paragraphs() {
        let document = parse("<p>one</p><p>two</p>");
        assert_eq!(texts(&document), vec!["one", "two"]);
    }

    #[test]
    fn runs_of_whitespace_collapse() {
        let document = parse("<p>one   \n\t two</p>");
        assert_eq!(texts(&document), vec!["one two"]);
    }

    #[test]
    fn a_title_names_the_page_and_is_not_in_it() {
        let document = parse("<html><head><title> A  Page </title></head><body>text</body></html>");
        assert_eq!(document.title.as_deref(), Some("A Page"));
        assert_eq!(texts(&document), vec!["text"]);
    }

    #[test]
    fn headings_keep_their_level() {
        let document = parse("<h1>big</h1><h3>small</h3>");
        assert_eq!(
            document.blocks,
            vec![
                Block::Heading(
                    1,
                    vec![Span {
                        text: String::from("big"),
                        style: Style::default(),
                        link: None
                    }]
                ),
                Block::Heading(
                    3,
                    vec![Span {
                        text: String::from("small"),
                        style: Style::default(),
                        link: None
                    }]
                ),
            ]
        );
    }

    #[test]
    fn a_link_is_a_run_with_somewhere_to_go() {
        let document = parse(r#"<p>see <a href="/there">this page</a> now</p>"#);
        assert_eq!(texts(&document), vec!["see this page now"]);
        assert_eq!(
            document.links(),
            vec![(String::from("/there"), String::from("this page"))]
        );
    }

    #[test]
    fn a_link_with_a_bold_word_is_still_one_link() {
        let document = parse(r#"<a href="/x">a <b>bold</b> word</a>"#);
        assert_eq!(
            document.links(),
            vec![(String::from("/x"), String::from("a bold word"))]
        );
    }

    #[test]
    fn a_link_to_nowhere_is_not_a_link() {
        let document = parse(r##"<a href="#">button</a><a href="javascript:go()">script</a>"##);
        assert!(document.links().is_empty());
        assert_eq!(texts(&document), vec!["buttonscript"]);
    }

    #[test]
    fn bold_and_italic_make_their_own_runs() {
        let document = parse("<p>plain <b>bold</b> <i>italic</i></p>");
        let spans = document.blocks[0].spans();
        assert!(spans
            .iter()
            .any(|span| span.style.bold && span.text == "bold"));
        assert!(spans
            .iter()
            .any(|span| span.style.italic && span.text == "italic"));
    }

    #[test]
    fn list_items_are_numbered_or_bulleted() {
        let document = parse("<ol><li>first</li><li>second</li></ol><ul><li>dot</li></ul>");
        let markers: Vec<String> = document
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::Item { marker, .. } => Some(marker.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(markers, vec!["1.", "2.", "•"]);
    }

    #[test]
    fn nested_lists_are_deeper() {
        let document = parse("<ul><li>outer<ul><li>inner</li></ul></li></ul>");
        let depths: Vec<usize> = document
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::Item { depth, .. } => Some(*depth),
                _ => None,
            })
            .collect();
        assert_eq!(depths, vec![0, 1]);
    }

    #[test]
    fn a_script_is_not_shown_and_its_angle_brackets_are_not_markup() {
        let document = parse(
            "<p>before</p><script>if (a < b) { document.write('<p>no</p>') }</script><p>after</p>",
        );
        assert_eq!(texts(&document), vec!["before", "after"]);
    }

    #[test]
    fn a_style_block_is_not_shown() {
        let document = parse("<style>p { color: red }</style><p>text</p>");
        assert_eq!(texts(&document), vec!["text"]);
    }

    #[test]
    fn comments_are_skipped_and_an_unclosed_one_takes_the_rest() {
        assert_eq!(
            texts(&parse("<p>a</p><!-- hidden --><p>b</p>")),
            vec!["a", "b"]
        );
        assert_eq!(texts(&parse("<p>a</p><!-- never ends <p>b</p>")), vec!["a"]);
    }

    #[test]
    fn a_doctype_is_skipped() {
        let document = parse("<!DOCTYPE html><p>text</p>");
        assert_eq!(texts(&document), vec!["text"]);
    }

    #[test]
    fn a_less_than_in_running_text_is_text() {
        let document = parse("<p>a < b and c &lt; d</p>");
        assert_eq!(texts(&document), vec!["a < b and c < d"]);
    }

    #[test]
    fn entities_become_what_they_stand_for() {
        assert_eq!(unescape("&amp;&lt;&gt;&quot;"), "&<>\"");
        assert_eq!(unescape("&#65;&#x42;"), "AB");
        assert_eq!(unescape("&hellip;"), "…");
        // A name nothing knows is left exactly as written rather than deleted.
        assert_eq!(unescape("&notathing;"), "&notathing;");
        assert_eq!(unescape("a & b"), "a & b");
        assert_eq!(unescape("no entities here"), "no entities here");
    }

    #[test]
    fn an_unclosed_tag_does_not_swallow_the_page() {
        let document = parse("<p>one<p>two<p>three");
        assert_eq!(texts(&document), vec!["one", "two", "three"]);
    }

    #[test]
    fn a_closing_tag_that_closes_nothing_is_ignored() {
        let document = parse("</p></div></b>text");
        assert_eq!(texts(&document), vec!["text"]);
    }

    #[test]
    fn an_unclosed_quote_in_an_attribute_does_not_loop() {
        let document = parse(r#"<a href="/never-closed>text"#);
        // Whatever it decides, it decides it and returns.
        assert!(document.blocks.len() <= 1);
    }

    #[test]
    fn an_image_shows_the_words_that_stand_in_for_it() {
        let document = parse(r#"<p><img src="x.png" alt="a diagram"> follows</p>"#);
        assert_eq!(texts(&document), vec!["a diagram follows"]);
    }

    #[test]
    fn a_rule_is_a_block_of_its_own() {
        let document = parse("<p>a</p><hr><p>b</p>");
        assert_eq!(document.blocks.len(), 3);
        assert_eq!(document.blocks[1], Block::Rule);
    }

    #[test]
    fn a_break_ends_the_line_without_ending_the_text() {
        let document = parse("<p>one<br>two</p>");
        assert_eq!(texts(&document), vec!["one", "two"]);
    }

    #[test]
    fn table_cells_are_one_per_line() {
        let document = parse("<table><tr><td>a</td><td>b</td></tr></table>");
        assert_eq!(texts(&document), vec!["a", "b"]);
    }

    #[test]
    fn preformatted_text_keeps_its_spacing() {
        let document = parse_with_preformatted(
            "<p>before</p><pre>  two  spaces\n  and a line</pre><p>after</p>",
        );
        assert_eq!(
            document.blocks[1],
            Block::Preformatted(String::from("  two  spaces\n  and a line"))
        );
        assert_eq!(document.blocks[0].text(), "before");
        assert_eq!(document.blocks[2].text(), "after");
    }

    #[test]
    fn preformatted_text_still_decodes_entities_and_drops_tags() {
        let document = parse_with_preformatted("<pre><span>a &amp; b</span></pre>");
        assert_eq!(
            document.blocks[0],
            Block::Preformatted(String::from("a & b"))
        );
    }

    #[test]
    fn a_page_with_nothing_readable_is_empty_rather_than_wrong() {
        assert!(parse("").is_empty());
        assert!(parse("<html><head></head><body></body></html>").is_empty());
        assert!(parse("<script>everything</script>").is_empty());
    }

    #[test]
    fn nothing_in_the_head_is_shown() {
        let document = parse(
            "<html><head><meta charset=\"utf-8\"><title>t</title>stray</head><body>real</body></html>",
        );
        assert_eq!(texts(&document), vec!["real"]);
    }

    #[test]
    fn a_real_looking_page_comes_out_readable() {
        let page = r#"<!DOCTYPE html>
<html lang="en">
<head><title>Example Domain</title>
<meta charset="utf-8">
<style>body { margin: 0 }</style>
</head>
<body>
<div>
    <h1>Example Domain</h1>
    <p>This domain is for use in <b>illustrative</b> examples in documents. You may
    use this domain in literature without prior coordination or asking for
    permission.</p>
    <p><a href="https://www.iana.org/domains/example">More information...</a></p>
</div>
</body>
</html>"#;
        let document = parse(page);
        assert_eq!(document.title.as_deref(), Some("Example Domain"));
        assert_eq!(
            document.blocks[0],
            Block::Heading(
                1,
                vec![Span {
                    text: String::from("Example Domain"),
                    style: Style::default(),
                    link: None
                }]
            )
        );
        assert!(document.blocks[1]
            .text()
            .starts_with("This domain is for use in"));
        assert!(document.blocks[1]
            .text()
            .ends_with("asking for permission."));
        assert_eq!(
            document.links(),
            vec![(
                String::from("https://www.iana.org/domains/example"),
                String::from("More information...")
            )]
        );
    }

    #[test]
    fn a_page_of_nonsense_returns_rather_than_hangs() {
        // Every one of these has been a hang or a panic in somebody's parser.
        for page in [
            "<",
            "<<<<<<",
            "<a",
            "<a href",
            "<a href=",
            "<a href=\"",
            "<!--",
            "<!",
            "</",
            "<p>&",
            "<p>&#",
            "<p>&#x",
            "<script>",
            "<pre>",
            "<ul><li><ul><li><ul><li><ul><li><ul><li><ul><li><ul><li>deep",
        ] {
            let _ = parse(page);
            let _ = parse_with_preformatted(page);
        }
    }
}
