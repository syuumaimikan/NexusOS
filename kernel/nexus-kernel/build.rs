//! Build script: turns `locales/*.txt` into compiled-in string tables, and
//! rasterises the glyphs those strings need.
//!
//! The kernel has no filesystem yet, so translations cannot be loaded at
//! runtime; they are compiled in. Keeping them in data files anyway is what
//! [§77 of the specification] asks for and what makes adding a language a
//! matter of adding a file rather than editing the kernel.
//!
//! Two invariants are enforced here rather than discovered at runtime:
//!
//! * every locale must define exactly the same keys, so a missing translation
//!   is a build error and never a blank label on screen;
//! * the keys are emitted sorted, so the kernel can look one up with a binary
//!   search and no allocation.
//!
//! [§77 of the specification]: ../../docs/NEXUSOS_ROADMAP.md

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Width and height of a full-width glyph cell, in pixels.
const CELL: usize = 16;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo_root = manifest.parent().unwrap().parent().unwrap().to_path_buf();
    let locale_dir = repo_root.join("locales");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    println!("cargo:rerun-if-changed={}", locale_dir.display());

    let locales = read_locales(&locale_dir);
    let keys = validate_and_collect_keys(&locales);

    fs::write(out_dir.join("locales.rs"), emit_locales(&locales, &keys)).unwrap();

    // Every character any translation can put on screen, so the font contains
    // exactly what is needed and nothing more.
    let charset = required_characters(&locales);
    let glyphs = rasterise(&repo_root, &out_dir, &charset);
    fs::write(out_dir.join("wide_font.rs"), emit_font(&glyphs)).unwrap();
}

/// One parsed locale file.
struct LocaleFile {
    tag: String,
    entries: BTreeMap<String, String>,
}

/// Read and parse every `*.txt` in `directory`, sorted by file name.
fn read_locales(directory: &Path) -> Vec<LocaleFile> {
    let mut paths: Vec<PathBuf> = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "txt"))
        .collect();
    paths.sort();

    assert!(
        !paths.is_empty(),
        "no locale files in {}",
        directory.display()
    );

    paths
        .iter()
        .map(|path| {
            println!("cargo:rerun-if-changed={}", path.display());
            let text = fs::read_to_string(path)
                .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
            LocaleFile {
                tag: path.file_stem().unwrap().to_string_lossy().into_owned(),
                entries: parse(&text, path),
            }
        })
        .collect()
}

/// Parse `key = value` lines, ignoring blanks and `#` comments.
fn parse(text: &str, path: &Path) -> BTreeMap<String, String> {
    let mut entries = BTreeMap::new();

    for (number, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let Some((key, value)) = trimmed.split_once('=') else {
            panic!(
                "{}:{}: expected `key = value`, found {trimmed:?}",
                path.display(),
                number + 1
            );
        };

        let key = key.trim().to_string();
        let value = value.trim().to_string();
        if let Some(previous) = entries.insert(key.clone(), value) {
            panic!(
                "{}:{}: duplicate key {key:?} (previously {previous:?})",
                path.display(),
                number + 1
            );
        }
    }

    entries
}

/// Check that every locale defines the same keys, and return them sorted.
///
/// A missing translation is a build failure on purpose: the alternative is a
/// label that silently renders empty, or falls back to another language, in a
/// build that looked fine.
fn validate_and_collect_keys(locales: &[LocaleFile]) -> Vec<String> {
    let reference = &locales[0];
    let keys: Vec<String> = reference.entries.keys().cloned().collect();

    for locale in &locales[1..] {
        let missing: Vec<&String> = keys
            .iter()
            .filter(|key| !locale.entries.contains_key(*key))
            .collect();
        let extra: Vec<&String> = locale
            .entries
            .keys()
            .filter(|key| !reference.entries.contains_key(*key))
            .collect();

        assert!(
            missing.is_empty() && extra.is_empty(),
            "locale {} does not match {}: missing {missing:?}, unexpected {extra:?}",
            locale.tag,
            reference.tag
        );
    }

    keys
}

/// Emit the string tables.
fn emit_locales(locales: &[LocaleFile], keys: &[String]) -> String {
    let mut out = String::new();
    out.push_str("// Generated by build.rs from locales/*.txt. Do not edit.\n\n");

    let _ = writeln!(out, "pub const KEY_COUNT: usize = {};", keys.len());
    let _ = writeln!(
        out,
        "\n/// Translation keys, sorted so a lookup is a binary search."
    );
    let _ = writeln!(out, "pub const KEYS: [&str; KEY_COUNT] = [");
    for key in keys {
        let _ = writeln!(out, "    {key:?},");
    }
    out.push_str("];\n");

    for (index, locale) in locales.iter().enumerate() {
        let _ = writeln!(out, "\nstatic VALUES_{index}: [&str; KEY_COUNT] = [");
        for key in keys {
            let _ = writeln!(out, "    {:?},", locale.entries[key]);
        }
        out.push_str("];\n");
    }

    let _ = writeln!(out, "\npub const LOCALE_COUNT: usize = {};", locales.len());
    let _ = writeln!(out, "pub const LOCALES: [Locale; LOCALE_COUNT] = [");
    for (index, locale) in locales.iter().enumerate() {
        let name = locale
            .entries
            .get("locale.name")
            .map(String::as_str)
            .unwrap_or(&locale.tag);
        let _ = writeln!(
            out,
            "    Locale {{ tag: {:?}, name: {:?}, values: &VALUES_{index} }},",
            locale.tag, name
        );
    }
    out.push_str("];\n");

    out
}

