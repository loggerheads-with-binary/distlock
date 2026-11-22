//! `distlock-health`: operator tools for a distlock deployment. Every action is a named subcommand;
//! running it with none prints this help and exits with status 2.
//!
//! * `check`: log in and print the server's health figures, optionally to a webhook.
//! * `reset-users`: make the server re-read its users file.
//! * `tls new-ca | use-ca | server-cert`: the certificates for the server's TLS listener.
//! * `keys generate`: an Ed25519 key pair for `auth_mode=2` logins.
//!
//! Exit status: 0 success, 1 the action failed, 2 bad usage, 3 (`check --require-ready` only) the
//! server is in its startup grace period.
mod check;
mod conn;
mod human;
mod keys;
mod pki;
mod report;
mod reset;
mod tls;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "distlock-health",
    version,
    about = "Health checks and operator tools for a distlock server",
    arg_required_else_help = true,
    subcommand_required = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Log in to a server and print its health figures. Exits 1 if it cannot.
    Check(check::Args),
    /// Make the server re-read its users file, and print what changed and the file's MD5.
    ResetUsers(reset::Args),
    /// Make or install the certificates for the server's TLS listener.
    #[command(arg_required_else_help = true, subcommand_required = true)]
    Tls {
        #[command(subcommand)]
        command: tls::Command,
    },
    /// Make Ed25519 keys for `auth_mode=2` logins.
    #[command(arg_required_else_help = true, subcommand_required = true)]
    Keys {
        #[command(subcommand)]
        command: keys::Command,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Check(args) => check::run_command(args).await,
        Command::ResetUsers(args) => reset::run_command(args).await,
        Command::Tls { command } => tls::run_command(command),
        Command::Keys { command } => keys::run_command(command),
    }
}
