//! Connection plumbing: dialing and accepting authenticated channels, the
//! control-channel task (two priority queues, heartbeat, bounded everything),
//! and the pairing-channel task.
//!
//! Nothing in this module decides policy. It moves frames and reports what
//! happened; the engine decides what a frame means.

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::identity::{Fingerprint, Identity};
use crate::limits::*;
use crate::proto::{self, BulkCtl, Hello, HelloAck, Msg, Platform, RejectReason, ReleaseReason, Wire, control_wire, exchange_preamble};
use crate::tls::{self, ClientStream, TlsError};

pub type SessionId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialError {
    /// Nothing answered (refused, timed out, unroutable) – typically a firewall
    /// or the other computer being off.
    Unreachable,
    /// Something answered, but it is not the device that was approved.
    WrongPeer,
    /// The other computer does not (or no longer) trust this one.
    Unauthorized,
    VersionMismatch(u16),
    Rejected(RejectReason),
    Protocol,
}

impl From<TlsError> for DialError {
    fn from(e: TlsError) -> Self {
        match e {
            TlsError::Unauthorized => DialError::Unauthorized,
            TlsError::WrongPeer => DialError::WrongPeer,
            TlsError::Alpn | TlsError::Handshake(_) => DialError::Protocol,
            TlsError::Io(_) | TlsError::Timeout => DialError::Unreachable,
        }
    }
}

impl From<proto::ProtoError> for DialError {
    fn from(e: proto::ProtoError) -> Self {
        match e {
            proto::ProtoError::VersionMismatch(v) => DialError::VersionMismatch(v),
            // A TLS 1.3 rejection of our certificate shows up on first read.
            proto::ProtoError::Io(io) if io.to_string().contains("received fatal alert") => DialError::Unauthorized,
            proto::ProtoError::Io(_) => DialError::Unreachable,
            _ => DialError::Protocol,
        }
    }
}

pub struct Dialed {
    pub wire: Wire<ClientStream>,
    pub ack: HelloAck,
    /// The id this side put in its `Hello`; bulk channels must present it.
    pub session_id: [u8; 16],
}

/// Connect to an approved peer and complete the control handshake.
pub async fn dial(addr: SocketAddr, id: &Identity, expected: Fingerprint, hello: Hello) -> Result<Dialed, DialError> {
    let session_id = hello.session_id;
    let work = async {
        let mut stream = tls::connect_session(addr, id, expected).await?;
        exchange_preamble(&mut stream).await?;
        let mut wire = control_wire(stream);
        wire.send(proto::encode(&Msg::Hello(hello))?).await.map_err(proto::ProtoError::Io)?;
        let frame = wire.next().await.ok_or(DialError::Unreachable)?.map_err(proto::ProtoError::Io)?;
        match proto::decode(&frame)? {
            Msg::HelloAck(ack) => Ok(Dialed { wire, ack, session_id }),
            Msg::Reject(r) => Err(DialError::Rejected(r)),
            _ => Err(DialError::Protocol),
        }
    };
    timeout(HANDSHAKE_TIMEOUT, work).await.unwrap_or(Err(DialError::Unreachable))
}

/// Try every address together, the best first and each next one `stagger` later; the first success wins and the rest
/// are dropped. A laptop announces several addresses and most are unreachable from here: one after another, each would
/// cost the full handshake timeout. If nothing connects, the most informative error wins over a plain "nothing answered".
pub async fn race<T, F, Fut>(addrs: &[SocketAddr], stagger: Duration, attempt: F) -> Result<(SocketAddr, T), DialError>
where
    F: Fn(SocketAddr) -> Fut,
    Fut: std::future::Future<Output = Result<T, DialError>> + Send + 'static,
    T: Send + 'static,
{
    let mut set = tokio::task::JoinSet::new();
    for (i, &addr) in addrs.iter().enumerate() {
        let (fut, delay) = (attempt(addr), stagger * i as u32);
        set.spawn(async move {
            tokio::time::sleep(delay).await;
            (addr, fut.await)
        });
    }
    let mut worst = DialError::Unreachable;
    while let Some(done) = set.join_next().await {
        match done {
            Ok((addr, Ok(v))) => return Ok((addr, v)),
            Ok((_, Err(DialError::WrongPeer | DialError::Unreachable))) | Err(_) => {}
            Ok((_, Err(e))) => worst = e,
        }
    }
    Err(worst)
}

