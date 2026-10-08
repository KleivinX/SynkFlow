//! Wire protocol: version preamble, length-bounded frames, postcard messages.
//!
//! Compatibility rule: the enums below are **append-only** within a protocol
//! major version (postcard encodes variants by index). Anything else bumps
//! [`PROTOCOL_MAJOR`](crate::limits::PROTOCOL_MAJOR).

use std::io;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::geometry::LayoutDoc;
use crate::identity::Fingerprint;
use crate::limits::*;

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("network error: {0}")]
    Io(#[from] io::Error),
    #[error("peer speaks protocol v{0}, this build speaks v{PROTOCOL_MAJOR}")]
    VersionMismatch(u16),
    #[error("not a Synkflow peer")]
    BadMagic,
    #[error("malformed message")]
    Malformed,
    #[error("message too large")]
    TooLarge,
    #[error("message not allowed here")]
    Unexpected,
}

// ───────────────────────────── building blocks ─────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    MacOs,
    Windows,
    LinuxX11,
    LinuxWayland,
    Other,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Platform::MacOs
        } else if cfg!(target_os = "windows") {
            Platform::Windows
        } else if cfg!(target_os = "linux") {
            match std::env::var("XDG_SESSION_TYPE").as_deref() {
                Ok("wayland") => Platform::LinuxWayland,
                _ if std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("DISPLAY").is_none() => Platform::LinuxWayland,
                _ => Platform::LinuxX11,
            }
        } else {
            Platform::Other
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Platform::MacOs => "macOS",
            Platform::Windows => "Windows",
            Platform::LinuxX11 => "Linux (X11)",
            Platform::LinuxWayland => "Linux (Wayland)",
            Platform::Other => "Unknown",
        }
    }
    /// macOS uses ⌘ for shortcuts; everything else uses Ctrl.
    pub fn is_mac(self) -> bool {
        self == Platform::MacOs
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// Can capture local input and send it to a peer.
    pub input_source: bool,
    /// Can inject input received from a peer.
    pub input_inject: bool,
    pub clipboard_text: bool,
    pub clipboard_image: bool,
    pub files: bool,
}

impl Capabilities {
    pub fn intersect(self, o: Self) -> Self {
        Self {
            input_source: self.input_source && o.input_inject,
            input_inject: self.input_inject && o.input_source,
            clipboard_text: self.clipboard_text && o.clipboard_text,
            clipboard_image: self.clipboard_image && o.clipboard_image,
            files: self.files && o.files,
        }
    }
}

/// What the sender currently lets the *receiver* do to it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Grants {
    pub control: bool,
    pub clipboard: bool,
    pub files: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
    Other(u8),
}

/// One physical monitor, in the device's own logical points.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct DisplayInfo {
    pub id: u32,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Backing scale (pixels per point), e.g. 2.0 on Retina.
    pub scale: f32,
    /// 0, 90, 180 or 270.
    pub rotation: u16,
    pub primary: bool,
}

