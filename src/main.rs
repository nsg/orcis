use std::{io::IsTerminal, process::ExitCode};

use jiff::Timestamp;
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

    std::fs::create_dir_all(&config.data_path).map_err(|error| {
        format!(
            "failed to create ORCIS_DATA_PATH {:?}: {error}",
            config.data_path
        )
    })?;
    let db_path = config.db_path();
    let store = Store::open(&db_path)
        .map_err(|error| format!("failed to open database {db_path:?}: {error}"))?;
    let artifact_dir = config.artifact_dir();
    let state = AppState::with_artifact_dir(store, config.token, &artifact_dir)
        .map_err(|error| format!("failed to open artifact directory {artifact_dir:?}: {error}"))?;
    spawn_artifact_collector(state.clone());
    let app = router(state);
    let listener = TcpListener::bind(config.addr)
        .await
        .map_err(|error| format!("failed to bind ORCIS_ADDR {:?}: {error}", config.addr))?;
    let bound = listener
        .local_addr()
        .map_err(|error| format!("failed to inspect bound address: {error}"))?;
    info!(address = %bound, data = %config.data_path.display(), database = %db_path.display(), artifacts = %artifact_dir.display(), "orcis listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|error| format!("server error: {error}"))?;
    info!("orcis shutdown complete");
    Ok(())
}

fn spawn_artifact_collector(state: AppState) {
    const RETENTION_SECONDS: i64 = 7 * 24 * 60 * 60;
    const INTERVAL_SECONDS: u64 = 60 * 60;

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(INTERVAL_SECONDS));
        loop {
            interval.tick().await;
            let cutoff = Timestamp::from_second(Timestamp::now().as_second() - RETENTION_SECONDS)
                .expect("seven days before the current timestamp is representable");
            let state = state.clone();
            match tokio::task::spawn_blocking(move || state.collect_expired_artifacts(cutoff)).await
            {
                Ok(Ok((expired, orphans))) if expired > 0 || orphans > 0 => {
                    info!(expired, orphans, "collected artifact files");
                }
                Ok(Ok(_)) => {}
                Ok(Err(error)) => warn!(%error, "artifact collection failed"),
                Err(error) => warn!(%error, "artifact collector task failed"),
            }
        }
    });
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
