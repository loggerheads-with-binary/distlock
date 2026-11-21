//! The `distlock-health` binary against a real server and a fake webhook receiver.
mod common;
use common::*;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::Duration;
/// A tiny HTTP server that answers 200 to every request. Returns its URL and a channel that yields
/// each request body.
fn webhook() -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://{}/hook/secret-token",
        listener.local_addr().unwrap()
    );
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok((mut s, _)) = listener.accept() {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let (head_end, len) = loop {
                let n = s.read(&mut chunk).unwrap_or(0);
                buf.extend_from_slice(&chunk[..n]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    break (i + 4, len);
                }
                if n == 0 {
                    break (0, 0);
                }
            };
            while buf.len() < head_end + len {
                let n = s.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let _ =
                s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = tx.send(String::from_utf8_lossy(&buf[head_end..]).into_owned());
        }
    });
    (url, rx)
}
async fn health(args: Vec<String>, url: String) -> Output {
    tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_distlock-health"))
            .args(args)
            .env("DISTLOCK_URL", url)
            .env_remove("DISTLOCK_OK_WEBHOOK")
            .env_remove("DISTLOCK_ERROR_WEBHOOK")
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}
fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| (*x).to_owned()).collect()
}
#[tokio::test(flavor = "multi_thread")]
async fn healthy_server_reports_to_the_ok_webhook() {
    let srv = TestServer::start().await;
    let (hook, body) = webhook();
    let url = format!("distlock://billing:pw-billing@{}", srv.addr);
    let out = health(s(&["--ok-webhook", &hook]), url).await;
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let json: serde_json::Value =
        serde_json::from_str(&body.recv_timeout(Duration::from_secs(5)).unwrap()).unwrap();
    assert_eq!(json["status"], "ok");
    assert_eq!(json["stats"]["connected_clients"], 1);
    assert!(!json.to_string().contains("pw-billing"));
}
#[tokio::test(flavor = "multi_thread")]
async fn bad_password_reports_to_the_error_webhook_and_exits_1() {
    let srv = TestServer::start().await;
    let (hook, body) = webhook();
    let url = format!("distlock://billing:nope@{}", srv.addr);
    let out = health(s(&["--error-webhook", &hook]), url).await;
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let json: serde_json::Value =
        serde_json::from_str(&body.recv_timeout(Duration::from_secs(5)).unwrap()).unwrap();
    assert_eq!(json["status"], "error");
    assert!(json["error"].as_str().unwrap().contains("authentication"));
    // The secret in the webhook URL must not reach our stderr either.
    assert!(!String::from_utf8_lossy(&out.stderr).contains("secret-token"));
}
#[tokio::test(flavor = "multi_thread")]
async fn startup_grace_exits_3_only_with_require_ready() {
    let srv = TestServer::start_with("127.0.0.1:0".parse().unwrap(), |c| {
        c.lock.startup_grace = Duration::from_secs(60);
    })
    .await;
    let url = format!("distlock://billing:pw-billing@{}", srv.addr);
    assert_eq!(health(s(&[]), url.clone()).await.status.code(), Some(0));
    let (hook, body) = webhook();
    let out = health(s(&["--require-ready", "--error-webhook", &hook]), url).await;
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    let json: serde_json::Value =
        serde_json::from_str(&body.recv_timeout(Duration::from_secs(5)).unwrap()).unwrap();
    assert_eq!(json["status"], "error");
    assert_eq!(json["stats"]["ready"], false);
}
#[tokio::test(flavor = "multi_thread")]
async fn a_dead_webhook_does_not_change_the_exit_code_and_a_bad_one_is_refused() {
    let srv = TestServer::start().await;
    let url = format!("distlock://billing:pw-billing@{}", srv.addr);
    let out = health(s(&["--ok-webhook", "http://127.0.0.1:1/x"]), url.clone()).await;
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("webhook failed"));
    let out = health(s(&["--ok-webhook", "ftp://x"]), url).await;
    assert_eq!(out.status.code(), Some(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn interval_mode_keeps_checking_and_switches_webhook_when_the_server_dies() {
    let srv = TestServer::start().await;
    let (ok_hook, ok_body) = webhook();
    let (err_hook, err_body) = webhook();
    let mut child = Command::new(env!("CARGO_BIN_EXE_distlock-health"))
        .args([
            "--interval",
            "1",
            "--timeout",
            "3",
            "--ok-webhook",
            &ok_hook,
        ])
        .args(["--error-webhook", &err_hook])
        .env(
            "DISTLOCK_URL",
            format!("distlock://billing:pw-billing@{}", srv.addr),
        )
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let recv = |rx: &mpsc::Receiver<String>| {
        let body = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("a webhook post");
        serde_json::from_str::<serde_json::Value>(&body).unwrap()
    };
    // Two healthy reports prove it loops, not just runs once.
    assert_eq!(recv(&ok_body)["status"], "ok");
    assert_eq!(recv(&ok_body)["status"], "ok");
    assert!(
        child.try_wait().unwrap().is_none(),
        "the loop must keep running"
    );
    srv.stop().await;
    let down = recv(&err_body);
    assert_eq!(down["status"], "error");
    assert!(down["stats"].is_null());
    child.kill().unwrap();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread")]
async fn tls_flags_select_which_certificate_checks_run() {
    let pki = Pki::new();
    // Trusted CA, but the certificate names someone else.
    let srv = tls_server(&pki.issued("wrong-name", wrong_names())).await;
    let url = format!("distlock://billing:pw-billing@{}?tls=true", srv.addr);
    let ca = pki.ca_file().display().to_string();
    for (extra, want, mode) in [
        (vec![], 1, "full verification must reject a wrong name"),
        (vec!["--no-verify-host"], 0, "CA only"),
        (
            vec!["--no-verify-ca"],
            1,
            "name only, and the name is wrong",
        ),
        (vec!["--no-verify-ca", "--no-verify-host"], 0, "none"),
    ] {
        let mut args = vec!["--ca-file", ca.as_str()];
        args.extend(extra);
        let out = health(s(&args), url.clone()).await;
        assert_eq!(out.status.code(), Some(want), "{mode}: {out:?}");
    }
}
