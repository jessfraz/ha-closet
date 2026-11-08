//! Own reader tasks until their bounded RF/configuration cleanup has finished.

use std::{collections::BTreeMap, io::Read, path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use closet::{
    store::Store,
    tracking::{
        models::{
            CoverageProof, DataSource, ObservationBatch, ObservationPurpose, PortConfig,
            PortCoverage, ReaderConfig, ReaderConnection, ReaderFault, ReaderId,
        },
        reader::{
            EnvironmentSecrets, ReaderError, ReaderOptions, ReaderSecrets, SystemReaderClock,
            llrp::run_llrp, run_iot,
        },
    },
};
use tokio::{sync::watch, task::JoinHandle};

struct FileSecrets(BTreeMap<String, String>);
impl ReaderSecrets for FileSecrets {
    fn resolve(&self, name: &str) -> std::result::Result<String, ReaderError> {
        self.0.get(name).cloned().ok_or(ReaderError::Credentials)
    }
}

pub fn options(secrets: Option<&Path>, ca: Option<&Path>) -> Result<ReaderOptions> {
    let secrets: Arc<dyn ReaderSecrets> = if let Some(path) = secrets {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata =
                std::fs::symlink_metadata(path).context("read reader credential permissions")?;
            ensure!(
                metadata.is_file() && metadata.permissions().mode() & 0o077 == 0,
                "reader credentials must be an owner-only regular file"
            );
        }
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .context("open reader credentials")?
            .take(65_537)
            .read_to_end(&mut bytes)
            .context("read reader credentials")?;
        ensure!(bytes.len() <= 65_536, "reader credential file too large");
        let values: BTreeMap<String, String> = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid reader credential file"))?;
        ensure!(
            values.len() <= 64
                && values.iter().all(|(key, value)| !key.is_empty()
                    && key.len() <= 128
                    && !value.is_empty()
                    && value.len() <= 4096),
            "invalid reader credential entries"
        );
        Arc::new(FileSecrets(values))
    } else {
        Arc::new(EnvironmentSecrets)
    };
    let ca_certificate = if let Some(path) = ca {
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .context("open reader CA")?
            .take(65_537)
            .read_to_end(&mut bytes)
            .context("read reader CA")?;
        ensure!(bytes.len() <= 65_536, "reader CA file too large");
        Some(reqwest::Certificate::from_pem(&bytes).context("invalid reader CA")?)
    } else {
        None
    };
    Ok(ReaderOptions {
        secrets,
        ca_certificate,
        ..ReaderOptions::default()
    })
}

struct Worker {
    stop: watch::Sender<bool>,
    task: JoinHandle<Result<()>>,
}

async fn drain(workers: &mut BTreeMap<ReaderId, Worker>) -> Result<()> {
    for worker in workers.values() {
        let _ = worker.stop.send(true);
    }
    // Each adapter bounds transport and restore operations internally. Await all
    // owners even when one panics, so SQLite never closes underneath another.
    let mut failed = false;
    for (_, worker) in std::mem::take(workers) {
        failed |= !matches!(worker.task.await, Ok(Ok(())));
    }
    ensure!(!failed, "reader worker failed during shutdown");
    Ok(())
}

async fn failed_reader(store: &Store, reader: &ReaderConfig, ports: &[PortConfig]) {
    let now = chrono::Utc::now();
    let batch = ObservationBatch {
        reader_id: reader.id.clone(),
        purpose: ObservationPurpose::Inventory,
        data_source: DataSource::Live,
        capture_session: None,
        session: uuid::Uuid::now_v7(),
        sequence: 0,
        observed_at: now,
        connected: false,
        observations: Vec::new(),
        readback: None,
        coverage: ports
            .iter()
            .filter(|port| port.enabled)
            .map(|port| PortCoverage {
                port_id: port.id.clone(),
                started_at: now,
                ended_at: now,
                proof: CoverageProof::Fault {
                    code: ReaderFault::RecoveryRequired,
                },
            })
            .collect(),
    };
    let _ = store.tracking_ingest_clocked(batch, chrono::Utc::now).await;
}

