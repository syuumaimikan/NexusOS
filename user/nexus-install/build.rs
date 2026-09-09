//! Build script: compiles in the public key this installer trusts.
//!
//! The key is read from `keys/development.pub` rather than written into the
//! source, so that changing which key a machine trusts is changing one file
//! that is obviously a key — not editing an array of bytes in the middle of a
//! program, where a wrong digit looks like every other wrong digit.
//!
//! It is compiled in rather than read at runtime on purpose. A trusted key that
//! lived on the filesystem could be replaced by anything that can write to the
//! filesystem, which is precisely what a package installer is for.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo_root = manifest.parent().unwrap().parent().unwrap();
    let key = repo_root.join("keys").join("development.pub");
    println!("cargo:rerun-if-changed={}", key.display());

    let text = std::fs::read_to_string(&key).unwrap_or_else(|error| {
        panic!(
            "cannot read {}: {error}\nRun scripts/build.ps1, which writes it from the private key.",
            key.display()
        )
    });
    let hex: String = text
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .flat_map(str::chars)
        .filter(|character| !character.is_whitespace())
        .collect();
    assert!(
        hex.len() == 64,
        "{} should hold 64 hex characters, found {}",
        key.display(),
        hex.len()
    );

    let mut out = String::from(
        "/// The public key this installer trusts, from `keys/development.pub`.\n\
         pub const TRUSTED_KEY: [u8; 32] = [\n",
    );
    for index in 0..32 {
        let byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .unwrap_or_else(|_| panic!("{} is not hex", key.display()));
        out.push_str(&format!("    0x{byte:02x},\n"));
    }
    out.push_str("];\n");

    let destination = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("trusted_key.rs");
    std::fs::write(destination, out).unwrap();
}
