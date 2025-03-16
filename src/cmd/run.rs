use std::collections::HashMap;
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;
use chrono::{DateTime, Utc};
use deadpool_postgres::{ClientWrapper, Pool};
use thiserror::Error;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_postgres::{GenericClient, Notification};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::cli::{Options, RunOptions};
use crate::db::alert::Alert;
use crate::db::ping::{PingMonitor, PingMonitorExt};
use crate::db::{self, Connection, Database};

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

#[derive(Debug, Clone)]
struct AppData {
    db_pool: Pool,
    min_reload_interval: Duration,
    deadlines_updated_tx: UnboundedSender<()>,
    reload_tx: UnboundedSender<ReloadRequest>,
    shutdown_token: CancellationToken,
    started_at: DateTime<Utc>,
    state: Arc<Mutex<AppState>>,
}

impl AppData {
    pub fn new(db_pool: Pool, deadlines_updated_tx: UnboundedSender<()>, reload_tx: UnboundedSender<ReloadRequest>, shutdown_token: CancellationToken) -> Self
    {
        Self {
            db_pool,
            min_reload_interval: Duration::from_secs(5),
            deadlines_updated_tx,
            reload_tx,
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
        // ignore errors due to closed channel
        let _ = self.reload_tx.send(ReloadRequest::new());
    }
}

struct ReloadRequest {
    timestamp: Instant,
}

impl ReloadRequest {
    fn new() -> Self {
        ReloadRequest { timestamp: Instant::now() }
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
    let (reload_tx, reload_rx) = mpsc::unbounded_channel();

    let app: App = AppData::new(pool, deadlines_updated_tx, reload_tx.clone(), shutdown_token.clone()).into();

    // lock the state mutex while sub-tasks are starting up (controlled startup while initial data is loaded)
    let mut state = app.state.lock().await;

    let nf_handle = setup_db_notifications(&mut conn, app.clone()).await?;
    let reload_handle = tokio::spawn(process_reload_requests(reload_rx, reload_tx, app.clone()));

    let watch_handle = tokio::spawn(
        watch_deadlines(deadlines_updated_rx, app.clone())
    );

    let server_handle = tokio::spawn(
        run_server(run_options.listen_addr, app.clone())
    );

    // load initial data
    reload_ping_monitors(&app, &mut *state).await?;
    // release state mutex to unblock sub-tasks
    drop(state);

    info!("startup complete");

    // wait until shutdown signal
    shutdown_token.cancelled().await;

    // collect sub-tasks
    server_handle.await??;
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

async fn process_reload_requests(mut reload_rx: UnboundedReceiver<ReloadRequest>, reload_tx: UnboundedSender<ReloadRequest>, app: App)
{
    // On a db change notification (ping_monitors_changed):
    // - reload monitors immediately, unless reload happened previously within the last 5 seconds (basic rate limit).
    // - for now, just reload all the monitors instead of keeping track of fine-grained changes.
    // - take care to update state atomically, or take some kind of lock, to make sure we do not lose any updates while reloading
    //   (it is fine to miss updates for newly added items until the first reload happens)
    loop {
        let req = tokio::select! {
            _ = app.shutdown_token.cancelled() => break,
            Some(req) = reload_rx.recv() => req,
            else => break,
        };

        let mut state_guard = app.state.lock().await;
        let state = &mut *state_guard;

        // drop outdated requests
        // (this may happen if multiple requests arrive before a reload is completed)
        if req.timestamp < state.last_reload {
            continue;
        }

        let now = Instant::now();
        let elapsed = now.duration_since(state.last_reload);

        if elapsed >= app.min_reload_interval {
            if let Err(e) = reload_ping_monitors(&app, state).await.context("reloading ping monitors") {
                error!("error: {:#}", e);
            }
            state.last_reload = now;
        }
        else {
            drop(state_guard);
            let remaining_time = app.min_reload_interval - elapsed;
            let reload_tx = reload_tx.clone();
            // send the request again later
            tokio::spawn(async move {
                tokio::time::sleep(remaining_time).await;
                let _ = reload_tx.send(req);
            });
        }
    }
    debug!("process_reload_requests stopped");
}

async fn reload_ping_monitors(app: &App, state: &mut AppState) -> Result<()>
{
    info!("(re-)loading ping monitors");
    let now = Utc::now();
    let db = app.db_pool.get().await?;

    // We have the lock on the AppState at this point,
    // so we don't have to worry about pings coming in while reloading.

    let pms = PingMonitorExt::get_all(db.client(), now).await?;

    state.ping_monitors = PingMonitors::new(pms);
    info!("loaded {} ping monitors", state.ping_monitors.all.len());

    app.notify_deadlines_updated();

    Ok(())
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

    if let Err(e) = send_alert(db, pm, expired_at).await {
        error!("unable to send alert for monitor {}: {:#}", pm.id, e);
    }
}

async fn send_alert(db: &mut ClientWrapper, pm: &PingMonitorExt, occurred_at: DateTime<Utc>) -> Result<()>
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


async fn run_server(listen_addr: SocketAddr, app: App) -> Result<()>
{
    let router = Router::new()
        .route("/", get(http_dashboard))
        .route("/ping/{token}", get(http_ping))
        .with_state(app.clone());

    tracing::debug!("listening on {}", listen_addr);
    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(app.shutdown_token.clone().cancelled_owned())
        .await?;

    Ok(())
}

async fn http_dashboard(app: State<App>) -> &'static str
{
    // TODO:
    // dashboard should clone the data behind the mutex and then release it
    // then also check with database if there are any new monitors that are missing from the local cache
    // (we'd want to know, and display a warning on such "inactive" monitors.)
    // TODO: also display the stats (like 'ping list' command). could even display the list of previous pings if you click on one.
    let _ = app.started_at;  // display this in the footer
    "Hello World!"
}

async fn http_ping(app: State<App>, Path(token): Path<String>) -> Result<String, StatusCode>
{
    debug!(?token);

    match handle_ping(&app, &token).await {
        Ok(()) => Ok("OK".to_string()),
        Err(e) => {
            if e.should_log() {
                error!("ping error: {:#}", e);
            }
            Err(e.status_code())
        }
    }
}


#[derive(Debug, Error)]
enum PingError {
    #[error("no ping monitor exists for the given token")]
    DoesNotExist,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl PingError {
    pub fn status_code(&self) -> StatusCode
    {
        match self {
            Self::DoesNotExist => StatusCode::NOT_FOUND,
            Self::Other(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn should_log(&self) -> bool
    {
        match self {
            Self::DoesNotExist => false,
            Self::Other(_) => true,
        }
    }
}

async fn handle_ping(app: &App, token: &str) -> Result<(), PingError>
{
    // retrieve timestamp at the start, before we do any waiting
    let now = Utc::now();

    let mut state_guard = app.state.lock().await;
    let state = &mut *state_guard;

    let mut db_guard = app.db_pool.get().await.context("retrieving db connection from pool")?;
    let db = &mut *db_guard;

    let id_opt = PingMonitor::find_by_token(db.client(), token).await?;
    let Some(id) = id_opt else { return Err(PingError::DoesNotExist); };

    let idx = state.ping_monitors.by_id.get(&id).cloned().context("retrieving monitor index for id")?;
    let pm = &mut state.ping_monitors.all[idx];

    pm.event_ping(db.client(), now).await?;
    app.notify_deadlines_updated();

    Ok(())
}
