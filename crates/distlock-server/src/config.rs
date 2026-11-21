//! Server configuration, read from environment variables (see `.env.example`).
//!
//! Parsing goes through a lookup function instead of `std::env` directly, so tests never touch
//! process-global state.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use distlock_core::Config as LockConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Text,
    Json,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind_addr: SocketAddr,
    pub max_connections: usize,
    pub max_frame_bytes: usize,
    pub handshake_timeout: Duration,
    pub ping_interval: Duration,
    pub idle_connection_timeout: Duration,
    pub users_file: PathBuf,
    /// `DISTLOCK_TLS_ENABLED`. `true`: the listener speaks TLS only, and the server refuses to start
    /// without a usable certificate and key. `false`: plaintext.
    pub tls_enabled: bool,
    /// PEM certificate chain and PEM private key. Required when `tls_enabled`, an error otherwise.
    pub tls_cert_file: Option<PathBuf>,
    pub tls_key_file: Option<PathBuf>,
    pub auth_max_concurrent_bcrypt: usize,
    pub max_connections_per_session: usize,
    pub session_idle_timeout: Duration,
    /// Limits and behaviour of the lock table. `token_floor` is overwritten at startup.
    pub lock: LockConfig,
    pub log_format: LogFormat,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{var}: {message}")]
pub struct ConfigError {
    pub var: &'static str,
    pub message: String,
}

fn err(var: &'static str, message: impl Into<String>) -> ConfigError {
    ConfigError {
        var,
        message: message.into(),
    }
}

type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// A variable's value, or `None` when unset or blank (`FOO=` counts as unset).
fn raw(get: Lookup<'_>, var: &str) -> Option<String> {
    get(var)
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

fn parse<T>(get: Lookup<'_>, var: &'static str, default: T) -> Result<T, ConfigError>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    match raw(get, var) {
        None => Ok(default),
        Some(v) => v.parse().map_err(|e| err(var, format!("`{v}`: {e}"))),
    }
}

fn millis(get: Lookup<'_>, var: &'static str, default: Duration) -> Result<Duration, ConfigError> {
    let default_ms = u64::try_from(default.as_millis()).unwrap_or(u64::MAX);
    Ok(Duration::from_millis(parse(get, var, default_ms)?))
}

fn positive_millis(
    get: Lookup<'_>,
    var: &'static str,
    default: Duration,
) -> Result<Duration, ConfigError> {
    let d = millis(get, var, default)?;
    if d.is_zero() {
        return Err(err(var, "must be greater than zero"));
    }
    Ok(d)
}

fn at_least(var: &'static str, value: usize, min: usize) -> Result<usize, ConfigError> {
    if value < min {
        return Err(err(var, format!("must be at least {min}, got {value}")));
    }
    Ok(value)
}

