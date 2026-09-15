//! Motion-JPEG in an AVI container.
//!
//! # What this is, and what it is not
//!
//! Motion-JPEG is not a video codec in the sense H.264 is. There is no motion
//! estimation, no prediction between frames, no B-frames and no rate control:
//! every frame is a complete JPEG, compressed as if it were a photograph and
//! decoded without reference to any other. Twenty frames a second of a talking
//! head costs the same as twenty photographs of a talking head, which is why
//! nobody streams films this way and why every webcam, scanner and security
//! camera does.
//!
//! That property is the reason it is here. A real codec is tens of thousands of
//! lines and a patent history; this is a container reader on top of the JPEG
//! decoder that already exists. Adding it says something true about the machine
//! — it can play video — without pretending to something it cannot do.
//!
//! # The container
//!
//! AVI is RIFF: a tree of four-byte-tagged chunks, each with a little-endian
//! length, each padded to an even byte.
//!
//! ```text
//! RIFF "AVI "
//!   LIST "hdrl"
//!     avih                  main header: frame interval, size, count
//!     LIST "strl"
//!       strh                stream header: 'vids', 'MJPG', rate/scale
//!       strf                BITMAPINFOHEADER: width, height, 'MJPG'
//!   LIST "movi"
//!     00dc ...              one complete JPEG per chunk
//!   idx1                    an index, which this deliberately ignores
//! ```
//!
//! # Why the index is ignored
//!
//! `idx1` gives each frame's offset, and every reader has to guess whether
//! those offsets are relative to the `movi` list or absolute in the file —
//! writers disagree, and a reader that guesses wrong reads rubbish. Files
//! written for streaming have no index at all.
//!
//! Walking `movi` chunk by chunk needs neither guess and works on both. The
//! cost is that a frame can only be found by passing the ones before it, which
//! is exactly what playing forwards does anyway. [`Reel::seen`] accumulates the
//! offsets as they go past, so a second pass over what has already been played
//! is direct.
//!
//! # Nothing is held that does not have to be
//!
//! This reads a **table of contents**, not a film. [`read`] takes bytes and
//! returns where each frame is; it never holds frame data. A caller with a
//! file handle reads one frame's bytes, decodes it, draws it, and lets both
//! go. A three-minute recording is hundreds of megabytes and the largest thing
//! this machine will give a program is sixteen, so a player that held the file
//! would be a player with a two-minute limit written into it.
//!
//! A caller that cannot hold the file cannot pass it here either, so there are
//! two ways in:
//!
//! * [`read`] for bytes already in memory — the whole file, or its first few
//!   kilobytes, which is enough for the size and the frame rate.
//! * [`chunk`], [`is_frame`] and [`next_chunk`] for a caller walking a file it
//!   only has a handle to. [`Reel::movi_at`] says where to start and
//!   [`Reel::movi_end`] where to stop; the walk costs one eight-byte read per
//!   frame and holds nothing.

use alloc::vec::Vec;

use crate::Trouble;

/// The most frames one file may claim.
///
/// A file saying it has four billion frames would otherwise have this allocate
/// an index for four billion frames before reading any of them. Sixty thousand
/// is over half an hour at thirty frames a second, which is longer than
/// anything this machine has the storage to hold.
pub const MOST_FRAMES: usize = 60_000;

/// The largest single frame this will admit to.
///
/// One frame has to fit in memory to be decoded. Eight megabytes is a very
/// generous JPEG — the 320x240 frames this was written against are seven
/// kilobytes — and it is half of what the kernel will hand out in one piece.
pub const LARGEST_FRAME: u32 = 8 * 1024 * 1024;

/// Where one frame sits in the file, and how long it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    /// Bytes from the start of the file to the first byte of the JPEG.
    pub at: u64,
    /// How many bytes the JPEG is.
    pub bytes: u32,
}

/// A Motion-JPEG recording: what it is, and where its frames are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reel {
    pub width: u32,
    pub height: u32,
    /// Microseconds between frames, from the file's own header.
    pub interval_us: u32,
    /// Where each frame found so far begins.
    ///
    /// This is every frame when [`Reel::read`] was given the whole file, and
    /// the frames in the part it was given otherwise.
    pub seen: Vec<Frame>,
    /// How many frames the header claims, which may exceed `seen.len()` when
    /// only the head of the file was read.
    pub claimed: u32,
    /// Where the first chunk of the `movi` list begins, for a caller walking
    /// the file rather than holding it. Zero if no `movi` list was in what was
    /// read.
    pub movi_at: u64,
    /// One past the last byte of the `movi` list.
    pub movi_end: u64,
}

