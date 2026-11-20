//! rustls client setup, as user/tools/fetch does it: the ring provider and the roots from the CA bundle of the
//! image (`SSL_CERT_FILE` first, the OpenSSL convention). There is no way to switch verification off.

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use std::sync::Arc;

pub const BUNDLES: &[&str] = &["/etc/ssl/certs/ca-certificates.crt", "/etc/ssl/cert.pem"];

pub fn bundle_path() -> Result<String, String> {
    if let Ok(f) = std::env::var("SSL_CERT_FILE")
        && !f.is_empty()
    {
        return Ok(f);
    }
    BUNDLES
        .iter()
        .find(|p| std::path::Path::new(p).is_file())
        .map(|p| p.to_string())
        .ok_or_else(|| format!("no CA bundle: none of {} exists", BUNDLES.join(", ")))
}

pub fn config(bundle: &str) -> Result<Arc<ClientConfig>, String> {
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
    let mut cfg = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS setup: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

pub fn server_name(host: &str) -> Result<ServerName<'static>, String> {
    ServerName::try_from(host.to_string()).map_err(|e| format!("{host}: not a valid TLS server name ({e})"))
}

/// True for certificate errors that come from the clock (or a really expired certificate).
pub fn is_time_error(e: &rustls::Error) -> bool {
    use rustls::CertificateError as C;
    matches!(e, rustls::Error::InvalidCertificate(C::Expired | C::NotValidYet | C::ExpiredContext { .. } | C::NotValidYetContext { .. }))
}
