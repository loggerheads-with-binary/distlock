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
/// Run `distlock-health ARGS` with `DISTLOCK_URL` set.
async fn cli(args: Vec<String>, url: String) -> Output {
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
/// `distlock-health check ARGS`.
async fn health(args: Vec<String>, url: String) -> Output {
    cli([s(&["check"]), args].concat(), url).await
}
/// `distlock-health ARGS` with no server in the environment: for the commands that need none.
fn tool(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_distlock-health"))
        .args(args)
        .env_remove("DISTLOCK_URL")
        .output()
        .unwrap()
}
fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}
fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}
/// The value of a `key: value` line.
fn field(out: &Output, key: &str) -> String {
    let text = stdout(out);
    text.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}: ")))
        .unwrap_or_else(|| panic!("no `{key}` line in {text}"))
        .to_owned()
}
fn md5_hex(path: &std::path::Path) -> String {
    use md5::{Digest, Md5};
    Md5::digest(std::fs::read(path).unwrap())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
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
            "check",
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
// ------------------------------------------------------------------------------ no implicit behaviour
#[tokio::test(flavor = "multi_thread")]
async fn nothing_happens_without_a_subcommand() {
    let out = tool(&[]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let help = format!("{}{}", stdout(&out), stderr(&out));
    for sub in ["check", "reset-users", "tls", "keys"] {
        assert!(help.contains(sub), "the help lists `{sub}`: {help}");
    }
    // A server in the environment, or old-style flags, do not turn into a check.
    let srv = TestServer::start().await;
    let url = format!("distlock://billing:pw-billing@{}", srv.addr);
    let out = cli(vec![], url.clone()).await;
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(stdout(&out).is_empty(), "no check ran: {}", stdout(&out));
    let out = cli(s(&["--require-ready"]), url).await;
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    // A command group alone is not an action either.
    for group in ["tls", "keys"] {
        let out = tool(&[group]);
        assert_eq!(out.status.code(), Some(2), "{group}: {out:?}");
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn the_server_must_be_named_and_stdin_is_only_read_when_asked() {
    let out = tool(&["check"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(stderr(&out).contains("--url"), "{}", stderr(&out));
    // A URL on a pipe is ignored unless `--url -` says to read it.
    let srv = TestServer::start().await;
    let url = format!("distlock://billing:pw-billing@{}\n", srv.addr);
    let run = |args: &'static [&'static str]| {
        let url = url.clone();
        tokio::task::spawn_blocking(move || {
            let mut child = Command::new(env!("CARGO_BIN_EXE_distlock-health"))
                .args(args)
                .env_remove("DISTLOCK_URL")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(url.as_bytes())
                .unwrap();
            child.wait_with_output().unwrap()
        })
    };
    assert_eq!(run(&["check"]).await.unwrap().status.code(), Some(2));
    let out = run(&["check", "--url", "-"]).await.unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}
// ------------------------------------------------------------------------------ --human-readable
#[tokio::test(flavor = "multi_thread")]
async fn human_readable_formats_sizes_percentages_and_integers_but_not_the_webhook() {
    let srv = TestServer::start().await;
    let url = format!("distlock://billing:pw-billing@{}", srv.addr);
    let (hook, body) = webhook();
    let plain = health(s(&[]), url.clone()).await;
    let human = health(s(&["--human-readable", "--ok-webhook", &hook]), url).await;
    assert_eq!(plain.status.code(), Some(0), "{plain:?}");
    assert_eq!(human.status.code(), Some(0), "{human:?}");
    // Sizes: plain digits become binary units.
    assert!(
        field(&plain, "system_memory_total_bytes")
            .bytes()
            .all(|b| b.is_ascii_digit())
    );
    assert!(field(&human, "system_memory_total_bytes").ends_with("iB"));
    assert!(field(&human, "process_memory_bytes").ends_with("iB"));
    // Percentages.
    assert!(field(&human, "system_cpu_percent").ends_with('%'));
    assert!(!field(&plain, "system_cpu_percent").ends_with('%'));
    // Integers: the same number, with `_` between groups of three digits.
    let limit = field(&plain, "process_open_files_limit");
    let shown = field(&human, "process_open_files_limit");
    assert_eq!(shown.replace('_', ""), limit);
    assert_eq!(shown.contains('_'), limit.len() > 3, "{shown} for {limit}");
    // A process id is a name, not a quantity.
    assert!(!field(&human, "process_pid").contains('_'));
    // The webhook JSON keeps plain numbers.
    let json: serde_json::Value =
        serde_json::from_str(&body.recv_timeout(Duration::from_secs(5)).unwrap()).unwrap();
    assert!(
        json["stats"]["system_memory_total_bytes"].is_u64(),
        "{json}"
    );
    assert!(json["stats"]["system_cpu_percent"].is_f64(), "{json}");
}
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_webhook_variable_means_no_webhook() {
    let srv = TestServer::start().await;
    let url = format!("distlock://billing:pw-billing@{}", srv.addr);
    let out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_distlock-health"))
            .arg("check")
            .env("DISTLOCK_URL", url)
            .env("DISTLOCK_OK_WEBHOOK", "")
            .env("DISTLOCK_ERROR_WEBHOOK", "")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}
// ------------------------------------------------------------------------------ users hash and reset-users
#[tokio::test(flavor = "multi_thread")]
async fn check_and_reset_users_report_the_md5_of_the_users_file_in_force() {
    let srv = FileServer::start(&["billing", "reports"]).await;
    let url = format!("distlock://billing:pw-billing@{}", srv.addr);
    let first = md5_hex(srv.path());
    // Known from the first load, before any reset.
    let out = health(s(&[]), url.clone()).await;
    assert_eq!(field(&out, "users_md5"), first);
    // The file changes on disk, but the server keeps what it loaded until it is told.
    srv.write(&[("billing", true), ("reports", true), ("extra", true)]);
    let second = md5_hex(srv.path());
    assert_ne!(first, second);
    assert_eq!(
        field(&health(s(&[]), url.clone()).await, "users_md5"),
        first
    );
    let out = cli(s(&["reset-users"]), url.clone()).await;
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(field(&out, "users_md5"), second);
    assert_eq!(field(&out, "added"), "1");
    assert_eq!(
        field(&health(s(&[]), url.clone()).await, "users_md5"),
        second
    );
    // Asking again with the same file changes nothing and says the same.
    let out = cli(s(&["reset-users"]), url.clone()).await;
    assert_eq!(field(&out, "users_md5"), second);
    assert_eq!(field(&out, "added"), "0");
    // A broken file is refused, the old users stay, and so does their hash.
    srv.write_raw("{ not json");
    let out = cli(s(&["reset-users"]), url.clone()).await;
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        stderr(&out).contains("previous users stay in force"),
        "{}",
        stderr(&out)
    );
    assert_eq!(field(&health(s(&[]), url).await, "users_md5"), second);
}
#[tokio::test(flavor = "multi_thread")]
async fn reset_users_by_a_user_who_was_removed_still_reports_and_says_so() {
    let srv = FileServer::start(&["billing", "reports"]).await;
    srv.write(&[("reports", true)]);
    let url = format!("distlock://billing:pw-billing@{}", srv.addr);
    let out = cli(s(&["reset-users"]), url).await;
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(field(&out, "removed"), "1");
    assert_eq!(field(&out, "invoker_dropped"), "true");
    assert!(
        stderr(&out).contains("no longer in the users file"),
        "{}",
        stderr(&out)
    );
}
#[tokio::test(flavor = "multi_thread")]
async fn reset_users_fails_cleanly_when_the_server_is_unreachable() {
    let out = cli(
        s(&["reset-users", "--timeout", "2"]),
        "distlock://billing:pw@127.0.0.1:1".into(),
    )
    .await;
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(stdout(&out).is_empty());
}
// ------------------------------------------------------------------------------ keys generate
#[tokio::test(flavor = "multi_thread")]
async fn a_generated_key_logs_in_and_the_printed_users_entry_is_valid_json() {
    let out = tool(&[
        "keys",
        "generate",
        "--client-id",
        "billing",
        "--identifier",
        "ci-laptop",
    ]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let after = |marker: &str| {
        let at = lines.iter().position(|l| l.starts_with(marker)).unwrap();
        lines[at + 1].to_owned()
    };
    let (private, public) = (after("private key"), after("public key"));
    let entry: serde_json::Value =
        serde_json::from_str(&after("add to the user in users.json")).unwrap();
    assert_eq!(entry["key"], public.as_str());
    assert_eq!(entry["identifier"], "ci-laptop");
    assert!(
        text.contains(&format!(
            "distlock://billing:{private}@HOST:7878?auth_mode=v2"
        )),
        "{text}"
    );
    // The server accepts the key it was given the public half of.
    let db = distlock_server::UserDb::for_tests_with_keys(&[(
        "billing",
        &[distlock_proto::decode_key32(&public).unwrap()],
        true,
    )]);
    let srv = TestServer::start_with_db("127.0.0.1:0".parse().unwrap(), db, |_| {}).await;
    let url = format!("distlock://billing:{private}@{}?auth_mode=v2", srv.addr);
    assert_eq!(health(s(&[]), url).await.status.code(), Some(0));
    // A different key does not.
    let other = tool(&["keys", "generate"]);
    let other_private = stdout(&other).lines().nth(1).unwrap().trim().to_owned();
    let url = format!(
        "distlock://billing:{other_private}@{}?auth_mode=v2",
        srv.addr
    );
    assert_eq!(health(s(&[]), url).await.status.code(), Some(1));
    // A bad expiry is refused before anything is printed.
    let out = tool(&["keys", "generate", "--expiry", "next week"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stdout(&out).is_empty());
}
// ------------------------------------------------------------------------------ tls
fn path_str(p: &std::path::Path) -> String {
    p.to_str().unwrap().to_owned()
}
fn ok(out: &Output) {
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}
#[tokio::test(flavor = "multi_thread")]
async fn certificates_from_the_tool_work_for_a_real_tls_server_and_check_the_name() {
    let work = tempfile::tempdir().unwrap();
    let dir = work.path().join("pki");
    let d = path_str(&dir);
    ok(&tool(&["tls", "new-ca", "--dir", &d, "--cn", "Test CA"]));
    ok(&tool(&[
        "tls",
        "server-cert",
        "--dir",
        &d,
        "--name",
        "localhost,a.example",
        "--name",
        "*.svc.example",
        "--ip",
        "127.0.0.1",
    ]));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |f: &str| std::fs::metadata(dir.join(f)).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode("ca.key"), mode("server.key")), (0o600, 0o600));
        assert_eq!((mode("ca.crt"), mode("server.crt")), (0o644, 0o644));
    }
    let srv = tls_server(&Pem {
        cert: dir.join("server.crt"),
        key: dir.join("server.key"),
    })
    .await;
    let url = format!("distlock://billing:pw-billing@{}", srv.addr);
    let ca = path_str(&dir.join("ca.crt"));
    // Trusting the CA, and the certificate names 127.0.0.1: a full check passes.
    ok(&health(s(&["--tls", "--ca-file", &ca]), url.clone()).await);
    // Public roots do not know this CA.
    assert_eq!(
        health(s(&["--tls"]), url.clone()).await.status.code(),
        Some(1)
    );
    // A certificate that names only another host fails the name check, and passes without it.
    let other = work.path().join("other");
    let o = path_str(&other);
    ok(&tool(&["tls", "new-ca", "--dir", &o]));
    ok(&tool(&[
        "tls",
        "server-cert",
        "--dir",
        &o,
        "--name",
        "other.example",
    ]));
    let srv = tls_server(&Pem {
        cert: other.join("server.crt"),
        key: other.join("server.key"),
    })
    .await;
    let url = format!("distlock://billing:pw-billing@{}", srv.addr);
    let other_ca = path_str(&other.join("ca.crt"));
    assert_eq!(
        health(s(&["--tls", "--ca-file", &other_ca]), url.clone())
            .await
            .status
            .code(),
        Some(1)
    );
    ok(&health(
        s(&["--tls", "--ca-file", &other_ca, "--no-verify-host"]),
        url,
    )
    .await);
}
#[test]
fn the_tls_tools_never_overwrite_without_force_and_check_their_input() {
    let work = tempfile::tempdir().unwrap();
    let d = path_str(&work.path().join("pki"));
    ok(&tool(&["tls", "new-ca", "--dir", &d]));
    let key_before = std::fs::read(work.path().join("pki/ca.key")).unwrap();
    let out = tool(&["tls", "new-ca", "--dir", &d]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("already exists"), "{}", stderr(&out));
    assert_eq!(
        std::fs::read(work.path().join("pki/ca.key")).unwrap(),
        key_before
    );
    ok(&tool(&["tls", "new-ca", "--dir", &d, "--force"]));
    assert_ne!(
        std::fs::read(work.path().join("pki/ca.key")).unwrap(),
        key_before
    );
    // A server certificate needs a name, and the names must be valid.
    for bad in [
        &["tls", "server-cert", "--dir", &d][..],
        &["tls", "server-cert", "--dir", &d, "--name", "*.com"],
        &["tls", "server-cert", "--dir", &d, "--ip", "10.0.0.0/24"],
    ] {
        assert_eq!(tool(bad).status.code(), Some(2), "{bad:?}");
    }
    // No CA in the folder.
    let empty = path_str(&work.path().join("empty"));
    let out = tool(&["tls", "server-cert", "--dir", &empty, "--name", "x.example"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("new-ca"), "{}", stderr(&out));
    // The first certificate is kept when a second is asked for without --force.
    ok(&tool(&[
        "tls",
        "server-cert",
        "--dir",
        &d,
        "--name",
        "x.example",
    ]));
    let out = tool(&["tls", "server-cert", "--dir", &d, "--name", "y.example"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    ok(&tool(&[
        "tls",
        "server-cert",
        "--dir",
        &d,
        "--name",
        "y.example",
        "--force",
    ]));
}
#[tokio::test(flavor = "multi_thread")]
async fn use_ca_installs_a_ca_you_have_and_refuses_what_cannot_sign() {
    let work = tempfile::tempdir().unwrap();
    let p = |name: &str| work.path().join(name);
    let (a, c) = (path_str(&p("a")), path_str(&p("c")));
    ok(&tool(&["tls", "new-ca", "--dir", &a]));
    ok(&tool(&["tls", "new-ca", "--dir", &c]));
    ok(&tool(&[
        "tls",
        "server-cert",
        "--dir",
        &a,
        "--name",
        "localhost",
        "--ip",
        "127.0.0.1",
    ]));
    let (a_crt, a_key) = (path_str(&p("a/ca.crt")), path_str(&p("a/ca.key")));
    // The CA and its key move to another folder, which can then sign for real.
    let b = path_str(&p("b"));
    ok(&tool(&[
        "tls", "use-ca", "--dir", &b, "--cert", &a_crt, "--key", &a_key,
    ]));
    ok(&tool(&[
        "tls",
        "server-cert",
        "--dir",
        &b,
        "--name",
        "localhost",
        "--ip",
        "127.0.0.1",
    ]));
    let srv = tls_server(&Pem {
        cert: p("b/server.crt"),
        key: p("b/server.key"),
    })
    .await;
    let url = format!("distlock://billing:pw-billing@{}", srv.addr);
    ok(&health(s(&["--tls", "--ca-file", &path_str(&p("b/ca.crt"))]), url).await);
    // Another CA's key, a certificate that is not a CA, a missing file: each is refused, and nothing is written.
    let wrong_key = path_str(&p("c/ca.key"));
    let leaf = path_str(&p("a/server.crt"));
    for (what, args) in [
        ("wrong key", vec!["--cert", &a_crt, "--key", &wrong_key]),
        ("not a CA", vec!["--cert", &leaf]),
        ("missing", vec!["--cert", "/no/such/ca.crt"]),
    ] {
        let d = path_str(&p(&format!("refused-{}", what.replace(' ', "-"))));
        let mut argv = vec!["tls", "use-ca", "--dir", d.as_str()];
        argv.extend(args);
        let out = tool(&argv);
        assert_eq!(out.status.code(), Some(1), "{what}: {out:?}");
        assert!(
            !std::path::Path::new(&d).join("ca.crt").exists(),
            "{what}: nothing written"
        );
    }
    // A CA that has expired cannot be used.
    {
        use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::days(30);
        params.not_after = now - time::Duration::days(1);
        std::fs::write(p("old.crt"), params.self_signed(&key).unwrap().pem()).unwrap();
        let out = tool(&[
            "tls",
            "use-ca",
            "--dir",
            &path_str(&p("old")),
            "--cert",
            &path_str(&p("old.crt")),
        ]);
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        assert!(stderr(&out).contains("expired"), "{}", stderr(&out));
    }
    // With the certificate only, server-cert writes a signing request for your own CA.
    let e = path_str(&p("e"));
    ok(&tool(&["tls", "use-ca", "--dir", &e, "--cert", &a_crt]));
    let out = tool(&["tls", "server-cert", "--dir", &e, "--name", "x.example"]);
    ok(&out);
    assert!(stdout(&out).contains("server.csr"), "{}", stdout(&out));
    assert!(p("e/server.csr").exists() && p("e/server.key").exists());
    assert!(!p("e/server.crt").exists(), "nothing was signed");
    assert!(
        std::fs::read_to_string(p("e/server.csr"))
            .unwrap()
            .contains("CERTIFICATE REQUEST")
    );
}
#[tokio::test(flavor = "multi_thread")]
async fn an_intermediate_ca_gets_its_certificate_appended_so_clients_trusting_the_root_connect() {
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    };
    let work = tempfile::tempdir().unwrap();
    let p = |name: &str| work.path().join(name);
    let ca_params = |cn: &str| {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name.push(DnType::CommonName, cn);
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params
    };
    let (root_key, root_params) = (KeyPair::generate().unwrap(), ca_params("root"));
    let root = root_params.self_signed(&root_key).unwrap();
    let root_issuer = Issuer::new(root_params, root_key);
    let (mid_key, mid_params) = (KeyPair::generate().unwrap(), ca_params("intermediate"));
    let mid = mid_params.signed_by(&mid_key, &root_issuer).unwrap();
    std::fs::write(p("root.crt"), root.pem()).unwrap();
    std::fs::write(p("mid.crt"), mid.pem()).unwrap();
    std::fs::write(p("mid.key"), mid_key.serialize_pem()).unwrap();
    let d = path_str(&p("pki"));
    ok(&tool(&[
        "tls",
        "use-ca",
        "--dir",
        &d,
        "--cert",
        &path_str(&p("mid.crt")),
        "--key",
        &path_str(&p("mid.key")),
    ]));
    let out = tool(&[
        "tls",
        "server-cert",
        "--dir",
        &d,
        "--name",
        "localhost",
        "--ip",
        "127.0.0.1",
    ]);
    ok(&out);
    assert!(stdout(&out).contains("intermediate"), "{}", stdout(&out));
    let chain = std::fs::read_to_string(p("pki/server.crt")).unwrap();
    assert_eq!(
        chain.matches("BEGIN CERTIFICATE").count(),
        2,
        "server cert then intermediate"
    );
    let srv = tls_server(&Pem {
        cert: p("pki/server.crt"),
        key: p("pki/server.key"),
    })
    .await;
    let url = format!("distlock://billing:pw-billing@{}", srv.addr);
    ok(&health(s(&["--tls", "--ca-file", &path_str(&p("root.crt"))]), url).await);
}
