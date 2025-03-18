use std::ops::DerefMut;

use anyhow::{bail, Context, Result};
use deadpool_postgres::Pool;
use tokio_postgres::{Client, GenericClient};
use tracing::debug;

use notifications::Notifications;

pub mod alert;
pub mod notifications;
pub mod ping;
pub mod util;


/// convenience function
pub async fn connect(config: &str) -> Result<Database>
{
    let config = DatabaseConfig::new(config)?;
    Database::connect(config).await
}

/// convenience function
/// intended for one-off commands that do not need a connection pool
pub async fn connect_simple(config: &str) -> Result<&'static mut Client>
{
    let client = connect(config).await?.into_simple_client().await?;
    Ok(client)
}


type Connection = tokio_postgres::Connection<tokio_postgres::Socket, postgres_native_tls::TlsStream<tokio_postgres::Socket>>;

#[derive(Debug, Clone)]
pub struct DatabaseConfig {
    pub pg_config: tokio_postgres::Config,
}

impl DatabaseConfig {

    pub fn new(config_str: &str) -> Result<Self>
    {
        let pg_config  = config_str.parse::<tokio_postgres::Config>()?;
        Ok(DatabaseConfig { pg_config })
    }

    async fn connect_client(&self) -> Result<(Client, Connection)>
    {
        let connector = tls_connector()?;
        let (client, connection) =
            self.pg_config.connect(connector).await
            .context("unable to connect to database")?;
        Ok((client, connection))
    }

    async fn connect_pool(&self) -> Result<Pool>
    {
        let mgr_config = deadpool_postgres::ManagerConfig {
            recycling_method: deadpool_postgres::RecyclingMethod::Verified,
        };
        let connector = tls_connector()?;
        let mgr = deadpool_postgres::Manager::from_config(self.pg_config.clone(), connector, mgr_config);
        let pool = Pool::builder(mgr).max_size(8).build()?;
        Ok(pool)
    }

}

fn tls_connector() -> Result<postgres_native_tls::MakeTlsConnector>
{
    let connector = native_tls::TlsConnector::builder()
        .min_protocol_version(Some(native_tls::Protocol::Tlsv12))
        .build()?;
    let connector = postgres_native_tls::MakeTlsConnector::new(connector);
    Ok(connector)
}


#[derive(Debug)]
pub struct Database {
    config: DatabaseConfig,
    pool: Pool,
}

impl Database {
    pub async fn connect(config: DatabaseConfig) -> Result<Self>
    {
        let pool = config.connect_pool().await?;

        let mut client_guard = pool.get().await?;
        let client = client_guard.deref_mut().deref_mut();

        init_schema(client).await?;

        Ok(Self { config, pool })
    }

    pub fn spawn_notification_listener(&self) -> Notifications
    {
        Notifications::new(self.config.clone())
    }

    pub fn pool(&self) -> &Pool
    {
        &self.pool
    }

    async fn into_simple_client(self) -> Result<&'static mut Client>
    {
        let guard = self.pool.get().await?;
        let guard = Box::leak(Box::new(guard));
        let client = guard.deref_mut().deref_mut();
        Ok(client)
    }
}


pub mod property {
    pub const SCHEMA_VERSION: &str = "schema_version";
    pub const CREATED_AT: &str = "created_at";
}


