//! Helpers for the `tls` subcommands: names and addresses, files, keys and certificates.
use std::net::IpAddr;
use std::path::Path;

use rcgen::{KeyPair, PublicKeyData, SerialNumber};
use time::{Duration, OffsetDateTime};

/// A host name for a certificate's subjectAltName, or a wildcard whose `*` is the whole left-most
/// label: `*.example.com` covers `api.example.com` but neither `example.com` nor `a.b.example.com`.
/// `*.com` would cover a whole TLD, so it is refused.
pub fn dns_name(v: &str) -> Result<String, String> {
    let v = v.trim();
    let bad = || {
        Err(
            "not a valid host name (letters, digits, '-' and '.'; a wildcard is '*.' plus a name \
             of at least two labels)"
                .to_owned(),
        )
    };
    if v.is_empty()
        || v.starts_with(['.', '-'])
        || v.ends_with('.')
        || v.contains("..")
        || v.contains(".-")
        || v.contains("-.")
        || v.chars()
            .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '*')))
    {
        return bad();
    }
    match v.strip_prefix("*.") {
        Some(rest) if rest.contains('*') || !rest.contains('.') => return bad(),
        None if v.contains('*') => return bad(),
        _ => {}
    }
    Ok(v.to_owned())
}

/// A single IPv4 or IPv6 address. A certificate cannot list a range.
pub fn ip_addr(v: &str) -> Result<IpAddr, String> {
    let v = v.trim();
    if v.contains('/') {
        return Err("IP ranges (CIDR blocks) are not supported: list single addresses".into());
    }
    v.parse().map_err(|_| "not a valid IP address".into())
}

/// Not valid before a few minutes ago, so a client whose clock runs slightly behind does not see a
/// brand-new certificate as "not yet valid".
pub fn validity(days: u32) -> (OffsetDateTime, OffsetDateTime) {
    let now = OffsetDateTime::now_utc();
    (
        now - Duration::minutes(5),
        now + Duration::days(i64::from(days)),
    )
}

/// A random positive serial number of 16 bytes.
pub fn random_serial() -> Result<SerialNumber, String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| format!("no random numbers from the system: {e}"))?;
    b[0] = (b[0] & 0x7f) | 0x40; // positive, and never a leading zero byte
    Ok(SerialNumber::from_slice(&b))
}

/// Fail early, before anything is written, if one of `paths` exists (a dangling symlink counts).
pub fn refuse_overwrite(paths: &[&Path], force: bool) -> Result<(), String> {
    if force {
        return Ok(());
    }
    for p in paths {
        if p.symlink_metadata().is_ok() {
            return Err(format!(
                "{} already exists (use --force to replace it)",
                p.display()
            ));
        }
    }
    Ok(())
}

/// Write a file. A secret one (a private key) is created readable by its owner only; without `force`
/// an existing file is never touched.
#[cfg_attr(not(unix), allow(unused_variables))]
pub fn write_file(path: &Path, contents: &str, secret: bool, force: bool) -> Result<(), String> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if force {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if secret { 0o600 } else { 0o644 });
    }
    let fail = |e: std::io::Error| format!("{}: {e}", path.display());
    let mut file = options.open(path).map_err(fail)?;
    #[cfg(unix)]
    {
        // The mode above only applies to a file that did not exist; --force may be replacing one.
        use std::os::unix::fs::PermissionsExt;
        let mode = if secret { 0o600 } else { 0o644 };
        file.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(fail)?;
    }
    file.write_all(contents.as_bytes()).map_err(fail)
}

/// A PKCS#8 PEM private key. An encrypted one cannot be read: nobody is here to type a passphrase.
pub fn read_key(path: &Path) -> Result<KeyPair, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if text.contains("ENCRYPTED") {
        return Err(format!(
            "{}: the key has a passphrase, which this tool cannot read. Remove it first: \
             openssl pkey -in {} -out plain.key",
            path.display(),
            path.display()
        ));
    }
    KeyPair::from_pem(&text).map_err(|e| {
        format!(
            "{}: not a usable PKCS#8 PEM private key ({e}). An RSA or EC key in the older format \
             can be converted: openssl pkcs8 -topk8 -nocrypt -in {} -out new.key",
            path.display(),
            path.display()
        )
    })
}

/// What the `tls` commands need to know about a CA certificate.
pub struct CaCert {
    /// The certificate as the file had it.
    pub pem: String,
    pub is_ca: bool,
    pub self_signed: bool,
    /// Unix seconds.
    pub not_after: i64,
    /// DER of the SubjectPublicKeyInfo, to compare with a private key.
    spki: Vec<u8>,
}

impl CaCert {
    pub fn read(path: &Path) -> Result<Self, String> {
        let pem = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let not_pem = || format!("{} is not a PEM certificate", path.display());
        let (_, block) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).map_err(|_| not_pem())?;
        let cert = block.parse_x509().map_err(|_| not_pem())?;
        let is_ca = matches!(cert.basic_constraints(), Ok(Some(b)) if b.value.ca);
        Ok(Self {
            is_ca,
            self_signed: cert.tbs_certificate.subject.as_raw()
                == cert.tbs_certificate.issuer.as_raw(),
            not_after: cert.validity().not_after.timestamp(),
            spki: cert.public_key().raw.to_vec(),
            pem,
        })
    }

    /// Fail unless this can be used as a CA today.
    pub fn check_usable(&self, path: &Path) -> Result<(), String> {
        if !self.is_ca {
            return Err(format!(
                "{} is not a CA certificate (it lacks basicConstraints CA:TRUE)",
                path.display()
            ));
        }
        if self.not_after < OffsetDateTime::now_utc().unix_timestamp() {
            return Err(format!("{} has expired", path.display()));
        }
        Ok(())
    }

    pub fn belongs_to(&self, key: &KeyPair) -> bool {
        key.subject_public_key_info() == self.spki
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_names_follow_the_certificate_rules() {
        for ok in [
            "localhost",
            "locks.internal.example",
            "a-b.example",
            "*.svc.example",
            " x.y ",
        ] {
            assert!(dns_name(ok).is_ok(), "{ok:?}");
        }
        for bad in [
            "",
            ".x",
            "x.",
            "x..y",
            "-x",
            "x.-y",
            "x-.y",
            "a b",
            "a_b",
            "*",
            "*.com",
            "a.*.com",
            "*x.example.com",
            "**.example.com",
            "*.*.example.com",
            "ünï.example",
        ] {
            assert!(dns_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn addresses_are_single_ipv4_or_ipv6_never_ranges() {
        assert!(ip_addr("10.0.0.5").is_ok());
        assert!(ip_addr("::1").is_ok());
        assert!(ip_addr(" 127.0.0.1 ").is_ok());
        assert!(ip_addr("10.0.0.0/24").unwrap_err().contains("CIDR"));
        for bad in ["", "256.0.0.1", "10.0.0", "host"] {
            assert!(ip_addr(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn files_are_never_replaced_without_force_and_secrets_stay_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        write_file(&path, "one", true, false).unwrap();
        assert!(write_file(&path, "two", true, false).is_err());
        assert!(refuse_overwrite(&[&path], false).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one");
        write_file(&path, "two", true, true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            let public = dir.path().join("c");
            write_file(&public, "x", false, false).unwrap();
            assert_eq!(mode(&public), 0o644);
            // Replacing a world-readable file with a secret must tighten it.
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            write_file(&path, "three", true, true).unwrap();
            assert_eq!(mode(&path), 0o600);
        }
    }
}
