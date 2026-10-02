mod app;
mod args;
mod bot;
mod config;
mod db;
mod error;
mod kick;
mod logs;
mod migrator;
mod web;

pub type Result<T> = std::result::Result<T, error::Error>;
pub type ShutdownRx = watch::Receiver<()>;

use anyhow::{anyhow, Context};
use app::App;
use args::{Args, Command};
use clap::Parser;
use config::Config;
use db::{setup_db, writer::create_writer};
use futures::future::try_join_all;
use kick::api::KickApi;
use migrator::Migrator;
use mimalloc::MiMalloc;
use std::{
    env,
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::{mpsc, watch},
    time::timeout,
};
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;

use crate::app::cache::UsersCache;

const SHUTDOWN_TIMEOUT_SECONDS: u64 = 8;
/// Without a limit on the connections to Kick the file descriptor limit is raised to allow about
/// this many of them, which is more than anybody is going to need
const UNLIMITED_CONNECTIONS_ESTIMATE: usize = 60_000;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // The HTTP client and the websocket client both use rustls, which has to be told
    // which crypto provider to use. Installing it more than once is harmless.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let use_ansi = env::var("RUST_LOG_ANSI")
        .ok()
        .and_then(|ansi| ansi.parse().ok())
        .unwrap_or(true);
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(use_ansi)
        .init();

    let args = Args::parse();

    let config = Config::load(&args.config_path)?;
    let mut db = clickhouse::Client::default()
        .with_url(&config.clickhouse_url)
        .with_database(&config.clickhouse_db)
        .with_compression(clickhouse::Compression::None);

    if let Some(user) = &config.clickhouse_username {
        db = db.with_user(user);
    }

    if let Some(password) = &config.clickhouse_password {
        db = db.with_password(password);
    }

    setup_db(&db, &config.clickhouse_db)
        .await
        .context("Could not run DB migrations")?;

    match args.subcommand {
        None => run(config, db).await,
        Some(Command::Migrate {
            source_dir,
            channel_id,
            jobs,
        }) => migrate(db, source_dir, channel_id, jobs).await,
    }
}

async fn run(config: Config, db: clickhouse::Client) -> anyhow::Result<()> {
    // Every connection to Kick needs a file descriptor
    let connections_needed = config
        .channels
        .read()
        .unwrap()
        .len()
        .div_ceil(config.pusher_max_channels_per_connection.max(1));
    let connections_to_prepare_for = config
        .pusher_connection_limit()
        .unwrap_or(UNLIMITED_CONNECTIONS_ESTIMATE)
        .max(connections_needed);
    raise_file_descriptor_limit(connections_to_prepare_for, connections_needed);

    let mut shutdown_rx = listen_shutdown().await;

    let kick = KickApi::new(config.client_id.clone(), config.client_secret.clone())?;
    kick.authenticate()
        .await
        .context("Could not get a Kick app access token, check clientID and clientSecret")?;

    let (writer_tx, flush_buffer, mut writer_handle) = create_writer(
        db.clone(),
        shutdown_rx.clone(),
        config.clickhouse_flush_interval,
    )
    .await?;

    let app = App {
        kick: Arc::new(kick),
        users: UsersCache::default(),
        config: Arc::new(config),
        db: Arc::new(db),
        optout_codes: Arc::default(),
        flush_buffer,
    };

    // Joining many channels takes a while (every unknown chatroom is looked up on Kick's
    // website), so requests to join or leave channels are queued
    let (bot_tx, bot_rx) = mpsc::channel(100);

    let mut bot_handle = tokio::spawn(bot::run(
        app.clone(),
        writer_tx,
        shutdown_rx.clone(),
        bot_rx,
    ));
    let mut web_handle = tokio::spawn(web::run(app, shutdown_rx.clone(), bot_tx));

    tokio::select! {
        _ = shutdown_rx.changed() => {
            debug!("Waiting for tasks to shut down");

            let started_at = Instant::now();

            let shutdown_future = try_join_all([bot_handle, web_handle, writer_handle]);
            match timeout(Duration::from_secs(SHUTDOWN_TIMEOUT_SECONDS), shutdown_future).await {
                Ok(Ok(_)) => {
                    debug!("Cleanup finished in {}ms", started_at.elapsed().as_millis());
                    Ok(())
                }
                Ok(Err(err)) => Err(anyhow!("Could not shut down properly: {err}")),
                Err(_) => {
                    Err(anyhow!("Tasks did not shut down after {} seconds", SHUTDOWN_TIMEOUT_SECONDS))
                }
            }

        }
        _ = &mut bot_handle => {
            Err(anyhow!("Bot task exited unexpectedly"))
        }
        _ = &mut web_handle => {
            Err(anyhow!("Web task exited unexpectedly"))
        }
        _ = &mut writer_handle => {
            Err(anyhow!("Writer task exited unexpectedly"))
        }
    }
}

