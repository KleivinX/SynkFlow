//! Device identity: a self-signed Ed25519 certificate whose SHA-256 digest is
//! the *only* thing other devices trust. Names and addresses are never identity.

use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// Full SHA-256 of a certificate's DER encoding.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    pub fn of_cert(der: &[u8]) -> Self {
        Self(Sha256::digest(der).into())
    }

    /// 64 uppercase hex characters, no separators.
    pub fn hex(&self) -> String {
        use fmt::Write;
        self.0.iter().fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02X}");
            s
        })
    }

    /// Sixteen groups of four hex characters separated by spaces.
    pub fn grouped(&self) -> String {
        let h = self.hex();
        h.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap_or("")).collect::<Vec<_>>().join(" ")
    }

    /// Four lines of four groups – the layout shown on screen so two people can
    /// read it aloud or compare it line by line.
    pub fn grouped_lines(&self) -> String {
        let h = self.hex();
        h.as_bytes()
            .chunks(16)
            .map(|line| line.chunks(4).map(|c| std::str::from_utf8(c).unwrap_or("")).collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// First 8 hex characters. For logs and discovery hints only – never for
    /// verification.
    pub fn short(&self) -> String {
        self.hex()[..8].to_string()
    }

    /// Accepts upper/lower case with optional spaces, colons or dashes.
    pub fn parse(s: &str) -> Option<Self> {
        let clean: Vec<u8> = s.bytes().filter(|b| !matches!(b, b' ' | b':' | b'-' | b'\n' | b'\r' | b'\t')).collect();
        if clean.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, pair) in clean.chunks(2).enumerate() {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            out[i] = (hi * 16 + lo) as u8;
        }
        Some(Self(out))
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fp:{}…", self.short())
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.grouped())
    }
}

impl Serialize for Fingerprint {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() { s.serialize_str(&self.hex()) } else { self.0.serialize(s) }
    }
}

impl<'de> Deserialize<'de> for Fingerprint {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if d.is_human_readable() {
            let s = String::deserialize(d)?;
            Fingerprint::parse(&s).ok_or_else(|| serde::de::Error::custom("bad fingerprint"))
        } else {
            Ok(Fingerprint(<[u8; 32]>::deserialize(d)?))
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("could not generate identity: {0}")]
    Generate(String),
    #[error("stored identity is damaged")]
    Corrupt,
    #[error("secret storage failed: {0}")]
    Storage(String),
    /// The credential store exists but refused us (macOS Keychain prompt declined or store locked). Not a reason to
    /// make a new identity: other computers have the old one pinned.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    #[error(
        "the system refused access to this computer's identity key ({0}). Choose Always Allow when macOS asks, or allow Synkflow in Keychain Access, then start Synkflow again. Nothing was changed."
    )]
    Denied(String),
}

/// A device's certificate and private key.
pub struct Identity {
    cert: CertificateDer<'static>,
    key_pkcs8: Zeroizing<Vec<u8>>,
    fingerprint: Fingerprint,
}

#[derive(Serialize, Deserialize)]
struct StoredIdentity {
    version: u8,
    cert: Vec<u8>,
    key: Vec<u8>,
}

