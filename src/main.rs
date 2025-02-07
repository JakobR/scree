use std::time::Duration;

use anyhow::Result;
use tracing::info;
use tracing_subscriber::EnvFilter;


#[tokio::main]
async fn main() -> Result<()>
{
    setup_tracing();

    info!("Hello, world!");

    let mut cfg = scree::Config::new();
    cfg.ping("f43a7112-2a54-4562-8fde-29e27cdf6c02", "server1/backup", Duration::from_secs(3600), Duration::from_secs(600));
    scree::run(cfg).await?;

    Ok(())
}


fn setup_tracing()
{
    let format = tracing_subscriber::fmt::format()
        .with_thread_ids(true);
        // .pretty();
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))
        .unwrap();
    tracing_subscriber::fmt()
        .event_format(format)
        .with_env_filter(filter)
        .init();
}
