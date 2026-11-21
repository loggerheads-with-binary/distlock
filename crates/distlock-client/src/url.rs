//! The `distlock://` URL form, parsed the way the Python client does (`distlock.settings`).
use std::time::Duration;

use crate::client::DistLockClient;
use crate::error::{Error, Result};

const QUERY_KEYS: [&str; 3] = ["pool_size", "tls", "default_ttl"];

fn bad<T>(m: impl Into<String>) -> Result<T> {
    Err(Error::Config(m.into()))
}

/// `%XX` decoding, as `urllib.parse.unquote` does. A malformed `%` stays as it is.
fn unquote(s: &str) -> Result<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (
            b[i],
            b.get(i + 1).and_then(|c| hex(*c)),
            b.get(i + 2).and_then(|c| hex(*c)),
        ) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, ..) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).or_else(|_| bad("percent-encoded text in the URL is not valid UTF-8"))
}

impl DistLockClient {
    /// Parse `distlock://client_id:password@host[:port][/namespace][?pool_size=&tls=&default_ttl=]`.
    ///
    /// User and password may be percent-encoded. The path, if any, is the namespace (one segment).
    /// `default_ttl` is in seconds. Unknown query parameters are an error, not ignored.
    pub fn from_url(url: &str) -> Result<Self> {
        let url = url.trim();
        let Some((scheme, rest)) = url.split_once("://") else {
            return bad("the URL must start with distlock://");
        };
        if !scheme.eq_ignore_ascii_case("distlock") {
            return bad(format!(
                "unsupported URL scheme {scheme:?}: expected 'distlock://'"
            ));
        }
        if rest.contains('#') {
            return bad("a distlock URL cannot have a #fragment");
        }
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (authority, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
        let Some((userinfo, hostport)) = authority.rsplit_once('@') else {
            return bad("the URL must carry credentials: distlock://client_id:password@host");
        };
        let Some((user, password)) = userinfo.split_once(':') else {
            return bad("the URL must carry credentials: distlock://client_id:password@host");
        };
        // A colon starts a port only after the closing bracket of an IPv6 literal.
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) if !p.contains(']') => match p.parse::<u16>() {
                Ok(port) => (h, Some(port)),
                Err(_) => return bad(format!("malformed distlock URL: bad port {p:?}")),
            },
            _ => (hostport, None),
        };
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if host.is_empty() {
            return bad("the URL must name a host");
        }
        let mut c = DistLockClient {
            client_id: unquote(user)?,
            password: unquote(password)?,
            host: host.to_ascii_lowercase(),
            ..Default::default()
        };
        if let Some(port) = port {
            c.port = port;
        }
        let ns = unquote(path.strip_prefix('/').unwrap_or(path))?;
        let ns = ns.strip_suffix('/').unwrap_or(&ns);
        if ns.contains('/') {
            return bad("the URL path is the namespace and must be a single segment (no '/')");
        }
        if !ns.is_empty() {
            c.namespace = Some(ns.to_owned());
        }
        let mut seen = Vec::new();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let (key, value) = (
                unquote(&key.replace('+', " "))?,
                unquote(&value.replace('+', " "))?,
            );
            if !QUERY_KEYS.contains(&key.as_str()) {
                return bad(format!(
                    "unknown URL parameter {key:?}: expected one of {}",
                    QUERY_KEYS.join(", ")
                ));
            }
            if seen.contains(&key) {
                return bad(format!("URL parameter {key:?} appears more than once"));
            }
            let invalid = || Error::Config(format!("invalid distlock URL: {key} = {value:?}"));
            match key.as_str() {
                "pool_size" => c.pool_size = value.parse().map_err(|_| invalid())?,
                "tls" => {
                    c.tls = match value.to_ascii_lowercase().as_str() {
                        "1" | "true" | "yes" | "on" => true,
                        "0" | "false" | "no" | "off" => false,
                        _ => return Err(invalid()),
                    }
                }
                _ => {
                    let secs: f64 = value.parse().map_err(|_| invalid())?;
                    c.default_ttl = Duration::try_from_secs_f64(secs).map_err(|_| invalid())?;
                }
            }
            seen.push(key);
        }
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_decodes() {
        let c = DistLockClient::from_url(
            "distlock://bill%40ing:p%40ss%3Aw%2Frd@Host.Example:9000/team?pool_size=2&default_ttl=1.5",
        )
        .unwrap();
        assert_eq!(
            (c.client_id.as_str(), c.password.as_str()),
            ("bill@ing", "p@ss:w/rd")
        );
        assert_eq!((c.host.as_str(), c.port), ("host.example", 9000));
        assert_eq!(c.namespace.as_deref(), Some("team"));
        assert_eq!(
            (c.pool_size, c.default_ttl),
            (2, Duration::from_millis(1500))
        );
        let c = DistLockClient::from_url("distlock://a:@[::1]").unwrap();
        assert_eq!(
            (c.host.as_str(), c.port, c.password.as_str()),
            ("::1", 7878, "")
        );
        let c = DistLockClient::from_url("distlock://a:b@[::1]:1/").unwrap();
        assert_eq!((c.host.as_str(), c.port, c.namespace), ("::1", 1, None));
    }

    #[test]
    fn rejects_what_python_rejects() {
        for url in [
            "http://a:b@h",
            "distlock://h",
            "distlock://a@h",
            "distlock://a:b@",
            "distlock://a:b@h:nope",
            "distlock://a:b@h#frag",
            "distlock://a:b@h/x/y",
            "distlock://a:b@h?bogus=1",
            "distlock://a:b@h?tls=1&tls=0",
            "distlock://a:b@h?tls=maybe",
        ] {
            assert!(DistLockClient::from_url(url).is_err(), "{url}");
        }
    }
}
