//! TLS 1.3 with mutual authentication and pinned peer identities.
//!
//! Two trust modes, selected by ALPN before any application data flows:
//!
//! * `synkflow/1` – **strict**. The peer certificate's SHA-256 must be in the
//!   locally approved [`TrustSet`] (server side) or equal the fingerprint we
//!   intended to reach (client side). Everything else is refused in the
//!   handshake. All input, clipboard, control and file traffic uses this.
//! * `synkflow-pair/1` – **pairing capture**. The handshake still verifies that
//!   the peer *possesses* the private key for the certificate it presents, but
//!   grants no trust. The connection can exchange pairing messages and nothing
//!   else; a person then compares fingerprints out of band before either side
//!   pins the identity. This is deliberately not a "trust every certificate"
//!   verifier: it never yields an authenticated session.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock, RwLock};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, DistinguishedName, Error as TlsErr, ServerConfig, SignatureScheme};
use tokio::net::TcpStream;
use tokio_rustls::{LazyConfigAcceptor, TlsConnector};

use crate::identity::{Fingerprint, Identity};
use crate::limits::{ALPN_PAIR, ALPN_SESSION};

pub type ServerStream = tokio_rustls::server::TlsStream<TcpStream>;
pub type ClientStream = tokio_rustls::client::TlsStream<TcpStream>;

pub fn provider() -> Arc<CryptoProvider> {
    static P: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    P.get_or_init(|| Arc::new(rustls::crypto::ring::default_provider())).clone()
}

/// Live set of approved peer fingerprints. Cloning shares the same set, so a
/// revocation takes effect for the very next handshake.
#[derive(Clone, Default, Debug)]
pub struct TrustSet(Arc<RwLock<HashSet<Fingerprint>>>);

