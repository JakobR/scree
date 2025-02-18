use anyhow::Result;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;

pub use cfg::Config;
use tracing::debug;

mod cfg;

pub async fn run(cfg: Config) -> Result<()>
{
    let state = ();

    let app = Router::new()
        .route("/", get(dashboard))
        .route("/ping/{token}", get(ping))
        .with_state(state.clone());

    tracing::debug!("Listening on {}", cfg.listen_addr);
    let listener = tokio::net::TcpListener::bind(cfg.listen_addr).await?;
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
