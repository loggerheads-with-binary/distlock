//! Client side of TLS: build a connector from the settings.
use crate::error::{Error, Result};
use std::path::Path;
use std::sync::Arc;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::{WebPkiSupportedAlgorithms, ring};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{self, DigitallySignedStruct, RootCertStore, SignatureScheme};

/// A ready connector and the name the server's certificate must carry (a DNS name or an IP).
#[derive(Clone)]
pub(crate) struct Tls {
    pub connector: TlsConnector,
    pub server_name: ServerName<'static>,
}

pub(crate) fn build(host: &str, verify: bool, ca_file: Option<&Path>) -> Result<Tls> {
    let bad = |m: String| Error::Config(m);
    let server_name = ServerName::try_from(host.to_owned()).map_err(|e| {
        bad(format!(
            "host `{host}` is not usable as a TLS server name: {e}"
        ))
    })?;
    let provider = Arc::new(ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| bad(format!("TLS setup: {e}")))?;
    let config = if verify {
        let mut roots = RootCertStore::empty();
        match ca_file {
            // A private CA replaces the public roots: trust exactly what the operator named.
            Some(path) => {
                let certs = CertificateDer::pem_file_iter(path)
                    .and_then(Iterator::collect::<std::result::Result<Vec<_>, _>>)
                    .map_err(|e| bad(format!("tls_ca_file {}: {e}", path.display())))?;
                let (added, _) = roots.add_parsable_certificates(certs);
                if added == 0 {
                    return Err(bad(format!(
                        "tls_ca_file {}: no usable certificates",
                        path.display()
                    )));
                }
            }
            None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    } else {
        tracing::warn!(
            "TLS certificate verification is OFF: traffic is encrypted but the server is not authenticated"
        );
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify(
                provider.signature_verification_algorithms,
            )))
            .with_no_client_auth()
    };
    Ok(Tls {
        connector: TlsConnector::from(Arc::new(config)),
        server_name,
    })
}

/// Accepts any certificate. Handshake signatures are still checked, so the peer must hold the key
/// of whatever certificate it presents, but nothing says that certificate is the right server's.
#[derive(Debug)]
struct NoVerify(WebPkiSupportedAlgorithms);

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}
