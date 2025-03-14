#![allow(unused)]  // TODO: remove

use std::collections::{BTreeMap, HashMap};
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
use deadpool_postgres::Pool;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_postgres::{GenericClient, Notification};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::cli::{Options, RunOptions};
use crate::db::ping::{PingMonitor, Stats};
use crate::db::util::WithId;
use crate::db::{self, Connection, Database};

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
    shutdown_token: CancellationToken,
    started_at: DateTime<Utc>,
    state: Arc<Mutex<AppState>>,
}

impl AppData {
    fn new(db_pool: Pool, shutdown_token: CancellationToken) -> Self
    {
        Self {
            db_pool,
            min_reload_interval: Duration::from_secs(5),
            shutdown_token,
            started_at: Utc::now(),
            state: Arc::new(Mutex::new(AppState::new())),
        }
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
    // by_token: HashMap<String, usize>,  // by_token queries should go through database because of collation
    by_name: BTreeMap<String, usize>,
}

impl PingMonitors {
    fn new(pms: Vec<PingMonitorExt>) -> Self
    {
        let by_id = pms.iter()
            .enumerate()
            .map(|(idx, pm)| (pm.id, idx))
            .collect();

        let by_name = pms.iter()
            .enumerate()
            .map(|(idx, pm)| (pm.name.clone(), idx))
            .collect();

        Self {
            all: pms,
            by_id,
            by_name,
        }
    }
}

#[derive(Debug)]
struct PingMonitorExt {
    inner: WithId<PingMonitor>,
    stats: Stats,
    state: MonitorStateInfo,
}

impl PingMonitorExt {
    fn new(pm: WithId<PingMonitor>, stats: Stats, now: DateTime<Utc>) -> Self
    {
        let state = MonitorStateInfo::compute(&pm, stats.last_ping_at, now);
        Self {
            inner: pm,
            stats,
            state,
        }
    }

    fn update_state(&mut self, now: DateTime<Utc>)
    {
        self.state = MonitorStateInfo::compute(&self, self.stats.last_ping_at ,now);
    }
}

impl Deref for PingMonitorExt {
    type Target = WithId<PingMonitor>;

    fn deref(&self) -> &Self::Target
    {
        &self.inner
    }
}

impl DerefMut for PingMonitorExt {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

#[derive(Debug)]
pub struct MonitorStateInfo {
    state: MonitorState,
    warn_at: DateTime<Utc>,
    error_at: DateTime<Utc>,
}

impl MonitorStateInfo {
    pub fn compute(pm: &PingMonitor, last_ping_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Self
    {
        // If no ping has been recorded yet, we use the monitor's creation timestamp as base.
        // If the last ping is in the future (if the system clock changed), then use the current time instead.
        // (We could use the monotonic clock (std::time::Instant) to track deadlines instead. The question remains how to convert to/from database timestamps.)
        let last_ping_at = last_ping_at.unwrap_or(pm.created_at).min(now);

        let warn_at = last_ping_at + pm.period;
        let error_at = warn_at + pm.grace;
        let state =
            if now < warn_at {
                MonitorState::Ok
            } else if now < error_at {
                MonitorState::Warning
            } else {
                MonitorState::Failed
            };

        assert!(last_ping_at < warn_at);
        assert!(warn_at <= error_at);

        Self { state, warn_at, error_at }
    }
}

#[derive(Debug)]
pub enum MonitorState {
    Ok,
    Warning,
    Failed,
}

pub async fn execute_command(options: &Options, run_options: &RunOptions) -> Result<()>
{
    let shutdown_token = CancellationToken::new();

    setup_signal_handling(shutdown_token.clone())?;
    setup_panic_hook(shutdown_token.clone());

    let db = Database::new(&options.db)?;
    // Long-running database connection is used to respond to database notifications
    let mut conn = db.connect().await?;
    // Connection pool is used for individual requests
    let pool = db.connect_pool().await?;

    let app: App = AppData::new(pool, shutdown_token.clone()).into();

    // lock the state mutex while sub-tasks are starting up (controlled startup while initial data is loaded)
    let mut state = app.state.lock().await;

    let nf_handle = setup_db_notifications(&mut conn, app.clone()).await?;

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
    let (reload_tx, reload_rx) = mpsc::unbounded_channel();

    let _reload_handle = tokio::spawn(process_reload_requests(reload_rx, reload_tx.clone(), app));

    let rx = conn.take_notification_rx().expect("notification receiver is available");
    let handle = tokio::spawn(poll_db_notifications(rx, reload_tx));

    conn.client.batch_execute(/*sql*/ r"
        LISTEN ping_monitors_changed;
    ").await?;

    Ok(handle)
}

async fn poll_db_notifications(mut rx: UnboundedReceiver<Notification>, reload_tx: UnboundedSender<ReloadRequest>)
{
    while let Some(nf) = rx.recv().await {
        match nf.channel() {
            "ping_monitors_changed" => {
                // we do not care whether the channel is closed
                let _ = reload_tx.send(ReloadRequest::new());
            }
            //on_ping_monitors_changed(&app).await,
            _channel => warn!("unhandled notification: {:?}", nf),
        }
    }
    // notifications channel has closed
    debug!("poll_db_notifications stopped");
}

struct ReloadRequest {
    timestamp: Instant,
}

impl ReloadRequest {
    fn new() -> Self {
        ReloadRequest { timestamp: Instant::now() }
    }
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

    let pms =
        db::ping::get_all_with_stats(db.client()).await?
        .into_iter()
        .map(|(pm, st)|
            PingMonitorExt::new(pm, st, now)
        )
        .collect::<Vec<_>>();

    state.ping_monitors = PingMonitors::new(pms);

    // TODO: notify monitor task to check new deadlines.
    // TODO: need to remember which alerts have been sent already.
    //       we could store all failure events in the database, along with whether an alert has already been sent.

    info!("loaded {} ping monitors", state.ping_monitors.all.len());
    // todo!()
    Ok(())
}


async fn run_server(listen_addr: SocketAddr, app: App) -> Result<()>
{
    let router = Router::new()
        .route("/", get(dashboard))
        .route("/ping/{token}", get(ping))
        .with_state(app.clone());

    tracing::debug!("listening on {}", listen_addr);
    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(app.shutdown_token.clone().cancelled_owned())
        .await?;

    Ok(())
}

async fn dashboard() -> &'static str
{
    // TODO:
    // dashboard should clone the data behind the mutex and then release it
    // then also check with database if there are any new monitors that are missing from the local cache
    // (we'd want to know, and display a warning on such "inactive" monitors.)
    // TODO: also display the stats (like 'ping list' command). could even display the list of previous pings if you click on one.
    "Hello World!"
}

async fn ping(_app: State<App>, Path(token): Path<String>) -> Result<String, StatusCode>
{
    debug!(?token);

    Ok(format!("pinged {}", &token))
}
