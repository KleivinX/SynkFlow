//! Central protocol and resource limits.
//!
//! These are initial *safety* limits, not performance claims. Every length read
//! from the wire is checked against one of these before any allocation.

use std::time::Duration;

/// Wire protocol major version. A mismatch is fatal for the connection.
pub const PROTOCOL_MAJOR: u16 = 1;
/// Wire protocol minor version. Additive, negotiated through capabilities.
pub const PROTOCOL_MINOR: u16 = 0;

/// ALPN for authenticated sessions (strict pinned mutual TLS).
pub const ALPN_SESSION: &[u8] = b"synkflow/1";
/// ALPN for the pairing-only channel. Grants no capabilities whatsoever.
pub const ALPN_PAIR: &[u8] = b"synkflow-pair/1";

/// DNS-SD service type.
pub const SERVICE_TYPE: &str = "_synkflow._tcp.local.";
/// Default TCP listen port (0 in settings means "pick automatically").
pub const DEFAULT_PORT: u16 = 24847;

/// Input / control / pairing frame (64 KiB).
pub const MAX_CONTROL_FRAME: usize = 64 * 1024;
/// Plain / rich text clipboard payload (1 MiB).
pub const MAX_CLIPBOARD_TEXT: usize = 1024 * 1024;
/// Image clipboard payload on the wire (16 MiB).
pub const MAX_CLIPBOARD_IMAGE: usize = 16 * 1024 * 1024;
/// Clipboard payloads are split into chunks this large so input is never
/// stuck behind a big frame on the control channel.
pub const CLIPBOARD_CHUNK: usize = 32 * 1024;
/// Bulk file chunk (256 KiB).
pub const MAX_FILE_CHUNK: usize = 256 * 1024;
/// Largest bulk frame: one chunk plus a tag and a small header allowance.
pub const MAX_BULK_FRAME: usize = MAX_FILE_CHUNK + 4096;
/// Decoded clipboard images may not exceed this many pixels (defence against
/// small-but-huge PNGs). 64 Mpx ≈ 256 MiB of RGBA.
pub const MAX_IMAGE_PIXELS: u64 = 64 * 1024 * 1024;

/// Display names / labels are cut to this many bytes before storing or showing.
pub const MAX_NAME_BYTES: usize = 64;
/// Displays a single peer may announce.
pub const MAX_DISPLAYS_PER_DEVICE: usize = 16;
/// Files in one transfer offer.
pub const MAX_FILES_PER_OFFER: usize = 256;
/// Longest accepted file name (bytes, UTF-8).
pub const MAX_FILE_NAME_BYTES: usize = 200;

/// Pending outgoing messages per session (input/control priority queue).
pub const SESSION_QUEUE_DEPTH: usize = 1024;
/// Pending low-priority (clipboard chunk) messages per session.
pub const SESSION_BULKISH_QUEUE_DEPTH: usize = 8;
/// Captured raw input events waiting for the engine.
pub const CAPTURE_QUEUE_DEPTH: usize = 4096;
/// Injected events waiting for the injector thread.
pub const INJECT_QUEUE_DEPTH: usize = 1024;

/// TLS + preamble + hello must finish within this long.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// When a peer announces several addresses they are tried together, the best one first and each next one this much later.
pub const DIAL_STAGGER: Duration = Duration::from_millis(250);
/// Heartbeat period on an idle control channel.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
/// No frame received for this long ⇒ session lost.
pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(8);
/// Pairing invitations / windows expire after this long.
pub const PAIRING_TTL: Duration = Duration::from_secs(120);
/// At most this many pairing attempts per source address per window.
pub const PAIRING_RATE_MAX: usize = 5;
pub const PAIRING_RATE_WINDOW: Duration = Duration::from_secs(60);
/// A control-session offer for an unknown or revoked identity is refused at
/// TLS level; this bounds repeated *trusted* reconnect storms.
pub const ACCEPT_BACKLOG: usize = 64;

/// Reconnect backoff bounds.
pub const BACKOFF_MIN: Duration = Duration::from_millis(500);
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Receiver prompts for a file offer expire after this long.
pub const OFFER_TTL: Duration = Duration::from_secs(120);
