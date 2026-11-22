//! `distlock-health reset-users`: make the server re-read its users file.
//!
//! Prints what changed and the MD5 of the file now in force, so a deploy script can compare it with
//! the hash of the file it just rolled out. Any logged-in client may do this (there is no per-user
//! permission yet); removing or disabling a user ends that user's sessions.
use std::process::ExitCode;

use clap::Args as ClapArgs;
use distlock_client::DistLockClient;

use crate::conn::Conn;

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[command(flatten)]
    conn: Conn,
}

async fn reset(client: DistLockClient) -> Result<distlock_client::ResetUsersReport, String> {
    let c = client.connect().await.map_err(|e| e.to_string())?;
    let report = c
        .maintenance()
        .reset_users()
        .await
        .map_err(|e| e.to_string());
    let _ = c.close().await;
    report
}

/// Exit status: 0 the users file was re-read (or was unchanged), 1 it could not be (the server keeps
/// the users it had), 2 bad usage.
pub async fn run_command(args: Args) -> ExitCode {
    let client = match args.conn.client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("distlock-health: {e}");
            return ExitCode::from(2);
        }
    };
    let outcome = match tokio::time::timeout(args.conn.timeout(), reset(client)).await {
        Ok(r) => r,
        Err(_) => Err(format!("no answer within {}s", args.conn.timeout)),
    };
    match outcome {
        Ok(r) => {
            let md5 = if r.users_md5.is_empty() {
                "n/a"
            } else {
                &r.users_md5
            };
            println!("users_md5: {md5}");
            println!("added: {}", r.added);
            println!("removed: {}", r.removed);
            println!("disabled: {}", r.disabled);
            println!("enabled: {}", r.enabled);
            println!("password_changed: {}", r.password_changed);
            println!("sessions_closed: {}", r.sessions_closed);
            println!("connections_closed: {}", r.connections_closed);
            println!("invoker_dropped: {}", r.invoker_dropped);
            if r.invoker_dropped {
                eprintln!(
                    "distlock-health: the user in the URL is no longer in the users file; \
                     its sessions were ended"
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("distlock-health: {e}");
            ExitCode::FAILURE
        }
    }
}