impl Identity {
    pub fn generate() -> Result<Self, IdentityError> {
        let gen_err = |e: rcgen::Error| IdentityError::Generate(e.to_string());
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).map_err(gen_err)?;
        let mut params = rcgen::CertificateParams::new(vec!["synkflow.invalid".to_string()]).map_err(gen_err)?;
        params.distinguished_name.push(rcgen::DnType::CommonName, "Synkflow device identity");
        let cert = params.self_signed(&key).map_err(gen_err)?;
        Ok(Self::from_parts(cert.der().clone(), Zeroizing::new(key.serialize_der())))
    }

    fn from_parts(cert: CertificateDer<'static>, key_pkcs8: Zeroizing<Vec<u8>>) -> Self {
        let fingerprint = Fingerprint::of_cert(cert.as_ref());
        Self { cert, key_pkcs8, fingerprint }
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    pub fn cert_der(&self) -> CertificateDer<'static> {
        self.cert.clone()
    }

    pub fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivatePkcs8KeyDer::from(self.key_pkcs8.to_vec()).into()
    }

    fn encode(&self) -> Result<Zeroizing<Vec<u8>>, IdentityError> {
        let stored = StoredIdentity { version: 1, cert: self.cert.to_vec(), key: self.key_pkcs8.to_vec() };
        postcard::to_stdvec(&stored).map(Zeroizing::new).map_err(|_| IdentityError::Corrupt)
    }

    fn decode(bytes: &[u8]) -> Result<Self, IdentityError> {
        let stored: StoredIdentity = postcard::from_bytes(bytes).map_err(|_| IdentityError::Corrupt)?;
        if stored.version != 1 {
            return Err(IdentityError::Corrupt);
        }
        let id = Self::from_parts(CertificateDer::from(stored.cert), Zeroizing::new(stored.key));
        // Cert and key must belong together and the key must parse.
        let provider = crate::tls::provider();
        let key = provider.key_provider.load_private_key(id.private_key()).map_err(|_| IdentityError::Corrupt)?;
        rustls::sign::CertifiedKey::new(vec![id.cert_der()], key).keys_match().map_err(|_| IdentityError::Corrupt)?;
        Ok(id)
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity").field("fingerprint", &self.fingerprint).finish_non_exhaustive()
    }
}

/// Where the identity secret lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreKind {
    /// macOS Keychain, Windows Credential Manager, or Secret Service.
    OsCredentialStore,
    /// A 0600 file in the config directory. Weaker: any process running as the
    /// same user can read it.
    File,
}

#[derive(Debug, Clone)]
pub enum SecretStore {
    Keyring { service: String, account: String },
    File { path: PathBuf },
}

impl SecretStore {
    pub fn kind(&self) -> StoreKind {
        match self {
            Self::Keyring { .. } => StoreKind::OsCredentialStore,
            Self::File { .. } => StoreKind::File,
        }
    }

    fn entry(service: &str, account: &str) -> Result<keyring::Entry, IdentityError> {
        keyring::Entry::new(service, account).map_err(|e| IdentityError::Storage(e.to_string()))
    }

    pub fn load(&self) -> Result<Option<Zeroizing<Vec<u8>>>, IdentityError> {
        match self {
            Self::Keyring { service, account } => match Self::entry(service, account)?.get_secret() {
                Ok(v) => Ok(Some(Zeroizing::new(v))),
                Err(keyring::Error::NoEntry) => Ok(None),
                #[cfg(target_os = "macos")]
                Err(keyring::Error::NoStorageAccess(e)) => Err(IdentityError::Denied(e.to_string())),
                Err(e) => Err(IdentityError::Storage(e.to_string())),
            },
            Self::File { path } => match std::fs::read(path) {
                Ok(v) => Ok(Some(Zeroizing::new(v))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(IdentityError::Storage(e.to_string())),
            },
        }
    }

    pub fn save(&self, secret: &[u8]) -> Result<(), IdentityError> {
        match self {
            Self::Keyring { service, account } => Self::entry(service, account)?.set_secret(secret).map_err(|e| IdentityError::Storage(e.to_string())),
            Self::File { path } => write_private_file(path, secret).map_err(|e| IdentityError::Storage(e.to_string())),
        }
    }

    pub fn delete(&self) -> Result<(), IdentityError> {
        match self {
            Self::Keyring { service, account } => match Self::entry(service, account)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(IdentityError::Storage(e.to_string())),
            },
            Self::File { path } => match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(IdentityError::Storage(e.to_string())),
            },
        }
    }
}

/// Create `path` (and parents) readable only by the current user, then rename
/// into place so a crash never leaves a half-written secret.
pub fn write_private_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

