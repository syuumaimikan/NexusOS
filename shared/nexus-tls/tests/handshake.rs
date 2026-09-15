//! A real TLS 1.3 handshake, against a real server.
//!
//! Everything in the crate's own tests is checked against published vectors or
//! against OpenSSL's signatures. This is the one that puts the whole thing
//! together and talks to something that does not know it exists: an
//! `openssl s_server` on a loopback port.
//!
//! It is an integration test rather than a unit test because it needs `std` --
//! sockets, processes, threads -- and the crate itself is `no_std` and will
//! stay that way. That division is the point: nothing in `nexus_tls` opens a
//! socket, so this is the only place that can.
//!
//! # If OpenSSL is not on this machine
//!
//! The tests report that and pass. A build machine without `openssl` is a real
//! thing, and failing there would mean a red suite that says nothing about the
//! code. The QEMU stage is where a missing handshake would actually be caught.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use nexus_tls::chain::Roots;
use nexus_tls::client::Client;

/// Where the fixtures are.
fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// Whether `openssl` can be run at all.
fn have_openssl() -> bool {
    Command::new("openssl")
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// A server, stopped when this is dropped.
struct Server {
    process: Child,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// Start `openssl s_server` with a certificate, and wait for it to listen.
fn start(certificate: &str, key: &str, port: u16) -> Option<Server> {
    let directory = fixtures();
    let process = Command::new("openssl")
        .arg("s_server")
        .args(["-accept", &port.to_string()])
        .arg("-cert")
        .arg(directory.join(certificate))
        .arg("-key")
        .arg(directory.join(key))
        // TLS 1.3 only, and the one suite this client speaks. Pinned rather
        // than left to negotiation so that a failure here is this client's
        // fault and not a difference of opinion about versions.
        .args(["-tls1_3", "-ciphersuites", "TLS_CHACHA20_POLY1305_SHA256"])
        .arg("-www")
        .arg("-quiet")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let mut server = Server { process, port };
    // Wait for the port rather than sleeping a fixed time: `s_server` takes
    // tens of milliseconds usually and much longer on a loaded machine, and a
    // sleep that is right today is a flake tomorrow.
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Some(server);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = server.process.kill();
    None
}

/// Unguessable enough for a test. **Not** how a real connection gets its key.
///
/// A real one uses the processor's hardware generator and refuses if there is
/// none; see `kernel/nexus-kernel/src/random.rs`. This is a test making two
/// different fixed-ish values so the handshake is deterministic enough to
/// debug.
fn test_random(seed: u8) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = seed
            .wrapping_mul(31)
            .wrapping_add(index as u8)
            .wrapping_add(7);
    }
    // Never all zeros, which `Client::start` refuses.
    out[0] |= 1;
    out
}

/// Do a whole handshake and fetch a page.
fn fetch(host: &str, port: u16, certificate: &str) -> Result<String, String> {
    let der = std::fs::read(fixtures().join(certificate)).map_err(|why| why.to_string())?;
    let mut roots = Roots::empty();
    assert!(roots.add(&der), "the fixture root should parse");

    // The fixtures are valid for ten years from September 2026; a fixed moment
    // inside that, so the test does not start failing in 2036 for a reason that
    // has nothing to do with the code.
    let now = Some(1_790_000_000i64);

    let mut client = Client::start(host, roots, now, test_random(3), test_random(11))
        .map_err(|why| format!("could not start: {why}"))?;

    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|why| why.to_string())?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .ok();

    let first = client.take_outgoing();
    stream.write_all(&first).map_err(|why| why.to_string())?;

    let mut buffer = [0u8; 4096];
    let mut asked = false;
    let mut page = Vec::new();

    for _ in 0..200 {
        let read = match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(why) => return Err(format!("reading: {why}")),
        };
        client
            .received(&buffer[..read])
            .map_err(|why| format!("the handshake failed: {why}"))?;

        let out = client.take_outgoing();
        if !out.is_empty() {
            stream.write_all(&out).map_err(|why| why.to_string())?;
        }

        if client.ready() && !asked {
            asked = true;
            let request = format!("GET / HTTP/1.0\r\nHost: {host}\r\n\r\n");
            client
                .send(request.as_bytes())
                .map_err(|why| format!("sending: {why}"))?;
            let out = client.take_outgoing();
            stream.write_all(&out).map_err(|why| why.to_string())?;
        }

        page.extend_from_slice(&client.take_plaintext());
        if page.windows(4).any(|window| window == b"\r\n\r\n") && page.len() > 64 {
            break;
        }
    }

    if !client.ready() {
        return Err(String::from("the handshake never finished"));
    }
    String::from_utf8(page).map_err(|why| why.to_string())
}