async fn migrate(
    db: clickhouse::Client,
    source_logs_path: String,
    channel_ids: Vec<String>,
    jobs: usize,
) -> anyhow::Result<()> {
    let migrator = Migrator::new(db, source_logs_path, channel_ids).await?;
    migrator.run(jobs).await
}

async fn listen_shutdown() -> watch::Receiver<()> {
    let shutdown_signal = shutdown_signal();
    let (tx, rx) = watch::channel(());

    tokio::spawn(async move {
        shutdown_signal.await;
        info!("Received shutdown signal");
        let _ = tx.send(());
    });

    rx
}

/// Completes when the process is asked to stop (SIGINT or SIGTERM)
#[cfg(unix)]
fn shutdown_signal() -> impl Future<Output = ()> {
    use tokio::signal::unix::{signal, SignalKind};

    // The handlers are registered right away, not only when the future is first polled
    let mut interrupt = signal(SignalKind::interrupt()).expect("Could not listen for SIGINT");
    let mut terminate = signal(SignalKind::terminate()).expect("Could not listen for SIGTERM");

    async move {
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
    }
}

/// Completes when the process is asked to stop (Ctrl+C)
#[cfg(not(unix))]
fn shutdown_signal() -> impl Future<Output = ()> {
    async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            // Failing to listen is not a request to stop, so wait forever instead
            tracing::error!("Could not listen for Ctrl+C: {err}");
            std::future::pending::<()>().await;
        }
    }
}

/// Every websocket connection to Kick needs a file descriptor, and the default limit
/// (1024 on many Linux systems) is lower than the connections needed for a very large
/// number of channels. The limit is raised as far as the system allows, up to what is needed
/// for `prepare_for` connections. A warning is logged if it is not enough for the `needed`
/// connections the configured channels require.
#[cfg(unix)]
fn raise_file_descriptor_limit(prepare_for: usize, needed: usize) {
    // Descriptors for the web server, database connections, HTTP requests, files...
    const RESERVED: u64 = 256;
    const MINIMUM_RESERVED: u64 = 64;

    let wanted = prepare_for as u64 + RESERVED;
    let required = needed as u64 + MINIMUM_RESERVED;

    match rlimit::increase_nofile_limit(wanted) {
        Ok(limit) if limit >= required => debug!("The limit of open files is {limit}"),
        Ok(limit) => tracing::warn!(
            "The limit of open files is {limit}, which is not enough for the {needed} connections \
            to Kick the configured channels need. Raise it (for example with `ulimit -n {required}`) \
            or put more channels on one connection (pusherMaxChannelsPerConnection)."
        ),
        Err(err) => tracing::warn!("Could not raise the limit of open files: {err}"),
    }
}

/// There is no such limit to raise
#[cfg(not(unix))]
fn raise_file_descriptor_limit(_prepare_for: usize, _needed: usize) {}