impl Reel {
    /// How long the recording runs, in milliseconds, according to its header.
    #[must_use]
    pub fn length_ms(&self) -> u64 {
        u64::from(self.claimed) * u64::from(self.interval_us) / 1000
    }

    /// Frames per second, times a thousand, so it can be said exactly without
    /// a floating-point unit this kernel does not turn on.
    ///
    /// 10 fps is `10_000`; the awkward broadcast rate 29.97 is `29_970`.
    #[must_use]
    pub fn milli_fps(&self) -> u32 {
        if self.interval_us == 0 {
            return 0;
        }
        (1_000_000_000u64 / u64::from(self.interval_us)) as u32
    }
}

/// What the eight bytes that begin every chunk say: its tag, and how many
/// bytes of body follow them.
///
/// For a caller walking a file it holds only a handle to. The length does
/// **not** include the pad byte an odd-length chunk is followed by; use
/// [`next_chunk`] to step, rather than adding the length yourself, because
/// forgetting that pad is the classic way to read an AVI as rubbish from the
/// first odd-length frame onwards.
#[must_use]
pub fn chunk(header: &[u8; 8]) -> ([u8; 4], u32) {
    let mut kind = [0u8; 4];
    kind.copy_from_slice(&header[..4]);
    let length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    (kind, length)
}

/// Whether a chunk tag names a compressed video frame.
///
/// `00dc`, `01dc` and so on: a stream number, then `dc` for "compressed data".
/// `db` is uncompressed and cannot be a JPEG, and everything else in a `movi`
/// list is audio or padding.
#[must_use]
pub fn is_frame(kind: [u8; 4]) -> bool {
    kind[0].is_ascii_digit() && kind[2..] == *b"dc"
}

/// Where the chunk after this one begins.
///
/// `at` is where this chunk's eight-byte header began.
#[must_use]
pub fn next_chunk(at: u64, length: u32) -> u64 {
    at + 8 + u64::from(length) + u64::from(length & 1)
}

/// Read four bytes as a tag.
fn tag(bytes: &[u8], at: usize) -> Option<[u8; 4]> {
    bytes.get(at..at + 4)?.try_into().ok()
}