impl ServerConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let get: Lookup<'_> = &get;
        let core = LockConfig::default();

        let bind_addr = parse(
            get,
            "DISTLOCK_BIND_ADDR",
            SocketAddr::from(([127, 0, 0, 1], 7878)),
        )?;
        let max_connections = at_least(
            "DISTLOCK_MAX_CONNECTIONS",
            parse(get, "DISTLOCK_MAX_CONNECTIONS", 10_000)?,
            1,
        )?;
        let max_frame_bytes = parse(get, "DISTLOCK_MAX_FRAME_BYTES", 1_048_576usize)?;
        if !(4096..=64 * 1024 * 1024).contains(&max_frame_bytes) {
            return Err(err(
                "DISTLOCK_MAX_FRAME_BYTES",
                "must be between 4096 and 67108864",
            ));
        }
        let handshake_timeout =
            positive_millis(get, "DISTLOCK_HANDSHAKE_TIMEOUT_MS", Duration::from_secs(5))?;
        let ping_interval =
            positive_millis(get, "DISTLOCK_PING_INTERVAL_MS", Duration::from_secs(10))?;
        let idle_connection_timeout = positive_millis(
            get,
            "DISTLOCK_IDLE_CONNECTION_TIMEOUT_MS",
            Duration::from_secs(30),
        )?;
        if idle_connection_timeout < 2 * ping_interval {
            return Err(err(
                "DISTLOCK_IDLE_CONNECTION_TIMEOUT_MS",
                "must be at least twice DISTLOCK_PING_INTERVAL_MS",
            ));
        }

        let users_file = PathBuf::from(
            raw(get, "DISTLOCK_USERS_FILE").unwrap_or_else(|| "./config/users.json".to_owned()),
        );
        let tls_cert_file = raw(get, "DISTLOCK_TLS_CERT_FILE").map(PathBuf::from);
        let tls_key_file = raw(get, "DISTLOCK_TLS_KEY_FILE").map(PathBuf::from);
        let tls_enabled = parse(get, "DISTLOCK_TLS_ENABLED", false)?;
        if tls_enabled && (tls_cert_file.is_none() || tls_key_file.is_none()) {
            return Err(err(
                "DISTLOCK_TLS_ENABLED",
                "TLS is enabled, so DISTLOCK_TLS_CERT_FILE and DISTLOCK_TLS_KEY_FILE must both be set",
            ));
        }
        // Setting a certificate and silently serving plaintext would be a dangerous surprise.
        if !tls_enabled && (tls_cert_file.is_some() || tls_key_file.is_some()) {
            return Err(err(
                "DISTLOCK_TLS_ENABLED",
                "DISTLOCK_TLS_CERT_FILE or DISTLOCK_TLS_KEY_FILE is set but TLS is not enabled: \
                 set DISTLOCK_TLS_ENABLED=true, or unset the files",
            ));
        }
        let auth_max_concurrent_bcrypt = at_least(
            "DISTLOCK_AUTH_MAX_CONCURRENT_BCRYPT",
            parse(get, "DISTLOCK_AUTH_MAX_CONCURRENT_BCRYPT", 4)?,
            1,
        )?;
        let max_connections_per_session = at_least(
            "DISTLOCK_MAX_CONNECTIONS_PER_SESSION",
            parse(get, "DISTLOCK_MAX_CONNECTIONS_PER_SESSION", 32)?,
            1,
        )?;
        let session_idle_timeout = positive_millis(
            get,
            "DISTLOCK_SESSION_IDLE_TIMEOUT_MS",
            Duration::from_secs(60),
        )?;

        let max_ttl = positive_millis(get, "DISTLOCK_MAX_TTL_MS", core.max_ttl)?;
        let max_hold = match raw(get, "DISTLOCK_MAX_HOLD_MS") {
            None => None,
            Some(v) => {
                let ms: u64 = v
                    .parse()
                    .map_err(|e| err("DISTLOCK_MAX_HOLD_MS", format!("`{v}`: {e}")))?;
                if ms == 0 {
                    return Err(err(
                        "DISTLOCK_MAX_HOLD_MS",
                        "must be greater than zero, or empty for unbounded",
                    ));
                }
                Some(Duration::from_millis(ms))
            }
        };
        let default_state_ttl =
            millis(get, "DISTLOCK_DEFAULT_STATE_TTL_MS", core.default_state_ttl)?;
        let max_state_ttl = millis(get, "DISTLOCK_MAX_STATE_TTL_MS", core.max_state_ttl)?;
        if default_state_ttl > max_state_ttl {
            return Err(err(
                "DISTLOCK_DEFAULT_STATE_TTL_MS",
                "must not exceed DISTLOCK_MAX_STATE_TTL_MS",
            ));
        }
        let event_default_publish = millis(
            get,
            "DISTLOCK_EVENT_DEFAULT_PUBLISH_MS",
            core.event_default_publish,
        )?;
        let event_max_publish =
            millis(get, "DISTLOCK_EVENT_MAX_PUBLISH_MS", core.event_max_publish)?;
        if event_default_publish > event_max_publish {
            return Err(err(
                "DISTLOCK_EVENT_DEFAULT_PUBLISH_MS",
                "must not exceed DISTLOCK_EVENT_MAX_PUBLISH_MS",
            ));
        }
        let max_event_value_len = at_least(
            "DISTLOCK_MAX_EVENT_VALUE_BYTES",
            parse(
                get,
                "DISTLOCK_MAX_EVENT_VALUE_BYTES",
                core.max_event_value_len,
            )?,
            1,
        )?;
        if max_event_value_len > max_frame_bytes / 2 {
            return Err(err(
                "DISTLOCK_MAX_EVENT_VALUE_BYTES",
                "must be at most half of DISTLOCK_MAX_FRAME_BYTES",
            ));
        }
        let lock = LockConfig {
            max_ttl,
            max_wait: positive_millis(get, "DISTLOCK_MAX_WAIT_MS", core.max_wait)?,
            max_hold,
            lock_delay: millis(get, "DISTLOCK_LOCK_DELAY_MS", core.lock_delay)?,
            default_state_ttl,
            max_state_ttl,
            max_name_len: at_least(
                "DISTLOCK_MAX_NAME_LEN",
                parse(get, "DISTLOCK_MAX_NAME_LEN", core.max_name_len)?,
                1,
            )?,
            max_waiters_per_lock: at_least(
                "DISTLOCK_MAX_WAITERS_PER_LOCK",
                parse(
                    get,
                    "DISTLOCK_MAX_WAITERS_PER_LOCK",
                    core.max_waiters_per_lock,
                )?,
                1,
            )?,
            max_requests_per_session: at_least(
                "DISTLOCK_MAX_REQUESTS_PER_SESSION",
                parse(
                    get,
                    "DISTLOCK_MAX_REQUESTS_PER_SESSION",
                    core.max_requests_per_session,
                )?,
                1,
            )?,
            // The core defaults to no grace period; the server defaults to one full lease.
            startup_grace: millis(get, "DISTLOCK_STARTUP_GRACE_MS", max_ttl)?,
            token_floor: core.token_floor,
            event_default_publish,
            event_max_publish,
            max_event_value_len,
        };

        let log_format = match raw(get, "DISTLOCK_LOG_FORMAT").as_deref() {
            None | Some("text") => LogFormat::Text,
            Some("json") => LogFormat::Json,
            Some(other) => {
                return Err(err(
                    "DISTLOCK_LOG_FORMAT",
                    format!("`{other}`: expected `text` or `json`"),
                ));
            }
        };

        Ok(Self {
            bind_addr,
            max_connections,
            max_frame_bytes,
            handshake_timeout,
            ping_interval,
            idle_connection_timeout,
            users_file,
            tls_enabled,
            tls_cert_file,
            tls_key_file,
            auth_max_concurrent_bcrypt,
            max_connections_per_session,
            session_idle_timeout,
            lock,
            log_format,
        })
    }

    /// Settings that are valid but probably a mistake in production.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.lock.startup_grace < self.lock.max_ttl {
            out.push(format!(
                "DISTLOCK_STARTUP_GRACE_MS ({}ms) is shorter than DISTLOCK_MAX_TTL_MS ({}ms): after a restart, \
                 leases granted by the previous process may still be live while new ones are granted",
                self.lock.startup_grace.as_millis(),
                self.lock.max_ttl.as_millis()
            ));
        }
        if !self.tls_enabled && self.bind_addr.ip().is_unspecified() {
            out.push(format!(
                "listening on {} (all interfaces) with no TLS: credentials and traffic are readable on the wire. \
                 Set DISTLOCK_TLS_ENABLED=true (with a certificate and key), or bind to a private interface.",
                self.bind_addr
            ));
        }
        out
    }
}

