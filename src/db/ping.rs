use std::ops::Deref;
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

impl PingMonitor {

    #[allow(unused)]  // TODO: remove
    pub async fn get_all(db: &impl GenericClient) -> Result<Vec<WithId<PingMonitor>>>
    {
        let rows = db.query(/*sql*/ r"
            SELECT id, token, name, period_s, grace_s
            FROM ping_monitors
            ORDER BY name ASC
        ", &[]).await?;
        let pings = rows.iter()
            .map(WithId::<PingMonitor>::try_from)
            .collect::<Result<_>>()?;
        Ok(pings)
    }

    pub async fn insert(&self, db: &impl GenericClient) -> Result<Id>
    {
        let period_s: i32 = self.period.as_secs().try_into().context("period")?;
        let grace_s: i32 = self.grace.as_secs().try_into().context("grace")?;

        if period_s <= 0 {
            bail!("period must be at least 1 second");
        }

        assert!(grace_s >= 0);

        let row = db.query_one(/*sql*/ r"
            INSERT INTO ping_monitors (token, name, period_s, grace_s, created_at)
            VALUES ($1, $2, $3, $4, $5)
            RETURNING id
        ", &[ &self.token, &self.name, &period_s, &grace_s, &self.created_at ]).await?;

        let id = row.try_get(0)?;
        debug!("new id: {id}");

        Ok(id)
    }

    pub async fn find_by_token(db: &impl GenericClient, token: &str) -> Result<Option<Id>>
    {
        let row_opt = db.query_opt(/* sql */ r"
            SELECT id FROM ping_monitors WHERE token = $1
        ", &[&token]).await?;
        let id_opt = row_opt.map_or(Ok(None), |row| row.try_get(0))?;
        Ok(id_opt)
    }

}


#[derive(Debug, Clone, Copy, PartialEq, Eq, ToSql, FromSql)]
#[postgres(name = "monitor_state", rename_all = "snake_case")]
pub enum MonitorState {
    Ok,
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


#[derive(Debug, Clone, Copy, PartialEq, Eq, ToSql, FromSql)]
#[postgres(name = "ping_monitor_event", rename_all = "snake_case")]
pub enum PingMonitorEvent {
    Ping,
    DeadlineExpired,
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


#[derive(Debug)]
pub struct PingMonitorExt {
    inner: WithId<PingMonitor>,
    stats: Stats,
    deadlines: Deadlines,
}

impl PingMonitorExt {
    fn new(pm: WithId<PingMonitor>, stats: Stats, now: DateTime<Utc>) -> Self
    {
        let deadlines = Deadlines::compute(&pm, stats.last_ping_at, now);
        Self {
            inner: pm,
            stats,
            deadlines,
        }
    }

    async fn update_state(&mut self, db: &impl GenericClient, new_state: MonitorState, state_since: DateTime<Utc>) -> Result<()>
    {
        record_state_change(db, self.id, new_state, state_since).await
            .context("recording state change in database")?;
        self.stats.state = new_state;
        self.stats.state_since = state_since;
        Ok(())
    }

    fn update_deadlines(&mut self, now: DateTime<Utc>)
    {
        self.deadlines = Deadlines::compute(&self, self.stats.last_ping_at, now);
    }

    pub async fn event_ping(&mut self, db: &impl GenericClient, now: DateTime<Utc>) -> Result<()>
    {
        record_ping(db, self.id, now).await?;
        self.update_state(db, MonitorState::Ok, now).await?;
        self.stats.num_pings += 1;
        self.stats.last_ping_at = Some(now);
        self.update_deadlines(now);
        Ok(())
    }

    pub async fn event_deadline_expired(&mut self, db: &impl GenericClient, expired_at: DateTime<Utc>) -> Result<()>
    {
        self.update_state(db, MonitorState::Failed, expired_at).await?;
        Ok(())
    }

    pub fn is_ok(&self) -> bool
    {
        self.stats.state == MonitorState::Ok
    }

    // true if ping is late but still within the grace period (the state is still "ok")
    pub fn is_late(&self, now: DateTime<Utc>) -> bool
    {
        self.is_ok() && self.deadlines.warn_at <= now
    }

    pub fn is_failed(&self) -> bool
    {
        self.stats.state == MonitorState::Failed
    }

    pub fn stats(&self) -> &Stats
    {
        &self.stats
    }

    // next failure deadline
    pub fn deadline(&self) -> Option<DateTime<Utc>>
    {
        match self.stats.state {
            MonitorState::Ok =>
                Some(self.deadlines.fail_at),
            MonitorState::Failed =>
                // already failed, no deadline
                None,
        }
    }

    pub fn is_deadline_expired(&self, now: DateTime<Utc>) -> bool
    {
        match self.deadline() {
            Some(deadline) => deadline <= now,
            None => false,
        }
    }

    pub async fn get_all(db: &impl GenericClient, now: DateTime<Utc>) -> Result<Vec<PingMonitorExt>>
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
                Ok(Self::new(pm, stats, now))
            })
            .collect::<Result<_>>()?;
        Ok(pings)
    }

}

impl Deref for PingMonitorExt {
    type Target = WithId<PingMonitor>;

    fn deref(&self) -> &Self::Target
    {
        &self.inner
    }
}

#[derive(Debug)]
pub struct Deadlines {
    warn_at: DateTime<Utc>,
    fail_at: DateTime<Utc>,
}

impl Deadlines {
    pub fn compute(pm: &PingMonitor, last_ping_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Self
    {
        // If no ping has been recorded yet, we use the monitor's creation timestamp as base.
        // If the last ping is in the future (if the system clock changed), then use the current time instead.
        // (We could use the monotonic clock (std::time::Instant) to track deadlines instead. The question remains how to convert to/from database timestamps.)
        let last_ping_at = last_ping_at.unwrap_or(pm.created_at).min(now);

        let warn_at = last_ping_at + pm.period;
        let fail_at = warn_at + pm.grace;

        assert!(last_ping_at < warn_at);
        assert!(warn_at <= fail_at);

        Self { warn_at, fail_at }
    }
}


async fn record_state_change(db: &impl GenericClient, pm_id: Id, state: MonitorState, state_since: DateTime<Utc>) -> Result<()>
{
    db.execute(/* sql */ r"
        INSERT INTO ping_state_history (monitor_id, state, state_since) VALUES ($1, $2, $3)
    ", &[&pm_id, &state, &state_since]).await?;

    Ok(())
}

async fn record_ping(db: &impl GenericClient, pm_id: Id, occurred_at: DateTime<Utc>) -> Result<()>
{
    db.execute(/* sql */ r"
        INSERT INTO ping_events (monitor_id, occurred_at) VALUES ($1, $2)
    ", &[&pm_id, &occurred_at]).await?;

    Ok(())
}
