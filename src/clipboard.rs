//! Clipboard sync rules that do not depend on the OS: loop prevention,
//! de-duplication without a history, bounded reassembly, defensive PNG
//! handling.
//!
//! Nothing here stores clipboard content: the guard keeps at most the single
//! most recent content hash for ten seconds, and forgets it on pause.

use std::collections::VecDeque;
use std::io::Cursor;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::identity::Fingerprint;
use crate::limits::*;
use crate::platform::ClipContent;
use crate::proto::{ClipKind, Msg};

const HASH_TTL: Duration = Duration::from_secs(10);
const SEEN_IDS: usize = 16;
/// An in-flight reassembly with no new chunk for this long is dropped.
pub const INCOMING_STALL: Duration = Duration::from_secs(10);

pub fn encode_png_rgba(w: u32, h: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    if w == 0 || h == 0 || (w as u64) * (h as u64) > MAX_IMAGE_PIXELS || rgba.len() as u64 != (w as u64) * (h as u64) * 4 {
        return Err("image has unsupported dimensions".into());
    }
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    let mut wr = enc.write_header().map_err(|e| e.to_string())?;
    wr.write_image_data(rgba).map_err(|e| e.to_string())?;
    wr.finish().map_err(|e| e.to_string())?;
    Ok(out)
}

/// Decode an untrusted PNG to RGBA8. The declared dimensions are checked
/// *before* any pixel buffer is allocated, so a tiny file cannot demand
/// gigabytes.
pub fn decode_png_rgba(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let mut dec = png::Decoder::new(Cursor::new(bytes));
    dec.set_limits(png::Limits { bytes: (MAX_IMAGE_PIXELS as usize) * 4 });
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().map_err(|e| format!("not a valid image: {e}"))?;
    let (w, h) = (reader.info().width, reader.info().height);
    if w == 0 || h == 0 || (w as u64) * (h as u64) > MAX_IMAGE_PIXELS {
        return Err("image is too large".into());
    }
    let mut buf = vec![0u8; reader.output_buffer_size().ok_or("image is too large")?];
    let frame = reader.next_frame(&mut buf).map_err(|e| format!("not a valid image: {e}"))?;
    buf.truncate(frame.buffer_size());
    let px = (w as usize) * (h as usize);
    let rgba = match frame.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf.chunks_exact(3).flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf.chunks_exact(2).flat_map(|c| [c[0], c[0], c[0], c[1]]).collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|g| [*g, *g, *g, 255]).collect(),
        png::ColorType::Indexed => return Err("unsupported image format".into()),
    };
    if rgba.len() != px * 4 {
        return Err("image data is inconsistent".into());
    }
    Ok((w, h, rgba))
}

fn digest(c: &ClipContent) -> [u8; 32] {
    let mut h = Sha256::new();
    match c {
        ClipContent::Text(t) => {
            h.update([0u8]);
            h.update(t.as_bytes());
        }
        ClipContent::Image(i) => {
            h.update([1u8]);
            h.update(i);
        }
    }
    h.finalize().into()
}

/// Stops clipboard ping-pong and duplicate sends.
#[derive(Default)]
pub struct ClipGuard {
    /// Change token produced by *our own* write; that change is not the user's.
    own_token: Option<u64>,
    /// Latest applied/sent content, for backends without a change token.
    last: Option<([u8; 32], Instant)>,
    /// (origin, id) of recent offers. Identifiers only – no content.
    seen: VecDeque<(Fingerprint, u64)>,
    next_id: u64,
}

impl ClipGuard {
    pub fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Should content the watcher just saw be sent to peers?
    pub fn should_send(&mut self, token: Option<u64>, c: &ClipContent, now: Instant) -> bool {
        if token.is_some() && token == self.own_token {
            return false;
        }
        let d = digest(c);
        if matches!(self.last, Some((h, t)) if h == d && now.duration_since(t) < HASH_TTL) {
            return false;
        }
        self.last = Some((d, now));
        true
    }

    /// We just wrote `c` into the OS clipboard on behalf of a peer.
    pub fn note_applied(&mut self, token_after_write: Option<u64>, c: &ClipContent, now: Instant) {
        self.own_token = token_after_write;
        self.last = Some((digest(c), now));
    }