#[test]
fn a_whole_handshake_against_openssl_with_an_rsa_certificate() {
    if !have_openssl() {
        eprintln!("openssl is not on this machine; skipping the handshake test");
        return;
    }
    let Some(server) = start("rsa-leaf.pem", "rsa-leaf.key", 14433) else {
        eprintln!("openssl s_server would not start; skipping");
        return;
    };

    let page = fetch("example.test", server.port, "rsa-leaf.der")
        .expect("the handshake should finish and the page should arrive");
    assert!(
        page.contains("HTTP/1.0 200") || page.contains("s_server"),
        "the page did not look like s_server's: {}",
        &page[..page.len().min(200)]
    );
}

#[test]
fn a_whole_handshake_against_openssl_with_an_ecdsa_certificate() {
    if !have_openssl() {
        eprintln!("openssl is not on this machine; skipping the handshake test");
        return;
    }
    let Some(server) = start("ecdsa-leaf.pem", "ecdsa-leaf.key", 14434) else {
        eprintln!("openssl s_server would not start; skipping");
        return;
    };

    let page = fetch("ecdsa.test", server.port, "ecdsa-leaf.der")
        .expect("the handshake should finish and the page should arrive");
    assert!(
        page.contains("HTTP/1.0 200") || page.contains("s_server"),
        "the page did not look like s_server's"
    );
}

#[test]
fn a_certificate_this_machine_does_not_trust_is_refused() {
    // The test that says the verification is doing anything. The same server,
    // the same handshake, and an empty root store -- and it must fail.
    if !have_openssl() {
        eprintln!("openssl is not on this machine; skipping the handshake test");
        return;
    }
    let Some(server) = start("rsa-leaf.pem", "rsa-leaf.key", 14435) else {
        eprintln!("openssl s_server would not start; skipping");
        return;
    };

    let why = fetch_with_roots("example.test", server.port, Roots::empty())
        .expect_err("an untrusted certificate must not verify");
    assert!(
        why.contains("trust"),
        "the reason should say the chain is untrusted: {why}"
    );
}

#[test]
fn the_wrong_host_name_is_refused_against_a_real_server() {
    if !have_openssl() {
        eprintln!("openssl is not on this machine; skipping the handshake test");
        return;
    }
    let Some(server) = start("rsa-leaf.pem", "rsa-leaf.key", 14436) else {
        eprintln!("openssl s_server would not start; skipping");
        return;
    };

    let der = std::fs::read(fixtures().join("rsa-leaf.der")).unwrap();
    let mut roots = Roots::empty();
    assert!(roots.add(&der));

    // The certificate covers example.test and *.example.test, and this asks
    // for something else entirely.
    let why = fetch_with_roots("elsewhere.invalid", server.port, roots)
        .expect_err("a certificate for another name must not verify");
    assert!(
        why.contains("not for elsewhere.invalid"),
        "the reason should name the host: {why}"
    );
}

/// `fetch`, with the root store given rather than built from a fixture.
fn fetch_with_roots(host: &str, port: u16, roots: Roots) -> Result<String, String> {
    let now = Some(1_790_000_000i64);
    let mut client = Client::start(host, roots, now, test_random(5), test_random(13))
        .map_err(|why| format!("could not start: {why}"))?;

    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|why| why.to_string())?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .ok();
    stream
        .write_all(&client.take_outgoing())
        .map_err(|why| why.to_string())?;

    let mut buffer = [0u8; 4096];
    for _ in 0..200 {
        let read = match stream.read(&mut buffer) {
            Ok(0) => return Err(String::from("the server closed the connection")),
            Ok(count) => count,
            Err(why) => return Err(format!("reading: {why}")),
        };
        client
            .received(&buffer[..read])
            .map_err(|why| format!("{why}"))?;
        let out = client.take_outgoing();
        if !out.is_empty() {
            stream.write_all(&out).map_err(|why| why.to_string())?;
        }
        if client.ready() {
            return Ok(String::new());
        }
    }
    Err(String::from("the handshake never finished"))
}
