use std::collections::HashMap;
use std::future::Future;
use std::ops::Deref;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
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
use crate::util::into_result::IntoResult;
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
    options: &'static RunOptions,
    min_reload_interval: Duration,
    deadline_watcher: DeadlineWatchToken,
    reload: ReloadToken,
    shutdown_token: CancellationToken,
    started_at: DateTime<Utc>,
    state: Arc<Mutex<AppState>>,
    tasks: Mutex<Vec<Task>>,
}

#[derive(Debug)]
struct Task {
    name: &'static str,
    handle: JoinHandle<Result<()>>,
}

impl AppData {
    pub fn new(db: Database, options: &'static RunOptions, deadline_watcher: DeadlineWatchToken, reload: ReloadToken, shutdown_token: CancellationToken) -> Self
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
            tasks: Mutex::new(Vec::new()),
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

    /// Spawn the given task into the background using `tokio::spawn`.
    /// The app will wait for all registered tasks to finish before exiting.
    ///
    /// This version is intended for critical tasks that live until the end of the program:
    /// when the spawned task finishes, the app's shutdown_token will be cancelled.
    ///
    /// IMPORTANT: the task must finish "soon" when the `shutdown_token` is cancelled.
    pub async fn spawn<F, O>(&self, name: &'static str, future: F)
        where
            F: Future<Output = O> + Send + 'static,
            O: IntoResult<()> + Send + 'static,
    {
        self.spawn_impl(name, false, true, future).await;
    }

    // pub async fn spawn_noncritical<F, O>(&self, name: &'static str, future: F)
    //     where
    //         F: Future<Output = O> + Send + 'static,
    //         O: IntoResult<()> + Send + 'static,
    // {
    //     self.spawn_impl(name, false, false, future).await;
    // }

    /// Run the given task when the shutdown_token has been cancelled.
    pub async fn spawn_on_shutdown<F, O>(&self, name: &'static str, future: F)
        where
            F: Future<Output = O> + Send + 'static,
            O: IntoResult<()> + Send + 'static,
    {
        self.spawn_impl(name, true, false, future).await;
    }

    /// Register the given task to be awaited before exiting.
    /// See also the notes on `spawn`.
    async fn spawn_impl<F, O>(&self, name: &'static str, wait_on_shutdown: bool, critical: bool, future: F)
        where
            F: Future<Output = O> + Send + 'static,
            O: IntoResult<()> + Send + 'static
    {
        let shutdown_token = self.shutdown_token.clone();
        let handle = tokio::spawn(async move {
            if wait_on_shutdown {
                shutdown_token.cancelled().await;
            }
            let result =
                future.await
                .into_result()
                .with_context(|| format!("error in task '{}'", name))
                .inspect_err(|e| {
                    error!("{:#}", e);
                });
            if critical {
                shutdown_token.cancel();
            }
            result
        });

        let mut tasks_guard = self.tasks.lock().await;
        let tasks = &mut *tasks_guard;
        tasks.push(Task { name, handle });
    }

    async fn take_active_tasks(&self) -> Vec<Task>
    {
        let mut tasks_guard = self.tasks.lock().await;
        let tasks = &mut *tasks_guard;
        std::mem::take(tasks)
    }

    async fn join_active_tasks(&self) -> Vec<anyhow::Error>
    {
        let mut errors: Vec<anyhow::Error> = Vec::new();
        // collect sub-tasks until none are left
        loop {
            let tasks = self.take_active_tasks().await;
            if tasks.is_empty() {
                break;
            }
            debug!("joining {} tasks...", tasks.len());
            for task in tasks {
                let result =
                    task.handle.await
                    .with_context(|| format!("error joining task '{}'", task.name));
                match result {
                    Ok(Ok(())) => {
                        debug!("task '{}' joined.", task.name);
                    }
                    Ok(Err(task_err)) => {
                        // not logging this since we already reported the error in the wrapper task (see `register_task`)
                        errors.push(task_err);
                    }
                    Err(join_err) => {
                        error!("{:#}", &join_err);
                        errors.push(join_err);
                    }
                }
            }
        }
        errors
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


pub async fn main(options: &'static Options, run_options: &'static RunOptions) -> Result<()>
{
    let shutdown_token = CancellationToken::new();

    setup_signal_handling(shutdown_token.clone())?;
    setup_panic_hook(shutdown_token.clone());

    let db =  db::connect(&options.db).await?;

    let deadline_watcher = DeadlineWatchTask::new();
    let reload_task = ReloadTask::new();

    let app: App = AppData::new(db, run_options, deadline_watcher.token(), reload_task.token(), shutdown_token.clone()).into();

    let startup_result =
        start_subtasks(&app, reload_task, deadline_watcher).await
        .inspect_err(|_e| {
            // if startup fails, initiate shutdown
            shutdown_token.cancel();
        });

    // wait for shutdown signal
    shutdown_token.cancelled().await;

    let task_results = app.join_active_tasks().await;

    // TODO: merge errors instead of only returning the first one (this is not too important because we already report all of them in the logs)
    startup_result?;
    for e in task_results {
        return Err(e);
    }
    Ok(())
}

async fn start_subtasks(app: &App, reload_task: ReloadTask, deadline_watcher: DeadlineWatchTask) -> Result<()>
{
    // lock the state mutex while sub-tasks are starting up (controlled startup while initial data is loaded)
    let mut state_guard = app.state.lock().await;
    let state = &mut *state_guard;

    let mut nf = app.db.spawn_notification_listener();
    setup_db_notifications(&mut nf, app).await?;
    app.spawn_on_shutdown("nf.close", nf.close()).await;

    reload_task.spawn(app).await;
    deadline_watcher.spawn(app).await;
    app.spawn("http server", http::run_http_server(app.clone())).await;

    // load initial data
    reload::reload_ping_monitors(&app, &mut *state).await?;

    info!("startup complete");

    // release state mutex to unblock sub-tasks
    drop(state_guard);

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

async fn setup_db_notifications(nf: &mut Notifications, app: &App) -> Result<()>
{
    let rx = nf.take_notification_rx().expect("notification receiver is available");
    app.spawn("poll_db_notifications", poll_db_notifications(app.clone(), rx)).await;

    let subscription_token = nf.subscribe("ping_monitors_changed").await?;

    // we never unsubscribe
    std::mem::forget(subscription_token);

    Ok(())
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
