//! rustls client setup: the CA bundle (PEM), the ring provider, and an optional fixed verification time for
//! machines whose clock is wrong (V2 without a readable RTC counts from 1970, so every certificate looks
//! "not valid yet").

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use std::sync::Arc;

/// Where Linux distributions keep the Mozilla bundle (CachyOS/Arch and Debian; Fedora's cert.pem path is the
/// second), after `--ca` and `SSL_CERT_FILE` (the OpenSSL convention).
pub const BUNDLES: &[&str] = &["/etc/ssl/certs/ca-certificates.crt", "/etc/ssl/cert.pem"];

pub fn bundle_path(flag: Option<&str>) -> Result<String, String> {
    if let Some(f) = flag {
        return Ok(f.to_string());
    }
    if let Ok(f) = std::env::var("SSL_CERT_FILE")
        && !f.is_empty()
    {
        return Ok(f);
    }
    BUNDLES
        .iter()
        .find(|p| std::path::Path::new(p).is_file())
        .map(|p| p.to_string())
        .ok_or_else(|| format!("no CA bundle: none of {} exists (give one with --ca FILE or SSL_CERT_FILE)", BUNDLES.join(", ")))
}

pub fn config(bundle: &str, verify_time: Option<UnixTime>) -> Result<Arc<ClientConfig>, String> {
    let mut roots = RootCertStore::empty();
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(bundle)
        .map_err(|e| format!("CA bundle {bundle}: {e}"))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("CA bundle {bundle}: {e}"))?;
    let (added, _ignored) = roots.add_parsable_certificates(certs);
    if added == 0 {
        return Err(format!("CA bundle {bundle}: no usable certificate in it"));
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone()).with_safe_default_protocol_versions().map_err(|e| format!("TLS setup: {e}"))?;
    let roots = Arc::new(roots);
    let mut cfg = match verify_time {
        None => builder.with_root_certificates(roots).with_no_client_auth(),
        Some(now) => {
            let inner = WebPkiServerVerifier::builder_with_provider(roots, provider).build().map_err(|e| format!("TLS setup: {e}"))?;
            builder.dangerous().with_custom_certificate_verifier(Arc::new(FixedTime { inner, now })).with_no_client_auth()
        }
    };
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

pub fn server_name(host: &str) -> Result<ServerName<'static>, String> {
    ServerName::try_from(host.to_string()).map_err(|e| format!("{host}: not a valid TLS server name ({e})"))
}

/// Full WebPKI verification (chain, name, signatures), only the clock is replaced by `--verify-time`.
#[derive(Debug)]
struct FixedTime {
    inner: Arc<WebPkiServerVerifier>,
    now: UnixTime,
}

impl ServerCertVerifier for FixedTime {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.inner.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, self.now)
    }

    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(m, c, d)
    }

    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(m, c, d)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// True for certificate errors that come from the clock (or a really expired certificate).
pub fn is_time_error(e: &rustls::Error) -> bool {
    use rustls::CertificateError as C;
    matches!(e, rustls::Error::InvalidCertificate(C::Expired | C::NotValidYet | C::ExpiredContext { .. } | C::NotValidYetContext { .. }))
}
