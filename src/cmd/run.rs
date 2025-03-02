use std::net::SocketAddr;

use anyhow::Result;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;
use tracing::debug;

use crate::cli::{Options, RunOptions};
use crate::db;

pub async fn execute_command(options: &Options, run_options: &RunOptions) -> Result<()>
{
    let db = db::Database::new(&options.db)?;
    let mut conn = db.connect().await?;
    let _nf_rx = conn.take_notification_rx().expect("notification receiver is available");

    conn.client.batch_execute(r"
        LISTEN ping_monitors_changed;
        LISTEN my_channel;
        NOTIFY my_channel, 'hello!';
        NOTIFY my_channel, 'good bye!';
    ").await?;

    // TODO: on a db change notification (ping_monitors_changed):
    // - reload monitors immediately, unless reload happened previously within the last 5 seconds (basic rate limit).
    // - for now, just reload all the monitors instead of keeping track of fine-grained changes.
    // - take care to update state atomically, or take some kind of lock, to make sure we do not lose any updates while reloading
    //   (it is fine to miss updates for newly added items until the first reload happens)

    run_server(run_options.listen_addr).await?;

    conn.close().await?;

    Ok(())
}

async fn run_server(listen_addr: SocketAddr) -> Result<()>
{
    let state = ();

    let app = Router::new()
        .route("/", get(dashboard))
        .route("/ping/{token}", get(ping))
        .with_state(state.clone());

    tracing::debug!("Listening on {}", listen_addr);
    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    axum::serve(listener, app).await?;

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