    /// False if this offer was already handled (a relay loop).
    pub fn accept_offer(&mut self, origin: Fingerprint, id: u64) -> bool {
        if self.seen.contains(&(origin, id)) {
            return false;
        }
        if self.seen.len() == SEEN_IDS {
            self.seen.pop_front();
        }
        self.seen.push_back((origin, id));
        true
    }

    /// Forget everything (sharing paused, session ended).
    pub fn clear(&mut self) {
        self.own_token = None;
        self.last = None;
        self.seen.clear();
    }
}

/// Reassembles one incoming clipboard payload with a hard size cap.
pub struct Incoming {
    pub id: u64,
    pub origin: Fingerprint,
    kind: ClipKind,
    expected: usize,
    buf: Vec<u8>,
    pub last_chunk: Instant,
}

#[derive(Debug, PartialEq, Eq)]
pub enum IncomingError {
    TooLarge,
    Overrun,
    BadText,
}

impl Incoming {
    pub fn new(id: u64, origin: Fingerprint, kind: ClipKind, size: u32, now: Instant) -> Result<Self, IncomingError> {
        let max = match kind {
            ClipKind::Text => MAX_CLIPBOARD_TEXT,
            ClipKind::Image => MAX_CLIPBOARD_IMAGE,
        };
        let expected = size as usize;
        if expected > max {
            return Err(IncomingError::TooLarge);
        }
        // Grow as data actually arrives rather than trusting the declared size.
        Ok(Self { id, origin, kind, expected, buf: Vec::with_capacity(expected.min(CLIPBOARD_CHUNK)), last_chunk: now })
    }

    pub fn push(&mut self, data: &[u8], now: Instant) -> Result<Option<ClipContent>, IncomingError> {
        if self.buf.len() + data.len() > self.expected {
            return Err(IncomingError::Overrun);
        }
        self.buf.extend_from_slice(data);
        self.last_chunk = now;
        if self.buf.len() < self.expected {
            return Ok(None);
        }
        let bytes = std::mem::take(&mut self.buf);
        Ok(Some(match self.kind {
            ClipKind::Text => ClipContent::Text(String::from_utf8(bytes).map_err(|_| IncomingError::BadText)?),
            ClipKind::Image => ClipContent::Image(bytes),
        }))
    }
}

