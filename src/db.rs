use anyhow::{bail, Context, Result};
use tokio::task::JoinHandle;
use tokio_postgres::{Client, NoTls};
use tracing::{debug, error};

pub struct Database {
    pub client: Client,

    #[allow(dead_code)]  // TODO: remove
    connection_handle: JoinHandle<()>,
}

impl Database {

    pub async fn connect(config: &str) -> Result<Self>
    {
        // TODO: set up TLS support, see https://docs.rs/tokio-postgres/latest/tokio_postgres/#ssltls-support
        let (client, connection) =
            tokio_postgres::connect(config, NoTls).await
            .context("unable to connect to database")?;

        // The connection object performs the actual communication with the database,
        // so we spawn it off to run on its own.
        let connection_handle =
            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    error!("database connection error: {}", e);
                }
            });

        let mut db = Self {
            client,
            connection_handle,
        };

        db.init_schema().await?;

        Ok(db)
    }

    pub async fn get_property(&self, name: &str) -> Result<Option<String>>
    {
        let row_opt =
            self.client.query_opt(r#"
                SELECT value FROM scree_properties WHERE name = $1
            "#, &[&name]).await?;

        let value_opt = row_opt.map(|row| row.try_get(0)).transpose()?;
        Ok(value_opt)
    }

    #[allow(dead_code)]  // TODO: remove
    pub async fn set_property(&self, name: &str, value: &str) -> Result<()>
    {
        self.client.execute(r#"
            INSERT OR REPLACE INTO scree_properties (name, value)
            VALUES ($1, $2)
        "#, &[&name, &value]).await.context("set_property")?;
        Ok(())
    }

    #[allow(dead_code)]  // TODO: remove
    pub async fn delete_property(&self, name: &str) -> Result<()>
    {
        self.client.execute(r#"
            DELETE FROM scree_properties WHERE name = $1
        "#, &[&name]).await.context("delete_property")?;
        Ok(())
    }

    async fn get_schema_version(&self) -> Result<Option<u32>>
    {
        let version_str = self.get_property("schema_version").await?;
        let version = version_str.map(|s| s.parse()).transpose()?;
        Ok(version)
    }

    async fn init_schema(&mut self) -> Result<()>
    {
        // Create property table early, to ensure we can query the schema version
        self.client.execute(r#"
            CREATE TABLE IF NOT EXISTS scree_properties
            ( name TEXT PRIMARY KEY
            , value TEXT NOT NULL
            )
        "#, &[]).await?;

        if self.get_schema_version().await?.is_none() {
            self.create_schema().await?;
        }

        self.migrate_schema().await?;

        Ok(())
    }

    const SCHEMA_VERSION: u32 = 1;

    async fn create_schema(&mut self) -> Result<()>
    {
        debug!("Creating database schema...");
        let t = self.client.transaction().await?;

        let create_property = t.prepare("INSERT INTO scree_properties (name, value) VALUES ($1, $2)").await?;
        t.execute(&create_property, &[&"schema_version", &Self::SCHEMA_VERSION.to_string()]).await?;
        t.execute(&create_property, &[&"created_at", &chrono::offset::Utc::now().to_rfc3339()]).await?;

        t.execute(r#"
            CREATE COLLATION case_insensitive
            ( provider = icu
            , locale = 'und-u-ks-level2'
            , deterministic = false
            )
        "#, &[]).await?;

        // TODO: check whether COLLATE case_insensitive does what I want
        t.execute(r#"
            CREATE TABLE ping_monitors
            ( id SERIAL PRIMARY KEY
            , token TEXT UNIQUE NOT NULL COLLATE case_insensitive
            , name TEXT NOT NULL
            , period_ns INTEGER NOT NULL
            , grace_ns INTEGER NOT NULL
            )
        "#, &[]).await?;

        // TODO: add index for sorting on time stamps?
        // TODO: could log source ip address as well
        t.execute(r#"
            CREATE TABLE ping_events
            ( id SERIAL PRIMARY KEY
            , monitor_id INTEGER NOT NULL REFERENCES ping_monitors(id)
            , occurred_at TIMESTAMP WITH TIME ZONE
            )
        "#, &[]).await?;

        t.commit().await?;
        Ok(())
    }

    async fn migrate_schema(&self) -> Result<()>
    {
        let schema_version = self.get_schema_version().await?
            .context("unable to determine database schema version")?;
        debug!("Database schema version: {}", schema_version);

        if schema_version < Self::SCHEMA_VERSION {
            todo!("upgrade schema");
        }

        if schema_version > Self::SCHEMA_VERSION {
            bail!("unsupported schema version {} (supported version is {})", schema_version, Self::SCHEMA_VERSION);
        }

        assert_eq!(schema_version, Self::SCHEMA_VERSION);
        Ok(())
    }

}