impl Default for ServerConfig {
    /// The defaults documented in `.env.example`.
    fn default() -> Self {
        Self::from_lookup(|_| None).expect("built-in defaults are valid")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn parse_dotenv(text: &str) -> HashMap<String, String> {
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| {
                let (k, v) = l
                    .split_once('=')
                    .unwrap_or_else(|| panic!("not KEY=VALUE: {l}"));
                (k.to_owned(), v.to_owned())
            })
            .collect()
    }

    #[test]
    fn env_example_parses_to_exactly_the_built_in_defaults() {
        let vars = parse_dotenv(include_str!("../../../.env.example"));
        let from_file = ServerConfig::from_lookup(|k| vars.get(k).cloned()).unwrap();
        let defaults = ServerConfig::default();
        assert_eq!(format!("{from_file:?}"), format!("{defaults:?}"));

        // Every variable the file sets must be one this loader actually reads.
        let seen = std::cell::RefCell::new(std::collections::HashSet::new());
        let _ = ServerConfig::from_lookup(|k| {
            seen.borrow_mut().insert(k.to_owned());
            vars.get(k).cloned()
        });
        for k in vars.keys().filter(|k| k.starts_with("DISTLOCK_")) {
            assert!(
                seen.borrow().contains(k),
                "{k} is in .env.example but never read"
            );
        }
    }

