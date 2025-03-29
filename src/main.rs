use anyhow::Result;
use cli::{Command, Options};
use tracing::debug;
use tracing_subscriber::EnvFilter;

mod cli;
mod db;

mod cmd {
    pub mod alert;
    pub mod migrate;
    pub mod ping;
    pub mod run;
}

mod util {
    pub mod into_result;
    pub mod signal;
}

#[tokio::main]
async fn main() -> Result<()>
{
    setup_tracing();

    let options = read_options();

    match &options.command {
        Command::Ping(ping_options) =>
            cmd::ping::main(options, ping_options).await,
        Command::Alert(alert_options) =>
            cmd::alert::main(options, alert_options).await,
        Command::Run(run_options) =>
            cmd::run::main(options, run_options).await,
        Command::Migrate =>
            cmd::migrate::main(options).await,
    }
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

fn read_options() -> &'static Options
{
    let options = Options::parse();
    debug!(?options);
    Box::leak(Box::new(options))
}