/// Load the device identity or create it. An identity that already exists in either store always wins: it is what
/// the other computers have pinned, so it must never change just because a store was briefly unavailable. Only when
/// there is none is a new one created, in `preferred` if it works, else in `fallback`. Reports which store ended up in
/// use so the UI can say so.
pub fn load_or_create(preferred: &SecretStore, fallback: &SecretStore) -> Result<(Identity, StoreKind), IdentityError> {
    match preferred.load() {
        Ok(Some(bytes)) => return Ok((Identity::decode(&bytes)?, preferred.kind())),
        Err(e @ IdentityError::Denied(_)) => return Err(e),
        Err(e) => tracing::warn!("identity store {:?} unavailable: {e}", preferred.kind()),
        Ok(None) => {}
    }
    if let Ok(Some(bytes)) = fallback.load() {
        return Ok((Identity::decode(&bytes)?, fallback.kind()));
    }
    for store in [preferred, fallback] {
        let id = Identity::generate()?;
        match store.save(&id.encode()?) {
            Ok(()) => return Ok((id, store.kind())),
            Err(e) => tracing::warn!("identity store {:?} unusable for saving: {e}", store.kind()),
        }
    }
    Err(IdentityError::Storage("no usable secret store".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_formats_and_parses() {
        let fp = Fingerprint([0xAB; 32]);
        assert_eq!(fp.hex().len(), 64);
        assert_eq!(fp.grouped().split(' ').count(), 16);
        assert_eq!(fp.grouped_lines().lines().count(), 4);
        assert_eq!(Fingerprint::parse(&fp.grouped()), Some(fp));
        assert_eq!(Fingerprint::parse(&fp.hex().to_lowercase()), Some(fp));
        assert_eq!(Fingerprint::parse("abc"), None);
        assert_eq!(Fingerprint::parse(&"zz".repeat(32)), None);
    }

    #[test]
    fn identity_round_trips_and_detects_tampering() {
        let id = Identity::generate().unwrap();
        let enc = id.encode().unwrap();
        let back = Identity::decode(&enc).unwrap();
        assert_eq!(id.fingerprint(), back.fingerprint());
        // A different key under the same certificate must be rejected.
        let other = Identity::generate().unwrap();
        let stored = StoredIdentity { version: 1, cert: id.cert.to_vec(), key: other.key_pkcs8.to_vec() };
        let bad = postcard::to_stdvec(&stored).unwrap();
        assert!(matches!(Identity::decode(&bad), Err(IdentityError::Corrupt)));
        assert!(matches!(Identity::decode(&enc[..enc.len() - 3]), Err(IdentityError::Corrupt)));
    }

    #[test]
    fn an_existing_identity_is_kept_when_the_preferred_store_has_none() {
        // First run happened while the credential store was unavailable (file fallback). A later run where the
        // preferred store works but is empty must not mint a new identity: the other computers pinned this one.
        let dir = tempfile::tempdir().unwrap();
        let fallback = SecretStore::File { path: dir.path().join("id.bin") };
        let preferred = SecretStore::File { path: dir.path().join("elsewhere").join("id.bin") };
        let (first, _) = load_or_create(&fallback, &fallback).unwrap();
        let (again, kind) = load_or_create(&preferred, &fallback).unwrap();
        assert_eq!(first.fingerprint(), again.fingerprint());
        assert_eq!(kind, StoreKind::File);
        assert!(!dir.path().join("elsewhere").join("id.bin").exists(), "nothing new was created");
    }

    #[test]
    fn file_store_creates_private_file_and_reuses_identity() {
        let dir = tempfile::tempdir().unwrap();
        let file = SecretStore::File { path: dir.path().join("id.bin") };
        let (a, kind) = load_or_create(&file, &file).unwrap();
        assert_eq!(kind, StoreKind::File);
        let (b, _) = load_or_create(&file, &file).unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("id.bin")).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "identity file must not be group/world accessible");
        }
    }
}