pub async fn run(
    store: Store,
    options: ReaderOptions,
    logger: slog::Logger,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut workers = BTreeMap::new();
    let mut active_version = None;
    let mut ticks = tokio::time::interval(Duration::from_millis(250));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut configuration = tokio::time::interval(Duration::from_secs(1));
    configuration.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let outcome = loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { break Ok(()); } }
            _ = ticks.tick() => {
                if store.tracking_tick_clocked(chrono::Utc::now).await.is_err() { break Err(anyhow::anyhow!("tracking clock worker failed")); }
            }
            _ = configuration.tick() => {
                let (version, config) = match store.tracking_runtime_config_clocked(chrono::Utc::now).await {
                    Ok(value) => value,
                    Err(_) => break Err(anyhow::anyhow!("tracking configuration unavailable")),
                };
                if active_version == Some(version) { continue; }
                if drain(&mut workers).await.is_err() {
                    // Durable claims keep the old connection reachable. Start
                    // its guarded recovery owner below instead of abandoning
                    // cleanup because the first network attempt failed.
                    slog::warn!(logger, "reader_cleanup_pending");
                }
                let snapshot = match store.tracking_snapshot_clocked(chrono::Utc::now).await {
                    Ok(snapshot) => snapshot,
                    Err(_) => break Err(anyhow::anyhow!("tracking recovery state unavailable")),
                };
                if snapshot.configuration_version != version { continue; }
                // Disabling tracking must not orphan a reader program if the
                // process died after committing the configuration but before
                // its old worker restored the device. Disabled recovery workers
                // may only release proven-owned resources, never start inventory.
                let recovery: std::collections::BTreeSet<_> = snapshot.reader_operations.iter()
                    .filter(|operation| operation.requires_cleanup()).map(|operation| operation.reader_id.clone())
                    .chain(snapshot.reader_controls.iter().filter(|operation| operation.requires_cleanup()).map(|operation| operation.reader_id.clone())).collect();
                active_version = Some(version);
                for reader in config.readers.into_iter().filter(|reader| config.enabled && reader.enabled || recovery.contains(&reader.id)) {
                    if matches!(reader.connection, ReaderConnection::Simulator) { continue; }
                    let ports: Vec<_> = config.ports.iter().filter(|port| port.reader_id == reader.id).cloned().collect();
                    let (stop, stopped) = watch::channel(false);
                    let worker_store = store.clone(); let worker_options = options.clone(); let worker_logger = logger.clone();
                    let reader_id = reader.id.clone();
                    let task = tokio::spawn(async move {
                        let clock = SystemReaderClock::default();
                        let result = match &reader.connection {
                            ReaderConnection::ImpinjIot { .. } => run_iot(&worker_store, &reader, &ports, worker_options, &clock, stopped).await,
                            ReaderConnection::ImpinjLlrp { .. } => run_llrp(&worker_store, &reader, &ports, &clock, stopped).await,
                            ReaderConnection::Simulator => Ok(()),
                        };
                        if let Err(error) = result {
                            failed_reader(&worker_store, &reader, &ports).await;
                            // ReaderError contains only fixed codes, never
                            // transport payloads or resolved credentials.
                            slog::warn!(worker_logger, "reader_worker_stopped"; "code" => error.to_string());
                            if matches!(error, ReaderError::RecoveryRequired) {
                                return Err(anyhow::anyhow!("reader configuration recovery required"));
                            }
                        }
                        Ok(())
                    });
                    workers.insert(reader_id, Worker { stop, task });
                }
            }
        }
    };
    let cleanup = drain(&mut workers).await;
    outcome.and(cleanup)
}

#[cfg(test)]
mod tests {
    use crate::tracking_runtime::options;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn private_resolver_never_falls_back_to_process_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("reader.json");
        std::fs::write(&path, r#"{"READER_USER":"fixture"}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let configured = options(Some(&path), None).unwrap();
        assert_eq!(
            configured.secrets.resolve("READER_USER").unwrap(),
            "fixture"
        );
        assert!(configured.secrets.resolve("PATH").is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(options(Some(&path), None).is_err());
    }
}