/// Read four bytes as a little-endian number.
fn word(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Say what a file is, and where its frames are.
///
/// `bytes` may be the whole file or only its beginning. The headers are near
/// the front, so a few kilobytes is enough to learn the size and frame rate;
/// every frame found in what was given is listed, and `claimed` says how many
/// the file says it has.
///
/// For a caller that means to walk the file through a handle, the head has to
/// reach the start of the `movi` list — [`Reel::movi_at`] is zero when it did
/// not, and the answer is to read more rather than to believe there are no
/// frames. **Sixty-four kilobytes is the size to ask for**: writers pad their
/// headers to an alignment and ffmpeg's come to about six, so that is an order
/// of magnitude of room, and it costs one read of a file that is going to be
/// megabytes.
///
/// # Errors
///
/// [`Trouble::NotAPicture`] if it is not a RIFF/AVI file or the headers are not
/// where the format says, and [`Trouble::Unsupported`] if the stream is some
/// other codec. A length that runs past the end of the file is not an error:
/// what was read before it is kept, because a recording cut off mid-copy is
/// still most of a recording.
pub fn read(bytes: &[u8]) -> Result<Reel, Trouble> {
    if tag(bytes, 0) != Some(*b"RIFF") || tag(bytes, 8) != Some(*b"AVI ") {
        return Err(Trouble::NotAPicture);
    }

    let mut width = 0u32;
    let mut height = 0u32;
    let mut interval_us = 0u32;
    let mut claimed = 0u32;
    let mut motion_jpeg = false;
    let mut seen = Vec::new();
    let mut movi_at = 0u64;
    let mut movi_end = 0u64;

    // The RIFF body starts after "RIFF", its length, and "AVI ".
    walk(
        bytes,
        12,
        bytes.len(),
        &mut movi_at,
        &mut movi_end,
        &mut |kind, body, end| {
            match &kind {
                b"avih" => {
                    // dwMicroSecPerFrame, then four more, then dwTotalFrames at 16,
                    // then dwWidth and dwHeight at 32 and 36.
                    interval_us = word(bytes, body).unwrap_or(0);
                    claimed = word(bytes, body + 16).unwrap_or(0);
                    width = word(bytes, body + 32).unwrap_or(0);
                    height = word(bytes, body + 36).unwrap_or(0);
                }
                b"strf" => {
                    // BITMAPINFOHEADER. biCompression is at 16. Some writers say
                    // "MJPG" and some "mjpg"; both mean the same thing and a
                    // reader that accepted only one would refuse half of them.
                    if let Some(compression) = tag(bytes, body + 16) {
                        let upper = compression.map(|byte| byte.to_ascii_uppercase());
                        if &upper == b"MJPG" {
                            motion_jpeg = true;
                        }
                    }
                }
                _ => {
                    // A frame. The stream number varies ("00dc", "01dc"); what
                    // matters is that it is compressed video, which "dc" says.
                    // "db" is uncompressed and cannot be a JPEG.
                    if kind[2..] == *b"dc" && kind[0].is_ascii_digit() {
                        let length = (end - body) as u32;
                        if length > 0 && length <= LARGEST_FRAME && seen.len() < MOST_FRAMES {
                            seen.push(Frame {
                                at: body as u64,
                                bytes: length,
                            });
                        }
                    }
                }
            }
            Ok(())
        },
    )?;

    if width == 0 || height == 0 {
        return Err(Trouble::NotAPicture);
    }
    if !motion_jpeg {
        return Err(Trouble::Unsupported(
            "a video stream that is not Motion-JPEG",
        ));
    }
    // A file whose header says nothing about timing still has to play at some
    // speed. Ten a second is slow enough to be watchable and fast enough to
    // read as movement; a zero here would divide by zero in every caller.
    if interval_us == 0 {
        interval_us = 100_000;
    }
    // The header's frame count is a claim, and a file truncated mid-recording
    // has more claimed than are there. Believe whichever is larger only when
    // the whole file was read -- which is the case a caller signals by there
    // being frames at all.
    if claimed == 0 {
        claimed = seen.len() as u32;
    }

    Ok(Reel {
        width,
        height,
        interval_us,
        seen,
        claimed,
        movi_at,
        movi_end,
    })
}

/// Walk the chunks between `from` and `to`, descending into `LIST`.
///
/// `visit` is called with the chunk's tag, where its body starts and where it
/// ends. `LIST` chunks are descended into rather than reported, because a list
/// is a container and the caller wants what is in it.
fn walk(
    bytes: &[u8],
    from: usize,
    to: usize,
    movi_at: &mut u64,
    movi_end: &mut u64,
    visit: &mut impl FnMut([u8; 4], usize, usize) -> Result<(), Trouble>,
) -> Result<(), Trouble> {
    let mut at = from;
    // Eight bytes is the smallest a chunk can be: a tag and a length.
    while at + 8 <= to {
        let Some(kind) = tag(bytes, at) else {
            return Ok(());
        };
        let Some(length) = word(bytes, at + 4) else {
            return Ok(());
        };
        let body = at + 8;
        // Where the file says this chunk ends, which is not always where the
        // bytes in hand end.
        let Some(declared) = body.checked_add(length as usize) else {
            return Ok(());
        };
        let short = declared > to;

        if &kind == b"LIST" || &kind == b"RIFF" {
            // The first four bytes of a list's body name what kind it is.
            if body + 4 <= to {
                // Recorded before descending, and with the length the *file*
                // gives rather than the bytes in hand, so that a caller given
                // only the head of a recording still learns where its frames
                // begin and end and can walk the rest through a handle.
                if tag(bytes, body) == Some(*b"movi") {
                    *movi_at = (body + 4) as u64;
                    *movi_end = declared as u64;
                }
                // A list cut off by the end of what was read is still
                // descended into, for what is there. This is the ordinary case
                // when a player reads a few hundred bytes to learn a file's
                // size: `hdrl` declares four kilobytes, the header it wants is
                // in the first hundred, and a reader that refused to look
                // inside a list it could not see the end of would have to hold
                // the whole file to learn anything at all.
                walk(bytes, body + 4, declared.min(to), movi_at, movi_end, visit)?;
            }
            if short {
                return Ok(());
            }
        } else {
            // A leaf is different: its body is the thing being reported, and
            // half a body is not a short answer but a wrong one. A length that
            // runs past the end is a file truncated mid-copy or a hostile one,
            // and stopping keeps whatever was read before it.
            if short {
                return Ok(());
            }
            visit(kind, body, declared)?;
        }

        // Chunks are padded to an even length, and the padding byte is not
        // counted in the length. A reader that forgets this walks off by one
        // and finds rubbish from the first odd-length chunk onwards.
        at = declared + (length as usize & 1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an AVI around the frames given, the way a writer would.
    fn an_avi(frames: &[&[u8]], width: u32, height: u32, interval_us: u32) -> Vec<u8> {
        let mut movi = Vec::new();
        movi.extend_from_slice(b"movi");
        for frame in frames {
            movi.extend_from_slice(b"00dc");
            movi.extend_from_slice(&(frame.len() as u32).to_le_bytes());
            movi.extend_from_slice(frame);
            if frame.len() % 2 == 1 {
                movi.push(0);
            }
        }

        let mut avih = Vec::new();
        avih.extend_from_slice(&interval_us.to_le_bytes());
        avih.extend_from_slice(&[0u8; 12]); // max bytes/sec, padding, flags
        avih.extend_from_slice(&(frames.len() as u32).to_le_bytes());
        avih.extend_from_slice(&[0u8; 12]); // initial frames, streams, buffer
        avih.extend_from_slice(&width.to_le_bytes());
        avih.extend_from_slice(&height.to_le_bytes());
        avih.extend_from_slice(&[0u8; 16]); // reserved
        assert_eq!(avih.len(), 56);

        let mut strf = Vec::new();
        strf.extend_from_slice(&40u32.to_le_bytes());
        strf.extend_from_slice(&width.to_le_bytes());
        strf.extend_from_slice(&height.to_le_bytes());
        strf.extend_from_slice(&1u16.to_le_bytes());
        strf.extend_from_slice(&24u16.to_le_bytes());
        strf.extend_from_slice(b"MJPG");
        strf.extend_from_slice(&[0u8; 20]);
        assert_eq!(strf.len(), 40);

        let mut strl = Vec::new();
        strl.extend_from_slice(b"strl");
        strl.extend_from_slice(b"strf");
        strl.extend_from_slice(&(strf.len() as u32).to_le_bytes());
        strl.extend_from_slice(&strf);

        let mut hdrl = Vec::new();
        hdrl.extend_from_slice(b"hdrl");
        hdrl.extend_from_slice(b"avih");
        hdrl.extend_from_slice(&(avih.len() as u32).to_le_bytes());
        hdrl.extend_from_slice(&avih);
        hdrl.extend_from_slice(b"LIST");
        hdrl.extend_from_slice(&(strl.len() as u32).to_le_bytes());
        hdrl.extend_from_slice(&strl);

        let mut body = Vec::new();
        body.extend_from_slice(b"AVI ");
        body.extend_from_slice(b"LIST");
        body.extend_from_slice(&(hdrl.len() as u32).to_le_bytes());
        body.extend_from_slice(&hdrl);
        body.extend_from_slice(b"LIST");
        body.extend_from_slice(&(movi.len() as u32).to_le_bytes());
        body.extend_from_slice(&movi);

        let mut file = Vec::new();
        file.extend_from_slice(b"RIFF");
        file.extend_from_slice(&(body.len() as u32).to_le_bytes());
        file.extend_from_slice(&body);
        file
    }

    #[test]
    fn a_file_can_be_walked_with_a_handle_and_no_bytes() {
        // The path a player takes: learn the shape from the first few hundred
        // bytes, then find every frame with one eight-byte read apiece and the
        // file never held.
        let file = include_bytes!("../fixtures/clip.avi");
        // Eight kilobytes: past the headers, past the start of the frame list,
        // and nowhere near the end of a thirty-four kilobyte recording.
        let head = &file[..8 * 1024];
        let reel = read(head).unwrap();
        assert_eq!(reel.width, 160);
        assert_eq!(reel.height, 120);
        assert_eq!(reel.claimed, 10);
        assert_ne!(reel.movi_at, 0, "the head should say where the frames are");

        // Walking, as a caller with only `read_at` would.
        let mut found = Vec::new();
        let mut at = reel.movi_at;
        while at + 8 <= reel.movi_end {
            let mut header = [0u8; 8];
            header.copy_from_slice(&file[at as usize..at as usize + 8]);
            let (kind, length) = chunk(&header);
            if is_frame(kind) {
                found.push(Frame {
                    at: at + 8,
                    bytes: length,
                });
            }
            at = next_chunk(at, length);
        }

        // And it finds exactly what reading the whole file finds.
        let whole = read(file).unwrap();
        assert_eq!(found, whole.seen);
        assert_eq!(found.len(), 10);
        // The head really was only a head: it saw the shape of the recording
        // and a frame or two, not all ten.
        assert!(
            reel.seen.len() < 10,
            "the head should not have contained every frame"
        );
    }

    #[test]
    fn a_head_too_short_to_reach_the_frames_says_so_rather_than_saying_none() {
        // A player that read too little and believed `movi_at` was where the
        // frames were would seek to zero and decode the RIFF header as a JPEG.
        // Zero means "read more", and it has to be distinguishable.
        let file = include_bytes!("../fixtures/clip.avi");
        let reel = read(&file[..512]).unwrap();
        assert_eq!(
            reel.width, 160,
            "the size is in the first few hundred bytes"
        );
        assert_eq!(reel.movi_at, 0, "and the frames are not");
        assert!(reel.seen.is_empty());
    }

    #[test]
    fn stepping_over_a_chunk_accounts_for_its_pad_byte() {
        assert_eq!(next_chunk(0, 4), 12, "an even chunk has no pad");
        assert_eq!(next_chunk(0, 5), 14, "an odd chunk is followed by one");
    }

    #[test]
    fn only_compressed_video_chunks_are_frames() {
        assert!(is_frame(*b"00dc"));
        assert!(is_frame(*b"01dc"));
        assert!(!is_frame(*b"00db"), "db is uncompressed, not a JPEG");
        assert!(!is_frame(*b"01wb"), "wb is audio");
        assert!(!is_frame(*b"idx1"));
        assert!(!is_frame(*b"JUNK"));
    }

    #[test]
    fn the_size_and_rate_come_from_the_header() {
        let file = an_avi(&[b"one", b"two"], 320, 240, 100_000);
        let reel = read(&file).unwrap();
        assert_eq!(reel.width, 320);
        assert_eq!(reel.height, 240);
        assert_eq!(reel.interval_us, 100_000);
        assert_eq!(reel.claimed, 2);
        assert_eq!(reel.milli_fps(), 10_000);
        assert_eq!(reel.length_ms(), 200);
    }

    #[test]
    fn every_frame_is_found_where_it_is() {
        let file = an_avi(&[b"alpha", b"beta", b"gamma"], 16, 16, 40_000);
        let reel = read(&file).unwrap();
        assert_eq!(reel.seen.len(), 3);
        for (frame, expected) in reel
            .seen
            .iter()
            .zip([b"alpha".as_slice(), b"beta", b"gamma"])
        {
            let at = frame.at as usize;
            assert_eq!(&file[at..at + frame.bytes as usize], expected);
        }
    }

    #[test]
    fn an_odd_length_frame_does_not_shift_the_next_one() {
        // "alpha" is five bytes, so a pad byte follows it that is not counted
        // in its length. A reader that misses the pad reads the next chunk's
        // tag one byte early and finds nothing after the first odd frame.
        let file = an_avi(&[b"alpha", b"bee"], 16, 16, 40_000);
        let reel = read(&file).unwrap();
        assert_eq!(
            reel.seen.len(),
            2,
            "the pad byte after an odd frame was missed"
        );
        let second = reel.seen[1];
        assert_eq!(
            &file[second.at as usize..second.at as usize + second.bytes as usize],
            b"bee"
        );
    }

    #[test]
    fn a_lowercase_fourcc_is_the_same_format() {
        let mut file = an_avi(&[b"one"], 8, 8, 50_000);
        let at = file
            .windows(4)
            .position(|window| window == b"MJPG")
            .expect("the fixture should contain the tag");
        file[at..at + 4].copy_from_slice(b"mjpg");
        assert!(read(&file).is_ok(), "mjpg and MJPG are the same format");
    }

    #[test]
    fn something_that_is_not_an_avi_is_refused() {
        assert_eq!(
            read(b"not a file at all").unwrap_err(),
            Trouble::NotAPicture
        );
        let mut file = an_avi(&[b"one"], 8, 8, 50_000);
        file[8..12].copy_from_slice(b"WAVE");
        assert_eq!(read(&file).unwrap_err(), Trouble::NotAPicture);
    }

    #[test]
    fn a_stream_that_is_not_motion_jpeg_is_refused_rather_than_guessed_at() {
        let mut file = an_avi(&[b"one"], 8, 8, 50_000);
        let at = file
            .windows(4)
            .position(|window| window == b"MJPG")
            .expect("the fixture should contain the tag");
        file[at..at + 4].copy_from_slice(b"H264");
        assert!(matches!(read(&file), Err(Trouble::Unsupported(_))));
    }

    #[test]
    fn a_length_running_past_the_end_stops_rather_than_reading_rubbish() {
        let mut file = an_avi(&[b"one", b"two"], 8, 8, 50_000);
        // Find the second frame's chunk and claim it is enormous.
        let at = file
            .windows(4)
            .enumerate()
            .filter(|(_, window)| *window == b"00dc")
            .nth(1)
            .map(|(at, _)| at)
            .expect("two frames should have two chunk headers");
        file[at + 4..at + 8].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        // The first frame still reads. The second does not, and nothing walks
        // off the end looking for it.
        let reel = read(&file).unwrap();
        assert_eq!(reel.seen.len(), 1);
    }

    #[test]
    fn a_file_with_no_frame_interval_still_plays() {
        let file = an_avi(&[b"one", b"two"], 8, 8, 0);
        let reel = read(&file).unwrap();
        assert_ne!(reel.interval_us, 0, "a zero interval would divide by zero");
        assert_ne!(reel.milli_fps(), 0);
    }

    #[test]
    fn the_frames_of_a_real_recording_are_all_jpegs() {
        // ffmpeg's own Motion-JPEG output: ten frames of a 160x120 test
        // pattern at ten a second. This is the test that matters, because
        // everything above is this file format as this project understands it
        // and this is the format as somebody else writes it.
        let file = include_bytes!("../fixtures/clip.avi");
        let reel = read(file).unwrap();
        assert_eq!(reel.width, 160);
        assert_eq!(reel.height, 120);
        assert_eq!(reel.interval_us, 100_000);
        assert_eq!(reel.claimed, 10);
        assert_eq!(reel.seen.len(), 10, "all ten frames should be found");

        for (number, frame) in reel.seen.iter().enumerate() {
            let at = frame.at as usize;
            let jpeg = &file[at..at + frame.bytes as usize];
            assert_eq!(
                &jpeg[..2],
                b"\xFF\xD8",
                "frame {number} does not start with a JPEG marker"
            );
        }
    }

    #[test]
    fn a_real_recording_decodes_frame_by_frame() {
        // And they are not merely JPEG-shaped: every one of them goes through
        // this project's decoder and comes out the size the container said.
        let file = include_bytes!("../fixtures/clip.avi");
        let reel = read(file).unwrap();
        let mut decoded = 0;
        for frame in &reel.seen {
            let at = frame.at as usize;
            let picture = crate::decode(
                &file[at..at + frame.bytes as usize],
                0x000000,
                crate::MOST_PIXELS,
            )
            .expect("every frame of a real recording should decode");
            assert_eq!(picture.width, reel.width);
            assert_eq!(picture.height, reel.height);
            decoded += 1;
        }
        assert_eq!(decoded, 10);
    }

    #[test]
    fn the_picture_changes_between_frames() {
        // A decoder that returned the first frame forever would pass every
        // test above. This is the one that says the film moves.
        let file = include_bytes!("../fixtures/clip.avi");
        let reel = read(file).unwrap();
        let frame_at = |index: usize| {
            let frame = reel.seen[index];
            let at = frame.at as usize;
            crate::decode(
                &file[at..at + frame.bytes as usize],
                0x000000,
                crate::MOST_PIXELS,
            )
            .unwrap()
            .pixels
        };
        let first = frame_at(0);
        let last = frame_at(reel.seen.len() - 1);
        let moved = first
            .iter()
            .zip(last.iter())
            .filter(|(a, b)| a != b)
            .count();
        assert!(
            moved > first.len() / 20,
            "only {moved} of {} pixels differ between the first and last frame",
            first.len()
        );
    }

    #[test]
    fn frames_are_not_held_in_the_reel() {
        // The whole point of returning offsets rather than bytes. A reel over
        // a thirty-kilobyte file must not itself be thirty kilobytes, or a long
        // recording could not be opened at all.
        let file = include_bytes!("../fixtures/clip.avi");
        let reel = read(file).unwrap();
        let held = reel.seen.len() * core::mem::size_of::<Frame>();
        assert!(
            held < file.len() / 8,
            "the table of contents is {held} bytes for a {} byte file",
            file.len()
        );
        assert_eq!(core::mem::size_of::<Frame>(), 16);
    }
}
