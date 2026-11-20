use std::process::ExitCode;

use distlock_server::{LogFormat, Server, ServerConfig, UserDb};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    // A `.env` file is optional; real environment variables win over it.
    let _ = dotenvy::dotenv();

    let cfg = match ServerConfig::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("configuration error: {e}");
            return ExitCode::from(2);
        }
    };
    init_logging(cfg.log_format);
    for warning in cfg.warnings() {
        warn!("{warning}");
    }

    // Blocks for about one bcrypt hash (the dummy hash); fine before we accept anyone.
    let users = match UserDb::load(&cfg.users_file, cfg.auth_max_concurrent_bcrypt) {
        Ok(users) => users,
        Err(e) => {
            error!("{e}");
            return ExitCode::from(2);
        }
    };
    info!(file = %cfg.users_file.display(), users = users.user_count(), "users loaded");

    let bind_addr = cfg.bind_addr;
    let tls = cfg.tls_cert_file.is_some();
    let server = match Server::bind(cfg, users).await {
        Ok(server) => server,
        Err(e) => {
            error!("cannot listen on {bind_addr}: {e}");
            return ExitCode::FAILURE;
        }
    };
    match server.local_addr() {
        Ok(addr) => info!(%addr, tls, "distlock-server listening"),
        Err(e) => warn!("listening, but cannot read the local address: {e}"),
    }

    let shutdown = server.shutdown_token();
    tokio::spawn(async move {
        wait_for_signal().await;
        info!("shutdown signal received");
        shutdown.cancel();
    });

    match server.run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("server stopped: {e}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match format {
        LogFormat::Text => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