    #[test]
    fn tls_needs_the_flag_and_both_files() {
        let cfg = |pairs: &[(&str, &str)]| ServerConfig::from_lookup(lookup(pairs));
        let (cert, key) = (
            ("DISTLOCK_TLS_CERT_FILE", "/c.pem"),
            ("DISTLOCK_TLS_KEY_FILE", "/k.pem"),
        );
        let on = ("DISTLOCK_TLS_ENABLED", "true");
        // Enabled without both files: refuse.
        assert_eq!(cfg(&[on]).unwrap_err().var, "DISTLOCK_TLS_ENABLED");
        assert!(cfg(&[on, cert]).is_err());
        assert!(cfg(&[on, key]).is_err());
        // Files without the flag: refuse, rather than quietly serve plaintext.
        assert!(cfg(&[cert, key]).is_err());
        assert!(cfg(&[cert]).is_err());
        assert!(cfg(&[("DISTLOCK_TLS_ENABLED", "yes")]).is_err());
        let c = cfg(&[on, cert, key]).unwrap();
        assert!(c.tls_enabled && c.tls_cert_file.is_some() && c.tls_key_file.is_some());
        let off = cfg(&[("DISTLOCK_TLS_ENABLED", "false")]).unwrap();
        assert!(!off.tls_enabled && !ServerConfig::default().tls_enabled);
    }

    #[test]
    fn defaults_match_the_documented_values() {
        let c = ServerConfig::default();
        assert_eq!(c.bind_addr, "127.0.0.1:7878".parse().unwrap());
        assert_eq!(c.lock.max_ttl, Duration::from_secs(60));
        assert_eq!(
            c.lock.startup_grace, c.lock.max_ttl,
            "server grace defaults to one max lease"
        );
        assert_eq!(c.lock.max_hold, None);
        assert_eq!(c.log_format, LogFormat::Text);
        assert!(c.warnings().is_empty());
    }

    #[test]
    fn overrides_are_applied_and_blank_means_default() {
        let c = ServerConfig::from_lookup(lookup(&[
            ("DISTLOCK_BIND_ADDR", "10.0.0.5:9000"),
            ("DISTLOCK_MAX_TTL_MS", "1500"),
            ("DISTLOCK_MAX_HOLD_MS", "9000"),
            ("DISTLOCK_STARTUP_GRACE_MS", "0"),
            ("DISTLOCK_LOCK_DELAY_MS", ""),
            ("DISTLOCK_LOG_FORMAT", "json"),
        ]))
        .unwrap();
        assert_eq!(c.bind_addr.port(), 9000);
        assert_eq!(c.lock.max_ttl, Duration::from_millis(1500));
        assert_eq!(c.lock.max_hold, Some(Duration::from_secs(9)));
        assert_eq!(c.lock.startup_grace, Duration::ZERO);
        assert_eq!(
            c.lock.lock_delay,
            Duration::from_secs(5),
            "blank falls back to the default"
        );
        assert_eq!(c.log_format, LogFormat::Json);
        assert_eq!(
            c.warnings().len(),
            1,
            "grace shorter than max ttl is flagged"
        );
    }

