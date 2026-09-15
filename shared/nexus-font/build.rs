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

    // Two faces, because anti-aliasing is not a switch that can be applied to
    // the first one. MS Gothic at sixteen pixels draws from hand-tuned embedded
    // bitmaps, which have no soft edges to capture; a face with soft edges has
    // to be asked for at rasterisation time and from a different renderer. So
    // one face is crisp by construction and the other is smooth by
    // construction, and the setting chooses between them.
    let crisp = rasterise(&repo_root, &out_dir, &charset, "crisp", false);
    let smooth = rasterise(&repo_root, &out_dir, &charset, "smooth", true);

    fs::write(out_dir.join("wide_font.rs"), emit_font(&crisp, &smooth)).unwrap();
}

/// One rasterised face.
struct Face {
    description: String,
    glyphs: Vec<Glyph>,
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
    /// Sixteen rows of sixteen four-bit coverage values, leftmost pixel in the
    /// most significant nibble.
    rows: [u64; CELL],
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
    name: &str,
    smooth: bool,
) -> Face {
    let none = || Face {
        description: String::from("built-in 8x8 only"),
        glyphs: Vec::new(),
    };

    let script = repo_root.join("scripts").join("generate-font.ps1");
    println!("cargo:rerun-if-changed={}", script.display());

    if !script.exists() {
        println!("cargo:warning=font generator missing; using the built-in ASCII font only");
        return none();
    }

    let charset_path = out_dir.join("charset.txt");
    let text: String = characters.iter().collect();
    fs::write(&charset_path, &text).unwrap();

    let glyph_path = out_dir.join(format!("glyphs-{name}.txt"));
    let mut arguments = vec![
        String::from("-NoProfile"),
        String::from("-ExecutionPolicy"),
        String::from("Bypass"),
        String::from("-File"),
        script.to_string_lossy().into_owned(),
        String::from("-CharsetFile"),
        charset_path.to_string_lossy().into_owned(),
        String::from("-OutputFile"),
        glyph_path.to_string_lossy().into_owned(),
    ];
    if smooth {
        // The same font, rasterised a different way.
        //
        // Not a different font, which was the first attempt and was wrong twice
        // over: the faces that look soft at this size have CJK glyphs too wide
        // for a sixteen-pixel cell, so the fitting logic dropped to eight or
        // nine pixels and the smooth face came out half the height of the crisp
        // one. Rendering the *same* face through the outline rasteriser keeps
        // every metric identical -- same em size, same cap height, same
        // advances -- and changes only the edges.
        arguments.push(String::from("-Smooth"));
    }
    let output = Command::new("powershell").args(&arguments).output();

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
    Face {
        description: font_description(&table),
        glyphs,
    }
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

        // Sixteen hex digits a row, one per pixel, leftmost first. Sixteen
        // nibbles is exactly a `u64`, which is why the row is one.
        let mut rows = [0u64; CELL];
        let mut ok = true;
        for (index, field) in fields[2..].iter().enumerate() {
            match u64::from_str_radix(field, 16) {
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

    glyphs
}

/// Write the generated module: two faces over one list of codepoints.
///
/// The codepoint list is shared because both faces are rasterised from the same
/// charset, so they cover the same characters in the same order. That is
/// checked rather than assumed -- a mismatch would mean one face silently
/// drawing a different character than the other, which is the kind of bug that
/// looks like a font problem for a week.
fn emit_font(crisp: &Face, smooth: &Face) -> String {
    let glyphs = &crisp.glyphs;
    let matched = smooth.glyphs.len() == glyphs.len()
        && smooth
            .glyphs
            .iter()
            .zip(glyphs.iter())
            .all(|(one, other)| one.codepoint == other.codepoint);
    if !smooth.glyphs.is_empty() && !matched {
        println!(
            "cargo:warning=the smooth face covers different characters than the crisp one;              using the crisp face for both"
        );
    }
    let smooth_glyphs: &[Glyph] = if matched { &smooth.glyphs } else { glyphs };

    let mut out = String::new();
    out.push_str(
        "// Generated by build.rs. Do not edit.
",
    );
    out.push_str(
        "//
",
    );
    out.push_str(
        "// Rasterised from fonts installed on the build machine and never
",
    );
    out.push_str(
        "// committed; see the `rasterise` function in build.rs for why.

",
    );

    let description = &crisp.description;
    let _ = writeln!(
        out,
        "/// Which font the crisp table came from, reported at boot.
pub const SOURCE: &str = {description:?};"
    );
    let smooth_description = if matched {
        smooth.description.clone()
    } else {
        crisp.description.clone()
    };
    let _ = writeln!(
        out,
        "
/// And the smooth one.
pub const SMOOTH_SOURCE: &str = {smooth_description:?};"
    );
    let _ = writeln!(
        out,
        "
/// Whether the two faces are actually different.
pub const HAS_SMOOTH: bool = {};",
        matched && !smooth.glyphs.is_empty()
    );

    let _ = writeln!(
        out,
        "
pub const GLYPH_COUNT: usize = {};",
        glyphs.len()
    );
    let _ = writeln!(
        out,
        "
/// Codepoints present in both faces, ascending, for binary search."
    );
    let _ = writeln!(out, "pub const CODEPOINTS: [u32; GLYPH_COUNT] = [");
    for glyph in glyphs {
        let _ = writeln!(out, "    {:#06x},", glyph.codepoint);
    }
    out.push_str(
        "];
",
    );

    let _ = writeln!(
        out,
        "
/// Advance width in pixels for each entry of [`CODEPOINTS`]."
    );
    let _ = writeln!(out, "pub const WIDTHS: [u8; GLYPH_COUNT] = [");
    for glyph in glyphs {
        let _ = writeln!(out, "    {},", glyph.width);
    }
    out.push_str(
        "];
",
    );

    for (name, note, face) in [
        (
            "CRISP",
            "Sixteen rows a glyph, sixteen four-bit coverage values a row, leftmost pixel in the most significant nibble.",
            glyphs.as_slice(),
        ),
        ("SMOOTH", "The same, from the anti-aliased face.", smooth_glyphs),
    ] {
        let _ = writeln!(out, "
/// {note}");
        // `static` and not `const`: a const array is copied at every use site,
        // and these are a hundred and fifty kilobytes each. What the renderer
        // wants is a reference to one row of one glyph, which is what a static
        // gives it.
        let _ = writeln!(out, "pub static {name}: [[u64; 16]; GLYPH_COUNT] = [");
        for glyph in face {
            out.push_str("    [");
            for (index, row) in glyph.rows.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "{row:#018x}");
            }
            out.push_str("],
");
        }
        out.push_str("];
");
    }

    out
}
