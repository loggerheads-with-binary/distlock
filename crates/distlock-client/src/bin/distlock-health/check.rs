//! `distlock-health check`: log in, ask for the server's figures, print them, and optionally tell a
//! webhook. One-shot by default; with `--interval` it checks forever, logging in afresh each time (so a
//! check also proves that logging in works).
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Args as ClapArgs;
use distlock_client::{DistLockClient, ServerStats};
use serde_json::{Value, json};

use crate::conn::Conn;
use crate::report;

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[command(flatten)]
    conn: Conn,

    /// Exit 3 while the server is in its startup grace period.
    #[arg(long)]
    require_ready: bool,

    /// Check forever, once per this many seconds, instead of once.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..), value_name = "SECS")]
    interval: Option<u64>,

    /// POST a JSON report here when the server is healthy (http:// or https://).
    #[arg(long, env = "DISTLOCK_OK_WEBHOOK", hide_env_values = true, value_parser = webhook_url, value_name = "URL")]
    ok_webhook: Option<String>,

    /// POST a JSON report here when it is not (http:// or https://).
    #[arg(long, env = "DISTLOCK_ERROR_WEBHOOK", hide_env_values = true, value_parser = webhook_url, value_name = "URL")]
    error_webhook: Option<String>,

    /// Print sizes as 16.4 GiB, percentages as 24.3% and integers as 92_160. The webhook JSON is not
    /// affected.
    #[arg(long)]
    human_readable: bool,
}

/// An empty value (a variable that is set but empty) means "no webhook".
fn webhook_url(v: &str) -> Result<String, String> {
    let v = v.trim();
    if v.is_empty() || v.starts_with("http://") || v.starts_with("https://") {
        Ok(v.to_owned())
    } else {
        Err("must be an http:// or https:// URL".into())
    }
}

fn non_empty(v: &Option<String>) -> Option<&str> {
    v.as_deref().filter(|v| !v.is_empty())
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
    let result = match tokio::time::timeout(args.conn.timeout(), run(client)).await {
        Ok(r) => r,
        Err(_) => Err(format!("no answer within {}s", args.conn.timeout)),
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
        "stats": report.stats.as_ref().map(|s| report::rows(s)
                .into_iter()
                .map(|r| (r.key.to_owned(), r.value))
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

/// Exit status (one-shot): 0 healthy, 1 cannot connect / log in / get stats, 3 the server is in its
/// startup grace period and `--require-ready` was given. Watch mode never exits by itself.
pub async fn run_command(args: Args) -> ExitCode {
    let client = match args.conn.client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("distlock-health: {e}");
            return ExitCode::from(2);
        }
    };
    let server = format!("{}:{}", client.host, client.port);
    loop {
        let report = check(client.clone(), &args).await;
        if args.interval.is_some() {
            println!("# {} unix={}", server, unix_now());
        }
        if let Some(s) = &report.stats {
            report::print(s, args.human_readable);
        }
        if let Some(e) = &report.error {
            eprintln!("distlock-health: {e}");
        }
        let hook = match report.status {
            Status::Ok => non_empty(&args.ok_webhook),
            Status::NotReady | Status::Failed => non_empty(&args.error_webhook),
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
        tokio::time::sleep(Duration::from_secs(every)).await;
    }
}
