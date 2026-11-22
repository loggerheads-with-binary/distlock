//! How the subcommands that talk to a server say which server, and how to connect to it.
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use distlock_client::DistLockClient;

#[derive(Args, Debug)]
pub struct Conn {
    /// The server: `distlock://CLIENT_ID:PASSWORD@HOST[:PORT]` (port defaults to 7878). The same URL
    /// and query as the Python client takes. `-` reads one line from stdin. Prefer the environment
    /// variable: a password in argv is visible to other users of the machine.
    #[arg(long, env = "DISTLOCK_URL", hide_env_values = true, value_name = "URL")]
    pub url: String,

    /// Give up on the server after this many seconds.
    #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..), value_name = "SECS")]
    pub timeout: u64,

    /// Connect over TLS (also `?tls=true` in the URL).
    #[arg(long)]
    pub tls: bool,

    /// With TLS: do not require the certificate to chain to a trusted CA ("name only").
    #[arg(long)]
    pub no_verify_ca: bool,

    /// With TLS: do not require the certificate to name the host or IP in the URL ("CA only").
    #[arg(long)]
    pub no_verify_host: bool,

    /// With TLS: the PEM file of the CA to trust. Without it, public roots are used.
    #[arg(long, value_name = "PEM")]
    pub ca_file: Option<PathBuf>,
}

impl Conn {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout)
    }

    /// The client for this server. One connection is plenty for a probe.
    pub fn client(&self) -> Result<DistLockClient, String> {
        let url = if self.url == "-" {
            read_stdin_line()?
        } else {
            self.url.trim().to_owned()
        };
        if url.is_empty() {
            return Err("the URL is empty".into());
        }
        let c = DistLockClient::from_url(&url).map_err(|e| e.to_string())?;
        Ok(DistLockClient {
            pool_size: 1,
            // Flags can only add TLS or relax a check; they never undo what the URL asks for.
            tls: c.tls || self.tls,
            tls_verify_ca: c.tls_verify_ca && !self.no_verify_ca,
            tls_verify_host: c.tls_verify_host && !self.no_verify_host,
            ca_file_path: self.ca_file.clone().or(c.ca_file_path),
            ..c
        })
    }
}

fn read_stdin_line() -> Result<String, String> {
    let mut buf = String::new();
    std::io::stdin()
        .take(4096)
        .read_to_string(&mut buf)
        .map_err(|e| format!("cannot read stdin: {e}"))?;
    Ok(buf.lines().next().unwrap_or("").trim().to_owned())
}