    #[test]
    fn bad_values_name_the_variable() {
        for (var, value) in [
            ("DISTLOCK_BIND_ADDR", "not-an-addr"),
            ("DISTLOCK_MAX_TTL_MS", "0"),
            ("DISTLOCK_MAX_TTL_MS", "soon"),
            ("DISTLOCK_MAX_HOLD_MS", "0"),
            ("DISTLOCK_MAX_FRAME_BYTES", "10"),
            ("DISTLOCK_MAX_CONNECTIONS", "0"),
            ("DISTLOCK_AUTH_MAX_CONCURRENT_BCRYPT", "0"),
            ("DISTLOCK_LOG_FORMAT", "xml"),
        ] {
            let e = ServerConfig::from_lookup(lookup(&[(var, value)])).unwrap_err();
            assert_eq!(e.var, var, "{var}={value}");
        }
    }

    #[test]
    fn cross_variable_constraints_are_enforced() {
        let e = ServerConfig::from_lookup(lookup(&[
            ("DISTLOCK_PING_INTERVAL_MS", "10000"),
            ("DISTLOCK_IDLE_CONNECTION_TIMEOUT_MS", "15000"),
        ]))
        .unwrap_err();
        assert_eq!(e.var, "DISTLOCK_IDLE_CONNECTION_TIMEOUT_MS");

        let e = ServerConfig::from_lookup(lookup(&[
            ("DISTLOCK_DEFAULT_STATE_TTL_MS", "5000"),
            ("DISTLOCK_MAX_STATE_TTL_MS", "1000"),
        ]))
        .unwrap_err();
        assert_eq!(e.var, "DISTLOCK_DEFAULT_STATE_TTL_MS");

        let e = ServerConfig::from_lookup(lookup(&[
            ("DISTLOCK_EVENT_DEFAULT_PUBLISH_MS", "5000"),
            ("DISTLOCK_EVENT_MAX_PUBLISH_MS", "1000"),
        ]))
        .unwrap_err();
        assert_eq!(e.var, "DISTLOCK_EVENT_DEFAULT_PUBLISH_MS");

        // A value must fit in a frame with room to spare.
        let e = ServerConfig::from_lookup(lookup(&[
            ("DISTLOCK_MAX_FRAME_BYTES", "4096"),
            ("DISTLOCK_MAX_EVENT_VALUE_BYTES", "4096"),
        ]))
        .unwrap_err();
        assert_eq!(e.var, "DISTLOCK_MAX_EVENT_VALUE_BYTES");
        let e = ServerConfig::from_lookup(lookup(&[("DISTLOCK_MAX_EVENT_VALUE_BYTES", "0")]))
            .unwrap_err();
        assert_eq!(e.var, "DISTLOCK_MAX_EVENT_VALUE_BYTES");
    }

    #[test]
    fn event_settings_have_the_documented_defaults_and_can_be_overridden() {
        let c = ServerConfig::default();
        assert_eq!(c.lock.event_default_publish, Duration::from_secs(10));
        assert_eq!(c.lock.event_max_publish, Duration::from_secs(600));
        assert_eq!(c.lock.max_event_value_len, 4096);

        let c = ServerConfig::from_lookup(lookup(&[
            ("DISTLOCK_EVENT_DEFAULT_PUBLISH_MS", "0"),
            ("DISTLOCK_EVENT_MAX_PUBLISH_MS", "30000"),
            ("DISTLOCK_MAX_EVENT_VALUE_BYTES", "100"),
        ]))
        .unwrap();
        assert_eq!(c.lock.event_default_publish, Duration::ZERO);
        assert_eq!(c.lock.event_max_publish, Duration::from_secs(30));
        assert_eq!(c.lock.max_event_value_len, 100);
    }

    #[test]
    fn binding_all_interfaces_is_flagged() {
        let c =
            ServerConfig::from_lookup(lookup(&[("DISTLOCK_BIND_ADDR", "0.0.0.0:7878")])).unwrap();
        assert!(c.warnings().iter().any(|w| w.contains("no TLS")));
    }
}