/// Split content into wire messages: one offer, then bounded chunks.
pub fn outgoing(id: u64, origin: Fingerprint, c: &ClipContent) -> Option<(Msg, Vec<Msg>)> {
    let (kind, bytes) = match c {
        ClipContent::Text(t) => (ClipKind::Text, t.as_bytes()),
        ClipContent::Image(i) => (ClipKind::Image, i.as_slice()),
    };
    let max = if kind == ClipKind::Text { MAX_CLIPBOARD_TEXT } else { MAX_CLIPBOARD_IMAGE };
    if bytes.is_empty() || bytes.len() > max {
        return None;
    }
    let offer = Msg::ClipOffer { id, origin, kind, size: bytes.len() as u32 };
    let chunks = bytes.chunks(CLIPBOARD_CHUNK).map(|c| Msg::ClipChunk { id, data: c.to_vec() }).collect();
    Some((offer, chunks))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(n: u8) -> Fingerprint {
        Fingerprint([n; 32])
    }
    fn text(s: &str) -> ClipContent {
        ClipContent::Text(s.into())
    }

    #[test]
    fn own_write_is_not_echoed_back() {
        let mut g = ClipGuard::default();
        let t = Instant::now();
        g.note_applied(Some(5), &text("from peer"), t);
        assert!(!g.should_send(Some(5), &text("from peer"), t), "same change token");
        assert!(g.should_send(Some(6), &text("user typed this"), t), "a later, different change is the user's");
    }

    #[test]
    fn without_a_token_a_short_lived_hash_stops_the_echo_then_expires() {
        let mut g = ClipGuard::default();
        let t = Instant::now();
        g.note_applied(None, &text("x"), t);
        assert!(!g.should_send(None, &text("x"), t + Duration::from_secs(1)));
        assert!(g.should_send(None, &text("x"), t + Duration::from_secs(11)), "after the TTL the user may copy it again");
        assert!(g.should_send(None, &text("y"), t));
    }

    #[test]
    fn repeated_identical_copies_are_sent_once() {
        let mut g = ClipGuard::default();
        let t = Instant::now();
        assert!(g.should_send(Some(1), &text("a"), t));
        assert!(!g.should_send(Some(2), &text("a"), t));
    }

    #[test]
    fn relayed_offers_are_recognised_by_origin_and_id_only() {
        let mut g = ClipGuard::default();
        assert!(g.accept_offer(fp(1), 7));
        assert!(!g.accept_offer(fp(1), 7));
        assert!(g.accept_offer(fp(2), 7));
        for i in 0..SEEN_IDS as u64 + 4 {
            g.accept_offer(fp(3), i);
        }
        assert!(g.accept_offer(fp(1), 7), "bounded memory: old ids age out");
        g.clear();
        assert!(g.accept_offer(fp(3), 0));
    }

    #[test]
    fn reassembly_enforces_declared_size_and_limits() {
        let t = Instant::now();
        assert_eq!(Incoming::new(1, fp(1), ClipKind::Text, (MAX_CLIPBOARD_TEXT + 1) as u32, t).err(), Some(IncomingError::TooLarge));
        assert!(Incoming::new(1, fp(1), ClipKind::Image, MAX_CLIPBOARD_IMAGE as u32, t).is_ok());
        let mut i = Incoming::new(1, fp(1), ClipKind::Text, 5, t).unwrap();
        assert_eq!(i.push(b"he", t).unwrap(), None);
        assert_eq!(i.push(b"llo", t).unwrap(), Some(text("hello")));
        let mut i = Incoming::new(1, fp(1), ClipKind::Text, 3, t).unwrap();
        assert_eq!(i.push(b"toolong", t).err(), Some(IncomingError::Overrun));
        let mut i = Incoming::new(1, fp(1), ClipKind::Text, 2, t).unwrap();
        assert_eq!(i.push(&[0xFF, 0xFE], t).err(), Some(IncomingError::BadText));
    }

    #[test]
    fn outgoing_splits_into_bounded_chunks_that_reassemble() {
        let big = "ab".repeat(CLIPBOARD_CHUNK + 10);
        let (offer, chunks) = outgoing(9, fp(1), &text(&big)).unwrap();
        assert!(chunks.len() >= 3);
        let Msg::ClipOffer { size, kind, .. } = offer else { panic!() };
        let mut inc = Incoming::new(9, fp(1), kind, size, Instant::now()).unwrap();
        let mut done = None;
        for c in chunks {
            let Msg::ClipChunk { data, .. } = c else { panic!() };
            assert!(data.len() <= CLIPBOARD_CHUNK);
            done = inc.push(&data, Instant::now()).unwrap();
        }
        assert_eq!(done, Some(text(&big)));
        assert!(outgoing(1, fp(1), &text("")).is_none());
        assert!(outgoing(1, fp(1), &text(&"x".repeat(MAX_CLIPBOARD_TEXT + 1))).is_none());
    }

    #[test]
    fn png_round_trip_and_defensive_rejects() {
        let rgba: Vec<u8> = (0..4 * 4 * 4).map(|i| i as u8).collect();
        let png = encode_png_rgba(4, 4, &rgba).unwrap();
        assert_eq!(decode_png_rgba(&png).unwrap(), (4, 4, rgba));
        assert!(decode_png_rgba(b"not a png").is_err());
        assert!(decode_png_rgba(&png[..png.len() / 2]).is_err());
        assert!(encode_png_rgba(0, 4, &[]).is_err());
        assert!(encode_png_rgba(2, 2, &[0; 3]).is_err());
    }

    #[test]
    fn png_bomb_header_is_rejected_before_allocating_pixels() {
        // Valid signature + IHDR claiming 60000 x 60000 (3.6 Gpx) in a ~30-byte file.
        let mut bomb = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R'];
        bomb.extend_from_slice(&60000u32.to_be_bytes());
        bomb.extend_from_slice(&60000u32.to_be_bytes());
        bomb.extend_from_slice(&[8, 6, 0, 0, 0]);
        let crc = {
            // CRC-32 of "IHDR"+data, computed bitwise to avoid another dependency.
            let mut c = 0xFFFF_FFFFu32;
            for b in &bomb[12..] {
                c ^= *b as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
                }
            }
            !c
        };
        bomb.extend_from_slice(&crc.to_be_bytes());
        let started = Instant::now();
        let err = decode_png_rgba(&bomb).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(err.contains("too large") || err.contains("not a valid"), "{err}");
    }
}