impl TrustSet {
    pub fn contains(&self, fp: &Fingerprint) -> bool {
        self.0.read().map(|s| s.contains(fp)).unwrap_or(false)
    }
    pub fn insert(&self, fp: Fingerprint) {
        if let Ok(mut s) = self.0.write() {
            s.insert(fp);
        }
    }
    pub fn remove(&self, fp: &Fingerprint) {
        if let Ok(mut s) = self.0.write() {
            s.remove(fp);
        }
    }
    pub fn replace(&self, all: impl IntoIterator<Item = Fingerprint>) {
        if let Ok(mut s) = self.0.write() {
            *s = all.into_iter().collect();
        }
    }
    pub fn len(&self) -> usize {
        self.0.read().map(|s| s.len()).unwrap_or(0)
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("network error: {0}")]
    Io(#[from] std::io::Error),
    #[error("TLS handshake failed: {0}")]
    Handshake(String),
    #[error("the other device does not trust this one")]
    Unauthorized,
    #[error("the device that answered is not the one that was approved")]
    WrongPeer,
    #[error("peer did not negotiate the expected protocol")]
    Alpn,
    #[error("handshake timed out")]
    Timeout,
}

fn map_rustls(e: std::io::Error) -> TlsError {
    let text = e.to_string();
    if text.contains("received fatal alert") {
        // TLS 1.3: a server's rejection of *our* certificate arrives as an alert
        // after our side already finished, so it often surfaces on first read.
        TlsError::Unauthorized
    } else if text.contains("invalid peer certificate") {
        // Our own pin check refused what the other side presented.
        TlsError::WrongPeer
    } else if e.get_ref().is_some_and(|i| i.is::<rustls::Error>()) {
        TlsError::Handshake(text)
    } else {
        TlsError::Io(e)
    }
}

/// Pin check shared by both strict verifiers.
fn pin_error() -> TlsErr {
    TlsErr::InvalidCertificate(CertificateError::ApplicationVerificationFailure)
}

fn tls12_unsupported() -> TlsErr {
    TlsErr::PeerIncompatible(rustls::PeerIncompatible::Tls12NotOffered)
}

#[derive(Debug)]
struct PinnedClientVerifier {
    trust: TrustSet,
    algs: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for PinnedClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(&self, end_entity: &CertificateDer<'_>, intermediates: &[CertificateDer<'_>], _now: UnixTime) -> Result<ClientCertVerified, TlsErr> {
        if !intermediates.is_empty() || !self.trust.contains(&Fingerprint::of_cert(end_entity)) {
            return Err(pin_error());
        }
        Ok(ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsErr> {
        Err(tls12_unsupported())
    }
    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsErr> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

#[derive(Debug)]
struct PinnedServerVerifier {
    expected: Fingerprint,
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsErr> {
        if !intermediates.is_empty() || Fingerprint::of_cert(end_entity) != self.expected {
            return Err(pin_error());
        }
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsErr> {
        Err(tls12_unsupported())
    }
    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsErr> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Pairing-only: proves key possession, grants nothing. See module docs.
#[derive(Debug)]
struct PairingClientVerifier {
    algs: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for PairingClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(&self, _: &CertificateDer<'_>, intermediates: &[CertificateDer<'_>], _: UnixTime) -> Result<ClientCertVerified, TlsErr> {
        if !intermediates.is_empty() {
            return Err(pin_error());
        }
        Ok(ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsErr> {
        Err(tls12_unsupported())
    }
    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsErr> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

#[derive(Debug)]
struct PairingServerVerifier {
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PairingServerVerifier {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, TlsErr> {
        if !intermediates.is_empty() {
            return Err(pin_error());
        }
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsErr> {
        Err(tls12_unsupported())
    }
    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsErr> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

fn server_config(id: &Identity, verifier: Arc<dyn ClientCertVerifier>, alpn: &[u8]) -> Result<Arc<ServerConfig>, TlsError> {
    let mut cfg = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| TlsError::Handshake(e.to_string()))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![id.cert_der()], id.private_key())
        .map_err(|e| TlsError::Handshake(e.to_string()))?;
    cfg.alpn_protocols = vec![alpn.to_vec()];
    // No resumption: a resumed session would skip the client-certificate check
    // and let a revoked device back in.
    cfg.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    cfg.send_tls13_tickets = 0;
    Ok(Arc::new(cfg))
}

fn client_config(id: &Identity, verifier: Arc<dyn ServerCertVerifier>, alpn: &[u8]) -> Result<Arc<ClientConfig>, TlsError> {
    let mut cfg = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| TlsError::Handshake(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![id.cert_der()], id.private_key())
        .map_err(|e| TlsError::Handshake(e.to_string()))?;
    cfg.alpn_protocols = vec![alpn.to_vec()];
    cfg.resumption = rustls::client::Resumption::disabled();
    Ok(Arc::new(cfg))
}

fn server_name() -> ServerName<'static> {
    // Sent as SNI only; never used for trust.
    ServerName::try_from("synkflow.invalid").expect("static name is valid")
}

/// Listener-side TLS: picks strict or pairing mode from the client's ALPN.
pub struct TlsEndpoint {
    session: Arc<ServerConfig>,
    pairing: Arc<ServerConfig>,
}

pub enum Accepted {
    Session { stream: ServerStream, peer: Fingerprint },
    Pairing { stream: ServerStream, peer: Fingerprint },
}

impl TlsEndpoint {
    pub fn new(id: &Identity, trust: TrustSet) -> Result<Self, TlsError> {
        let algs = provider().signature_verification_algorithms;
        Ok(Self {
            session: server_config(id, Arc::new(PinnedClientVerifier { trust, algs }), ALPN_SESSION)?,
            pairing: server_config(id, Arc::new(PairingClientVerifier { algs }), ALPN_PAIR)?,
        })
    }

    pub async fn accept(&self, tcp: TcpStream) -> Result<Accepted, TlsError> {
        let _ = tcp.set_nodelay(true);
        let start = LazyConfigAcceptor::new(rustls::server::Acceptor::default(), tcp).await.map_err(map_rustls)?;
        let offers = |p: &[u8]| start.client_hello().alpn().is_some_and(|mut it| it.any(|a| a == p));
        // If a client offers both, the strict mode wins.
        let (cfg, pairing) = if offers(ALPN_SESSION) {
            (self.session.clone(), false)
        } else if offers(ALPN_PAIR) {
            (self.pairing.clone(), true)
        } else {
            return Err(TlsError::Alpn);
        };
        let stream = start.into_stream(cfg).await.map_err(map_rustls)?;
        let peer = peer_fingerprint(stream.get_ref().1.peer_certificates())?;
        let want = if pairing { ALPN_PAIR } else { ALPN_SESSION };
        if stream.get_ref().1.alpn_protocol() != Some(want) {
            return Err(TlsError::Alpn);
        }
        Ok(if pairing { Accepted::Pairing { stream, peer } } else { Accepted::Session { stream, peer } })
    }
}

fn peer_fingerprint(certs: Option<&[CertificateDer<'static>]>) -> Result<Fingerprint, TlsError> {
    match certs {
        Some([leaf]) => Ok(Fingerprint::of_cert(leaf)),
        _ => Err(TlsError::Unauthorized),
    }
}

/// Dial a *specific* peer. The handshake fails unless that exact identity answers.
pub async fn connect_session(addr: SocketAddr, id: &Identity, expected: Fingerprint) -> Result<ClientStream, TlsError> {
    let algs = provider().signature_verification_algorithms;
    let cfg = client_config(id, Arc::new(PinnedServerVerifier { expected, algs }), ALPN_SESSION)?;
    let stream = dial(addr, cfg, ALPN_SESSION).await?;
    Ok(stream)
}

/// Dial for pairing; returns the unauthenticated candidate's fingerprint.
pub async fn connect_pairing(addr: SocketAddr, id: &Identity) -> Result<(ClientStream, Fingerprint), TlsError> {
    let algs = provider().signature_verification_algorithms;
    let cfg = client_config(id, Arc::new(PairingServerVerifier { algs }), ALPN_PAIR)?;
    let stream = dial(addr, cfg, ALPN_PAIR).await?;
    let peer = peer_fingerprint(stream.get_ref().1.peer_certificates())?;
    Ok((stream, peer))
}

async fn dial(addr: SocketAddr, cfg: Arc<ClientConfig>, alpn: &[u8]) -> Result<ClientStream, TlsError> {
    let tcp = TcpStream::connect(addr).await?;
    tcp.set_nodelay(true)?;
    let stream = TlsConnector::from(cfg).connect(server_name(), tcp).await.map_err(map_rustls)?;
    if stream.get_ref().1.alpn_protocol() != Some(alpn) {
        return Err(TlsError::Alpn);
    }
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn listener(id: &Identity, trust: TrustSet) -> (SocketAddr, Arc<TlsEndpoint>, TcpListener) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        (l.local_addr().unwrap(), Arc::new(TlsEndpoint::new(id, trust).unwrap()), l)
    }

    /// Echo one byte over an accepted session so the client sees success *or*
    /// the TLS 1.3 post-handshake rejection.
    async fn serve_once(l: TcpListener, ep: Arc<TlsEndpoint>) -> Option<(bool, Fingerprint)> {
        let (tcp, _) = l.accept().await.ok()?;
        match ep.accept(tcp).await {
            Ok(Accepted::Session { mut stream, peer }) => {
                let mut b = [0u8; 1];
                stream.read_exact(&mut b).await.ok()?;
                stream.write_all(&b).await.ok()?;
                let _ = stream.flush().await;
                Some((false, peer))
            }
            Ok(Accepted::Pairing { mut stream, peer }) => {
                let mut b = [0u8; 1];
                stream.read_exact(&mut b).await.ok()?;
                stream.write_all(&b).await.ok()?;
                let _ = stream.flush().await;
                Some((true, peer))
            }
            Err(_) => None,
        }
    }

    async fn ping<S: AsyncReadExt + AsyncWriteExt + Unpin>(s: &mut S) -> std::io::Result<()> {
        s.write_all(&[7]).await?;
        s.flush().await?;
        let mut b = [0u8; 1];
        s.read_exact(&mut b).await?;
        assert_eq!(b[0], 7);
        Ok(())
    }

    #[tokio::test]
    async fn trusted_peer_connects_and_is_identified() {
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let trust = TrustSet::default();
        trust.insert(a.fingerprint());
        let (addr, ep, l) = listener(&b, trust).await;
        let srv = tokio::spawn(serve_once(l, ep));
        let mut c = connect_session(addr, &a, b.fingerprint()).await.unwrap();
        ping(&mut c).await.unwrap();
        assert_eq!(srv.await.unwrap(), Some((false, a.fingerprint())));
    }

    #[tokio::test]
    async fn untrusted_client_is_refused() {
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (addr, ep, l) = listener(&b, TrustSet::default()).await; // nobody trusted
        let srv = tokio::spawn(serve_once(l, ep));
        let res = match connect_session(addr, &a, b.fingerprint()).await {
            Ok(mut c) => ping(&mut c).await.is_ok(),
            Err(_) => false,
        };
        assert!(!res, "an unpaired client must never reach application data");
        assert_eq!(srv.await.unwrap(), None);
    }

    #[tokio::test]
    async fn wrong_server_identity_is_refused_by_client() {
        let (a, b, imposter) = (Identity::generate().unwrap(), Identity::generate().unwrap(), Identity::generate().unwrap());
        let trust = TrustSet::default();
        trust.insert(a.fingerprint());
        let (addr, ep, l) = listener(&imposter, trust).await;
        let _srv = tokio::spawn(serve_once(l, ep));
        // We expect `b` but `imposter` answers.
        assert!(matches!(connect_session(addr, &a, b.fingerprint()).await, Err(TlsError::WrongPeer)));
    }

    #[tokio::test]
    async fn revocation_applies_to_the_next_handshake() {
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let trust = TrustSet::default();
        trust.insert(a.fingerprint());
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let ep = Arc::new(TlsEndpoint::new(&b, trust.clone()).unwrap());
        let srv = tokio::spawn(async move {
            let mut ok = vec![];
            for _ in 0..2 {
                let (tcp, _) = l.accept().await.unwrap();
                ok.push(match ep.accept(tcp).await {
                    Ok(Accepted::Session { mut stream, .. }) => {
                        let mut b = [0u8; 1];
                        let _ = stream.read_exact(&mut b).await;
                        let _ = stream.write_all(&b).await;
                        let _ = stream.flush().await;
                        true
                    }
                    _ => false,
                });
            }
            ok
        });
        let mut c1 = connect_session(addr, &a, b.fingerprint()).await.unwrap();
        ping(&mut c1).await.unwrap(); // full round trip: server has verified us
        trust.remove(&a.fingerprint());
        let c2 = connect_session(addr, &a, b.fingerprint()).await;
        if let Ok(mut c) = c2 {
            assert!(ping(&mut c).await.is_err(), "revoked peer must not reach application data");
        }
        assert_eq!(srv.await.unwrap(), vec![true, false]);
    }

    #[tokio::test]
    async fn pairing_channel_reveals_identity_but_is_not_a_session() {
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (addr, ep, l) = listener(&b, TrustSet::default()).await;
        let srv = tokio::spawn(serve_once(l, ep));
        let (mut c, seen) = connect_pairing(addr, &a).await.unwrap();
        assert_eq!(seen, b.fingerprint());
        ping(&mut c).await.unwrap();
        assert_eq!(srv.await.unwrap(), Some((true, a.fingerprint())));
    }

    #[tokio::test]
    async fn session_alpn_cannot_be_downgraded_to_pairing_trust() {
        // A client that offers only the session ALPN but is unknown must not be
        // treated as a pairing client.
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (addr, ep, l) = listener(&b, TrustSet::default()).await;
        let srv = tokio::spawn(serve_once(l, ep));
        let _ = connect_session(addr, &a, b.fingerprint()).await;
        assert_eq!(srv.await.unwrap(), None);
    }
}
