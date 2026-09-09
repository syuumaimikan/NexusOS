//! Build script: rasterises the glyphs the interface needs.
//!
//! The face is generated rather than committed. Hand-authoring a CJK face is
//! not realistic, and putting a downloaded font's data in the repository would
//! be redistributing it, which its licence generally does not allow. So the
//! glyphs are rendered here, once, from a font already on the build machine.
//!
//! It reads `locales/*.txt` for the same reason the kernel does: the set of
//! characters the interface can put on screen is exactly the set its
//! translations use, so the face contains what is needed and nothing more.
//!
//! When no usable font is found the crate ships its built-in ASCII face alone,
//! and text outside that set draws as a placeholder rather than as nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Height of a glyph cell, in pixels.
const CELL: usize = 16;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo_root = manifest.parent().unwrap().parent().unwrap().to_path_buf();
    let locale_dir = repo_root.join("locales");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    println!("cargo:rerun-if-changed={}", locale_dir.display());

    let locales = read_locales(&locale_dir);
    let extra = manifest.join("charset.txt");
    println!("cargo:rerun-if-changed={}", extra.display());

    let mut charset = required_characters(&locales);
    charset.extend(extra_characters(&extra));
    let (description, glyphs) = rasterise(&repo_root, &out_dir, &charset);
    fs::write(
        out_dir.join("wide_font.rs"),
        emit_font(&description, &glyphs),
    )
    .unwrap();
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

/// Characters a program asked for, from `charset.txt`.
///
/// Programs draw text that is not a translation and is therefore in none of the
/// locale files, so there has to be somewhere to say "the face needs this too".
/// A missing file is not an error: it means nothing has asked yet.
fn extra_characters(path: &Path) -> BTreeSet<char> {
    let Ok(text) = fs::read_to_string(path) else {
        return BTreeSet::new();
    };
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .flat_map(str::chars)
        .filter(|character| !character.is_whitespace())
        .collect()
}

/// One parsed locale file.
struct LocaleFile {
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
///
/// Returns which font was actually used alongside the glyphs, so the kernel can
/// report it at boot. Font resolution has already gone wrong twice in ways that
/// were invisible until someone looked closely at a screenshot; a line in the
/// serial log costs nothing and makes the next substitution obvious.
fn rasterise(
    repo_root: &Path,
    out_dir: &Path,
    characters: &BTreeSet<char>,
) -> (String, Vec<Glyph>) {
    let none = || (String::from("built-in 8x8 only"), Vec::new());

    let script = repo_root.join("scripts").join("generate-font.ps1");
    println!("cargo:rerun-if-changed={}", script.display());

    if !script.exists() {
        println!("cargo:warning=font generator missing; using the built-in ASCII font only");
        return none();
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
            return none();
        }
        Err(error) => {
            println!("cargo:warning=could not run the font generator ({error}); using the built-in ASCII font only");
            return none();
        }
    }

    let table = fs::read_to_string(&glyph_path).unwrap_or_default();
    let glyphs = parse_glyphs(&table);
    if glyphs.is_empty() {
        return none();
    }
    (font_description(&table), glyphs)
}

/// The generator's `# font: ...` header, or a placeholder if it is absent.
fn font_description(table: &str) -> String {
    table
        .lines()
        .find_map(|line| line.trim().strip_prefix("# font:"))
        .map_or_else(|| String::from("unknown"), |rest| rest.trim().to_string())
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
fn emit_font(description: &str, glyphs: &[Glyph]) -> String {
    let mut out = String::new();
    out.push_str("// Generated by build.rs. Do not edit.\n");
    out.push_str("//\n");
    out.push_str("// Rasterised from a font installed on the build machine and never\n");
    out.push_str("// committed; see the `rasterise` function in build.rs for why.\n\n");

    let _ = writeln!(
        out,
        "/// Which font this table came from, reported at boot.\npub const SOURCE: &str = {description:?};"
    );
    let _ = writeln!(out, "\npub const GLYPH_COUNT: usize = {};", glyphs.len());
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