/// Open a bulk channel for one accepted transfer, bound to a control session.
pub async fn dial_bulk(
    addr: SocketAddr,
    id: &Identity,
    expected: Fingerprint,
    session_id: [u8; 16],
    token: [u8; 16],
    transfer_id: u64,
) -> Result<Wire<ClientStream>, DialError> {
    let work = async {
        let mut stream = tls::connect_session(addr, id, expected).await?;
        exchange_preamble(&mut stream).await?;
        let mut wire = control_wire(stream);
        wire.send(proto::encode(&Msg::Attach { session_id, token, transfer_id })?).await.map_err(proto::ProtoError::Io)?;
        // From here on the connection speaks the larger bulk framing.
        let mut wire = wire.map_codec(|_| proto::bulk_codec());
        let frame = wire.next().await.ok_or(DialError::Unreachable)?.map_err(proto::ProtoError::Io)?;
        match proto::decode_bulk_ctl(&frame)? {
            BulkCtl::AttachOk => Ok(wire),
            _ => Err(DialError::Protocol),
        }
    };
    timeout(HANDSHAKE_TIMEOUT, work).await.unwrap_or(Err(DialError::Unreachable))
}

/// The first frame of an accepted session-ALPN connection.
pub enum First<S> {
    Control { wire: Wire<S>, hello: Hello },
    Bulk { wire: Wire<S>, session_id: [u8; 16], token: [u8; 16], transfer_id: u64 },
}

pub async fn read_first<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S) -> Result<First<S>, proto::ProtoError> {
    let work = async {
        exchange_preamble(&mut stream).await?;
        let mut wire = control_wire(stream);
        let frame = wire.next().await.ok_or(proto::ProtoError::Unexpected)??;
        match proto::decode(&frame)? {
            Msg::Hello(hello) => Ok(First::Control { wire, hello }),
            Msg::Attach { session_id, token, transfer_id } => Ok(First::Bulk { wire: wire.map_codec(|_| proto::bulk_codec()), session_id, token, transfer_id }),
            _ => Err(proto::ProtoError::Unexpected),
        }
    };
    timeout(HANDSHAKE_TIMEOUT, work).await.unwrap_or(Err(proto::ProtoError::Unexpected))
}

/// Answer a refused connection politely, then drop it.
pub async fn refuse<S: AsyncRead + AsyncWrite + Unpin>(mut wire: Wire<S>, why: RejectReason) {
    if let Ok(b) = proto::encode(&Msg::Reject(why)) {
        let _ = timeout(Duration::from_millis(500), wire.send(b)).await;
    }
}

pub async fn accept_bulk_ok<S: AsyncRead + AsyncWrite + Unpin>(wire: &mut Wire<S>) -> Result<(), proto::ProtoError> {
    wire.send(proto::encode_bulk_ctl(&BulkCtl::AttachOk)?).await.map_err(proto::ProtoError::Io)
}

// ───────────────────────────── control session task ─────────────────────────────

