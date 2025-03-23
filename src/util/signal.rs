use std::time::Duration;

use anyhow::{Context, Result};
use tokio::signal::unix::{signal, SignalKind};
use tokio::time::sleep;
use tokio_stream::{StreamMap, StreamExt};
use tokio_stream::wrappers::SignalStream;
use tracing::{info, warn, error};

fn install_signal_handler<'a>(signals: &mut StreamMap<&'a str, SignalStream>, name: &'a str, sig_kind: SignalKind) -> Result<()>
{
    let sig = signal(sig_kind).with_context(|| format!("unable to install signal handler for {}", name))?;
    signals.insert(name, SignalStream::new(sig));
    Ok(())
}

pub fn install_signal_handlers<F>(exit_handler: F) -> Result<()>
    where F: FnOnce() + Send + 'static
{
    let mut signals = StreamMap::new();
    install_signal_handler(&mut signals, "SIGINT", SignalKind::interrupt())?;
    install_signal_handler(&mut signals, "SIGTERM", SignalKind::terminate())?;
    install_signal_handler(&mut signals, "SIGHUP", SignalKind::hangup())?;
    install_signal_handler(&mut signals, "SIGALRM", SignalKind::alarm())?;
    install_signal_handler(&mut signals, "SIGUSR1", SignalKind::user_defined1())?;
    install_signal_handler(&mut signals, "SIGUSR2", SignalKind::user_defined2())?;

    tokio::spawn(async move {
        tokio::select! {
            Some((sig, ())) = signals.next() =>
                info!("Received signal: {}", sig),
            else =>
                error!("All signal streams are closed. This should never happen."),
        };
        // Run the exit handler first.
        exit_handler();
        // Fail-safe in case of stuck cleanup:
        // wait a few seconds, and then abort on subsequent signals.
        let delay_secs = 5;
        let delay = Duration::from_secs(delay_secs);
        loop {
            tokio::select! {
                // Need to consume any signals that appear during the delay
                _ = signals.next() => { /* do nothing */ }
                _ = sleep(delay) => break,
            };
        }
        warn!("Clean-up is taking longer than expected. Re-installing terminating signal handlers in {delay_secs} seconds.");
        // We do a second delay instead of re-installing immediately to warn users that are spamming Ctrl+C
        loop {
            tokio::select! {
                // Need to consume any signals that appear during the delay
                _ = signals.next() => { /* do nothing */ }
                _ = sleep(delay) => break,
            };
        }
        warn!("Re-installed terminating signal handlers.");
        tokio::select! {
            Some((sig, ())) = signals.next() =>
                warn!("Received signal: {} ... terminating process.", sig),
            else =>
                error!("All signal streams are closed. This should never happen. Terminating process."),
        };
        std::process::abort();
    });

    Ok(())
}
