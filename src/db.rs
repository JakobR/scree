use anyhow::{bail, Context, Result};
use futures::{stream, StreamExt};
use tokio::sync::{mpsc, Mutex, OnceCell};
use tokio::task::JoinHandle;
use tokio_postgres::{AsyncMessage, Client, NoTls, Notification, Statement};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};


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
    set_property_stmt: OnceCell<Statement>,
    delete_property_stmt: OnceCell<Statement>,

    connection_handle: JoinHandle<()>,
    connection_token: CancellationToken,
    notification_rx: Option<mpsc::UnboundedReceiver<Notification>>,
}

// The tokio_postgres::Connection performs the actual communication with the database when it is being polled.
// This should be spawned off into the background.
async fn poll_postgres_connection<S, T>(mut connection: tokio_postgres::Connection<S, T>, token: CancellationToken, notification_tx: mpsc::UnboundedSender<Notification>)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut stream = stream::poll_fn(|ctx| connection.poll_message(ctx));
    loop {
        tokio::select! {
            _ = token.cancelled() => {
                debug!("database connection polling cancelled");
                break;
            }
            msg = stream.next() => {
                match msg {
                    Some(Ok(AsyncMessage::Notification(nf))) => {
                        debug!("notification: {}: {}", nf.channel(), nf.payload());
                        // a send error only happens if the receiver was dropped or closed; we simply ignore this case and drop the notification
                        // TODO: if no one is listening to the notifications, the channel will continue to grow (memory leak)
                        let _ = notification_tx.send(nf);
                    }
                    Some(Ok(AsyncMessage::Notice(notice))) =>
                        info!("{}: {}", notice.severity(), notice.message()),
                    Some(Ok(other_msg)) =>
                        warn!("unknown database message: {:?}", other_msg),
                    Some(Err(e)) => {
                        error!("database connection error: {}", e);
                        break;
                    }
                    None =>
                        break,
                }
            }
        }
    }
}

impl Connection {

    async fn new(config: &str) -> Result<Self>
    {
        // TODO: set up TLS support, see https://docs.rs/tokio-postgres/latest/tokio_postgres/#ssltls-support
        let (client, connection) =
            tokio_postgres::connect(config, NoTls).await
            .context("unable to connect to database")?;

        let connection_token = CancellationToken::new();
        let (notification_tx, notification_rx) = mpsc::unbounded_channel();

        // The connection object performs the actual communication with the database,
        // so we spawn it off to run on its own.
        let connection_handle = tokio::spawn(poll_postgres_connection(connection, connection_token.clone(), notification_tx));

        Ok(Self {
            client,
            get_property_stmt: OnceCell::new(),
            set_property_stmt: OnceCell::new(),
            delete_property_stmt: OnceCell::new(),
            connection_handle,
            connection_token,
            notification_rx: Some(notification_rx),
        })
    }

    pub fn take_notification_rx(&mut self) -> Option<mpsc::UnboundedReceiver<Notification>>
    {
        self.notification_rx.take()
    }

    pub async fn close(self) -> Result<()>
    {
        debug!("closing database connection");
        self.connection_token.cancel();
        self.connection_handle.await?;
        Ok(())
    }

    pub async fn get_property(&self, name: &str) -> Result<Option<String>>
    {
        let stmt = self.get_property_stmt.get_or_try_init(|| {
            self.client.prepare(/*sql*/ r"
                SELECT value FROM scree_properties WHERE name = $1
            ")
        }).await?;
        let row_opt = self.client.query_opt(stmt, &[&name]).await?;
        let value_opt = row_opt.map(|row| row.try_get(0)).transpose()?;
        Ok(value_opt)
    }

    #[allow(unused)]  // TODO: remove
    pub async fn set_property(&self, name: &str, value: &str) -> Result<()>
    {
        let stmt = self.set_property_stmt.get_or_try_init(|| {
            self.client.prepare(/*sql*/ r"
                INSERT OR REPLACE INTO scree_properties (name, value) VALUES ($1, $2)
            ")
        }).await?;
        self.client.execute(stmt, &[&name, &value]).await?;
        Ok(())
    }

    #[allow(unused)]  // TODO: remove
    pub async fn delete_property(&self, name: &str) -> Result<()>
    {
        let stmt = self.delete_property_stmt.get_or_try_init(|| {
            self.client.prepare(/*sql*/ r"
                DELETE FROM scree_properties WHERE name = $1
            ")
        }).await?;
        self.client.execute(stmt, &[&name]).await?;
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
        self.client.batch_execute(/*sql*/ r#"
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

        let create_property = t.prepare(/*sql*/ r"
            INSERT INTO scree_properties (name, value) VALUES ($1, $2)
        ").await?;
        t.execute(&create_property, &[&"schema_version", &Self::SCHEMA_VERSION.to_string()]).await?;
        t.execute(&create_property, &[&"created_at", &chrono::offset::Utc::now().to_rfc3339()]).await?;

        t.batch_execute(/*sql*/ r#"

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
        debug!("database schema version: {}", schema_version);

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
