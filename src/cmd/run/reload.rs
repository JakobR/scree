use std::time::Instant;

use anyhow::{Context, Result};
use chrono::Utc;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_postgres::GenericClient;
use tracing::{debug, error, info};

use crate::db::ping::PingMonitorExt;
use super::{App, AppState, PingMonitors};

#[derive(Debug)]
struct ReloadRequest {
    timestamp: Instant,
}

impl ReloadRequest {
    fn new() -> Self {
        ReloadRequest { timestamp: Instant::now() }
    }
}

#[derive(Debug)]
pub struct ReloadToken {
    reload_tx: UnboundedSender<ReloadRequest>,
}

impl ReloadToken {
    pub fn reload_ping_monitors(&self)
    {
        // ignore errors due to closed channel
        let _ = self.reload_tx.send(ReloadRequest::new());
    }
}

#[derive(Debug)]
pub struct ReloadTask {
    reload_tx: UnboundedSender<ReloadRequest>,
    reload_rx: UnboundedReceiver<ReloadRequest>,
}

impl ReloadTask {

    pub fn new() -> Self
    {
        let (reload_tx, reload_rx) = mpsc::unbounded_channel();
        ReloadTask { reload_tx, reload_rx }
    }

    pub fn token(&self) -> ReloadToken
    {
        ReloadToken { reload_tx: self.reload_tx.clone() }
    }

    pub async fn spawn(self, app: &App)
    {
        app.spawn("reload_task",
            process_reload_requests(self.reload_rx, self.reload_tx, app.clone())
        ).await;
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

pub async fn reload_ping_monitors(app: &App, state: &mut AppState) -> Result<()>
{
    info!("(re-)loading ping monitors");
    let now = Utc::now();
    let db = app.db.pool().get().await?;

    // We have the lock on the AppState at this point,
    // so we don't have to worry about pings coming in while reloading.

    let pms = PingMonitorExt::get_all(db.client(), now).await?;

    state.ping_monitors = PingMonitors::new(pms);
    info!("loaded {} ping monitors", state.ping_monitors.all.len());

    app.notify_deadlines_updated();

    Ok(())
}
