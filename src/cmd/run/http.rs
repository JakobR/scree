use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;
use chrono::Utc;
use thiserror::Error;
use tokio_postgres::GenericClient;
use tracing::{debug, error};

use crate::db::ping::PingMonitor;
use super::App;


pub async fn run_server(listen_addr: SocketAddr, app: App) -> Result<()>
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
