use anyhow::Result;
use chrono::Utc;

use crate::cli::{AlertCommand, AlertOptions, EmailOptions, Options, TelegramCommand, TelegramEnableOptions, TelegramOptions};
use crate::db;
use crate::db::alert::Alert;

pub async fn execute_command(options: &Options, alert_options: &AlertOptions) -> Result<()>
{
    match &alert_options.command {
        AlertCommand::Email(email_options) => email_command(options, email_options).await?,
        AlertCommand::Telegram(telegram_options) => telegram_command(options, telegram_options).await?,
        AlertCommand::Test => test(options).await?,
    }
    Ok(())
}

async fn email_command(options: &Options, email_options: &EmailOptions) -> Result<()>
{
    todo!("{:?} {:?}", options, email_options)
}

async fn telegram_command(options: &Options, telegram_options: &TelegramOptions) -> Result<()>
{
    match &telegram_options.command {
        TelegramCommand::Enable(telegram_enable_options) => telegram_enable(options, telegram_enable_options).await?,
        TelegramCommand::Disable => telegram_disable(options).await?,
    }
    Ok(())
}

async fn telegram_enable(options: &Options, telegram_enable_options: &TelegramEnableOptions) -> Result<()>
{
    let mut db = db::connect(&options.db).await?;
    let cfg = db::alert::TelegramConfig {
        bot_token: telegram_enable_options.bot_token.clone(),
        chat_id: telegram_enable_options.chat_id.clone(),
    };
    db::alert::set_telegram_config(&mut db.client, &cfg).await?;
    Ok(())
}

async fn telegram_disable(options: &Options) -> Result<()>
{
    let mut db = db::connect(&options.db).await?;
    db::alert::delete_telegram_config(&mut db.client).await?;
    Ok(())
}

async fn test(options: &Options) -> Result<()>
{
    let mut db = db::connect(&options.db).await?;
    let alert = Alert {
        subject: "Test".to_string(),
        message: "This is a test message.".to_string(),
        created_at: Utc::now(),
    };
    db::alert::send(&mut db.client, &alert).await;
    Ok(())
}
