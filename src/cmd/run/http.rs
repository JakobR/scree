use std::net::{IpAddr, SocketAddr};

use anyhow::{Context, Result};
use askama::Template;
use axum::extract::connect_info::Connected;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::Html;
use axum::routing::get;
use axum::serve::{IncomingStream, Listener};
use axum::Router;
use chrono::{DateTime, Utc};
use thiserror::Error;
use tokio_postgres::GenericClient;
use tracing::{debug, error, warn};

use crate::cli::SocketAddrOrPath;
use crate::db::ping::{PingMonitor, PingMonitorExt};
use super::App;


pub async fn run_server(listen_addr: SocketAddrOrPath, app: App) -> Result<()>
{
    tracing::debug!("listening on {}", listen_addr);
    match listen_addr {
        SocketAddrOrPath::Inet(addr) => {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            run_server_impl(listener, app).await
        }
        SocketAddrOrPath::Unix(path) => {
            let listener = tokio::net::UnixListener::bind(&path)?;
            run_server_impl(listener, app).await
        }
    }
}

async fn run_server_impl<L>(listener: L, app: App) -> Result<()>
    where
        L: Listener,
        L::Addr: std::fmt::Debug,
        for<'a> MyConnectInfo: Connected<IncomingStream<'a, L>>,
{
    let router = Router::new()
        .route("/", get(http_dashboard))
        .route("/ping/{token}", get(http_ping))
        .with_state(app.clone())
        .into_make_service_with_connect_info::<MyConnectInfo>();

    axum::serve(listener, router)
        .with_graceful_shutdown(app.shutdown_token.clone().cancelled_owned())
        .await?;
    Ok(())
}

#[derive(Debug, Clone)]
struct MyConnectInfo {
    /// Remote address (IP address and port) is available for connections to regular sockets,
    /// but not to unix sockets.
    remote_addr: Option<SocketAddr>,
    /*
    /// Address of the connecting unix socket.
    peer_addr: Option<std::os::unix::net::SocketAddr>,
    /// Credentials of the connecting unix socket.
    peer_cred: Option<tokio::net::unix::UCred>,
    */
}

impl Connected<IncomingStream<'_, tokio::net::TcpListener>> for MyConnectInfo {
    fn connect_info(stream: IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        MyConnectInfo {
            remote_addr: Some(*stream.remote_addr()),
            /*
            peer_addr: None,
            peer_cred: None,
            */
        }
    }
}

impl Connected<IncomingStream<'_, tokio::net::UnixListener>> for MyConnectInfo {
    fn connect_info(stream: IncomingStream<'_, tokio::net::UnixListener>) -> Self {
        let _ = stream;
        // let io = stream.io();
        // let peer_addr =
        //     io.peer_addr()
        //     .map(Into::into)
        //     .inspect_err(|e| warn!("unable to get peer addr: {:#}", e))
        //     .ok();
        // let peer_cred =
        //     io.peer_cred()
        //     .inspect_err(|e| warn!("unable to get peer cred: {:#}", e))
        //     .ok();
        MyConnectInfo {
            remote_addr: None,
            // peer_addr,
            // peer_cred,
        }
    }
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
    now: DateTime<Utc>,
    pms: Vec<PingMonitorExt>,
    started_at: String,
    version: &'static str,
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
        pms,
        now,
        started_at: app.started_at.format("%F %T %:z").to_string(),
        version: clap::crate_version!(),
    };

    let html = dashboard.render()
        .context("while rendering dashboard template")?;

    Ok(Html(html))
}


const REALIP_HEADER: HeaderName = HeaderName::from_static("x-real-ip");

fn get_remote_addr(app: &App, connect_info: &MyConnectInfo, headers: &HeaderMap) -> Option<IpAddr>
{
    if app.options.set_real_ip {
        if let Some(ip_bytes) = headers.get(REALIP_HEADER) {
            let ip_str = ip_bytes.to_str()
                .inspect_err(|e| warn!("unable to read header '{}' as string: {:#}", REALIP_HEADER, e))
                .ok()?;
            let ip = ip_str.parse()
                .inspect_err(|e| warn!("unable parse IP address: {:#}", e))
                .ok()?;
            return Some(ip);
        }
    }
    connect_info.remote_addr
        .map(|addr| addr.ip())
}


async fn http_ping(
    State(app): State<App>,
    ConnectInfo(connect_info): ConnectInfo<MyConnectInfo>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Result<&'static str, StatusCode>
{
    debug!(?token);
    debug!(?connect_info);
    debug!(?headers);

    let remote_addr = get_remote_addr(&app, &connect_info, &headers);

    match handle_ping(&app, remote_addr, &token).await {
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

async fn handle_ping(app: &App, source_addr: Option<IpAddr>, token: &str) -> Result<(), PingError>
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
