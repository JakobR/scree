use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
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
    let period = create_options.period.into();
    let grace = create_options.grace.map(Into::into).unwrap_or_else(|| period / 10);

    let ping = PingMonitor {
        token: Uuid::new_v4().to_string(),
        name: create_options.name.clone(),
        period,
        grace,
        created_at: Utc::now(),
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
    let pings = db::ping::get_all_with_stats(&conn.client).await?;

    use comfy_table::{Cell, CellAlignment, Table};

    let mut table = Table::new();
    table.load_preset(comfy_table::presets::NOTHING);

    table.set_header(["ID", "TOKEN", "NAME", "PERIOD", "GRACE", "CREATED", "PINGS", "LAST_PING", "STATE", "STATE_SINCE"]);

    let now = chrono::Utc::now();

    for (pm, stats) in pings {
        let last_ping_str =
            if let Some(last_ping_at) = stats.last_ping_at {
                format!("{} ({})", last_ping_at.format("%F %T %:z").to_string(), format_last_ping_delta(now, last_ping_at))
            } else {
                "".to_string()
            };

        table.add_row([
            Cell::new(pm.id).set_alignment(CellAlignment::Right),
            Cell::new(&pm.token),
            Cell::new(&pm.name),
            Cell::new(humantime::Duration::from(pm.period)).set_alignment(CellAlignment::Right),
            Cell::new(humantime::Duration::from(pm.grace)).set_alignment(CellAlignment::Right),
            Cell::new(pm.created_at.format("%F %T %:z")),
            Cell::new(stats.num_pings).set_alignment(CellAlignment::Right),
            Cell::new(last_ping_str),
            Cell::new(stats.state),  // TODO: show "Late" when in grace period
            Cell::new(stats.state_since.format("%F %T %:z")),
        ]);
    }

    println!("{}", table);

    Ok(())
}

fn format_last_ping_delta(now: DateTime<Utc>, last_ping_at: DateTime<Utc>) -> String
{
    if now < last_ping_at {
        return "error: in the future".to_string();
    }

    match (now - last_ping_at).to_std() {
        Ok(d) => {
            let d = Duration::from_secs(d.as_secs());  // truncate sub-seconds
            format!("{} ago", humantime::Duration::from(d))
        }
        Err(_e) => "error: out of range".to_string(),
    }
}
