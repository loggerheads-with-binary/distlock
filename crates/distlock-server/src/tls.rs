//! Optional TLS for the listener (rustls, `ring` provider).
use std::io;
use std::path::Path;
use std::sync::Arc;

use rustls::ServerConfig;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;

fn bad(what: &str, path: &Path, e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("TLS: {what} {}: {e}", path.display()))
}

/// Read the certificate chain and key. Fails (so the server does not start) on a missing or
/// unreadable file, an empty chain, or a key that does not match the certificate.
pub(crate) fn acceptor(cert: &Path, key: &Path) -> io::Result<TlsAcceptor> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .map_err(|e| bad("cannot read the certificate file", cert, e))?
        .collect::<Result<_, _>>()
        .map_err(|e| bad("invalid certificate in", cert, e))?;
    if chain.is_empty() {
        return Err(bad(
            "no certificate in",
            cert,
            "the file has no PEM CERTIFICATE block",
        ));
    }
    let key = PrivateKeyDer::from_pem_file(key)
        .map_err(|e| bad("cannot read the private key", key, e))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|e| bad("the certificate and key do not form a usable pair", cert, e))?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::acceptor;

    #[test]
    fn unusable_files_stop_start_up_with_a_clear_message() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = (dir.path().join("c.pem"), dir.path().join("k.pem"));
        let msg = |r: std::io::Result<_>| {
            r.map(|_: tokio_rustls::TlsAcceptor| ())
                .unwrap_err()
                .to_string()
        };
        assert!(msg(acceptor(&cert, &key)).contains("cannot read the certificate file"));
        std::fs::write(&cert, "not pem").unwrap();
        assert!(msg(acceptor(&cert, &key)).contains("no certificate"));
        let pair = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        std::fs::write(&cert, pair.cert.pem()).unwrap();
        assert!(msg(acceptor(&cert, &key)).contains("cannot read the private key"));
        std::fs::write(&key, pair.signing_key.serialize_pem()).unwrap();
        assert!(acceptor(&cert, &key).is_ok());
        // A key from another pair is refused at start-up, not at the first handshake.
        let other = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        std::fs::write(&key, other.signing_key.serialize_pem()).unwrap();
        assert!(msg(acceptor(&cert, &key)).contains("do not form a usable pair"));
    }
}
