//! Mutual TLS 1.3 with pinned keys (MC-8b). The service shows its self-signed certificate (`keys`) and asks every
//! client for one; only a certificate that is byte for byte one of the paired ones gets past the handshake, the
//! rest end with a TLS alert and nothing else (no HTTP status, no token prompt). No CA, no names and no dates are
//! judged: a pinned key is trusted because it was paired, the Mini-PC's clock is not. No session resumption, so
//! every connection shows its key again.
//!
//! While no client is paired the service serves plain HTTP with the token (MC-8a) and takes one `bitsos/pair`;
//! after it, it serves TLS only. MC-10: the proof of both keys is the TLS 1.3 handshake itself (the server's
//! and the client's CertificateVerify sign this connection's transcript, so nothing of an earlier connection
//! can be replayed); `serve` takes the client's key from the verified certificate for every request.

use crate::cert;
use crate::keys::Keys;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::NoServerSessionStorage;
use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, Error, ServerConfig, ServerConnection, SignatureScheme};
use std::fs::File;
use std::io::Write;
use std::sync::{Arc, Mutex, RwLock};

/// The ring provider (rustls' default aws-lc-rs is not built here).
pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The paired clients' certificates; the verifier rustls asks.
#[derive(Debug)]
pub struct Paired {
    certs: RwLock<Vec<Vec<u8>>>,
    algs: WebPkiSupportedAlgorithms,
}

impl Paired {
    fn knows(&self, der: &[u8]) -> bool {
        self.certs.read().unwrap_or_else(|e| e.into_inner()).iter().any(|c| c == der)
    }
    fn count(&self) -> usize {
        self.certs.read().unwrap_or_else(|e| e.into_inner()).len()
    }
}

impl ClientCertVerifier for Paired {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(&self, end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>], _now: UnixTime) -> Result<ClientCertVerified, Error> {
        if self.knows(end_entity.as_ref()) {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure))
        }
    }
    fn verify_tls12_signature(&self, _message: &[u8], _cert: &CertificateDer<'_>, _dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, Error> {
        // TLS 1.3 only (the config offers nothing else).
        Err(Error::General(String::from("TLS 1.2 is not offered")))
    }
    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Why a pairing was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// A client is paired already (or the slot is used): further pairings go through MC-9's confirmation.
    Closed,
    /// `paired\first.der` could not be opened at the start.
    NoSlot,
    /// Another pairing is being written right now (it is not waited for: B4).
    Busy,
    /// This start's one-time pairing failed to write; the next start opens it again.
    Spent,
    /// The certificate is no certificate, or the server's own.
    Bad(String),
    /// Writing it to the stick failed.
    Write(String),
}

impl Refused {
    pub fn text(&self) -> String {
        match self {
            Refused::Closed => String::from("pairing is closed: a client is paired already (more pairings need MC-9's confirmation at the machine)"),
            Refused::NoSlot => String::from("pairing is not possible this start: the key directory on the stick could not be written (see the service's start line)"),
            Refused::Busy => String::from("not paired: another pairing is being written right now"),
            Refused::Spent => String::from("pairing is not possible this start: writing an earlier pairing to the stick failed; restart the service (or BitsOS) to pair again"),
            Refused::Bad(w) => format!("not paired: {w}"),
            Refused::Write(w) => format!("not paired: writing the key to the stick failed: {w}"),
        }
    }
}

/// The TLS side of the running service.
pub struct Tls {
    config: Arc<ServerConfig>,
    paired: Arc<Paired>,
    slot: Mutex<Slot>,
    cert: Vec<u8>,
}

impl Tls {
    pub fn new(keys: Keys) -> Result<Tls, String> {
        let p = provider();
        let paired = Arc::new(Paired { certs: RwLock::new(keys.paired), algs: p.signature_verification_algorithms });
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(keys.key));
        let mut config = ServerConfig::builder_with_provider(p)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| format!("TLS 1.3: {e}"))?
            .with_client_cert_verifier(paired.clone())
            .with_single_cert(vec![CertificateDer::from(keys.cert.clone())], key)
            .map_err(|e| format!("the server key: {e}"))?;
        config.session_storage = Arc::new(NoServerSessionStorage {});
        config.send_tls13_tickets = 0;
        let slot = keys.slot.map_or(Slot::Absent, Slot::Open);
        Ok(Tls { config: Arc::new(config), paired, slot: Mutex::new(slot), cert: keys.cert })
    }
    /// True once a client is paired: from then on every connection is TLS.
    pub fn on(&self) -> bool {
        self.paired.count() > 0
    }
    pub fn paired(&self) -> usize {
        self.paired.count()
    }
    /// The server's certificate (DER) and its fingerprint.
    pub fn cert(&self) -> &[u8] {
        &self.cert
    }
    pub fn fingerprint(&self) -> String {
        cert::fingerprint(&self.cert)
    }
    pub fn connection(&self) -> Result<ServerConnection, Error> {
        ServerConnection::new(self.config.clone())
    }
    /// The checks before the pairing's slot (and, MC-10, before Georg is asked): not the server's own
    /// certificate, a usable client certificate, the pairing still open.
    pub fn usable(&self, der: &[u8]) -> Result<(), Refused> {
        if der == self.cert.as_slice() {
            return Err(Refused::Bad(String::from("that is the service's own certificate")));
        }
        cert::client_usable(der).map_err(Refused::Bad)?;
        if self.on() {
            return Err(Refused::Closed);
        }
        Ok(())
    }

    /// The one-time pairing: `der` becomes the paired client (written to `paired\first.der` and synced first).
    /// A certificate that cannot be a client's is refused before the slot is touched, so it does not spend the
    /// one pairing. Ok: the client's fingerprint.
    pub fn pair(&self, der: &[u8]) -> Result<String, Refused> {
        if der == self.cert.as_slice() {
            return Err(Refused::Bad(String::from("that is the service's own certificate")));
        }
        cert::client_usable(der).map_err(Refused::Bad)?;
        // try_lock: a second pairing does not wait behind a slow stick (B4); it is told and may try again.
        let mut slot = match self.slot.try_lock() {
            Ok(g) => g,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return Err(Refused::Busy),
        };
        if self.on() {
            return Err(Refused::Closed);
        }
        let mut f = match std::mem::replace(&mut *slot, Slot::Spent) {
            Slot::Open(f) => f,
            Slot::Absent => {
                *slot = Slot::Absent;
                return Err(Refused::NoSlot);
            }
            Slot::Spent => return Err(Refused::Spent),
        };
        if let Err(e) = f.write_all(der).and_then(|()| f.sync_all()) {
            // B6: a state change. The slot stays spent (a part may be on the stick: the next start keeps it as
            // first.bad and opens the pairing again).
            crate::log::warn(&format!("the one-time pairing failed to write paired/first.der ({e}); no pairing until the service starts again"));
            return Err(Refused::Write(e.to_string()));
        }
        self.paired.certs.write().unwrap_or_else(|e| e.into_inner()).push(der.to_vec());
        Ok(cert::fingerprint(der))
    }
}

/// The one-time pairing's file.
enum Slot {
    /// `paired\first.der`, open and empty: the pairing is to give.
    Open(File),
    /// There was none this start (a client is paired, or the file could not be opened).
    Absent,
    /// Used: written (a client is paired) or failed to write.
    Spent,
}
