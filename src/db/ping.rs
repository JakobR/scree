use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use tokio_postgres::{GenericClient, Row};
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
    /// The timestamp when this ping monitor was created.
    pub created_at: DateTime<Utc>,
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
        let created_at = row.try_get("created_at")?;
        Ok(PingMonitor { token, name, period, grace, created_at })
    }
}

#[allow(unused)]  // TODO: remove
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

#[derive(Debug)]
pub struct Stats {
    pub num_pings: i64,
    pub last_ping_at: Option<DateTime<Utc>>,
}

impl TryFrom<&Row> for Stats {
    type Error = anyhow::Error;

    fn try_from(row: &Row) -> std::result::Result<Self, Self::Error>
    {
        let num_pings = row.try_get("num_pings")?;
        let last_ping_at = row.try_get("last_ping_at")?;
        Ok(Stats { num_pings, last_ping_at })
    }
}

pub async fn get_all_with_stats(db: &impl GenericClient) -> Result<Vec<(WithId<PingMonitor>, Stats)>>
{
    let rows = db.query(/*sql*/ r"
        WITH
            ranked_events AS (
                SELECT
                    id, monitor_id, occurred_at,
                    ROW_NUMBER() OVER (PARTITION BY monitor_id ORDER BY occurred_at DESC) AS rnk,
                    COUNT(id) OVER (PARTITION BY monitor_id) AS count
                FROM ping_events
            )
        SELECT
            pm.id AS id,
            pm.token AS token,
            pm.name AS name,
            pm.period_s AS period_s,
            pm.grace_s AS grace_s,
            pm.created_at AS created_at,
            COALESCE(re.count, 0) AS num_pings,
            re.occurred_at AS last_ping_at
        FROM ping_monitors AS pm
        LEFT OUTER JOIN ranked_events AS re ON (pm.id = re.monitor_id AND re.rnk = 1)
        ORDER BY name ASC
    ", &[]).await?;
    let pings = rows.iter()
        .map(|row| {
            let pm = WithId::<PingMonitor>::try_from(row)?;
            let stats = Stats::try_from(row)?;
            Ok((pm, stats))
        })
        .collect::<Result<_>>()?;
    Ok(pings)
}

pub async fn insert(conn: &super::Connection, ping: &PingMonitor) -> Result<Id>
{
    let period_s: i32 = ping.period.as_secs().try_into().context("period")?;
    let grace_s: i32 = ping.grace.as_secs().try_into().context("grace")?;

    if period_s <= 0 {
        bail!("period must be at least 1 second");
    }

    assert!(grace_s >= 0);

    let row = conn.client.query_one(/*sql*/ r"
        INSERT INTO ping_monitors (token, name, period_s, grace_s, created_at)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id
    ", &[ &ping.token, &ping.name, &period_s, &grace_s, &ping.created_at ]).await?;

    let id = row.try_get(0)?;
    debug!("new id: {id}");

    Ok(id)
}
