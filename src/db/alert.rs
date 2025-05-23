use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use thiserror::Error;
use tokio_postgres::GenericClient;
use tracing::{debug, error};

use crate::db;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub subject: String,
    pub message: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Email,
    Telegram,
}

impl Channel {
    pub const ALL: [Self; 2] = [Self::Email, Self::Telegram];

    fn as_str(self) -> &'static str {
        match self {
            Channel::Email => "email",
            Channel::Telegram => "telegram",
        }
    }
}

// pub struct EmailConfig {
//     /* TODO */
// }

pub struct TelegramConfig {
    pub bot_token: String,
    pub chat_id: String,
}

// pub async fn get_email_config(db: &mut impl GenericClient) -> Result<Option<EmailConfig>>
// {
//     Ok(None)
// }

pub async fn set_telegram_config(db: &mut impl GenericClient, cfg: &TelegramConfig) -> Result<()>
{
    let t = db.transaction().await?;
    db::set_property(&t, db::property::TELEGRAM_BOT_TOKEN, Some(&cfg.bot_token)).await?;
    db::set_property(&t, db::property::TELEGRAM_CHAT_ID, Some(&cfg.chat_id)).await?;
    t.commit().await?;
    Ok(())
}

pub async fn delete_telegram_config(db: &mut impl GenericClient) -> Result<()>
{
    let t = db.transaction().await?;
    db::set_property(&t, db::property::TELEGRAM_BOT_TOKEN, None).await?;
    db::set_property(&t, db::property::TELEGRAM_CHAT_ID, None).await?;
    t.commit().await?;
    Ok(())
}

pub async fn get_telegram_config(db: &mut impl GenericClient) -> Result<Option<TelegramConfig>>
{
    let t = db.transaction().await?;
    let bot_token = db::get_property(&t, db::property::TELEGRAM_BOT_TOKEN).await?;
    let chat_id = db::get_property(&t, db::property::TELEGRAM_CHAT_ID).await?;
    t.commit().await?;

    let Some(bot_token) = bot_token else { return Ok(None); };
    let Some(chat_id) = chat_id else { return Ok(None); };
    Ok(Some(TelegramConfig { bot_token, chat_id }))
}

pub async fn record(db: &impl GenericClient, channel: &str, alert: &Alert, delivered: bool) -> Result<()>
{
    let delivered_at = if delivered { Some(Utc::now()) } else { None };
    db.execute(/* sql */ r"
        INSERT INTO alert_history (subject, message, channel, created_at, delivered_at) VALUES ($1, $2, $3, $4, $5)
    ", &[&alert.subject, &alert.message, &channel, &alert.created_at, &delivered_at]).await?;
    Ok(())
}

pub async fn send(db: &mut impl GenericClient, alert: &Alert)
{
    for channel in Channel::ALL {
        let result =
            match channel {
                Channel::Email => send_email(db, alert).await,
                Channel::Telegram => send_telegram(db, alert).await,
            };

        let delivered =
            match result {
                Ok(()) => true,
                Err(SendError::NotConfigured) => {
                    debug!("channel '{}' is not configured, skipping", channel.as_str());
                    continue;
                }
                Err(SendError::Other(e)) => {
                    error!("unable to send alert {:?}: {:#}", alert, e);
                    false
                }
            };

        if let Err(e) = record(db, channel.as_str(), alert, delivered).await {
            error!("unable to record alert {:?}: {:#}", alert, e);
        }
    }
}

#[derive(Error, Debug)]
enum SendError {
    #[error("channel has not been configured.")]
    NotConfigured,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

async fn send_email(db: &mut impl GenericClient, alert: &Alert) -> Result<(), SendError>
{
    let _ = db;
    let _ = alert;
    Err(SendError::NotConfigured)
}

async fn send_telegram(db: &mut impl GenericClient, alert: &Alert) -> Result<(), SendError>
{
    let cfg = get_telegram_config(db).await?;
    let Some(cfg) = cfg else { return Err(SendError::NotConfigured); };

    let url = format!("https://api.telegram.org/bot{}/sendMessage", cfg.bot_token);

    let text = format!("<b>{}</b>\n{}", alert.subject, alert.message);

    let client = reqwest::Client::new();
    let response_str = client.get(&url)
        .query(&[
            ("parse_mode", "HTML"),
            ("chat_id", cfg.chat_id.as_str()),
            ("text", text.as_str()),
        ])
        .send().await.context("Telegram GET sendMessage")?
        .text().await.context("Telegram read response body")?;

    debug!("Telegram response: {}", response_str);

    #[derive(Debug, Deserialize)]
    struct Response {
        ok: bool,
        description: Option<String>,
    }

    let r: Response = serde_json::from_str(&response_str).context("Telegram: unable to parse response JSON")?;

    if !r.ok {
        let description = r.description.unwrap_or_else(|| "<unkown error>".to_string());
        return Err(anyhow!("Telegram: failed to send message: {}", description).into());
    }

    Ok(())
}
