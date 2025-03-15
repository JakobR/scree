use std::net::SocketAddr;

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
    /// Commands to configure alerts.
    Alert(AlertOptions),
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
    /// in a "late" state for this amount of time, and will enter a failure
    /// state only after the grace period expires as well.
    /// A grace period of 0 will lead to spurious failure alerts.
    /// If no grace period is explicitly specified, it will be 10% of the given period.
    pub grace: Option<Duration>,
}

#[derive(Parser, Debug)]
pub struct AlertOptions {
    #[command(subcommand)]
    pub command: AlertCommand,
}

#[derive(Subcommand, Debug)]
pub enum AlertCommand {
    /// Manage email alerts.
    Email(EmailOptions),
    /// Manage Telegram alerts.
    Telegram(TelegramOptions),
    /// Send a test alert to all configured targets.
    Test,
}

#[derive(Parser, Debug)]
pub struct EmailOptions {
    #[command(subcommand)]
    pub command: TelegramCommand,
}

#[derive(Subcommand, Debug)]
pub enum EmailCommand {
    /// Configure email sending.
    Configure/* EmailConfigurationOptions: from address, smtp server, credentials, ...)*/,
    /// Enable email alerts.
    Enable/* EmailEnableOptions: target address */,
    /// Disable email alerts.
    Disable,
}

#[derive(Parser, Debug)]
pub struct TelegramOptions {
    #[command(subcommand)]
    pub command: TelegramCommand,
}

#[derive(Subcommand, Debug)]
pub enum TelegramCommand {
    /// Enable Telegram alerts.
    Enable(TelegramEnableOptions),
    /// Disable Telegram alerts.
    Disable,
}

#[derive(Parser, Debug)]
pub struct TelegramEnableOptions {
    /// Token of the bot that should send alerts.
    #[arg(long)]
    pub bot_token: String,
    /// Chat id where alerts are sent to.
    #[arg(long)]
    pub chat_id: String,
}

#[derive(Parser, Debug)]
pub struct RunOptions {
    #[arg(long = "listen", default_value_t = ([127, 0, 0, 1], 3000).into())]
    pub listen_addr: SocketAddr,
}