pub async fn get_property(db: &impl GenericClient, name: &str) -> Result<Option<String>>
{
    let row_opt = db.query_opt(/*sql*/ r"
        SELECT value FROM scree_properties WHERE name = $1
    ", &[&name]).await?;
    let value_opt = row_opt.map(|row| row.try_get(0)).transpose()?;
    Ok(value_opt)
}

pub async fn set_property(db: &impl GenericClient, name: &str, value: Option<&str>) -> Result<()>
{
    match value {
        Some(value) => {
            db.execute(/*sql*/ r"
                INSERT INTO scree_properties (name, value) VALUES ($1, $2)
                ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value
            ", &[&name, &value]).await?;
        }
        None => {
            db.execute(/*sql*/ r"
                DELETE FROM scree_properties WHERE name = $1
            ", &[&name]).await?;
        }
    }
    Ok(())
}

async fn get_schema_version(db: &impl GenericClient) -> Result<Option<u32>>
{
    let version_str = get_property(db, property::SCHEMA_VERSION).await?;
    let version = version_str.map(|s| s.parse()).transpose()?;
    Ok(version)
}

async fn init_schema(db: &mut impl GenericClient) -> Result<()>
{
    // Create property table early, to ensure we can query the schema version
    db.batch_execute(/*sql*/ r#"
        CREATE TABLE IF NOT EXISTS scree_properties
        ( name TEXT PRIMARY KEY
        , value TEXT NOT NULL
        );
    "#).await?;

    if get_schema_version(db).await?.is_none() {
        create_schema(db).await?;
    }

    migrate_schema(db).await?;

    Ok(())
}

const SCHEMA_VERSION: u32 = 1;

async fn create_schema(db: &mut impl GenericClient) -> Result<()>
{
    debug!("Creating database schema...");
    let t = db.transaction().await?;

    let create_property = t.prepare(/*sql*/ r"
        INSERT INTO scree_properties (name, value) VALUES ($1, $2)
    ").await?;
    t.execute(&create_property, &[&property::SCHEMA_VERSION, &SCHEMA_VERSION.to_string()]).await?;
    t.execute(&create_property, &[&property::CREATED_AT, &chrono::offset::Utc::now().to_rfc3339()]).await?;

    t.batch_execute(/*sql*/ r#"

        CREATE COLLATION case_insensitive
        ( provider = icu
        , locale = 'und-u-ks-level2'
        , deterministic = false
        );

        CREATE TABLE ping_monitors
        ( id SERIAL PRIMARY KEY
        , token TEXT UNIQUE NOT NULL COLLATE case_insensitive
        , name TEXT UNIQUE NOT NULL
        , period_s INTEGER NOT NULL
            CONSTRAINT period_positive CHECK (period_s > 0)
        , grace_s INTEGER NOT NULL
            CONSTRAINT grace_nonnegative CHECK (grace_s >= 0)
        , created_at TIMESTAMP WITH TIME ZONE NOT NULL
        );

        CREATE TABLE pings
        ( id SERIAL PRIMARY KEY
        , monitor_id INTEGER NOT NULL REFERENCES ping_monitors(id)
        , occurred_at TIMESTAMP WITH TIME ZONE NOT NULL
        , source_addr INET
        , source_port INTEGER
        -- TODO: geo-ip information? e.g., using https://crates.io/crates/maxminddb
        );

        CREATE UNIQUE INDEX pings_by_monitor ON pings (monitor_id, occurred_at DESC);

        CREATE TYPE monitor_state AS ENUM ('ok', 'failed');

        CREATE TABLE ping_state_history
        ( id SERIAL PRIMARY KEY
        , monitor_id INTEGER NOT NULL REFERENCES ping_monitors(id)
        , state monitor_state NOT NULL
        , state_since TIMESTAMP WITH TIME ZONE NOT NULL
        );

        -- NOTE: because of UNIQUE, we may only have one state per instant per monitor which ensures deterministic ordering by time
        CREATE UNIQUE INDEX ping_state_history_by_monitor ON ping_state_history (monitor_id, state_since DESC);

        CREATE FUNCTION notify_ping_monitors_change() RETURNS TRIGGER AS $$
            BEGIN
                NOTIFY ping_monitors_changed;
                RETURN NULL;
            END;
        $$ LANGUAGE plpgsql;

        CREATE TRIGGER trigger_ping_monitors_change
            AFTER INSERT OR UPDATE OR DELETE OR TRUNCATE
            ON ping_monitors
            FOR EACH STATEMENT
            EXECUTE FUNCTION notify_ping_monitors_change();

        CREATE TABLE alert_history
        ( id SERIAL PRIMARY KEY
        , subject TEXT NOT NULL
        , message TEXT NOT NULL
        , channel TEXT NOT NULL
        , created_at TIMESTAMP WITH TIME ZONE NOT NULL
        , delivered_at TIMESTAMP WITH TIME ZONE
        );

    "#).await?;

    t.commit().await?;
    Ok(())
}

async fn migrate_schema(db: &mut impl GenericClient) -> Result<()>
{
    let schema_version = get_schema_version(db).await?
        .context("unable to determine database schema version")?;
    debug!("database schema version: {}", schema_version);

    if schema_version < SCHEMA_VERSION {
        todo!("upgrade schema");
    }

    if schema_version > SCHEMA_VERSION {
        bail!("unsupported schema version {} (supported version is {})", schema_version, SCHEMA_VERSION);
    }

    assert_eq!(schema_version, SCHEMA_VERSION);
    Ok(())
}

/*

when sending alerts, lock the row to make sure we send it only once: https://stackoverflow.com/a/52557413

*/


/*

To get detailed change notifications, we can record a change log in a separate history table:

            CREATE TYPE change_kind AS ENUM ('insert', 'update', 'delete');

            CREATE FUNCTION notify_ping_monitors_change() RETURNS TRIGGER AS $$
                BEGIN
                    -- Check the operation type
                    IF TG_OP = 'INSERT' THEN
                        INSERT INTO ping_monitor_history (monitor_id, change) VALUES ( NEW.id, 'insert' );
                    ELSIF TG_OP = 'UPDATE' THEN
                        INSERT INTO ping_monitor_history (monitor_id, change) VALUES ( NEW.id, 'update' );
                    ELSIF TG_OP = 'DELETE' THEN
                        INSERT INTO ping_monitor_history (monitor_id, change) VALUES ( OLD.id, 'delete' );
                    END IF;

                    NOTIFY ping_monitors_changed;

                    RETURN NULL;
                END;
            $$ LANGUAGE plpgsql;

            CREATE TRIGGER trigger_ping_monitors_change
                AFTER INSERT OR UPDATE OR DELETE
                ON ping_monitors
                FOR EACH ROW
                EXECUTE FUNCTION notify_ping_monitors_change();

To map the enum type, we can use the following:

    use postgres_types::{ToSql, FromSql};

    #[derive(Debug, ToSql, FromSql)]
    #[postgres(name = "change_kind", rename_all = "snake_case")]
    pub enum ChangeKind {
        Insert,
        Update,
        Delete,
    }

 */
