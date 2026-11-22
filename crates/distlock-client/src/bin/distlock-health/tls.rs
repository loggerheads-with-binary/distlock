//! `distlock-health tls ...`: the certificates for the server's TLS listener, without openssl.
//!
//! * `new-ca`: a private CA, `ca.key` (keep secret) and a self-signed `ca.crt` (give to clients).
//! * `use-ca`: check a CA you already have and put it where `server-cert` looks.
//! * `server-cert`: `server.crt` and `server.key`, signed by the CA in the same folder.
//!
//! Keys are ECDSA P-256 in PKCS#8 PEM, which the server reads. With a CA certificate but no CA key,
//! `server-cert` writes `server.csr` for your own CA to sign.
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{ArgGroup, Args as ClapArgs, Subcommand};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, SanType,
};
use time::OffsetDateTime;

use crate::pki::{self, CaCert};

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a private CA: ca.key (keep secret) and a self-signed ca.crt (give to clients).
    NewCa(NewCa),
    /// Use a CA you already have, instead of making one.
    UseCa(UseCa),
    /// Create server.crt and server.key for the server, signed by the CA in --dir.
    ServerCert(ServerCert),
}

/// Where the files go.
#[derive(ClapArgs, Debug)]
pub struct Place {
    /// The folder for ca.crt, ca.key, server.crt and server.key. Created if it is missing.
    #[arg(long, default_value = "config/tls", value_name = "DIR")]
    dir: PathBuf,

    /// Replace files that already exist. Without it, nothing is overwritten.
    #[arg(long)]
    force: bool,
}

#[derive(ClapArgs, Debug)]
pub struct NewCa {
    #[command(flatten)]
    place: Place,

    /// The CA's common name.
    #[arg(long, default_value = "distlock CA")]
    cn: String,

    /// How long the CA is valid.
    #[arg(long, default_value_t = 3650, value_parser = clap::value_parser!(u32).range(1..=36500))]
    days: u32,
}

#[derive(ClapArgs, Debug)]
pub struct UseCa {
    #[command(flatten)]
    place: Place,

    /// The CA certificate (PEM). It is checked, then copied to `<dir>/ca.crt`.
    #[arg(long, value_name = "FILE")]
    cert: PathBuf,

    /// The CA's private key (unencrypted PKCS#8 PEM), so this folder can sign certificates. Without
    /// it, `server-cert` writes a signing request for your CA instead.
    #[arg(long, value_name = "FILE")]
    key: Option<PathBuf>,
}

#[derive(ClapArgs, Debug)]
#[command(group(ArgGroup::new("names").required(true).multiple(true).args(["name", "ip"])))]
pub struct ServerCert {
    #[command(flatten)]
    place: Place,

    /// A host name clients put in their URL. A wildcard like '*.example.com' is fine. Comma-separated,
    /// and may be repeated.
    #[arg(long, value_delimiter = ',', value_parser = pki::dns_name, value_name = "HOST")]
    name: Vec<String>,

    /// An IPv4 or IPv6 address clients put in their URL. Comma-separated, and may be repeated.
    #[arg(long, value_delimiter = ',', value_parser = pki::ip_addr, value_name = "ADDRESS")]
    ip: Vec<IpAddr>,

    /// How long the certificate is valid.
    #[arg(long, default_value_t = 365, value_parser = clap::value_parser!(u32).range(1..=36500))]
    days: u32,

    /// Make the certificate for this existing private key (unencrypted PKCS#8 PEM). Without it, a new
    /// key is made.
    #[arg(long, value_name = "FILE")]
    key: Option<PathBuf>,
}

pub fn run_command(command: Command) -> ExitCode {
    let result = match command {
        Command::NewCa(a) => new_ca(&a),
        Command::UseCa(a) => use_ca(&a),
        Command::ServerCert(a) => server_cert(&a),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("distlock-health: {e}");
            ExitCode::FAILURE
        }
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn make_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))
}

fn new_ca(a: &NewCa) -> Result<(), String> {
    let (dir, force) = (&a.place.dir, a.place.force);
    let (crt, key) = (dir.join("ca.crt"), dir.join("ca.key"));
    pki::refuse_overwrite(&[&key, &crt], force)?;
    let pair = KeyPair::generate().map_err(err)?;
    let mut params = CertificateParams::new(Vec::<String>::new()).map_err(err)?;
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, a.cn.clone());
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    (params.not_before, params.not_after) = pki::validity(a.days);
    params.serial_number = Some(pki::random_serial()?);
    let cert = params.self_signed(&pair).map_err(err)?;
    make_dir(dir)?;
    pki::write_file(&key, &pair.serialize_pem(), true, force)?;
    pki::write_file(&crt, &cert.pem(), false, force)?;
    println!("CA created ({} days):", a.days);
    println!("  certificate (give to clients): {}", crt.display());
    println!("  private key (keep secret):     {}", key.display());
    println!(
        "next: distlock-health tls server-cert --dir {} --name HOST [--ip ADDRESS]",
        dir.display()
    );
    Ok(())
}

