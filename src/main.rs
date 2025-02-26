use std::time::Duration;

use anyhow::Result;
use cli::{Command, Options, RunOptions};
use tracing::debug;
use tracing_subscriber::EnvFilter;

mod cli;
mod db;
mod ping;

#[tokio::main]
async fn main() -> Result<()>
{
    setup_tracing();

    let options = Options::parse();
    debug!(?options);

    match &options.command {
        Command::Ping(ping_options) =>
            ping::execute_command(&options, &ping_options).await,
        Command::Run(run_options) =>
            run(&options, &run_options).await,
    }
}

async fn run(options: &Options, run_options: &RunOptions) -> Result<()>
{
    let db = db::Database::new(&options.db)?;
    let mut conn = db.connect().await?;
    let _nf_rx = conn.take_notification_rx().expect("notification receiver is available");

    conn.client.batch_execute(r"
        LISTEN ping_monitors_changed;
        LISTEN my_channel;
        NOTIFY my_channel, 'hello!';
        NOTIFY my_channel, 'good bye!';
    ").await?;

    // TODO: on a db change notification (ping_monitors_changed):
    // - reload monitors immediately, unless reload happened previously within the last 5 seconds (basic rate limit).
    // - for now, just reload all the monitors instead of keeping track of fine-grained changes.
    // - take care to update state atomically, or take some kind of lock, to make sure we do not lose any updates while reloading
    //   (it is fine to miss updates for newly added items until the first reload happens)

    let mut cfg = scree::Config::new();
    cfg.ping("f43a7112-2a54-4562-8fde-29e27cdf6c02", "server1/backup", Duration::from_secs(3600), Duration::from_secs(600));
    scree::run(cfg).await?;

    let _ = run_options;

    conn.close().await?;

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
