use clap::{Parser, Subcommand};
use humantime::Duration;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Options {
    /// Database connection string.
    /// See https://docs.rs/tokio-postgres/latest/tokio_postgres/config/struct.Config.html
    #[arg(long)]
    pub db: String,

    #[command(subcommand)]
    pub command: Command,
}

impl Options {
    /// Parse CLI options. Panic on failure.
    pub fn parse() -> Self
    {
        <Self as Parser>::parse()
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Commands related to ping monitors.
    Ping(PingOptions),
    /// Run the scree server (service monitoring, alerts, dashboard).
    Run(RunOptions),
}

#[derive(Parser, Debug)]
pub struct PingOptions {
    #[command(subcommand)]
    pub command: PingCommand,
}

#[derive(Subcommand, Debug)]
pub enum PingCommand {
    /// Create a new ping monitor.
    Create(PingCreateOptions),
    /// List the registered ping monitors.
    List,
}

#[derive(Parser, Debug)]
pub struct PingCreateOptions {
    /// Name of the new ping monitor.
    pub name: String,
    /// Expected time between pings.
    pub period: Duration,
    /// Grace period: when a deadline is missed, the monitor will be considered
    /// in a warning state for this amount of time, and will enter an error
    /// state only after the grace period expires as well.
    pub grace: Option<Duration>,
}

#[derive(Parser, Debug)]
pub struct RunOptions {
}
