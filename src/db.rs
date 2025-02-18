use anyhow::Result;
use tokio_postgres::NoTls;

mod schema;

pub async fn connect(config: &str) -> Result<()>
{
    let _db = tokio_postgres::connect(config, NoTls).await?;
    Ok(())
}
