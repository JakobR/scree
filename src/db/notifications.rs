use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures::{stream, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_postgres::{AsyncMessage, GenericClient, Notification};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use super::DatabaseConfig;

pub struct Notifications {
    token: CancellationToken,
    handle: JoinHandle<()>,
    command_tx: mpsc::UnboundedSender<Command>,
    notification_rx: Option<mpsc::UnboundedReceiver<Notification>>,
}

impl Notifications {
    pub(super) fn new(config: DatabaseConfig) -> Self
    {
        let token = CancellationToken::new();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (notification_tx, notification_rx) = mpsc::unbounded_channel();

        let handle = tokio::spawn(
            subscription_loop(config, command_rx, notification_tx, token.clone())
        );

        Self { token, handle, command_tx, notification_rx: Some(notification_rx) }
    }

    pub fn take_notification_rx(&mut self) -> Option<mpsc::UnboundedReceiver<Notification>>
    {
        self.notification_rx.take()
    }

    pub async fn close(self) -> Result<()>
    {
        self.token.cancel();
        self.handle.await?;
        Ok(())
    }

    pub async fn subscribe(&self, channel: &str) -> Result<SubscriptionToken>
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx.send(Command::Subscribe(channel.to_string(), reply_tx)).context("subscription loop is not running")?;
        let () = reply_rx.await.context("reply_tx dropped")?.context("unable to subscribe due to database error")?;
        Ok(SubscriptionToken {
            channel: channel.to_string(),
            command_tx: self.command_tx.clone(),
            active: true,
        })
    }
}


#[must_use]
pub struct SubscriptionToken {
    channel: String,
    command_tx: mpsc::UnboundedSender<Command>,
    active: bool,
}

impl SubscriptionToken {
    #[allow(unused)]
    pub async fn unsubscribe(mut self) -> Result<()>
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx.send(Command::Unsubscribe(self.channel.clone(), reply_tx)).context("subscription loop is not running")?;
        let () = reply_rx.blocking_recv().context("reply_tx dropped")?.context("unable to subscribe due to database error")?;
        self.active = false; // so 'Drop' doesn't send the command again
        Ok(())
    }
}

impl Drop for SubscriptionToken {
    fn drop(&mut self)
    {
        if self.active {
            let (reply_tx, reply_rx) = oneshot::channel();
            drop(reply_rx);  // make sure the sender notices we won't listen
            let _ = self.command_tx.send(Command::Unsubscribe(self.channel.clone(), reply_tx));
        }
    }
}



enum Command {
    Subscribe(String, oneshot::Sender<Result<()>>),
    Unsubscribe(String, oneshot::Sender<Result<()>>),
}


struct Backoff {
    prev_attempt: Option<Instant>,
    consecutive_failures: usize,
}

impl Backoff {
    pub const SUCCESS_THRESHOLD: Duration = Duration::from_secs(300);

    pub const FAILURE_DELAYS: [Duration; 5] = [
        Duration::from_secs(1),
        Duration::from_secs(5),
        Duration::from_secs(10),
        Duration::from_secs(30),
        Duration::from_secs(60),
    ];

    pub fn new() -> Self {
        Self {
            prev_attempt: None,
            consecutive_failures: 0,
        }
    }

    pub async fn wait(&mut self, token: &CancellationToken)
    {
        let prev_attempt = self.prev_attempt.clone();
        self.prev_attempt = Some(Instant::now());

        let Some(prev_attempt) = prev_attempt else {
            // first attempt, don't wait
            return;
        };
        let elapsed = prev_attempt.elapsed();

        if elapsed >= Self::SUCCESS_THRESHOLD {
            // we have been alive for some time, retry immediately
            self.consecutive_failures = 0;
            return;
        }

        let delay_idx = self.consecutive_failures.min(Self::FAILURE_DELAYS.len() - 1);
        let delay = Self::FAILURE_DELAYS[delay_idx];

        self.consecutive_failures += 1;

        tokio::select! {
            _ = tokio::time::sleep(delay) => { }
            _ = token.cancelled() => { }
        }
    }
}


struct SubscriptionManager {
    active: HashMap<String, usize>,
}

impl SubscriptionManager {
    pub fn new() -> Self {
        Self { active: HashMap::new() }
    }

    fn is_channel_safe(channel: &str) -> bool
    {
        // postgres doesn't seem to support parameters for LISTEN statements,
        // so we have to check the string before building the statement.
        // this predicate is probably overly restrictive but good enough for now.
        channel.chars().all(|c|
            c == '_' || c.is_ascii_alphabetic()
        )
    }

