use anyhow::Result;

use crate::cli::Options;
use crate::db::{get_schema_version, Database, DatabaseConfig, SCHEMA_VERSION};

pub async fn main(options: &Options) -> Result<()>
{
    let db_config =
        DatabaseConfig::new(&options.db)?
        .enable_migrations();
    let db = Database::connect(db_config).await?;
    let client_guard = db.pool().get().await?;
    let client = &**client_guard;
    let new_version = get_schema_version(client).await?;
    assert_eq!(new_version, Some(SCHEMA_VERSION));
    Ok(())
}
