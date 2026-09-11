use std::{io::IsTerminal, process::ExitCode};

use orcis::{
    api::{AppState, router},
    config::Config,
    store::Store,
};
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("orcis: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    let config = Config::from_env()?;
    let filter = EnvFilter::try_new(&config.rust_log)
        .map_err(|error| format!("invalid RUST_LOG {:?}: {error}", config.rust_log))?;
    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .with_env_filter(filter)
        .init();

    let store = Store::load(config.data_path.clone()).map_err(|error| {
        let path = config
            .data_path
            .as_ref()
            .map_or_else(|| "<memory>".to_owned(), |path| path.display().to_string());
        format!("failed to load ORCIS_DATA_PATH {path:?}: {error}")
    })?;
    let app = router(AppState::new(store, config.token));
    let listener = TcpListener::bind(config.addr)
        .await
        .map_err(|error| format!("failed to bind ORCIS_ADDR {:?}: {error}", config.addr))?;
    let bound = listener
        .local_addr()
        .map_err(|error| format!("failed to inspect bound address: {error}"))?;
    info!(address = %bound, "orcis listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|error| format!("server error: {error}"))?;
    info!("orcis shutdown complete");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            warn!(%error, "failed to install Ctrl-C handler");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => warn!(%error, "failed to install SIGTERM handler"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    info!("shutdown signal received");
}
