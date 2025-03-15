use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use postgres_types::{ToSql, FromSql};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ToSql, FromSql)]
#[postgres(name = "monitor_state", rename_all = "snake_case")]
pub enum MonitorState {
    Ok,
    // NOTE: "Warning" is not a true state we should track, since it does not trigger any events.
    //       Its purpose is simply to prevent spurious error alerts due to jitter in ping arrival times.
    //       If we were to record "Warning" states, we would end up with a lot of noise when warning is entered for a few seconds at a time.
    //       "Warning" should be treated as a sub-state of Ok that is merely distinguished visually in the dashboard.
    //       Instead of "Warning", better call this state "Late"?
    // Warning,
    Failed,
}

impl ToString for MonitorState {
    fn to_string(&self) -> String {
        match self {
            MonitorState::Ok => "Ok".to_string(),
            // MonitorState::Warning => "Warning".to_string(),
            MonitorState::Failed => "Failed".to_string(),
        }
    }
}

#[derive(Debug)]
pub struct Stats {
    pub num_pings: i64,
    pub last_ping_at: Option<DateTime<Utc>>,
    pub state: MonitorState,
    pub state_since: DateTime<Utc>,
}

impl TryFrom<&Row> for Stats {
    type Error = anyhow::Error;

    fn try_from(row: &Row) -> std::result::Result<Self, Self::Error>
    {
        let num_pings = row.try_get("num_pings")?;
        let last_ping_at = row.try_get("last_ping_at")?;
        let state = row.try_get("state")?;
        let state_since = row.try_get("state_since")?;
        Ok(Stats { num_pings, last_ping_at, state, state_since })
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
            ),
            ranked_state_history AS (
                SELECT
                    id, monitor_id, state, state_since,
                    ROW_NUMBER() OVER (PARTITION BY monitor_id ORDER BY state_since DESC) AS rnk
                FROM ping_state_history
            )
        SELECT
            pm.id AS id,
            pm.token AS token,
            pm.name AS name,
            pm.period_s AS period_s,
            pm.grace_s AS grace_s,
            pm.created_at AS created_at,
            COALESCE(re.count, 0) AS num_pings,
            re.occurred_at AS last_ping_at,
            -- if state/state_since are NULL, it means it is in state 'ok' since pm.created_at
            COALESCE(rsh.state, 'ok') AS state,
            COALESCE(rsh.state_since, pm.created_at) AS state_since
        FROM ping_monitors AS pm
        LEFT OUTER JOIN ranked_events AS re ON (pm.id = re.monitor_id AND re.rnk = 1)
        LEFT OUTER JOIN ranked_state_history AS rsh ON (pm.id = rsh.monitor_id AND rsh.rnk = 1)
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

pub async fn record_state_change(db: &impl GenericClient, pm_id: Id, state: MonitorState, state_since: DateTime<Utc>) -> Result<()>
{
    db.execute(/* sql */ r"
        INSERT INTO ping_state_history (monitor_id, state, state_since) VALUES ($1, $2, $3)
    ", &[&pm_id, &state, &state_since]).await?;

    Ok(())
}

pub async fn record_ping(db: &impl GenericClient, pm_id: Id, occurred_at: DateTime<Utc>) -> Result<()>
{
    db.execute(/* sql */ r"
        INSERT INTO ping_events (monitor_id, occurred_at) VALUES ($1, $2)
    ", &[&pm_id, &occurred_at]).await?;

    Ok(())
}

pub async fn find_by_token(db: &impl GenericClient, token: &str) -> Result<Option<Id>>
{
    let row_opt = db.query_opt(/* sql */ r"
        SELECT id FROM ping_monitors WHERE token = $1
    ", &[&token]).await?;
    let id_opt = row_opt.map_or(Ok(None), |row| row.try_get(0))?;
    Ok(id_opt)
}
