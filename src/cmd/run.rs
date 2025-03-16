use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use deadpool_postgres::{ClientWrapper, Pool};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_postgres::{GenericClient, Notification};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::cli::{Options, RunOptions};
use crate::db::alert::Alert;
use crate::db::ping::PingMonitorExt;
use crate::db::{self, Connection, Database};
use reload::{ReloadTask, ReloadToken};

mod http;
mod reload;
mod report;

#[derive(Debug, Clone)]
struct App {
    inner: Arc<AppData>,
}

impl Deref for App {
    type Target = AppData;

    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

impl From<AppData> for App {
    fn from(inner: AppData) -> Self {
        Self { inner: Arc::new(inner) }
    }
}

#[derive(Debug)]
struct AppData {
    db_pool: Pool,
    min_reload_interval: Duration,
    deadlines_updated_tx: UnboundedSender<()>,
    reload: ReloadToken,
    shutdown_token: CancellationToken,
    started_at: DateTime<Utc>,
    state: Arc<Mutex<AppState>>,
}

impl AppData {
    pub fn new(db_pool: Pool, deadlines_updated_tx: UnboundedSender<()>, reload: ReloadToken, shutdown_token: CancellationToken) -> Self
    {
        Self {
            db_pool,
            min_reload_interval: Duration::from_secs(5),
            deadlines_updated_tx,
            reload,
            shutdown_token,
            started_at: Utc::now(),
            state: Arc::new(Mutex::new(AppState::new())),
        }
    }

    pub fn notify_deadlines_updated(&self)
    {
        // ignore errors due to closed channel
        let _ = self.deadlines_updated_tx.send(());
    }

    pub fn reload_ping_monitors(&self)
    {
        self.reload.reload_ping_monitors()
    }
}

#[derive(Debug)]
struct AppState {
    last_reload: Instant,
    ping_monitors: PingMonitors,
}

impl AppState {
    fn new() -> Self
    {
        Self {
            last_reload: Instant::now(),
            ping_monitors: PingMonitors::new(vec![]),
        }
    }
}

#[derive(Debug)]
struct PingMonitors {
    all: Vec<PingMonitorExt>,
    by_id: HashMap<i32, usize>,
}

impl PingMonitors {
    fn new(pms: Vec<PingMonitorExt>) -> Self
    {
        let by_id = pms.iter()
            .enumerate()
            .map(|(idx, pm)| (pm.id, idx))
            .collect();

        Self {
            all: pms,
            by_id,
        }
    }
}


pub async fn main(options: &Options, run_options: &RunOptions) -> Result<()>
{
    let shutdown_token = CancellationToken::new();

    setup_signal_handling(shutdown_token.clone())?;
    setup_panic_hook(shutdown_token.clone());

    let db = Database::new(&options.db)?;
    // Long-running database connection is used to respond to database notifications
    let mut conn = db.connect().await?;
    // Connection pool is used for individual requests
    let pool = db.connect_pool().await?;

    let (deadlines_updated_tx, deadlines_updated_rx) = mpsc::unbounded_channel();
    let reload_task = ReloadTask::new();

    let app: App = AppData::new(pool, deadlines_updated_tx, reload_task.token(), shutdown_token.clone()).into();

    // lock the state mutex while sub-tasks are starting up (controlled startup while initial data is loaded)
    let mut state = app.state.lock().await;

    let nf_handle = setup_db_notifications(&mut conn, app.clone()).await?;
    let reload_handle = reload_task.spawn(app.clone());

    let watch_handle = tokio::spawn(
        watch_deadlines(deadlines_updated_rx, app.clone())
    );

    let http_handle = tokio::spawn(
        http::run_server(run_options.listen_addr, app.clone())
    );

    // load initial data
    reload::reload_ping_monitors(&app, &mut *state).await?;
    // release state mutex to unblock sub-tasks
    drop(state);

    info!("startup complete");

    // wait until shutdown signal
    shutdown_token.cancelled().await;

    // collect sub-tasks
    http_handle.await??;
    watch_handle.await?;
    reload_handle.await?;
    conn.close().await?;
    nf_handle.await?;  // stops after conn.close()

    Ok(())
}

fn setup_signal_handling(shutdown_token: CancellationToken) -> Result<()>
{
    crate::util::signal::install_signal_handlers(move || {
        shutdown_token.cancel();
    })?;
    Ok(())
}

fn setup_panic_hook(shutdown_token: CancellationToken)
{
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        error!("panic: {info}");
        shutdown_token.cancel();
    }));
}

async fn setup_db_notifications(conn: &mut Connection, app: App) -> Result<JoinHandle<()>>
{
    let rx = conn.take_notification_rx().expect("notification receiver is available");
    let handle = tokio::spawn(poll_db_notifications(app, rx));

    conn.client.batch_execute(/*sql*/ r"
        LISTEN ping_monitors_changed;
    ").await?;

    Ok(handle)
}

async fn poll_db_notifications(app: App, mut rx: UnboundedReceiver<Notification>)
{
    while let Some(nf) = rx.recv().await {
        match nf.channel() {
            "ping_monitors_changed" => app.reload_ping_monitors(),
            _ => warn!("unhandled notification: {:?}", nf),
        }
    }
    // notifications channel has closed
    debug!("poll_db_notifications stopped");
}

async fn watch_deadlines(mut deadlines_updated_rx: UnboundedReceiver<()>, app: App)
{
    loop {

        while let Ok(()) = deadlines_updated_rx.try_recv() {
            /* drain the update requests channel in case multiple requests have piled up (TODO: maybe a different channel type would fit better, e.g., BoundedChannel with bound 1; or even a Notify, but that seems more error-prone) */
        }

        let next_deadline = get_next_deadline(&app).await;
        let now = Utc::now();
        // if the deadline is already expired, wake up immediately
        let delta = (next_deadline - now).to_std().unwrap_or(Duration::from_secs(0));

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
