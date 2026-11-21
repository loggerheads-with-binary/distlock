//! TLS for the client, with the two independent checks the caller can switch off.
//!
//! | `verify_ca` | `verify_host` | what is checked |
//! |---|---|---|
//! | yes | yes | the certificate chains to a trusted CA **and** names the host or IP (full) |
//! | yes | no  | chains to a trusted CA; any name |
//! | no  | yes | names the host or IP; any issuer, so a self-signed certificate passes |
//! | no  | no  | nothing: the connection is encrypted but the server is not authenticated |
//!
//! The TLS handshake signature is verified in every case, so the peer always holds the private key
//! of the certificate it presents. The last two rows give no protection against an active attacker
//! who can impersonate the server.
use std::path::Path;
use std::sync::Arc;

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore,
    SignatureScheme,
};
use tokio_rustls::TlsConnector;

use crate::error::{Error, Result};

#[derive(Clone)]
pub(crate) struct TlsSetup {
    pub(crate) connector: TlsConnector,
    pub(crate) name: ServerName<'static>,
}

#[derive(Debug)]
struct Verifier {
    /// `Some`: check the chain against these roots.
    chain: Option<Arc<WebPkiServerVerifier>>,
    check_host: bool,
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, TlsError> {
        match (&self.chain, self.check_host) {
            (Some(chain), true) => {
                chain.verify_server_cert(end_entity, intermediates, server_name, ocsp, now)
            }
            // The name is checked after the chain, so this error means the chain was fine.
            (Some(chain), false) => {
                match chain.verify_server_cert(end_entity, intermediates, server_name, ocsp, now) {
                    Err(TlsError::InvalidCertificate(
                        CertificateError::NotValidForName
                        | CertificateError::NotValidForNameContext { .. },
                    )) => Ok(ServerCertVerified::assertion()),
                    other => other,
                }
            }
            (None, true) => {
                let cert = webpki::EndEntityCert::try_from(end_entity)
                    .map_err(|_| TlsError::InvalidCertificate(CertificateError::BadEncoding))?;
                cert.verify_is_valid_for_subject_name(server_name)
                    .map_err(|_| TlsError::InvalidCertificate(CertificateError::NotValidForName))?;
                Ok(ServerCertVerified::assertion())
            }
            (None, false) => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

fn bad(m: impl std::fmt::Display) -> Error {
    Error::Tls(m.to_string())
}

/// A CA file or PEM text (one or more certificates) replaces the built-in public roots.
pub(crate) fn setup(
    host: &str,
    verify_ca: bool,
    verify_host: bool,
    ca_file: Option<&Path>,
    ca_pem: Option<&str>,
) -> Result<TlsSetup> {
    let name = ServerName::try_from(host.to_owned())
        .map_err(|_| bad(format!("{host:?} is not a valid host name or IP address")))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let chain = if verify_ca {
        let mut roots = RootCertStore::empty();
        let (pem, source) = match (ca_file, ca_pem) {
            (Some(path), _) => (
                std::fs::read(path)
                    .map_err(|e| bad(format!("cannot read the CA file {}: {e}", path.display())))?,
                format!("the CA file {}", path.display()),
            ),
            (None, Some(text)) => (text.as_bytes().to_vec(), "ca_file_content".to_owned()),
            (None, None) => (Vec::new(), String::new()),
        };
        if source.is_empty() {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        } else {
            for cert in CertificateDer::pem_slice_iter(&pem) {
                let cert = cert.map_err(|e| bad(format!("bad PEM in {source}: {e}")))?;
                roots
                    .add(cert)
                    .map_err(|e| bad(format!("bad CA certificate in {source}: {e}")))?;
            }
            if roots.is_empty() {
                return Err(bad(format!("no certificate in {source}")));
            }
        }
        Some(
            WebPkiServerVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
                .build()
                .map_err(bad)?,
        )
    } else {
        None
    };
    let verifier = Arc::new(Verifier {
        chain,
        check_host: verify_host,
        algs: provider.signature_verification_algorithms,
    });
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(bad)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(TlsSetup {
        connector: TlsConnector::from(Arc::new(config)),
        name,
    })
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;
    use rcgen::{CertificateParams, KeyPair, SanType};

    /// The name check ("name only" mode) against a certificate with several names, a wildcard and
    /// several addresses, as `scripts/tls/server-cert.sh` makes them.
    #[test]
    fn names_wildcards_and_addresses_in_one_certificate() {
        let mut params = CertificateParams::new(vec![
            "*.example.test".to_owned(),
            "exact.example.org".to_owned(),
        ])
        .unwrap();
        params.subject_alt_names.extend([
            SanType::IpAddress(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))),
            SanType::IpAddress(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 6))),
        ]);
        let cert = params.self_signed(&KeyPair::generate().unwrap()).unwrap();
        let verifier = Verifier {
            chain: None,
            check_host: true,
            algs: rustls::crypto::ring::default_provider().signature_verification_algorithms,
        };
        let accepts = |host: &str| {
            let name = ServerName::try_from(host.to_owned()).unwrap();
            verifier
                .verify_server_cert(cert.der(), &[], &name, &[], UnixTime::now())
                .is_ok()
        };
        for ok in [
            "api.example.test",
            "x.example.test",
            "exact.example.org",
            "10.0.0.5",
            "10.0.0.6",
        ] {
            assert!(accepts(ok), "{ok} should match");
        }
        for bad in [
            "example.test",      // a wildcard needs a label in front
            "a.b.example.test",  // and covers exactly one
            "other.example.org", // not the exact name
            "api.example.org",
            "10.0.0.7", // not listed
        ] {
            assert!(!accepts(bad), "{bad} should not match");
        }
    }
}
