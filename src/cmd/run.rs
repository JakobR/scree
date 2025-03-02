use std::net::SocketAddr;

use anyhow::Result;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;
use tokio_postgres::Notification;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use crate::cli::{Options, RunOptions};
use crate::db::{Connection, Database};

pub async fn execute_command(options: &Options, run_options: &RunOptions) -> Result<()>
{
    let shutdown_token = CancellationToken::new();

    setup_signal_handling(shutdown_token.clone())?;
    setup_panic_hook(shutdown_token.clone());

    let db = Database::new(&options.db)?;
    let mut conn = db.connect().await?;

    let nf_handle = setup_db_notifications(&mut conn).await?;

    let server_handle = tokio::spawn(
        run_server(run_options.listen_addr, shutdown_token.clone())
    );

    shutdown_token.cancelled().await;

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

async fn setup_db_notifications(conn: &mut Connection) -> Result<JoinHandle<()>>
{
    let rx = conn.take_notification_rx().expect("notification receiver is available");
    let handle = tokio::spawn(poll_db_notifications(rx));

    conn.client.batch_execute(/*sql*/ r"
        LISTEN ping_monitors_changed;
    ").await?;

    Ok(handle)
}

async fn poll_db_notifications(mut rx: UnboundedReceiver<Notification>)
{
    while let Some(nf) = rx.recv().await {
        match nf.channel() {
            "ping_monitors_changed" => on_ping_monitors_changed().await,
            _channel => warn!("unhandled notification: {:?}", nf),
        }
    }
    // notifications channel has closed
    debug!("poll_db_notifications stopped");
}

async fn on_ping_monitors_changed()
{
    // TODO: on a db change notification (ping_monitors_changed):
    // - reload monitors immediately, unless reload happened previously within the last 5 seconds (basic rate limit).
    // - for now, just reload all the monitors instead of keeping track of fine-grained changes.
    // - take care to update state atomically, or take some kind of lock, to make sure we do not lose any updates while reloading
    //   (it is fine to miss updates for newly added items until the first reload happens)
}

async fn run_server(listen_addr: SocketAddr, shutdown_token: CancellationToken) -> Result<()>
{
    let state = ();

    let app = Router::new()
        .route("/", get(dashboard))
        .route("/ping/{token}", get(ping))
        .with_state(state.clone());

    tracing::debug!("Listening on {}", listen_addr);
    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_token.cancelled_owned())
        .await?;

    Ok(())
}

async fn dashboard() -> &'static str
{
    "Hello World!"
}

async fn ping(_state: State<()>, Path(token): Path<String>) -> Result<String, StatusCode>
{
    debug!(?token);

    Ok(format!("pinged {}", &token))
}
