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
//! nexus-pack <output.nexus> <name> <release> <path=file> [<path=file> ...]
//! ```
//!
//! where `path` is where the file goes when the package is installed, and
//! `file` is where to read it from now.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.len() < 3 {
        eprintln!(
            "usage: nexus-pack <output.nexus> <name> <release> <path=file> [<path=file> ...]"
        );
        return ExitCode::FAILURE;
    }

    let output = PathBuf::from(&arguments[0]);
    let name = &arguments[1];
    let release = &arguments[2];

    // Read everything first, so that a package is either written whole or not
    // at all: a half-written package on disk is a package something will
    // eventually try to install.
    let mut contents: Vec<(String, Vec<u8>)> = Vec::new();
    for argument in &arguments[3..] {
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

    let package = match nexus_pkg::build(name, release, &files) {
        Ok(package) => package,
        Err(error) => {
            eprintln!("nexus-pack: {error}");
            return ExitCode::FAILURE;
        }
    };

    // Read back what was just built, with the same reader the machine will use.
    // A tool that wrote a package it could not itself open would be a tool that
    // ships a corrupt file and finds out on the other side.
    match nexus_pkg::Package::open(&package).and_then(|opened| opened.verify()) {
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
        "packed {name} {release}: {} files, {} bytes, ",
        files.len(),
        package.len()
    );
    for byte in &digest[..8] {
        print!("{byte:02x}");
    }
    println!("...");
    ExitCode::SUCCESS
}
