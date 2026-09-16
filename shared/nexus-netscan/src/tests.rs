//! What the deciding does, checked where it is easy to be confidently wrong.

use super::*;
use alloc::string::ToString;
use alloc::vec;

#[test]
fn an_open_connection_is_an_open_port() {
    assert_eq!(conclude(Progress::Open, 0, 1000), Some(Finding::Open));
    // Opened and already finished still accepted.
    assert_eq!(conclude(Progress::PeerDone, 0, 1000), Some(Finding::Open));
}

/// The distinction the whole crate turns on: a reset is evidence of a machine
/// and a timeout is not.
///
/// Collapsing these into "not open" is the obvious simplification and it throws
/// away the only thing that separates a live host with a shut port from an
/// address with nothing on it.
#[test]
fn a_reset_and_a_timeout_are_different_answers() {
    assert_eq!(conclude(Progress::Closed, 5, 1000), Some(Finding::Refused));
    assert_eq!(
        conclude(Progress::Connecting, 1000, 1000),
        Some(Finding::Silent)
    );
    assert!(Finding::Refused.proves_a_machine());
    assert!(!Finding::Silent.proves_a_machine());
}

#[test]
fn a_connection_still_going_is_not_concluded_early() {
    assert_eq!(conclude(Progress::Connecting, 999, 1000), None);
    assert_eq!(conclude(Progress::Connecting, 0, 1000), None);
}

/// A connection the service has lost track of is silence, not a refusal.
///
/// The direction of this matters. Reading it as a refusal would be inventing
/// evidence that a machine is there, which is the worse of the two mistakes: a
/// scan that misses a host wastes time, and one that reports a host that does
/// not exist sends somebody looking for it.
#[test]
fn a_forgotten_connection_is_silence_rather_than_a_machine() {
    assert_eq!(conclude(Progress::Unknown, 0, 1000), Some(Finding::Silent));
}

#[test]
fn addresses_go_both_ways() {
    assert_eq!(address("10.0.2.15"), Some([10, 0, 2, 15]));
    assert_eq!(show([10, 0, 2, 15]), "10.0.2.15");
    assert_eq!(address(" 192.168.1.1 "), Some([192, 168, 1, 1]));
    assert_eq!(address("255.255.255.255"), Some([255, 255, 255, 255]));
}

/// Strict on purpose. Each of these is a shape something else accepts, and
/// accepting them here would mean scanning an address other than the one that
/// was typed.
#[test]
fn a_nearly_right_address_is_refused() {
    assert_eq!(address("1.2.3"), None, "three parts");
    assert_eq!(address("1.2.3.4.5"), None, "five parts");
    assert_eq!(address("1.2.3.256"), None, "does not fit in a byte");
    assert_eq!(address("1.2.3."), None, "empty last part");
    assert_eq!(address(""), None);
    assert_eq!(address("10.0.0.x"), None);
}

/// The ends of a range are not machines, and the first of them would be
/// answered by everything on the network if it were probed.
#[test]
fn a_network_has_its_ends_taken_off() {
    let all = network("10.0.2.0/24").unwrap();
    assert_eq!(all.len(), 254, "a /24 holds 254 machines");
    assert_eq!(all[0], [10, 0, 2, 1]);
    assert_eq!(all[253], [10, 0, 2, 254]);
    assert!(!all.contains(&[10, 0, 2, 0]), "the network address");
    assert!(!all.contains(&[10, 0, 2, 255]), "the broadcast address");
}

/// An address given with bits set below the prefix names the same network.
/// `10.0.2.15/24` is how a person writes "the network my machine is on".
#[test]
fn a_network_is_found_from_any_address_in_it() {
    let from_host = network("10.0.2.15/24").unwrap();
    let from_base = network("10.0.2.0/24").unwrap();
    assert_eq!(from_host, from_base);
}

/// `/31` and `/32` have no room for a network and broadcast address, and both
/// are ordinary ways to write one machine. Taking the ends off would leave
/// nothing to scan.
#[test]
fn one_machine_written_as_a_network_is_still_one_machine() {
    assert_eq!(network("10.0.2.15/32"), Some(vec![[10, 0, 2, 15]]));
    assert_eq!(
        network("10.0.2.14/31"),
        Some(vec![[10, 0, 2, 14], [10, 0, 2, 15]])
    );
}

/// Sixteen million addresses is not a scan, it is a hang with a progress bar.
#[test]
fn a_network_too_large_to_finish_is_refused() {
    assert_eq!(network("10.0.0.0/8"), None);
    assert_eq!(network("10.0.0.0/15"), None);
    assert!(network("10.0.0.0/16").is_some(), "the largest allowed");
    assert_eq!(network("10.0.2.0/33"), None);
    assert_eq!(network("10.0.2.0"), None, "no prefix at all");
}

