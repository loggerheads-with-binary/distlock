//! `distlock-health`: log in to a distlock server and print its health figures.
//!
//! The server is named by a URL, `distlock://CLIENT_ID:PASSWORD@HOST[:PORT]` (port defaults to
//! 7878). The user and password may be percent-encoded, and the URL takes the same optional path and
//! query as the Python client (see `DistLockClient::from_url`). The URL comes from,
//! in order: `--url URL`, `--url -` (read one line from stdin), the `DISTLOCK_URL` environment
//! variable, or stdin when it is piped. Prefer the last three: a password in argv is visible to
//! other users of the machine.
//!
//! One-shot by default. With `--interval SECS` it checks forever, once per interval, with a fresh
//! login each time (so a check also proves that logging in works).
//!
//! Webhooks: each check POSTs a JSON report to `--ok-webhook` (also `DISTLOCK_OK_WEBHOOK`) when the
//! server is healthy, or to `--error-webhook` (also `DISTLOCK_ERROR_WEBHOOK`) when it is not. Either
//! may be left out. Works in one-shot mode too.
//!
//! TLS: `--tls` (or `?tls=true` in the URL) turns it on. It then checks the server certificate's
//! chain (`--ca-file PEM` names the trusted CA, else public roots are used) and its name. The two
//! checks are independent: `--no-verify-host` skips the name ("CA only"), `--no-verify-ca` skips the
//! chain ("name only", self-signed passes), both skip everything (encryption only).
//!
//! Exit status (one-shot): 0 healthy, 1 cannot connect / log in / get stats, 2 bad usage, 3 the
//! server is in its startup grace period and `--require-ready` was given. Watch mode only exits on
//! bad usage (2).
use std::io::{IsTerminal, Read};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use distlock_client::{DistLockClient, ServerStats};
use serde_json::{Value, json};

const USAGE: &str = "usage: distlock-health [--url URL | --url -] [--require-ready] [--timeout SECS]\n\
                     \x20                       [--interval SECS] [--ok-webhook URL] [--error-webhook URL]\n\
                     \x20                       [--tls] [--no-verify-ca] [--no-verify-host] [--ca-file PEM]\n\
                     URL: distlock://CLIENT_ID:PASSWORD@HOST[:PORT], else $DISTLOCK_URL, else stdin\n\
                     webhooks: else $DISTLOCK_OK_WEBHOOK / $DISTLOCK_ERROR_WEBHOOK (http:// or https://)";

struct Args {
    url: Option<String>,
    require_ready: bool,
    timeout: Duration,
    interval: Option<Duration>,
    ok_webhook: Option<String>,
    error_webhook: Option<String>,
    tls: bool,
    no_verify_ca: bool,
    no_verify_host: bool,
    ca_file: Option<String>,
}

fn secs_arg(flag: &str, v: Option<String>) -> Result<Duration, String> {
    v.and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("{flag} needs a positive number of seconds"))
}

fn webhook(flag: &str, flag_value: Option<String>, var: &str) -> Result<Option<String>, String> {
    let v = flag_value
        .or_else(|| std::env::var(var).ok())
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    match v {
        Some(v) if !(v.starts_with("http://") || v.starts_with("https://")) => Err(format!(
            "{flag} (or ${var}) must be an http:// or https:// URL"
        )),
        v => Ok(v),
    }
}