fn use_ca(a: &UseCa) -> Result<(), String> {
    let (dir, force) = (&a.place.dir, a.place.force);
    let ca = CaCert::read(&a.cert)?;
    ca.check_usable(&a.cert)?;
    let key_pem = match &a.key {
        Some(path) => {
            let pair = pki::read_key(path)?;
            if !ca.belongs_to(&pair) {
                return Err(format!(
                    "{} is not the private key of {}",
                    path.display(),
                    a.cert.display()
                ));
            }
            Some(std::fs::read_to_string(path).map_err(err)?)
        }
        None => None,
    };
    let (crt, key) = (dir.join("ca.crt"), dir.join("ca.key"));
    pki::refuse_overwrite(&[&crt], force)?;
    if key_pem.is_some() {
        pki::refuse_overwrite(&[&key], force)?;
    }
    make_dir(dir)?;
    pki::write_file(&crt, &ca.pem, false, force)?;
    if let Some(pem) = &key_pem {
        pki::write_file(&key, pem, true, force)?;
    }
    println!("using CA:");
    println!("  certificate (give to clients): {}", crt.display());
    if key_pem.is_some() {
        println!("  private key (keep secret):     {}", key.display());
    } else {
        println!("  no private key: server-cert will write a signing request for your CA");
    }
    println!(
        "next: distlock-health tls server-cert --dir {} --name HOST [--ip ADDRESS]",
        dir.display()
    );
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// Every name and address once, in the order given.
fn alt_names(a: &ServerCert) -> Result<Vec<SanType>, String> {
    let mut sans: Vec<SanType> = Vec::new();
    let mut add = |san: SanType| {
        if !sans.contains(&san) {
            sans.push(san);
        }
    };
    for name in &a.name {
        add(SanType::DnsName(name.clone().try_into().map_err(err)?));
    }
    for ip in &a.ip {
        add(SanType::IpAddress(*ip));
    }
    Ok(sans)
}

fn describe(sans: &[SanType]) -> String {
    sans.iter()
        .map(|s| match s {
            SanType::DnsName(n) => format!("DNS:{}", n.as_str()),
            SanType::IpAddress(ip) => format!("IP:{ip}"),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn server_cert(a: &ServerCert) -> Result<(), String> {
    let (dir, force) = (&a.place.dir, a.place.force);
    let (ca_crt, ca_key) = (dir.join("ca.crt"), dir.join("ca.key"));
    let (crt, key, csr) = (
        dir.join("server.crt"),
        dir.join("server.key"),
        dir.join("server.csr"),
    );
    if !ca_crt.exists() {
        return Err(format!(
            "{} not found: run `tls new-ca` or `tls use-ca` first (same --dir)",
            ca_crt.display()
        ));
    }
    let can_sign = ca_key.exists();
    let reuse_key = a.key.as_deref().is_some_and(|k| same_file(k, &key));
    // Everything that can fail runs before anything is written.
    let mut wanted: Vec<&Path> = vec![if can_sign { &crt } else { &csr }];
    if !reuse_key {
        wanted.push(&key);
    }
    pki::refuse_overwrite(&wanted, force)?;

    let sans = alt_names(a)?;
    let server_key = match &a.key {
        Some(path) => pki::read_key(path)?,
        None => KeyPair::generate().map_err(err)?,
    };
    let mut params = CertificateParams::default();
    params.subject_alt_names = sans.clone();
    // Clients ignore the common name; the names are in the subjectAltName.
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "distlock-server");
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.use_authority_key_identifier_extension = true;
    (params.not_before, params.not_after) = pki::validity(a.days);
    params.serial_number = Some(pki::random_serial()?);

    if !can_sign {
        // The CA chooses the serial number and the authority key identifier, not the requester.
        params.serial_number = None;
        params.use_authority_key_identifier_extension = false;
        let request = params.serialize_request(&server_key).map_err(err)?;
        make_dir(dir)?;
        if !reuse_key {
            pki::write_file(&key, &server_key.serialize_pem(), true, force)?;
        }
        pki::write_file(&csr, &request.pem().map_err(err)?, false, force)?;
        println!("no {}, so this CA cannot sign here.", ca_key.display());
        println!(
            "  1. give {} to your CA, asking for a server certificate with subjectAltName {}",
            csr.display(),
            describe(&sans)
        );
        println!(
            "  2. save the certificate it returns as {} (add its intermediates after it)",
            crt.display()
        );
        println!("private key: {}", key.display());
        return Ok(());
    }

    let ca = CaCert::read(&ca_crt)?;
    ca.check_usable(&ca_crt)?;
    let ca_pair = pki::read_key(&ca_key)?;
    if !ca.belongs_to(&ca_pair) {
        return Err(format!(
            "{} is not the private key of {}",
            ca_key.display(),
            ca_crt.display()
        ));
    }
    let issuer = Issuer::from_ca_cert_pem(&ca.pem, ca_pair).map_err(err)?;
    let cert = params.signed_by(&server_key, &issuer).map_err(err)?;
    let mut chain = cert.pem();
    // An intermediate CA's certificate goes after the server's, so clients can build the chain.
    let intermediate = !ca.self_signed;
    if intermediate {
        chain.push_str(&ca.pem);
    }
    if params.not_after.unix_timestamp() > ca.not_after {
        let ends = OffsetDateTime::from_unix_timestamp(ca.not_after)
            .map_or_else(|_| "earlier".to_owned(), |t| t.date().to_string());
        eprintln!("warning: the CA expires on {ends}, before this certificate does");
    }
    make_dir(dir)?;
    if !reuse_key {
        pki::write_file(&key, &server_key.serialize_pem(), true, force)?;
    }
    pki::write_file(&crt, &chain, false, force)?;
    println!(
        "server certificate for: {} (valid {} days)",
        describe(&sans),
        a.days
    );
    let note = if intermediate {
        "  (includes the intermediate CA; clients need the root CA)"
    } else {
        ""
    };
    println!("  certificate: {}{note}", crt.display());
    println!("  private key: {}", key.display());
    println!("start the server with:");
    println!(
        "  DISTLOCK_TLS_ENABLED=true DISTLOCK_TLS_CERT_FILE={} DISTLOCK_TLS_KEY_FILE={} distlock-server",
        crt.display(),
        key.display()
    );
    println!(
        "clients trust {} (--ca-file / ca_file_path, or its text as ca_file_content)",
        ca_crt.display()
    );
    Ok(())
}
