use clap::{Parser, Subcommand};

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
    /// Commands related to pings.
    Ping(PingOptions),
    /// Run the scree server (service monitoring, alerts, dashboard).
    Run(RunOptions),
}

#[derive(Parser, Debug)]
pub struct PingOptions {
    #[command(subcommand)]
    pub command: PingCommand,
}

#[derive(Parser, Debug)]
pub enum PingCommand {
    /// List the registered pings.
    List,
}

#[derive(Parser, Debug)]
pub struct RunOptions {
}