#[derive(Debug)]
pub enum SessionEvent {
    Msg { sid: SessionId, msg: Msg },
    Ended { sid: SessionId, reason: EndReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// The peer said goodbye.
    PeerClosed(Option<ReleaseReason>),
    /// Network failure.
    Lost,
    /// Nothing heard for [`HEARTBEAT_TIMEOUT`].
    Timeout,
    /// The peer sent something invalid.
    Protocol,
    /// Closed on our side.
    Cancelled,
}

#[derive(Clone)]
pub struct SessionChannels {
    /// Input, control and heartbeat replies. Sent with `try_send`.
    pub hi: mpsc::Sender<Msg>,
    /// Clipboard offers and chunks. May be awaited.
    pub lo: mpsc::Sender<Msg>,
    pub cancel: CancellationToken,
}

/// Run a control session. `first` (e.g. the `HelloAck`) is written before
/// anything else. The task ends the session on cancel, error, or timeout and
/// always reports exactly one `Ended`.
pub fn spawn_session<S>(sid: SessionId, wire: Wire<S>, first: Option<Msg>, events: mpsc::Sender<SessionEvent>) -> SessionChannels
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (hi_tx, mut hi_rx) = mpsc::channel::<Msg>(SESSION_QUEUE_DEPTH);
    let (lo_tx, mut lo_rx) = mpsc::channel::<Msg>(SESSION_BULKISH_QUEUE_DEPTH);
    let cancel = CancellationToken::new();
    let ch = SessionChannels { hi: hi_tx.clone(), lo: lo_tx, cancel: cancel.clone() };

    tokio::spawn(async move {
        let (mut sink, mut stream) = wire.split();
        let started = tokio::time::Instant::now();
        let last_rx = std::sync::atomic::AtomicU64::new(0);
        let touch = || last_rx.store(started.elapsed().as_millis() as u64, std::sync::atomic::Ordering::Relaxed);
        let since_rx = || Duration::from_millis((started.elapsed().as_millis() as u64).saturating_sub(last_rx.load(std::sync::atomic::Ordering::Relaxed)));

        let reader = async {
            while let Some(frame) = stream.next().await {
                let Ok(frame) = frame else { return EndReason::Lost };
                touch();
                match proto::decode(&frame) {
                    Ok(Msg::Ping(n)) => {
                        let _ = hi_tx.try_send(Msg::Pong(n));
                    }
                    Ok(Msg::Pong(_)) => {}
                    Ok(Msg::Bye(r)) => return EndReason::PeerClosed(Some(r)),
                    Ok(msg) => {
                        if events.send(SessionEvent::Msg { sid, msg }).await.is_err() {
                            return EndReason::Cancelled;
                        }
                    }
                    Err(_) => return EndReason::Protocol,
                }
            }
            EndReason::Lost
        };

        let writer = async {
            let write = |m: &Msg| -> Result<Bytes, EndReason> { proto::encode(m).map_err(|_| EndReason::Protocol) };
            if let Some(m) = first {
                let Ok(b) = write(&m) else { return EndReason::Protocol };
                if sink.send(b).await.is_err() {
                    return EndReason::Lost;
                }
            }
            let mut beat = tokio::time::interval(HEARTBEAT_INTERVAL);
            beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut ping = 0u64;
            loop {
                let msg = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        if let Ok(b) = write(&Msg::Bye(ReleaseReason::Shutdown)) {
                            let _ = timeout(Duration::from_millis(300), sink.send(b)).await;
                        }
                        return EndReason::Cancelled;
                    }
                    Some(m) = hi_rx.recv() => m,
                    Some(m) = lo_rx.recv() => m,
                    _ = beat.tick() => {
                        if since_rx() > HEARTBEAT_TIMEOUT {
                            return EndReason::Timeout;
                        }
                        ping += 1;
                        Msg::Ping(ping)
                    }
                };
                let Ok(b) = write(&msg) else { return EndReason::Protocol };
                if sink.send(b).await.is_err() {
                    return EndReason::Lost;
                }
            }
        };

        let reason = tokio::select! { r = reader => r, w = writer => w };
        let _ = sink.close().await;
        let _ = events.send(SessionEvent::Ended { sid, reason }).await;
    });
    ch
}

// ───────────────────────────── pairing channel task ─────────────────────────────

