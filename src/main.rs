use std::time::Duration;

use anyhow::{Context, Result};
use cli::{Command, Options, PingOptions, RunOptions};
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;

mod cli;
mod db;

#[tokio::main]
async fn main() -> Result<()>
{
    setup_tracing();

    let options = Options::parse();
    debug!(?options);

    match &options.command {
        Command::Ping(ping_options) =>
            ping(&options, &ping_options).await
            .context("ping"),
        Command::Run(run_options) =>
            run(&options, &run_options).await
            .context("run"),
    }
}

async fn ping(_options: &Options, ping_options: &PingOptions) -> Result<()>
{
    debug!(?ping_options);
    info!("Hello World!");
    todo!()
}

async fn run(options: &Options, run_options: &RunOptions) -> Result<()>
{
    debug!(?run_options);

    let _db = db::connect(&options.db).await?;

    let mut cfg = scree::Config::new();
    cfg.ping("f43a7112-2a54-4562-8fde-29e27cdf6c02", "server1/backup", Duration::from_secs(3600), Duration::from_secs(600));
    scree::run(cfg).await?;

    Ok(())
}

fn setup_tracing()
{
    let default_filter_str =
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "info"
        };
    let format = tracing_subscriber::fmt::format()
        .with_thread_ids(true);
        // .pretty();
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(default_filter_str))
        .unwrap();
    tracing_subscriber::fmt()
        .event_format(format)
        .with_env_filter(filter)
        .init();
}
