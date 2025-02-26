use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio_postgres::Row;
use tracing::debug;

use super::util::WithId;

type Id = i32;

#[derive(Debug)]
pub struct PingMonitor {
    /// Token used in ping URL, unique identifier.
    pub token: String,
    /// Name of the ping to display on dashboard and in reports.
    pub name: String,
    /// Expected amount of time between pings.
    pub period: Duration,
    /// Amount of time after a missed deadline before this item is considered to be in error state.
    pub grace: Duration,
}

impl TryFrom<&Row> for PingMonitor {
    type Error = anyhow::Error;

    fn try_from(row: &Row) -> std::result::Result<Self, Self::Error>
    {
        let token: String = row.try_get("token")?;
        let name: String = row.try_get("name")?;
        let period_s: i32 = row.try_get("period_s")?;
        let period = Duration::from_secs(period_s.try_into()?);
        let grace_s: i32 = row.try_get("grace_s")?;
        let grace = Duration::from_secs(grace_s.try_into()?);
        Ok(PingMonitor { token, name, period, grace })
    }
}

pub async fn get_all(conn: &super::Connection) -> Result<Vec<WithId<PingMonitor>>>
{
    let rows = conn.client.query(/*sql*/ r"
        SELECT id, token, name, period_s, grace_s
        FROM ping_monitors
        ORDER BY name ASC
    ", &[]).await?;
    let pings = rows.iter()
        .map(WithId::<PingMonitor>::try_from)
        .collect::<Result<_>>()?;
    Ok(pings)
}

pub async fn insert(conn: &super::Connection, ping: &PingMonitor) -> Result<Id>
{
    let period_s: i32 = ping.period.as_secs().try_into().context("period")?;
    let grace_s: i32 = ping.grace.as_secs().try_into().context("grace")?;

    if period_s <= 0 {
        bail!("period must be greater than 0");
    }

    assert!(grace_s >= 0);

    let row = conn.client.query_one(/*sql*/ r"
        INSERT INTO ping_monitors (token, name, period_s, grace_s)
        VALUES ($1, $2, $3, $4)
        RETURNING id
    ", &[ &ping.token, &ping.name, &period_s, &grace_s ]).await?;

    let id = row.try_get(0)?;
    debug!("new id: {id}");

    Ok(id)
}
