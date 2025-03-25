use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
use deadline_watcher::{DeadlineWatchTask, DeadlineWatchToken};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_postgres::Notification;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::cli::{Options, RunOptions};
use crate::db::notifications::Notifications;
use crate::db::ping::PingMonitorExt;
use crate::db::{self, Database};
use reload::{ReloadTask, ReloadToken};

mod deadline_watcher;
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
    db: Database,
    options: RunOptions,
    min_reload_interval: Duration,
    deadline_watcher: DeadlineWatchToken,
    reload: ReloadToken,
    shutdown_token: CancellationToken,
    started_at: DateTime<Utc>,
    state: Arc<Mutex<AppState>>,
}

impl AppData {
    pub fn new(db: Database, options: RunOptions, deadline_watcher: DeadlineWatchToken, reload: ReloadToken, shutdown_token: CancellationToken) -> Self
    {
        Self {
            db,
            options,
            min_reload_interval: Duration::from_secs(5),
            deadline_watcher,
            reload,
            shutdown_token,
            started_at: Utc::now(),
            state: Arc::new(Mutex::new(AppState::new())),
        }
    }

    pub fn notify_deadlines_updated(&self)
    {
        self.deadline_watcher.notify_deadlines_updated();
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

    let db =  db::connect(&options.db).await?;

    let deadline_watcher = DeadlineWatchTask::new();
    let reload_task = ReloadTask::new();

    let app: App = AppData::new(db, run_options.clone(), deadline_watcher.token(), reload_task.token(), shutdown_token.clone()).into();

    // lock the state mutex while sub-tasks are starting up (controlled startup while initial data is loaded)
    let mut state = app.state.lock().await;

    let mut nf = app.db.spawn_notification_listener();
    let nf_handle = setup_db_notifications(&mut nf, app.clone()).await?;
    let reload_handle = reload_task.spawn(app.clone());
    let watch_handle = deadline_watcher.spawn(app.clone());

    let http_handle = tokio::spawn(
        http::run_http_server(run_options.clone(), app.clone())
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
    nf.close().await?;
    nf_handle.await?;  // stops after nf.close()

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

async fn setup_db_notifications(nf: &mut Notifications, app: App) -> Result<JoinHandle<()>>
{
    let rx = nf.take_notification_rx().expect("notification receiver is available");
    let handle = tokio::spawn(poll_db_notifications(app, rx));

    let subscription_token = nf.subscribe("ping_monitors_changed").await?;

    // we never unsubscribe
    std::mem::forget(subscription_token);

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