    pub async fn subscribe(&mut self, db: &impl GenericClient, channel: String) -> Result<()>
    {
        if !Self::is_channel_safe(&channel) {
            bail!("only ASCII letters and '_' are allowed in channel names");
        }
        match self.active.entry(channel) {
            Entry::Occupied(mut entry) => {
                *entry.get_mut() += 1;
            }
            Entry::Vacant(entry) => {
                let channel = entry.key();
                let stmt = format!("LISTEN {}", channel);
                db.batch_execute(&stmt).await?;
                entry.insert(1);
            }
        }
        Ok(())
    }

    pub async fn unsubscribe(&mut self, db: &impl GenericClient, channel: String) -> Result<()>
    {
        match self.active.entry(channel) {
            Entry::Occupied(mut entry) => {
                // channel name has been checked in the corresponding 'subscribe' call.
                let subscribers = entry.get_mut();
                if *subscribers <= 1 {
                    let channel = entry.key();
                    let stmt = format!("UNLISTEN {}", channel);
                    db.batch_execute(&stmt).await?;
                    entry.remove();
                } else {
                    *subscribers -= 1;
                }
                Ok(())
            }
            Entry::Vacant(entry) => {
                bail!("not subscribed to channel '{}'", entry.key());
            }
        }
    }
}


async fn subscription_loop(config: DatabaseConfig, mut command_rx: mpsc::UnboundedReceiver<Command>, notification_tx: mpsc::UnboundedSender<Notification>, token: CancellationToken)
{
    let mut subs = SubscriptionManager::new();
    let mut backoff = Backoff::new();

    loop {
        backoff.wait(&token).await;

        if token.is_cancelled() {
            return;
        }

        let (client, conn) =
            match config.connect_client().await {
                Ok(x) => x,
                Err(e) => {
                    error!("unable to connect to database: {:#}", e);
                    continue;
                }
            };
        let mut poll_handle = tokio::spawn(poll_postgres_connection(conn, token.clone(), notification_tx.clone()));

        loop {
            tokio::select! {
                Some(cmd) = command_rx.recv() => {
                    match cmd {
                        Command::Subscribe(channel, reply_tx) => {
                            let result = subs.subscribe(&client, channel).await;
                            if let Err(Err(e)) = reply_tx.send(result) {
                                // log the error if no one is listening to the reply
                                error!("unable to subscribe: {:#}", e);
                            }
                        }
                        Command::Unsubscribe(channel, reply_tx) => {
                            let result = subs.unsubscribe(&client, channel).await;
                            if let Err(Err(e)) = reply_tx.send(result) {
                                // log the error if no one is listening to the reply
                                error!("unable to unsubscribe: {:#}", e);
                            }
                        }
                    }
                }
                result = &mut poll_handle => {
                    match result {
                        Ok(Ok(())) => {
                            /* stopped */
                            return;
                        }
                        Ok(Err(conn_error)) => {
                            /* connection error, try to reconnect */
                            error!("database connection error: {:#}", conn_error);
                            break;
                        }
                        Err(join_error) => {
                            /* panicked */
                            error!("poll_postgres_connection failed: {:#}", join_error);
                            break;
                        }
                    }
                }
            }
        }
    }
}

// The tokio_postgres::Connection performs the actual communication with the database when it is being polled.
// This should be spawned off into the background.
async fn poll_postgres_connection<S, T>(mut connection: tokio_postgres::Connection<S, T>, token: CancellationToken, notification_tx: mpsc::UnboundedSender<Notification>) -> Result<(), tokio_postgres::Error>
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
                    Some(Ok(AsyncMessage::Notice(notice))) => {
                        // See https://www.postgresql.org/docs/current/protocol-error-fields.html
                        match notice.severity() {
                            "NOTICE" | "DEBUG" | "INFO" | "LOG" =>
                                debug!("{}: {}", notice.severity(), notice.message()),
                            "WARNING" =>
                                warn!("{}: {}", notice.severity(), notice.message()),
                            _ =>
                                // covers ERROR, FATAL, PANIC
                                error!("{}: {}", notice.severity(), notice.message()),
                        };
                    }
                    Some(Ok(other_msg)) =>
                        warn!("unknown database message: {:?}", other_msg),
                    Some(Err(e)) => {
                        return Err(e);
                    }
                    None =>
                        break,
                }
            }
        }
    }
    Ok(())
}