#[derive(Debug)]
pub enum PairEvent {
    Hello { name: String, platform: Platform, app_version: String, listen_port: u16 },
    Decision(bool),
    Ended(PairEnd),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairEnd {
    Closed,
    Expired,
    Protocol,
    Lost,
    Cancelled,
}

pub struct PairChannels {
    pub tx: mpsc::Sender<Msg>,
    pub cancel: CancellationToken,
}

/// Pairing is a short, strictly limited conversation: hello, decision, done.
/// Any other message type ends it.
pub fn spawn_pairing<S>(wire: Wire<S>, my_hello: Msg, events: mpsc::Sender<PairEvent>, ttl: Duration) -> PairChannels
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel::<Msg>(8);
    let cancel = CancellationToken::new();
    let ch = PairChannels { tx, cancel: cancel.clone() };
    tokio::spawn(async move {
        let (mut sink, mut stream) = wire.split();
        let reason = async {
            let Ok(b) = proto::encode(&my_hello) else { return PairEnd::Protocol };
            if sink.send(b).await.is_err() {
                return PairEnd::Lost;
            }
            let mut got_hello = false;
            let deadline = tokio::time::sleep(ttl);
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return PairEnd::Cancelled,
                    _ = &mut deadline => return PairEnd::Expired,
                    out = rx.recv() => {
                        let Some(m) = out else { return PairEnd::Cancelled };
                        let Ok(b) = proto::encode(&m) else { return PairEnd::Protocol };
                        if sink.send(b).await.is_err() { return PairEnd::Lost; }
                    }
                    frame = stream.next() => {
                        let Some(frame) = frame else { return PairEnd::Closed };
                        let Ok(frame) = frame else { return PairEnd::Lost };
                        let Ok(msg) = proto::decode(&frame) else { return PairEnd::Protocol };
                        if !proto::allowed_in_pairing(&msg) { return PairEnd::Protocol; }
                        match msg {
                            Msg::PairHello { name, platform, app_version, listen_port } if !got_hello => {
                                got_hello = true;
                                if events.send(PairEvent::Hello { name, platform, app_version, listen_port }).await.is_err() { return PairEnd::Cancelled; }
                            }
                            Msg::PairDecision { approved } if got_hello => {
                                if events.send(PairEvent::Decision(approved)).await.is_err() { return PairEnd::Cancelled; }
                            }
                            Msg::Ping(_) | Msg::Pong(_) => {}
                            Msg::Bye(_) => return PairEnd::Closed,
                            _ => return PairEnd::Protocol, // duplicate hello, decision before hello
                        }
                    }
                }
            }
        }
        .await;
        let _ = sink.close().await;
        let _ = events.send(PairEvent::Ended(reason)).await;
    });
    ch
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Capabilities, Grants};
    use crate::tls::{Accepted, TlsEndpoint, TrustSet};
    use std::sync::Arc;
    use tokio::net::TcpListener;

    fn ip(last: u8) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, last], 1))
    }

    #[tokio::test(start_paused = true)]
    async fn a_dead_first_address_costs_one_stagger_not_a_timeout() {
        let t0 = tokio::time::Instant::now();
        let r = race(&[ip(1), ip(2), ip(3)], Duration::from_millis(250), |a| async move {
            if a == ip(1) {
                tokio::time::sleep(Duration::from_secs(60)).await; // blackholed: never answers
                Err(DialError::Unreachable)
            } else {
                Ok(a)
            }
        })
        .await;
        assert_eq!(r.unwrap().0, ip(2));
        assert_eq!(t0.elapsed(), Duration::from_millis(250));
    }

    #[tokio::test(start_paused = true)]
    async fn the_best_address_wins_when_everything_answers() {
        let r = race(&[ip(1), ip(2)], Duration::from_millis(250), |a| async move { Ok(a) }).await;
        assert_eq!(r.unwrap().0, ip(1));
    }

    #[tokio::test(start_paused = true)]
    async fn when_nothing_connects_the_informative_error_is_reported() {
        let r: Result<(SocketAddr, ()), _> =
            race(
                &[ip(1), ip(2), ip(3)],
                Duration::from_millis(1),
                |a| async move { Err(if a == ip(2) { DialError::Unauthorized } else { DialError::WrongPeer }) },
            )
            .await;
        assert!(matches!(r, Err(DialError::Unauthorized)));
        let none: Result<(SocketAddr, ()), _> = race(&[], Duration::from_millis(1), |_| async { Ok(()) }).await;
        assert!(matches!(none, Err(DialError::Unreachable)));
    }

    fn hello(name: &str) -> Hello {
        Hello {
            name: name.into(),
            platform: Platform::Other,
            app_version: "t".into(),
            caps: Capabilities::default(),
            grants: Grants::default(),
            session_id: [7; 16],
            displays: vec![],
            paused: false,
        }
    }
    fn ack() -> HelloAck {
        HelloAck {
            name: "srv".into(),
            platform: Platform::Other,
            app_version: "t".into(),
            caps: Capabilities::default(),
            grants: Grants::default(),
            displays: vec![],
            paused: false,
            bulk_token: [9; 16],
        }
    }

    struct Pair {
        addr: SocketAddr,
        a: Arc<Identity>,
        b: Arc<Identity>,
        listener: TcpListener,
        ep: TlsEndpoint,
    }

    async fn pair() -> Pair {
        let (a, b) = (Arc::new(Identity::generate().unwrap()), Arc::new(Identity::generate().unwrap()));
        let trust = TrustSet::default();
        trust.insert(a.fingerprint());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let ep = TlsEndpoint::new(&b, trust).unwrap();
        Pair { addr, a, b, listener, ep }
    }

    #[tokio::test]
    async fn control_handshake_then_messages_flow_in_both_directions() {
        let p = pair().await;
        let (a, b_fp, addr) = (p.a.clone(), p.b.fingerprint(), p.addr);
        let client = tokio::spawn(async move { dial(addr, &a, b_fp, hello("alpha")).await });
        let (tcp, _) = p.listener.accept().await.unwrap();
        let Accepted::Session { stream, peer } = p.ep.accept(tcp).await.unwrap() else { panic!() };
        assert_eq!(peer, p.a.fingerprint());
        let First::Control { wire, hello: h } = read_first(stream).await.unwrap() else { panic!() };
        assert_eq!(h.name, "alpha");
        let (ev_tx, mut ev_rx) = mpsc::channel(16);
        let server = spawn_session(1, wire, Some(Msg::HelloAck(ack())), ev_tx);
        let dialed = client.await.unwrap().unwrap();
        assert_eq!(dialed.ack.bulk_token, [9; 16]);
        let (cev_tx, mut cev_rx) = mpsc::channel(16);
        let client_ch = spawn_session(2, dialed.wire, None, cev_tx);

        client_ch.hi.try_send(Msg::Layout(Default::default())).unwrap();
        let SessionEvent::Msg { sid, msg } = ev_rx.recv().await.unwrap() else { panic!() };
        assert_eq!((sid, matches!(msg, Msg::Layout(_))), (1, true));
        server.hi.try_send(Msg::State { paused: true, locked: false }).unwrap();
        let SessionEvent::Msg { msg, .. } = cev_rx.recv().await.unwrap() else { panic!() };
        assert_eq!(msg, Msg::State { paused: true, locked: false });

        // Closing one side is reported as a clean goodbye on the other.
        client_ch.cancel.cancel();
        let mut saw = None;
        while let Some(e) = ev_rx.recv().await {
            if let SessionEvent::Ended { reason, .. } = e {
                saw = Some(reason);
                break;
            }
        }
        assert_eq!(saw, Some(EndReason::PeerClosed(Some(ReleaseReason::Shutdown))));
    }

    #[tokio::test]
    async fn peer_that_never_answers_is_dropped_by_the_heartbeat() {
        tokio::time::pause();
        let (a, b) = tokio::io::duplex(4096);
        let (ev_tx, mut ev_rx) = mpsc::channel(4);
        let _ch = spawn_session(1, control_wire(a), None, ev_tx);
        let _silent = b; // never reads or writes
        tokio::time::sleep(HEARTBEAT_TIMEOUT + HEARTBEAT_INTERVAL * 2).await;
        let SessionEvent::Ended { reason, .. } = ev_rx.recv().await.unwrap() else { panic!() };
        assert_eq!(reason, EndReason::Timeout);
    }

    #[tokio::test]
    async fn garbage_from_the_peer_ends_the_session_as_a_protocol_error() {
        let (a, mut b) = tokio::io::duplex(4096);
        let (ev_tx, mut ev_rx) = mpsc::channel(4);
        let _ch = spawn_session(1, control_wire(a), None, ev_tx);
        use tokio::io::AsyncWriteExt;
        b.write_all(&5u32.to_be_bytes()).await.unwrap();
        b.write_all(&[0xFF; 5]).await.unwrap();
        let SessionEvent::Ended { reason, .. } = ev_rx.recv().await.unwrap() else { panic!() };
        assert_eq!(reason, EndReason::Protocol);
    }

    #[tokio::test]
    async fn dialing_the_wrong_device_or_an_unpaired_one_fails_with_distinct_errors() {
        let p = pair().await;
        let (addr, a) = (p.addr, p.a.clone());
        let stranger = Identity::generate().unwrap();
        // Wrong server identity.
        let wrong = tokio::spawn({
            let a = a.clone();
            async move { dial(addr, &a, stranger.fingerprint(), hello("x")).await.err() }
        });
        let (tcp, _) = p.listener.accept().await.unwrap();
        let _ = p.ep.accept(tcp).await;
        assert_eq!(wrong.await.unwrap(), Some(DialError::WrongPeer));
        // Nothing listening.
        let dead = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap()
        };
        assert_eq!(dial(dead, &p.a, p.b.fingerprint(), hello("x")).await.err(), Some(DialError::Unreachable));
    }

    #[tokio::test]
    async fn pairing_task_accepts_only_pairing_messages() {
        let (a, b) = tokio::io::duplex(4096);
        let (ev_tx, mut ev_rx) = mpsc::channel(8);
        let hello_msg = Msg::PairHello { name: "me".into(), platform: Platform::Other, app_version: "t".into(), listen_port: 1 };
        let _pc = spawn_pairing(control_wire(a), hello_msg.clone(), ev_tx, Duration::from_secs(30));
        let mut peer = control_wire(b);
        // The task greets first.
        let first = proto::decode(&peer.next().await.unwrap().unwrap()).unwrap();
        assert_eq!(first, hello_msg);
        // A decision before the peer's hello is a protocol error.
        peer.send(proto::encode(&Msg::PairDecision { approved: true }).unwrap()).await.unwrap();
        let PairEvent::Ended(end) = ev_rx.recv().await.unwrap() else { panic!() };
        assert_eq!(end, PairEnd::Protocol);

        // Control messages are not allowed on the pairing channel.
        let (a, b) = tokio::io::duplex(4096);
        let (ev_tx, mut ev_rx) = mpsc::channel(8);
        let _pc = spawn_pairing(control_wire(a), hello_msg.clone(), ev_tx, Duration::from_secs(30));
        let mut peer = control_wire(b);
        peer.next().await;
        peer.send(proto::encode(&Msg::Input(proto::InputEvent::Key { usage: 4, down: true, repeat: false })).unwrap()).await.unwrap();
        let PairEvent::Ended(end) = ev_rx.recv().await.unwrap() else { panic!() };
        assert_eq!(end, PairEnd::Protocol);
    }

    #[tokio::test]
    async fn pairing_task_expires() {
        tokio::time::pause();
        let (a, _b) = tokio::io::duplex(4096);
        let (ev_tx, mut ev_rx) = mpsc::channel(8);
        let _pc = spawn_pairing(control_wire(a), Msg::PairDecision { approved: false }, ev_tx, Duration::from_secs(120));
        tokio::time::sleep(Duration::from_secs(121)).await;
        let PairEvent::Ended(end) = ev_rx.recv().await.unwrap() else { panic!() };
        assert_eq!(end, PairEnd::Expired);
    }
}
