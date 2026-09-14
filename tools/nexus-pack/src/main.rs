//! `nexus-pack`: makes a `.nexus` package out of files on the build machine.
//!
//! Runs on the host, not on NexusOS. The machine that installs a package does
//! not build one — that is the whole shape of a package system, and a tool that
//! ran on both would be a tool whose interesting half never runs anywhere.
//!
//! It shares [`nexus_pkg`] with the installer, so the format has exactly one
//! implementation. A packer with its own idea of the layout is a packer that
//! agrees with the installer until somebody changes one of them.
//!
//! # Usage
//!
//! ```text
//! nexus-pack <output.nexus> <key> <name> <release> <path=file> [...]
//! ```
//!
//! where `path` is where the file goes when the package is installed, `file` is
//! where to read it from now, and `key` is a file of hex holding the private
//! key to sign with. The matching public key is written beside it with a
//! `.pub` extension the first time, and checked against the private key every
//! time after -- because a public key that has drifted from its private key is
//! a build that ships packages nothing can install, and that should fail here
//! rather than on somebody's machine.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.len() < 4 {
        eprintln!("usage: nexus-pack <output.nexus> <key> <name> <release> <path=file> [...]");
        return ExitCode::FAILURE;
    }

    let output = PathBuf::from(&arguments[0]);
    let key_path = PathBuf::from(&arguments[1]);
    let name = &arguments[2];
    let release = &arguments[3];

    let secret = match read_key(&key_path) {
        Ok(secret) => secret,
        Err(why) => {
            eprintln!("nexus-pack: {}: {why}", key_path.display());
            return ExitCode::FAILURE;
        }
    };
    let public = nexus_crypto::public_key(&secret);
    if let Err(why) = keep_public_key(&key_path, &public) {
        eprintln!("nexus-pack: {why}");
        return ExitCode::FAILURE;
    }

    // Read everything first, so that a package is either written whole or not
    // at all: a half-written package on disk is a package something will
    // eventually try to install.
    let mut contents: Vec<(String, Vec<u8>)> = Vec::new();
    for argument in &arguments[4..] {
        let Some((path, source)) = argument.split_once('=') else {
            eprintln!("nexus-pack: '{argument}' is not <path=file>");
            return ExitCode::FAILURE;
        };
        match std::fs::read(source) {
            Ok(bytes) => contents.push((path.to_string(), bytes)),
            Err(error) => {
                eprintln!("nexus-pack: cannot read {source}: {error}");
                return ExitCode::FAILURE;
            }
        }
    }

    let files: Vec<(&str, &[u8])> = contents
        .iter()
        .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
        .collect();

    let mut package = match nexus_pkg::build(name, release, &files) {
        Ok(package) => package,
        Err(error) => {
            eprintln!("nexus-pack: {error}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(error) = nexus_pkg::sign(&mut package, &secret) {
        eprintln!("nexus-pack: cannot sign: {error}");
        return ExitCode::FAILURE;
    }

    // Read back what was just built, with the same reader the machine will use,
    // and check the signature with the public key rather than assuming it. A
    // tool that wrote a package it could not itself verify would be a tool that
    // ships something unusable and finds out on the other side.
    match nexus_pkg::Package::open(&package).and_then(|opened| opened.verify_signed_by(&public)) {
        Ok(()) => {}
        Err(error) => {
            eprintln!("nexus-pack: what it built does not verify: {error}");
            return ExitCode::FAILURE;
        }
    }

    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    if let Err(error) = std::fs::write(&output, &package) {
        eprintln!("nexus-pack: cannot write {}: {error}", output.display());
        return ExitCode::FAILURE;
    }

    let digest = nexus_pkg::digest_of(&package);
    print!(
        "packed {name} {release}: {} files, {} bytes, signed, ",
        files.len(),
        package.len()
    );
    for byte in &digest[..8] {
        print!("{byte:02x}");
    }
    println!("...");
    ExitCode::SUCCESS
}

/// Read a private key: a file of hex, with `#` for comments.
fn read_key(path: &std::path::Path) -> Result<[u8; 32], String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let hex: String = text
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .flat_map(str::chars)
        .filter(|character| !character.is_whitespace())
        .collect();
    if hex.len() != 64 {
        return Err(format!("expected 64 hex characters, found {}", hex.len()));
    }
    let mut secret = [0u8; 32];
    for (index, slot) in secret.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .map_err(|_| String::from("not hex"))?;
    }
    Ok(secret)
}

/// Write the public key beside the private one, or check it if it is there.
fn keep_public_key(key_path: &std::path::Path, public: &[u8; 32]) -> Result<(), String> {
    let mut path = key_path.to_path_buf();
    path.set_extension("pub");

    let mut hex = String::new();
    for byte in public {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }

    if let Ok(existing) = std::fs::read_to_string(&path) {
        let found: String = existing
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .flat_map(str::chars)
            .filter(|character| !character.is_whitespace())
            .collect();
        if found != hex {
            return Err(format!(
                "{} does not belong to {}: the installer would refuse every \
                 package this signs",
                path.display(),
                key_path.display()
            ));
        }
        return Ok(());
    }

    let contents = format!(
        "# The public half of {}, written by nexus-pack.\n\
         # This is what the installer compiles in as the key it trusts.\n\
         {hex}\n",
        key_path.display()
    );
    std::fs::write(&path, contents).map_err(|error| error.to_string())
}