#[test]
fn ports_are_read_as_written() {
    assert_eq!(ports("22"), Some(vec![22]));
    assert_eq!(ports("80,443"), Some(vec![80, 443]));
    assert_eq!(ports("20-23"), Some(vec![20, 21, 22, 23]));
    assert_eq!(
        ports("443,80,443"),
        Some(vec![80, 443]),
        "sorted, once each"
    );
    assert_eq!(ports(""), Some(USUAL.to_vec()));
}

#[test]
fn a_port_list_that_makes_no_sense_is_refused() {
    assert_eq!(ports("0"), None, "there is no port zero");
    assert_eq!(ports("100-2"), None, "backwards");
    assert_eq!(ports("80,"), None, "an empty piece");
    assert_eq!(ports("http"), None);
    assert_eq!(ports("70000"), None, "does not fit in a port");
}

/// The reason this exists rather than a port-number table: whatever is
/// listening decides what it is.
#[test]
fn what_it_says_beats_what_the_port_number_suggests() {
    assert_eq!(identify(b"SSH-2.0-OpenSSH_9.6\r\n"), Some("ssh"));
    assert_eq!(identify(b"HTTP/1.1 400 Bad Request\r\n"), Some("http"));
    // A web server on 22. The table would say ssh with total confidence.
    let port = Port {
        number: 22,
        finding: Finding::Open,
        greeting: Some("HTTP/1.1 200 OK".to_string()),
    };
    assert!(line(&port).contains("http"), "{}", line(&port));
    assert!(!line(&port).contains("ssh"), "{}", line(&port));
}

#[test]
fn the_two_that_share_a_greeting_are_told_apart() {
    assert_eq!(
        identify(b"220 mail.example.com ESMTP Postfix"),
        Some("smtp")
    );
    assert_eq!(identify(b"220 ProFTPD 1.3.6 Server ready"), Some("ftp"));
    assert_eq!(identify(b"+OK POP3 ready"), Some("pop3"));
    assert_eq!(identify(b"* OK IMAP4rev1"), Some("imap"));
    assert_eq!(identify(b"RFB 003.008\n"), Some("vnc"));
}

/// Three bytes rather than one, so that a text protocol containing 0x16 does
/// not read as a TLS handshake.
#[test]
fn tls_is_recognised_by_three_bytes_and_not_one() {
    assert_eq!(identify(&[0x16, 0x03, 0x01, 0x02, 0x00]), Some("tls"));
    assert_eq!(identify(&[0x16, 0x03, 0x03]), Some("tls"));
    assert_eq!(identify(&[0x16, 0x99, 0x01]), None, "not a TLS version");
    assert_eq!(identify(&[0x16]), None, "one byte says nothing");
}

#[test]
fn a_greeting_nobody_recognises_is_not_guessed_at() {
    assert_eq!(identify(b"hello"), None);
    assert_eq!(identify(b""), None);
}

/// A banner is written by a machine somewhere else, and a window that printed
/// one raw would have its layout decided by that machine.
#[test]
fn a_greeting_cannot_take_over_the_window() {
    assert_eq!(shorten("SSH-2.0-OpenSSH\r\nmore"), "SSH-2.0-OpenSSH");
    assert_eq!(shorten("a\u{1b}[2Jb"), "a·[2Jb", "escapes are defanged");
    let long = "x".repeat(200);
    let cut = shorten(&long);
    assert!(
        cut.chars().count() <= 61,
        "{} characters",
        cut.chars().count()
    );
    assert!(cut.ends_with('…'));
}

#[test]
fn a_host_that_only_refused_is_still_a_host() {
    let host = Host {
        address: [10, 0, 2, 3],
        ports: vec![
            Port {
                number: 22,
                finding: Finding::Refused,
                greeting: None,
            },
            Port {
                number: 80,
                finding: Finding::Silent,
                greeting: None,
            },
        ],
    };
    assert!(host.answered(), "a reset is evidence of a machine");
    assert!(host.open().is_empty());
}

#[test]
fn a_host_that_said_nothing_at_all_is_not_reported_as_one() {
    let host = Host {
        address: [10, 0, 2, 3],
        ports: vec![Port {
            number: 22,
            finding: Finding::Silent,
            greeting: None,
        }],
    };
    assert!(!host.answered());
}

/// A name from the table is labelled as a convention, not as an observation.
#[test]
fn a_convention_is_not_reported_as_a_fact() {
    let port = Port {
        number: 22,
        finding: Finding::Open,
        greeting: None,
    };
    let said = line(&port);
    assert!(said.contains("ssh"), "{said}");
    assert!(said.contains("convention"), "{said}");
}
