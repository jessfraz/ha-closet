mod config;
mod logging;
mod supervisor;
mod tracking_runtime;

use std::sync::Arc;

use anyhow::{Context as _, Result};
use closet::{api, store::Store, today::TodayConfig};

use crate::{config::Config, supervisor::Supervisor};

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load()?;
    if let Some(path) = &config.openapi {
        std::fs::write(
            path,
            format!("{}\n", serde_json::to_string_pretty(&api::openapi()?)?),
        )?;
        return Ok(());
    }
    let logger = logging::logger();
    let reader_options = tracking_runtime::options(
        config.reader_secrets_file.as_deref(),
        config.reader_ca_file.as_deref(),
    )?;
    let supervisor = Supervisor::from_environment()?;
    let store = Store::open(&config.database).await?;
    let mut context = api::Context::new(
        store.clone(),
        config
            .api_token
            .as_deref()
            .context("owner credential required")?,
    )
    .with_today(TodayConfig::new(
        config.ha_origin,
        config.ha_token,
        &config.timezone,
        config.weather_entity,
        config.ai_task_entity,
    )?);
    if let Some(token) = &config.integration_token {
        context = context.with_integration(token);
    }
    let server = api::start_with_logger(config.bind, Arc::new(context.clone()), logger.clone())?;
    let ingress = config
        .ingress_bind
        .map(|bind| {
            api::start_with_logger(
                bind,
                Arc::new(context.clone().for_ingress(config.ingress_peer)),
                logger.clone(),
            )
        })
        .transpose();
    let ingress = match ingress {
        Ok(ingress) => ingress,
        Err(error) => {
            let _ = server.close().await;
            let _ = context.shutdown().await;
            store.close().await;
            return Err(error);
        }
    };
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    let tracking = tokio::spawn(tracking_runtime::run(
        store.clone(),
        reader_options,
        logger.clone(),
        stopped.clone(),
    ));
    let discovery = if let (Some(supervisor), Some(token)) = (supervisor, config.integration_token)
    {
        Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stopped.changed() => break,
                    result = supervisor.announce(&token) => {
                        if result.is_err() {
                            slog::warn!(logger, "integration_discovery_failed");
                        }
                    }
                }
                tokio::select! {
                    _ = stopped.changed() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {}
                }
            }
        }))
    } else {
        None
    };

    let signal_result = shutdown().await;
    let api_result = server.close().await;
    let ingress_result = if let Some(ingress) = ingress {
        ingress.close().await
    } else {
        Ok(())
    };
    let _ = stop.send(true);
    let discovery_result = if let Some(discovery) = discovery {
        discovery.await.map_err(anyhow::Error::from)
    } else {
        Ok(())
    };
    let tracking_result = tracking
        .await
        .map_err(anyhow::Error::from)
        .and_then(|result| result);
    // Accepted generation work owns its completion state until it drains.
    // Close SQLite only after no request or worker can still publish a result.
    let workers_result = context.shutdown().await;
    store.close().await;
    signal_result?;
    discovery_result?;
    tracking_result?;
    workers_result?;
    api_result.map_err(|_| anyhow::anyhow!("close API listener"))?;
    ingress_result.map_err(|_| anyhow::anyhow!("close ingress listener"))?;
    Ok(())
}

async fn shutdown() -> Result<()> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
