use std::time::Duration;

use anyhow::Result;
use tracing::debug;
use uuid::Uuid;

use crate::cli::{Options, PingCommand, PingCreateOptions, PingOptions};
use crate::db;
use crate::db::ping::PingMonitor;

pub async fn execute_command(options: &Options, ping_options: &PingOptions) -> Result<()>
{
    match &ping_options.command {
        PingCommand::Create(create_options) => create(options, create_options).await?,
        PingCommand::List => list(options).await?,
    }
    Ok(())
}

pub async fn create(options: &Options, create_options: &PingCreateOptions) -> Result<()>
{
    let ping = PingMonitor {
        token: Uuid::new_v4().to_string(),
        name: create_options.name.clone(),
        period: create_options.period.into(),
        grace: create_options.grace.map(Into::into).unwrap_or(Duration::ZERO),
    };

    let conn = db::connect(&options.db).await?;
    let _id = db::ping::insert(&conn, &ping).await?;

    println!("Added ping monitor:");
    println!("    token : {}", &ping.token);
    println!("    name  : {}", &ping.name);
    println!("    period: {}", humantime::Duration::from(ping.period));
    println!("    grace : {}", humantime::Duration::from(ping.grace));

    Ok(())
}

async fn list(options: &Options) -> Result<()>
{
    let conn = db::connect(&options.db).await?;
    let pings = db::ping::get_all(&conn).await?;

    debug!(?pings);
    todo!()  // TODO: display the list as ASCII table (id/token/name/period/grace). could also have a --csv flag
}