/// Every character that appears in any translation, plus printable ASCII.
///
/// Printable ASCII is included unconditionally because the kernel formats
/// numbers and untranslated identifiers with it regardless of locale.
fn required_characters(locales: &[LocaleFile]) -> BTreeSet<char> {
    let mut characters: BTreeSet<char> = (0x20u8..=0x7E).map(char::from).collect();
    for locale in locales {
        for value in locale.entries.values() {
            characters.extend(value.chars());
        }
    }
    // Placeholder braces are consumed by the formatter, never drawn.
    characters.remove(&'{');
    characters.remove(&'}');
    characters
}

/// One rasterised glyph.
struct Glyph {
    codepoint: u32,
    /// Advance width in pixels: 8 for half-width, 16 for full-width.
    width: u32,
    /// Sixteen rows, most significant bit leftmost.
    rows: [u16; CELL],
}

/// Rasterise `characters` by asking the host to draw them.
///
/// The kernel ships no font file. Hand-authoring a CJK face is not realistic —
/// even the kana alone would be hundreds of bitmaps — and bundling a system
/// font's data in the repository would be redistributing it, which its licence
/// generally does not allow. So the glyphs are rendered from a font already
/// installed on the machine doing the build, cached under `target/`, and never
/// committed. Each build machine uses its own licensed copy, exactly as linking
/// against a system library does.
///
/// If this cannot run, the build still succeeds: the table comes out empty, the
/// kernel falls back to its built-in ASCII font, and text outside that set
/// renders as a placeholder box.
fn rasterise(repo_root: &Path, out_dir: &Path, characters: &BTreeSet<char>) -> Vec<Glyph> {
    let script = repo_root.join("scripts").join("generate-font.ps1");
    println!("cargo:rerun-if-changed={}", script.display());

    if !script.exists() {
        println!("cargo:warning=font generator missing; using the built-in ASCII font only");
        return Vec::new();
    }

    let charset_path = out_dir.join("charset.txt");
    let text: String = characters.iter().collect();
    fs::write(&charset_path, &text).unwrap();

    let glyph_path = out_dir.join("glyphs.txt");
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            &script.to_string_lossy(),
            "-CharsetFile",
            &charset_path.to_string_lossy(),
            "-OutputFile",
            &glyph_path.to_string_lossy(),
        ])
        .output();

    match output {
        Ok(result) if result.status.success() => {}
        Ok(result) => {
            println!(
                "cargo:warning=font generation failed ({}); using the built-in ASCII font only",
                String::from_utf8_lossy(&result.stderr)
                    .trim()
                    .replace('\n', " ")
            );
            return Vec::new();
        }
        Err(error) => {
            println!("cargo:warning=could not run the font generator ({error}); using the built-in ASCII font only");
            return Vec::new();
        }
    }

    parse_glyphs(&fs::read_to_string(&glyph_path).unwrap_or_default())
}

/// Parse the generator's output: `codepoint width row0 row1 ... row15`, hex.
fn parse_glyphs(text: &str) -> Vec<Glyph> {
    let mut glyphs = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != CELL + 2 {
            continue;
        }

        let Ok(codepoint) = u32::from_str_radix(fields[0], 16) else {
            continue;
        };
        let Ok(width) = fields[1].parse::<u32>() else {
            continue;
        };

        let mut rows = [0u16; CELL];
        let mut ok = true;
        for (index, field) in fields[2..].iter().enumerate() {
            match u16::from_str_radix(field, 16) {
                Ok(row) => rows[index] = row,
                Err(_) => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            glyphs.push(Glyph {
                codepoint,
                width,
                rows,
            });
        }
    }

    glyphs.sort_by_key(|glyph| glyph.codepoint);
    glyphs
}

/// Emit the glyph table.
fn emit_font(glyphs: &[Glyph]) -> String {
    let mut out = String::new();
    out.push_str("// Generated by build.rs. Do not edit.\n");
    out.push_str("//\n");
    out.push_str("// Rasterised from a font installed on the build machine and never\n");
    out.push_str("// committed; see the `rasterise` function in build.rs for why.\n\n");

    let _ = writeln!(out, "pub const GLYPH_COUNT: usize = {};", glyphs.len());
    let _ = writeln!(
        out,
        "\n/// Codepoints present in [`GLYPHS`], ascending, for binary search."
    );
    let _ = writeln!(out, "pub const CODEPOINTS: [u32; GLYPH_COUNT] = [");
    for glyph in glyphs {
        let _ = writeln!(out, "    {:#06x},", glyph.codepoint);
    }
    out.push_str("];\n");

    let _ = writeln!(
        out,
        "\n/// Advance width in pixels for each entry of [`CODEPOINTS`]."
    );
    let _ = writeln!(out, "pub const WIDTHS: [u8; GLYPH_COUNT] = [");
    for glyph in glyphs {
        let _ = writeln!(out, "    {},", glyph.width);
    }
    out.push_str("];\n");

    let _ = writeln!(
        out,
        "\n/// Sixteen rows per glyph, most significant bit leftmost."
    );
    let _ = writeln!(out, "pub const GLYPHS: [[u16; 16]; GLYPH_COUNT] = [");
    for glyph in glyphs {
        out.push_str("    [");
        for (index, row) in glyph.rows.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "{row:#06x}");
        }
        out.push_str("],\n");
    }
    out.push_str("];\n");

    out
}
