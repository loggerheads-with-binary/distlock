//! `distlock-health keys generate`: an Ed25519 key pair for `auth_mode=2` logins, the same as
//! `python -m distlock.keys`, without Python.
//!
//! Both halves are 32 bytes, written as URL-safe base64 without padding (43 characters), so they can go
//! into a URL as they are. The private half is a secret: it is printed to stdout and nowhere else.
use std::process::ExitCode;

use clap::{Args as ClapArgs, Subcommand};
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Make a new Ed25519 key pair and print it with the users.json entry and the client URL.
    Generate(Generate),
}

#[derive(ClapArgs, Debug)]
pub struct Generate {
    /// The user the key is for. Only used in the text that is printed.
    #[arg(long, default_value = "CLIENT_ID")]
    client_id: String,

    /// A name for this key in the server log, e.g. laptop.
    #[arg(long)]
    identifier: Option<String>,

    /// Metadata only: who vouched for the key.
    #[arg(long)]
    issuer: Option<String>,

    /// RFC 3339 time after which the key stops logging anyone in, e.g. 2026-12-31T00:00:00Z.
    #[arg(long, value_parser = rfc3339, value_name = "TIME")]
    expiry: Option<String>,
}

/// The server refuses a users file with a bad expiry, so catch it here.
fn rfc3339(v: &str) -> Result<String, String> {
    OffsetDateTime::parse(v, &Rfc3339)
        .map(|_| v.to_owned())
        .map_err(|_| "not an RFC 3339 time with an offset, like 2026-12-31T00:00:00Z".to_owned())
}

/// `(private, public)`, each 43 characters of URL-safe base64.
fn generate_keypair() -> Result<(String, String), String> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| format!("no random numbers from the system: {e}"))?;
    let pair = Ed25519KeyPair::from_seed_unchecked(&seed)
        .map_err(|_| "could not make a key from the random seed".to_owned())?;
    Ok((
        distlock_proto::encode_key(&seed),
        distlock_proto::encode_key(pair.public_key().as_ref()),
    ))
}

pub fn run_command(command: Command) -> ExitCode {
    let Command::Generate(args) = command;
    let (private, public) = match generate_keypair() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("distlock-health: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Written by hand, not through a JSON map, so that "key" comes first.
    let mut fields = vec![format!("\"key\": {}", Value::String(public.clone()))];
    for (name, value) in [
        ("identifier", &args.identifier),
        ("issuer", &args.issuer),
        ("expiry", &args.expiry),
    ] {
        if let Some(v) = value {
            fields.push(format!("\"{name}\": {}", Value::String(v.clone())));
        }
    }
    println!("private key (SECRET: the client's password in an auth_mode=2 URL):");
    println!("  {private}");
    println!("public key:");
    println!("  {public}");
    println!("\nadd to the user in users.json, under \"public_keys\":");
    println!("  {{{}}}", fields.join(", "));
    println!("\nclient URL:");
    println!(
        "  distlock://{}:{private}@HOST:7878?auth_mode=v2",
        args.client_id
    );
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_pair_is_43_characters_each_and_the_halves_belong_together() {
        let (private, public) = generate_keypair().unwrap();
        assert_eq!((private.len(), public.len()), (43, 43));
        let seed = distlock_proto::decode_key32(&private).unwrap();
        let pair = Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
        assert_eq!(
            distlock_proto::encode_key(pair.public_key().as_ref()),
            public
        );
        assert_ne!(
            generate_keypair().unwrap().0,
            private,
            "keys must be random"
        );
    }

    #[test]
    fn the_expiry_must_be_rfc_3339_with_an_offset() {
        assert!(rfc3339("2026-12-31T00:00:00Z").is_ok());
        assert!(rfc3339("2026-12-31T00:00:00+05:30").is_ok());
        for bad in ["", "tomorrow", "2026-12-31", "2026-12-31T00:00:00"] {
            assert!(rfc3339(bad).is_err(), "{bad:?}");
        }
    }
}
