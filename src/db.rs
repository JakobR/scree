use anyhow::{bail, Context, Result};
use tokio::sync::{Mutex, OnceCell};
use tokio::task::JoinHandle;
use tokio_postgres::{Client, NoTls, Statement};
use tracing::{debug, error};


pub struct Database {
    config: String,
    is_initialized: Mutex<bool>,
}

impl Database {

    pub fn new(config: &str) -> Result<Self>
    {
        Ok(Database {
            config: config.to_string(),
            is_initialized: Mutex::new(false),
        })
    }

    pub async fn connect(&self) -> Result<Connection>
    {
        let mut conn = Connection::new(&self.config).await?;
        self.initialize(&mut conn).await?;
        Ok(conn)
    }

    async fn initialize(&self, conn: &mut Connection) -> Result<()>
    {
        let mut is_initialized = self.is_initialized.lock().await;
        if *is_initialized {
            return Ok(());
        }

        // Initialize/check/migrate the database schema only on the first connection
        conn.init_schema().await?;

        *is_initialized = true;
        Ok(())
    }

}


pub struct Connection {
    pub client: Client,

    get_property_stmt: OnceCell<Statement>,

    #[allow(dead_code)]  // TODO: remove
    connection_handle: JoinHandle<()>,
}

impl Connection {

    async fn new(config: &str) -> Result<Self>
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

        Ok(Self {
            client,
            get_property_stmt: OnceCell::new(),
            connection_handle,
        })
    }

    pub async fn close(self) -> Result<()>
    {
        // TODO: send cancellation flag to background task, join the connection_handle
        self.connection_handle.await?;
        Ok(())
    }

    pub async fn get_property(&self, name: &str) -> Result<Option<String>>
    {
        let stmt = self.get_property_stmt.get_or_try_init(|| {
            self.client.prepare(r"SELECT value FROM scree_properties WHERE name = $1")
        }).await?;
        let row_opt = self.client.query_opt(stmt, &[&name]).await?;
        let value_opt = row_opt.map(|row| row.try_get(0)).transpose()?;
        Ok(value_opt)
    }

    #[allow(dead_code)]  // TODO: remove
    pub async fn set_property(&self, name: &str, value: &str) -> Result<()>
    {
        self.client.execute(r#"
            INSERT OR REPLACE INTO scree_properties (name, value)
            VALUES ($1, $2)
        "#, &[&name, &value]).await?;
        Ok(())
    }

    #[allow(dead_code)]  // TODO: remove
    pub async fn delete_property(&self, name: &str) -> Result<()>
    {
        self.client.execute(r#"
            DELETE FROM scree_properties WHERE name = $1
        "#, &[&name]).await?;
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
        self.client.batch_execute(r#"
            CREATE TABLE IF NOT EXISTS scree_properties
            ( name TEXT PRIMARY KEY
            , value TEXT NOT NULL
            );
        "#).await?;

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

        let create_property = t.prepare(r"INSERT INTO scree_properties (name, value) VALUES ($1, $2)").await?;
        t.execute(&create_property, &[&"schema_version", &Self::SCHEMA_VERSION.to_string()]).await?;
        t.execute(&create_property, &[&"created_at", &chrono::offset::Utc::now().to_rfc3339()]).await?;

        t.batch_execute(r#"

            CREATE COLLATION case_insensitive
            ( provider = icu
            , locale = 'und-u-ks-level2'
            , deterministic = false
            );

            CREATE TABLE ping_monitors
            ( id SERIAL PRIMARY KEY
            , token TEXT UNIQUE NOT NULL COLLATE case_insensitive
            , name TEXT NOT NULL
            , period_ns INTEGER NOT NULL
            , grace_ns INTEGER NOT NULL
            );

            CREATE TABLE ping_events
            ( id SERIAL PRIMARY KEY
            , monitor_id INTEGER NOT NULL REFERENCES ping_monitors(id)
            , occurred_at TIMESTAMP WITH TIME ZONE
            );

        "#).await?;

        // TODO: ping_events: add index on for sorting on time stamps?
        // TODO: ping_events: could log the source ip address as well

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
