use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result};
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
    /// Migrate database schema to the latest version.
    Migrate,
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

#[derive(Parser, Debug, Clone)]
pub struct RunOptions {
    /// The address on which the HTTP server should listen.
    /// Either an IP address and port for a regular socket, or an absolute path for a unix socket.
    #[arg(long = "listen", default_value = "127.0.0.1:3000")]
    pub listen_addr: SocketAddrOrPath,
    /// Obtain the client's remote IP address from the header "x-real-ip".
    /// Useful if connections are going through a reverse proxy.
    #[arg(long)]
    pub set_real_ip: bool,
    #[arg(long, value_parser = parse_file_mode, default_value = "700")]
    pub unix_socket_mode: u32,
}

#[derive(Debug, Clone)]
pub enum SocketAddrOrPath {
    Inet(SocketAddr),
    Unix(PathBuf),
}

impl FromStr for SocketAddrOrPath {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        if s.starts_with("/") {
            Ok(Self::Unix(s.parse()?))
        } else {
            Ok(Self::Inet(s.parse()?))
        }
    }
}

impl std::fmt::Display for SocketAddrOrPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SocketAddrOrPath::Inet(addr) => addr.fmt(f),
            SocketAddrOrPath::Unix(path) => write!(f, "{:?}", path),
        }
    }
}

fn parse_file_mode(s: &str) -> Result<u32>
{
    u32::from_str_radix(s, 8)
        .context("unable to parse as octal number")
}
