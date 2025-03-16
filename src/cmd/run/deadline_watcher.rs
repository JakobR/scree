use std::ops::DerefMut;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use deadpool_postgres::ClientWrapper;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tokio_postgres::GenericClient;
use tracing::{error, info};

use crate::db::alert::Alert;
use crate::db::ping::PingMonitorExt;
use crate::db;
use super::App;


#[derive(Debug)]
pub struct DeadlineWatchToken {
    deadlines_updated_tx: UnboundedSender<()>,
}

impl DeadlineWatchToken {
    pub fn notify_deadlines_updated(&self)
    {
        // ignore errors due to closed channel
        let _ = self.deadlines_updated_tx.send(());
    }
}


#[derive(Debug)]
pub struct DeadlineWatchTask {
    deadlines_updated_tx: UnboundedSender<()>,
    deadlines_updated_rx: UnboundedReceiver<()>,
}

impl DeadlineWatchTask {
    pub fn new() -> Self
    {
        let (deadlines_updated_tx, deadlines_updated_rx) = mpsc::unbounded_channel();
        Self { deadlines_updated_tx, deadlines_updated_rx }
    }

    pub fn token(&self) -> DeadlineWatchToken
    {
        DeadlineWatchToken { deadlines_updated_tx: self.deadlines_updated_tx.clone() }
    }

    pub fn spawn(self, app: App) -> JoinHandle<()>
    {
        tokio::spawn(watch_deadlines(self.deadlines_updated_rx, app))
    }
}


async fn watch_deadlines(mut deadlines_updated_rx: UnboundedReceiver<()>, app: App)
{
    loop {

        while let Ok(()) = deadlines_updated_rx.try_recv() {
            /* drain the update requests channel in case multiple requests have piled up (TODO: maybe a different channel type would fit better, e.g., BoundedChannel with bound 1; or even a Notify, but that seems more error-prone) */
        }

        let next_deadline = get_next_deadline(&app).await;
        // if the deadline is already expired, wake up immediately
        let delta = (next_deadline - Utc::now()).to_std().unwrap_or(Duration::from_secs(0));

        info!("Next deadline: {} (in {})", next_deadline, humantime::Duration::from(delta));

        tokio::select! {
            _ = app.shutdown_token.cancelled() => break,
            Some(()) = deadlines_updated_rx.recv() => continue,
            _ = tokio::time::sleep(delta) => (),
            else => break,
        };

        info!("Estimated deadline expired, checking now");

        if let Err(e) = check_deadlines(&app).await.context("checking deadlines") {
            error!("{:#}", e);
        }
    }
}

async fn get_next_deadline(app: &App) -> DateTime<Utc>
{
    let mut state_guard = app.state.lock().await;
    let state = &mut *state_guard;

    // max deadline: in a day
    let mut deadline = Utc::now() + chrono::TimeDelta::days(1);

    for pm in &state.ping_monitors.all {
        let Some(pm_deadline) = pm.deadline() else { continue };

        if pm_deadline < deadline {
            deadline = pm_deadline;
        }
    }

    deadline
}

// check monitors for state changes and send out alerts
async fn check_deadlines(app: &App) -> Result<()>
{
    let mut state_guard = app.state.lock().await;
    let state = &mut *state_guard;

    let mut db_guard = app.db_pool.get().await?;
    let db = &mut *db_guard;

    info!("checking deadlines");

    let now = Utc::now();

    for pm in &mut state.ping_monitors.all {
        if !pm.is_deadline_expired(now) {
            continue;
        }
        on_deadline_expired(db, pm, now).await;
    }

    Ok(())
}

async fn on_deadline_expired(db: &mut ClientWrapper, pm: &mut PingMonitorExt, expired_at: DateTime<Utc>)
{
    if pm.is_failed() {
        error!("state unchanged: pm={:?} expired_at={}", pm, expired_at);
        return;
    }

    if let Err(e) = pm.event_deadline_expired(db.client(), expired_at).await {
        error!("unable to update state of monitor {}: {:#}", pm.id, e);
    }

    if let Err(e) = send_failure_alert(db, pm, expired_at).await {
        error!("unable to send alert for monitor {}: {:#}", pm.id, e);
    }
}

async fn send_failure_alert(db: &mut ClientWrapper, pm: &PingMonitorExt, occurred_at: DateTime<Utc>) -> Result<()>
{
    let subject =
        format!("Failed: {}", pm.name);

    let message =
        match pm.stats().last_ping_at {
            Some(last_ping_at) =>
                format!("Ping monitor failed: {} (last ping was at {})", pm.name, last_ping_at.format("%F %T %:z")),
            None =>
                format!("Ping monitor failed: {} (never pinged)", pm.name),
        };

    let alert = Alert { subject, message, created_at: occurred_at };
    db::alert::send(db.deref_mut(), &alert).await;

    Ok(())
}