fn parse_args(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut a = Args {
        url: None,
        require_ready: false,
        timeout: Duration::from_secs(15),
        interval: None,
        ok_webhook: None,
        error_webhook: None,
        tls: false,
        no_verify_ca: false,
        no_verify_host: false,
        ca_file: None,
    };
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--url" => a.url = Some(it.next().ok_or("--url needs a value")?),
            "--require-ready" => a.require_ready = true,
            "--timeout" => a.timeout = secs_arg("--timeout", it.next())?,
            "--interval" => a.interval = Some(secs_arg("--interval", it.next())?),
            "--tls" => a.tls = true,
            "--no-verify-ca" => a.no_verify_ca = true,
            "--no-verify-host" => a.no_verify_host = true,
            "--ca-file" => a.ca_file = Some(it.next().ok_or("--ca-file needs a value")?),
            "--ok-webhook" => a.ok_webhook = Some(it.next().ok_or("--ok-webhook needs a value")?),
            "--error-webhook" => {
                a.error_webhook = Some(it.next().ok_or("--error-webhook needs a value")?);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    a.ok_webhook = webhook("--ok-webhook", a.ok_webhook, "DISTLOCK_OK_WEBHOOK")?;
    a.error_webhook = webhook("--error-webhook", a.error_webhook, "DISTLOCK_ERROR_WEBHOOK")?;
    Ok(a)
}

fn read_stdin_line() -> Result<String, String> {
    let mut buf = String::new();
    std::io::stdin()
        .take(4096)
        .read_to_string(&mut buf)
        .map_err(|e| format!("cannot read stdin: {e}"))?;
    Ok(buf.lines().next().unwrap_or("").trim().to_owned())
}

fn resolve_url(flag: Option<String>) -> Result<String, String> {
    let url = match flag {
        Some(v) if v == "-" => read_stdin_line()?,
        Some(v) => v,
        None => match std::env::var("DISTLOCK_URL") {
            Ok(v) if !v.trim().is_empty() => v.trim().to_owned(),
            _ if !std::io::stdin().is_terminal() => read_stdin_line()?,
            _ => return Err("no URL: use --url, DISTLOCK_URL, or pipe it on stdin".into()),
        },
    };
    if url.is_empty() {
        return Err("the URL is empty".into());
    }
    Ok(url)
}

/// The figures, once, for both the text output and the webhook JSON. `null` = not reported.
fn rows(s: &ServerStats) -> Vec<(&'static str, Value)> {
    let pct = |v: f64| json!((v * 10.0).round() / 10.0);
    let mut r = vec![
        ("version", json!(s.version)),
        ("uptime_s", json!(s.uptime_ms / 1000)),
        ("ready", json!(!s.in_startup_grace)),
        (
            "startup_grace_remaining_ms",
            json!(s.startup_grace_remaining_ms),
        ),
        ("connected_clients", json!(s.connected_clients)),
        ("connections", json!(s.connections)),
        ("sessions", json!(s.sessions)),
        ("locks_held", json!(s.locks_held)),
        ("requests_queued", json!(s.requests_queued)),
        ("lock_names", json!(s.lock_names)),
        ("events", json!(s.events)),
    ];
    if let Some(y) = &s.system {
        r.extend([
            ("system_memory_total_bytes", json!(y.total_memory_bytes)),
            ("system_memory_used_bytes", json!(y.used_memory_bytes)),
            ("system_cpu_count", json!(y.cpu_count)),
            ("system_cpu_percent", pct(y.cpu_percent)),
            ("system_load_avg_1m", json!(y.load_avg_1m)),
            ("system_load_avg_5m", json!(y.load_avg_5m)),
            ("system_load_avg_15m", json!(y.load_avg_15m)),
        ]);
    }
    if let Some(p) = &s.process {
        r.extend([
            ("process_pid", json!(p.pid)),
            ("process_cpu_percent", pct(p.cpu_percent)),
            ("process_memory_bytes", json!(p.memory_bytes)),
            (
                "process_virtual_memory_bytes",
                json!(p.virtual_memory_bytes),
            ),
            ("process_cpu_time_ms", json!(p.cpu_time_ms)),
            ("process_open_files", json!(p.open_files)),
            ("process_open_files_limit", json!(p.open_files_limit)),
        ]);
    }
    r
}

fn print_stats(s: &ServerStats) {
    for (key, v) in rows(s) {
        match v {
            Value::Null => println!("{key}: n/a"),
            Value::String(t) => println!("{key}: {t}"),
            other => println!("{key}: {other}"),
        }
    }
}

enum Status {
    Ok,
    /// Reachable, but in its startup grace period and `--require-ready` was given.
    NotReady,
    Failed,
}

struct Report {
    status: Status,
    error: Option<String>,
    stats: Option<ServerStats>,
}

async fn run(client: DistLockClient) -> Result<ServerStats, String> {
    let c = client.connect().await.map_err(|e| e.to_string())?;
    let stats = c.stats().await.map_err(|e| e.to_string());
    let _ = c.close().await;
    stats
}

async fn check(client: DistLockClient, args: &Args) -> Report {
    let result = match tokio::time::timeout(args.timeout, run(client)).await {
        Ok(r) => r,
        Err(_) => Err(format!("no answer within {}s", args.timeout.as_secs())),
    };
    match result {
        Ok(stats) if args.require_ready && stats.in_startup_grace => Report {
            status: Status::NotReady,
            error: Some(format!(
                "the server is in its startup grace period ({} ms left)",
                stats.startup_grace_remaining_ms
            )),
            stats: Some(stats),
        },
        Ok(stats) => Report {
            status: Status::Ok,
            error: None,
            stats: Some(stats),
        },
        Err(e) => Report {
            status: Status::Failed,
            error: Some(e),
            stats: None,
        },
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The JSON a webhook receives. `text` is there for chat tools that show one string; everything
/// else is for programs. The server's password is never included.
fn payload(server: &str, report: &Report) -> Value {
    let ok = matches!(report.status, Status::Ok);
    let text = match &report.error {
        None => format!("distlock {server}: healthy"),
        Some(e) => format!("distlock {server}: UNHEALTHY: {e}"),
    };
    json!({
        "status": if ok { "ok" } else { "error" },
        "text": text,
        "server": server,
        "checked_at_unix": unix_now(),
        "error": report.error,
        "stats": report.stats.as_ref().map(|s| rows(s)
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect::<serde_json::Map<_, _>>()),
    })
}

/// Fire and forget: a dead webhook must not stop the monitoring. Webhook URLs often carry a secret,
/// so they are kept out of every message.
async fn notify(url: &str, body: Value) {
    let target = url.to_owned();
    let result = tokio::task::spawn_blocking(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .into();
        agent
            .post(&target)
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .map(|_| ())
            .map_err(|e| e.to_string().replace(&target, "<webhook>"))
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("distlock-health: webhook failed: {e}"),
        Err(e) => eprintln!("distlock-health: webhook failed: {e}"),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let usage = |e: String| {
        eprintln!("distlock-health: {e}\n{USAGE}");
        ExitCode::from(2)
    };
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => return usage(e),
    };
    let client = match resolve_url(args.url.clone())
        .and_then(|u| DistLockClient::from_url(&u).map_err(|e| e.to_string()))
    {
        // One connection is plenty for a probe.
        Ok(c) => DistLockClient {
            pool_size: 1,
            // Flags can only add TLS or relax a check; they never undo what the URL asks for.
            tls: c.tls || args.tls,
            tls_verify_ca: c.tls_verify_ca && !args.no_verify_ca,
            tls_verify_host: c.tls_verify_host && !args.no_verify_host,
            ca_file_path: args.ca_file.clone().map(Into::into).or(c.ca_file_path),
            ..c
        },
        Err(e) => return usage(e),
    };
    let server = format!("{}:{}", client.host, client.port);
    loop {
        let report = check(client.clone(), &args).await;
        if args.interval.is_some() {
            println!("# {} unix={}", server, unix_now());
        }
        if let Some(s) = &report.stats {
            print_stats(s);
        }
        if let Some(e) = &report.error {
            eprintln!("distlock-health: {e}");
        }
        let hook = match report.status {
            Status::Ok => &args.ok_webhook,
            Status::NotReady | Status::Failed => &args.error_webhook,
        };
        if let Some(url) = hook {
            notify(url, payload(&server, &report)).await;
        }
        let Some(every) = args.interval else {
            return match report.status {
                Status::Ok => ExitCode::SUCCESS,
                Status::NotReady => ExitCode::from(3),
                Status::Failed => ExitCode::FAILURE,
            };
        };
        tokio::time::sleep(every).await;
    }
}
