use std::net::SocketAddr;

use anyhow::{Context, Result};
use askama::Template;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use chrono::{DateTime, Utc};
use thiserror::Error;
use tokio_postgres::GenericClient;
use tracing::{debug, error};

use crate::db::ping::{PingMonitor, PingMonitorExt};
use super::App;


pub async fn run_server(listen_addr: SocketAddr, app: App) -> Result<()>
{
    let router = Router::new()
        .route("/", get(http_dashboard))
        .route("/ping/{token}", get(http_ping))
        .with_state(app.clone())
        .into_make_service_with_connect_info::<SocketAddr>();

    tracing::debug!("listening on {}", listen_addr);
    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(app.shutdown_token.clone().cancelled_owned())
        .await?;

    Ok(())
}

async fn http_dashboard(app: State<App>) -> Result<Html<String>, StatusCode>
{
    handle_dashboard(&app).await
        .map_err(|e| {
            if e.should_log() {
                error!("dashboard error: {:#}", e);
            }
            e.status_code()
        })
}

#[derive(Debug, Error)]
enum DashboardError {
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl DashboardError {
    pub fn status_code(&self) -> StatusCode
    {
        match self {
            Self::Other(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn should_log(&self) -> bool
    {
        match self {
            Self::Other(_) => true,
        }
    }
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    blah: i64,
    started_at: String,
    now: DateTime<Utc>,
    pms: Vec<PingMonitorExt>,
}

async fn handle_dashboard(app: &App) -> Result<Html<String>, DashboardError>
{
    // TODO:
    // dashboard should clone the data behind the mutex and then release it
    // then also check with database if there are any new monitors that are missing from the local cache
    // (we'd want to know, and display a warning on such "inactive" monitors.)
    // TODO: also display the stats (like 'ping list' command). could even display the list of previous pings if you click on one.

    let pms = {
        let state_guard = app.state.lock().await;
        let state = &*state_guard;
        state.ping_monitors.all.clone()
    };

    debug_assert!(pms.is_sorted_by(|x, y| x.name <= y.name));

    let now = Utc::now();

    let dashboard = DashboardTemplate {
        blah: 123,
        started_at: app.started_at.format("%F %T %:z").to_string(),
        pms,
        now,
    };

    let html = dashboard.render()
        .context("while rendering dashboard template")?;

    Ok(Html(html))
}




async fn http_ping(
    app: State<App>,
    ConnectInfo(source_addr): ConnectInfo<SocketAddr>,
    Path(token): Path<String>,
) -> Result<&'static str, StatusCode>
{
    debug!(?token);

    match handle_ping(&app, source_addr, &token).await {
        Ok(()) => Ok("OK"),
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

async fn handle_ping(app: &App, source_addr: SocketAddr, token: &str) -> Result<(), PingError>
{
    let mut state_guard = app.state.lock().await;
    let state = &mut *state_guard;

    // timestamp is retrieved after we have the state lock;
    // otherwise interleaving could be such that a failure is recorded with a later timestamp while we are waiting on the state mutex
    let now = Utc::now();

    let mut db_guard = app.db.pool().get().await.context("retrieving db connection from pool")?;
    let db = &mut *db_guard;

    let id_opt = PingMonitor::find_by_token(db.client(), token).await?;
    let Some(id) = id_opt else { return Err(PingError::DoesNotExist); };

    let idx = state.ping_monitors.by_id.get(&id).cloned().context("retrieving monitor index for id")?;
    let pm = &mut state.ping_monitors.all[idx];

    pm.event_ping(db.client(), now, source_addr).await?;
    app.notify_deadlines_updated();

    Ok(())
}