impl DisplayInfo {
    pub fn pixel_size(&self) -> (u32, u32) {
        ((self.width as f32 * self.scale).round() as u32, (self.height as f32 * self.scale).round() as u32)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    /// Absolute pointer position in a display's own logical points.
    PointerAbs {
        display: u32,
        x: f32,
        y: f32,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    /// `pixels == true`: smooth/trackpad deltas; otherwise wheel notches.
    Scroll {
        dx: f32,
        dy: f32,
        pixels: bool,
    },
    /// Physical key position as a USB HID usage on page 0x07.
    Key {
        usage: u16,
        down: bool,
        repeat: bool,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseReason {
    EdgeReturn,
    Manual,
    Panic,
    Paused,
    Locked,
    PermissionRevoked,
    Shutdown,
    SessionLost,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    VersionMismatch,
    Busy,
    AlreadyConnected,
    RateLimited,
    Unsupported,
    BadRequest,
    NotAllowed,
    Paused,
    Locked,
    NoSuchDisplay,
    TooLarge,
    NoSpace,
    UnsafeName,
    Declined,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipKind {
    Text,
    /// PNG bytes.
    Image,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Hello {
    pub name: String,
    pub platform: Platform,
    pub app_version: String,
    pub caps: Capabilities,
    pub grants: Grants,
    /// Chosen by the connecting side; names this session.
    pub session_id: [u8; 16],
    pub displays: Vec<DisplayInfo>,
    pub paused: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct HelloAck {
    pub name: String,
    pub platform: Platform,
    pub app_version: String,
    pub caps: Capabilities,
    pub grants: Grants,
    pub displays: Vec<DisplayInfo>,
    pub paused: bool,
    /// Secret bound to this session; required to attach bulk channels.
    pub bulk_token: [u8; 16],
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct FileMeta {
    pub name: String,
    pub size: u64,
}

/// Messages on the control channel (and, restricted, the pairing channel).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Msg {
    Hello(Hello),
    HelloAck(HelloAck),
    Reject(RejectReason),
    Ping(u64),
    Pong(u64),
    Bye(ReleaseReason),

    Displays(Vec<DisplayInfo>),
    Layout(LayoutDoc),
    Grants(Grants),
    State {
        paused: bool,
        locked: bool,
    },

    Enter {
        seq: u64,
        display: u32,
        x: f32,
        y: f32,
        held: Vec<u16>,
    },
    EnterAck {
        seq: u64,
        accepted: bool,
        reason: Option<RejectReason>,
    },
    Leave {
        reason: ReleaseReason,
    },
    Input(InputEvent),

    ClipOffer {
        id: u64,
        origin: Fingerprint,
        kind: ClipKind,
        size: u32,
    },
    ClipChunk {
        id: u64,
        data: Vec<u8>,
    },
    ClipAbort {
        id: u64,
    },

    FileOffer {
        transfer_id: u64,
        files: Vec<FileMeta>,
    },
    FileAnswer {
        transfer_id: u64,
        accepted: bool,
        reason: Option<RejectReason>,
    },
    FileCancel {
        transfer_id: u64,
    },

    /// First frame of a bulk connection: binds it to an existing control
    /// session and one accepted transfer.
    Attach {
        session_id: [u8; 16],
        token: [u8; 16],
        transfer_id: u64,
    },

    /// Pairing channel only.
    PairHello {
        name: String,
        platform: Platform,
        app_version: String,
        listen_port: u16,
    },
    /// Pairing channel only: the sender's user's decision after comparing
    /// fingerprints.
    PairDecision {
        approved: bool,
    },
}

/// Control frames on the pairing channel.
pub fn allowed_in_pairing(m: &Msg) -> bool {
    matches!(m, Msg::PairHello { .. } | Msg::PairDecision { .. } | Msg::Ping(_) | Msg::Pong(_) | Msg::Bye(_))
}

pub fn clean_text(s: &str, max_bytes: usize) -> String {
    let mut out = String::new();
    for c in s.chars().filter(|c| !c.is_control()) {
        if out.len() + c.len_utf8() > max_bytes {
            break;
        }
        out.push(c);
    }
    out.trim().to_string()
}

impl Msg {
    /// Structural limits that serde cannot express. Called on every decode.
    pub fn validate(&mut self) -> Result<(), ProtoError> {
        fn displays(d: &mut [DisplayInfo]) -> Result<(), ProtoError> {
            if d.len() > MAX_DISPLAYS_PER_DEVICE {
                return Err(ProtoError::Malformed);
            }
            for x in d.iter_mut() {
                if x.width == 0 || x.height == 0 || x.width > 65_536 || x.height > 65_536 || !x.scale.is_finite() || !(0.25..=16.0).contains(&x.scale) {
                    return Err(ProtoError::Malformed);
                }
                x.name = clean_text(&x.name, MAX_NAME_BYTES);
            }
            Ok(())
        }
        match self {
            Msg::Hello(h) => {
                h.name = clean_text(&h.name, MAX_NAME_BYTES);
                h.app_version = clean_text(&h.app_version, 32);
                displays(&mut h.displays)
            }
            Msg::HelloAck(h) => {
                h.name = clean_text(&h.name, MAX_NAME_BYTES);
                h.app_version = clean_text(&h.app_version, 32);
                displays(&mut h.displays)
            }
            Msg::Displays(d) => displays(d),
            Msg::PairHello { name, app_version, .. } => {
                *name = clean_text(name, MAX_NAME_BYTES);
                *app_version = clean_text(app_version, 32);
                Ok(())
            }
            Msg::Enter { x, y, held, .. } => {
                if !x.is_finite() || !y.is_finite() || held.len() > 16 {
                    return Err(ProtoError::Malformed);
                }
                Ok(())
            }
            Msg::Input(InputEvent::PointerAbs { x, y, .. }) if !x.is_finite() || !y.is_finite() => Err(ProtoError::Malformed),
            Msg::Input(InputEvent::Scroll { dx, dy, .. }) if !dx.is_finite() || !dy.is_finite() => Err(ProtoError::Malformed),
            Msg::ClipChunk { data, .. } if data.len() > CLIPBOARD_CHUNK => Err(ProtoError::TooLarge),
            Msg::ClipOffer { kind, size, .. } => {
                let max = match kind {
                    ClipKind::Text => MAX_CLIPBOARD_TEXT,
                    ClipKind::Image => MAX_CLIPBOARD_IMAGE,
                };
                if *size as usize > max { Err(ProtoError::TooLarge) } else { Ok(()) }
            }
            Msg::FileOffer { files, .. } => {
                if files.is_empty() || files.len() > MAX_FILES_PER_OFFER {
                    return Err(ProtoError::Malformed);
                }
                // Names are validated by the receiver's path-safety code; only
                // bound their size here.
                if files.iter().any(|f| f.name.len() > MAX_FILE_NAME_BYTES) {
                    return Err(ProtoError::Malformed);
                }
                Ok(())
            }
            Msg::Layout(l) => l.validate().map_err(|_| ProtoError::Malformed),
            _ => Ok(()),
        }
    }
}

// ───────────────────────────── bulk channel ─────────────────────────────

pub const BULK_DATA: u8 = 0;
pub const BULK_CTL: u8 = 1;

/// Control frames on a bulk channel. File bytes use `BULK_DATA` frames.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum BulkCtl {
    AttachOk,
    FileStart { index: u32, size: u64 },
    FileEnd { index: u32, sha256: [u8; 32] },
    FileResult { index: u32, ok: bool, reason: Option<RejectReason> },
    Cancel,
    Done,
}

// ───────────────────────────── framing ─────────────────────────────

pub type Wire<S> = Framed<S, LengthDelimitedCodec>;

pub fn control_wire<S: AsyncRead + AsyncWrite>(s: S) -> Wire<S> {
    Framed::new(s, LengthDelimitedCodec::builder().length_field_length(4).max_frame_length(MAX_CONTROL_FRAME).new_codec())
}

pub fn bulk_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder().length_field_length(4).max_frame_length(MAX_BULK_FRAME).new_codec()
}

pub fn bulk_wire<S: AsyncRead + AsyncWrite>(s: S) -> Wire<S> {
    Framed::new(s, bulk_codec())
}

const MAGIC: &[u8; 4] = b"SYNK";

/// Exchange `SYNK` + major + minor. Returns the peer's minor version.
pub async fn exchange_preamble<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S) -> Result<u16, ProtoError> {
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(MAGIC);
    out[4..6].copy_from_slice(&PROTOCOL_MAJOR.to_be_bytes());
    out[6..8].copy_from_slice(&PROTOCOL_MINOR.to_be_bytes());
    s.write_all(&out).await?;
    s.flush().await?;
    let mut inp = [0u8; 8];
    s.read_exact(&mut inp).await?;
    if &inp[..4] != MAGIC {
        return Err(ProtoError::BadMagic);
    }
    let major = u16::from_be_bytes([inp[4], inp[5]]);
    if major != PROTOCOL_MAJOR {
        return Err(ProtoError::VersionMismatch(major));
    }
    Ok(u16::from_be_bytes([inp[6], inp[7]]))
}

pub fn encode(msg: &Msg) -> Result<Bytes, ProtoError> {
    let v = postcard::to_stdvec(msg).map_err(|_| ProtoError::Malformed)?;
    if v.len() > MAX_CONTROL_FRAME {
        return Err(ProtoError::TooLarge);
    }
    Ok(Bytes::from(v))
}

pub fn decode(frame: &[u8]) -> Result<Msg, ProtoError> {
    let (mut msg, rest): (Msg, &[u8]) = postcard::take_from_bytes(frame).map_err(|_| ProtoError::Malformed)?;
    if !rest.is_empty() {
        return Err(ProtoError::Malformed);
    }
    msg.validate()?;
    Ok(msg)
}

pub fn encode_bulk_ctl(c: &BulkCtl) -> Result<Bytes, ProtoError> {
    let mut v = vec![BULK_CTL];
    v.extend(postcard::to_stdvec(c).map_err(|_| ProtoError::Malformed)?);
    if v.len() > MAX_BULK_FRAME {
        return Err(ProtoError::TooLarge);
    }
    Ok(Bytes::from(v))
}

pub fn decode_bulk_ctl(frame: &[u8]) -> Result<BulkCtl, ProtoError> {
    match frame.split_first() {
        Some((&BULK_CTL, rest)) => {
            let (c, tail): (BulkCtl, &[u8]) = postcard::take_from_bytes(rest).map_err(|_| ProtoError::Malformed)?;
            if tail.is_empty() { Ok(c) } else { Err(ProtoError::Malformed) }
        }
        _ => Err(ProtoError::Unexpected),
    }
}

/// `[BULK_DATA][payload]`; the payload is capped at one file chunk.
pub fn encode_bulk_data(payload: &[u8]) -> Result<Bytes, ProtoError> {
    if payload.len() > MAX_FILE_CHUNK {
        return Err(ProtoError::TooLarge);
    }
    let mut v = Vec::with_capacity(payload.len() + 1);
    v.push(BULK_DATA);
    v.extend_from_slice(payload);
    Ok(Bytes::from(v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};

    fn sample_display() -> DisplayInfo {
        DisplayInfo { id: 1, name: "Built-in".into(), x: 0, y: 0, width: 1440, height: 900, scale: 2.0, rotation: 0, primary: true }
    }

    #[test]
    fn messages_round_trip() {
        let msgs = vec![
            Msg::Ping(9),
            Msg::Input(InputEvent::Key { usage: 4, down: true, repeat: false }),
            Msg::Enter { seq: 1, display: 1, x: 0.0, y: 10.5, held: vec![0xE1] },
            Msg::Displays(vec![sample_display()]),
            Msg::FileOffer { transfer_id: 3, files: vec![FileMeta { name: "a.txt".into(), size: 5 }] },
            Msg::Layout(LayoutDoc::default()),
        ];
        for m in msgs {
            assert_eq!(decode(&encode(&m).unwrap()).unwrap(), m);
        }
    }

    #[test]
    fn garbage_and_trailing_bytes_are_rejected() {
        assert!(decode(&[]).is_err());
        assert!(decode(&[0xFF; 32]).is_err());
        let mut v = encode(&Msg::Ping(1)).unwrap().to_vec();
        v.push(0);
        assert!(matches!(decode(&v), Err(ProtoError::Malformed)));
    }

    #[test]
    fn structural_limits_are_enforced() {
        let many = vec![sample_display(); MAX_DISPLAYS_PER_DEVICE + 1];
        assert!(decode(&postcard::to_stdvec(&Msg::Displays(many)).unwrap()).is_err());
        let mut bad = sample_display();
        bad.scale = f32::NAN;
        assert!(decode(&postcard::to_stdvec(&Msg::Displays(vec![bad])).unwrap()).is_err());
        let inf = Msg::Input(InputEvent::PointerAbs { display: 1, x: f32::INFINITY, y: 0.0 });
        assert!(decode(&postcard::to_stdvec(&inf).unwrap()).is_err());
        let big_clip = Msg::ClipOffer { id: 1, origin: Fingerprint([0; 32]), kind: ClipKind::Text, size: (MAX_CLIPBOARD_TEXT + 1) as u32 };
        assert!(matches!(decode(&postcard::to_stdvec(&big_clip).unwrap()), Err(ProtoError::TooLarge)));
        let empty_offer = Msg::FileOffer { transfer_id: 1, files: vec![] };
        assert!(decode(&postcard::to_stdvec(&empty_offer).unwrap()).is_err());
    }

    #[test]
    fn names_are_sanitised() {
        let h = Hello {
            name: "Bad\u{202E}\u{0}\nName".into(),
            platform: Platform::Other,
            app_version: "1".into(),
            caps: Capabilities::default(),
            grants: Grants::default(),
            session_id: [0; 16],
            displays: vec![],
            paused: false,
        };
        let Msg::Hello(h) = decode(&postcard::to_stdvec(&Msg::Hello(h)).unwrap()).unwrap() else { panic!() };
        assert!(!h.name.contains('\n') && !h.name.contains('\0'));
        assert!(clean_text(&"é".repeat(100), 10).len() <= 10);
    }

    #[tokio::test]
    async fn oversized_frame_header_is_rejected_before_allocation() {
        let (mut a, b) = tokio::io::duplex(1024);
        let mut wire = control_wire(b);
        // Declare a ~4 GiB frame; the codec must refuse without allocating it.
        a.write_all(&0xFFFF_FFF0u32.to_be_bytes()).await.unwrap();
        let r = wire.next().await.unwrap();
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn preamble_detects_version_and_magic() {
        let (mut a, mut b) = tokio::io::duplex(64);
        let (ra, rb) = tokio::join!(exchange_preamble(&mut a), exchange_preamble(&mut b));
        assert!(ra.is_ok() && rb.is_ok());

        let (mut a, mut b) = tokio::io::duplex(64);
        let h = tokio::spawn(async move {
            let mut buf = [0u8; 8];
            b.read_exact(&mut buf).await.unwrap();
            b.write_all(b"SYNK\x00\x63\x00\x00").await.unwrap();
        });
        assert!(matches!(exchange_preamble(&mut a).await, Err(ProtoError::VersionMismatch(99))));
        h.await.unwrap();

        let (mut a, mut b) = tokio::io::duplex(64);
        let h = tokio::spawn(async move {
            let mut buf = [0u8; 8];
            b.read_exact(&mut buf).await.unwrap();
            b.write_all(b"HTTP/1.1").await.unwrap();
        });
        assert!(matches!(exchange_preamble(&mut a).await, Err(ProtoError::BadMagic)));
        h.await.unwrap();
    }

    #[tokio::test]
    async fn frames_travel_through_the_codec() {
        let (a, b) = tokio::io::duplex(4096);
        let (mut wa, mut wb) = (control_wire(a), control_wire(b));
        wa.send(encode(&Msg::Ping(42)).unwrap()).await.unwrap();
        let got = wb.next().await.unwrap().unwrap();
        assert_eq!(decode(&got).unwrap(), Msg::Ping(42));
    }

    #[test]
    fn bulk_frames() {
        let c = BulkCtl::FileEnd { index: 0, sha256: [7; 32] };
        assert_eq!(decode_bulk_ctl(&encode_bulk_ctl(&c).unwrap()).unwrap(), c);
        assert!(decode_bulk_ctl(&encode_bulk_data(b"abc").unwrap()).is_err());
        assert!(encode_bulk_data(&vec![0u8; MAX_FILE_CHUNK + 1]).is_err());
    }

    #[test]
    fn capability_negotiation_is_symmetric_and_conservative() {
        let a = Capabilities { input_source: true, input_inject: false, clipboard_text: true, clipboard_image: false, files: true };
        let b = Capabilities { input_source: false, input_inject: true, clipboard_text: true, clipboard_image: true, files: false };
        let n = a.intersect(b);
        assert!(n.input_source && !n.input_inject && n.clipboard_text && !n.clipboard_image && !n.files);
    }
}
