//! Deterministic semantic replay in a new, unreachable in-memory SQLite store.
//! There is deliberately no database path, HTTP destination or reader connection.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    models::{CatalogItem, SyncRequest},
    store::Store,
    tracking::models::{
        AreaId, DataSource, DecisionReason, EnrollmentPurpose, Epc, Installation, ObservationBatch,
        PortId, SessionId, TrackingCommand, TrackingConfig, TrackingError, UnitId, UnitStatus,
    },
};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub schema_version: u16,
    pub started_at: DateTime<Utc>,
    pub catalog: Vec<CatalogItem>,
    pub config: TrackingConfig,
    pub steps: Vec<Step>,
}

#[derive(Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnitSelector {
    pub catalog_id: String,
    pub ordinal: u32,
}

#[derive(Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    Observe {
        at: DateTime<Utc>,
        batch: ObservationBatch,
        capture: Option<String>,
    },
    Tick {
        at: DateTime<Utc>,
    },
    Restart {
        at: DateTime<Utc>,
    },
    Configure {
        at: DateTime<Utc>,
        config: TrackingConfig,
    },
    Capture {
        at: DateTime<Utc>,
        name: String,
        unit: Option<UnitSelector>,
        port: PortId,
        purpose: EnrollmentPurpose,
        borrow: bool,
    },
    Confirm {
        at: DateTime<Utc>,
        capture: String,
        epc: Epc,
        baseline_area: Option<AreaId>,
    },
    Skip {
        at: DateTime<Utc>,
        capture: String,
    },
    Finish {
        at: DateTime<Utc>,
        capture: String,
    },
    Baseline {
        at: DateTime<Utc>,
        unit: UnitSelector,
        area: AreaId,
    },
    Correct {
        at: DateTime<Utc>,
        unit: UnitSelector,
        status: UnitStatus,
        note: String,
    },
    Installation {
        at: DateTime<Utc>,
        unit: UnitSelector,
        installation: Installation,
    },
    Retire {
        at: DateTime<Utc>,
        unit: UnitSelector,
        note: String,
    },
    Unbind {
        at: DateTime<Utc>,
        unit: UnitSelector,
        note: String,
        #[serde(default)]
        mistaken: bool,
    },
    Pause {
        at: DateTime<Utc>,
        unit: UnitSelector,
        paused: bool,
    },
}
impl Step {
    fn at(&self) -> DateTime<Utc> {
        match self {
            Self::Observe { at, .. }
            | Self::Tick { at }
            | Self::Restart { at }
            | Self::Configure { at, .. }
            | Self::Capture { at, .. }
            | Self::Confirm { at, .. }
            | Self::Skip { at, .. }
            | Self::Finish { at, .. }
            | Self::Baseline { at, .. }
            | Self::Correct { at, .. }
            | Self::Installation { at, .. }
            | Self::Retire { at, .. }
            | Self::Unbind { at, .. }
            | Self::Pause { at, .. } => *at,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitState {
    pub catalog_id: String,
    pub ordinal: u32,
    pub status: UnitStatus,
    pub ready_to_wear: bool,
    pub last_confirmed_area: Option<AreaId>,
    pub retired: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    pub catalog_id: String,
    pub ordinal: u32,
    pub at: DateTime<Utc>,
    pub previous: Option<UnitStatus>,
    pub next: UnitStatus,
    pub reason: DecisionReason,
    pub automatic: bool,
    pub configuration_version: i64,
    pub source_area: Option<AreaId>,
}
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u16,
    pub simulated: bool,
    pub steps: usize,
    pub catalog_revision: i64,
    pub units: Vec<UnitState>,
    pub transitions: Vec<Transition>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid_replay")]
    Invalid,
    #[error("tracking_operation_failed")]
    Tracking(#[from] TrackingError),
    #[error("catalog_operation_failed")]
    Store(#[from] crate::store::StoreError),
    #[error("replay_serialization_failed")]
    Serialization(#[from] serde_json::Error),
}

pub async fn run(scenario: Scenario) -> Result<Report, Error> {
    if scenario.schema_version != 1 || scenario.steps.len() > 10_000 {
        return Err(Error::Invalid);
    }
    crate::tracking::engine::validate_config(&scenario.config)?;
    let store = Store::open_simulator(scenario.started_at).await?;
    // Even a failed scenario closes the only SQLite handle. No configured API
    // origin, production database or hardware adapter is ever constructed.
    let result = execute(&store, scenario).await;
    store.close().await;
    result
}

async fn mutate(
    store: &Store,
    at: DateTime<Utc>,
    key: &str,
    command: TrackingCommand,
) -> Result<crate::tracking::models::CommandResult, Error> {
    let revision = store.tracking_snapshot(at).await?.tracking_revision;
    Ok(store
        .tracking_command("simulator", key, revision, command, at)
        .await?)
}

async fn unit_id(
    store: &Store,
    at: DateTime<Utc>,
    selector: &UnitSelector,
) -> Result<UnitId, Error> {
    store
        .tracking_snapshot(at)
        .await?
        .units
        .iter()
        .find(|unit| unit.catalog_id == selector.catalog_id && unit.ordinal == selector.ordinal)
        .map(|unit| unit.id)
        .ok_or(Error::Invalid)
}

async fn session(
    store: &Store,
    at: DateTime<Utc>,
    captures: &BTreeMap<String, SessionId>,
    name: &str,
) -> Result<crate::tracking::models::EnrollmentSession, Error> {
    let id = captures.get(name).ok_or(Error::Invalid)?;
    store
        .tracking_snapshot(at)
        .await?
        .sessions
        .into_iter()
        .find(|session| session.id == *id)
        .ok_or(Error::Invalid)
}

async fn execute(store: &Store, scenario: Scenario) -> Result<Report, Error> {
    store
        .sync_at(
            SyncRequest {
                items: scenario.catalog,
                dry_run: false,
                allow_empty: true,
            },
            Some(0),
            "simulator",
            Some("seed-catalog"),
            scenario.started_at,
        )
        .await?;
    mutate(
        store,
        scenario.started_at,
        "seed-config",
        TrackingCommand::Configure {
            config: scenario.config,
        },
    )
    .await?;
    let steps = scenario.steps.len();
    let mut captures = BTreeMap::new();
    let mut now = scenario.started_at;
    for (index, step) in scenario.steps.into_iter().enumerate() {
        let at = step.at();
        if at < now {
            return Err(Error::Invalid);
        }
        now = at;
        let key = format!("step-{index}");
        let command = match step {
            Step::Observe {
                mut batch, capture, ..
            } => {
                if batch.data_source != DataSource::Simulation {
                    return Err(Error::Invalid);
                }
                if let Some(name) = capture {
                    batch.capture_session = Some(session(store, at, &captures, &name).await?.id);
                }
                store.tracking_ingest(batch, at).await?;
                continue;
            }
            Step::Tick { .. } => {
                store.tracking_tick(at).await?;
                continue;
            }
            Step::Restart { .. } => {
                store.tracking_startup(at).await?;
                continue;
            }
            Step::Configure { config, .. } => TrackingCommand::Configure { config },
            Step::Capture {
                name,
                unit,
                port,
                purpose,
                borrow,
                ..
            } => {
                if name.is_empty() || name.len() > 64 || captures.contains_key(&name) {
                    return Err(Error::Invalid);
                }
                let unit_ids = match unit {
                    Some(selector) => vec![unit_id(store, at, &selector).await?],
                    None => Vec::new(),
                };
                let result = mutate(
                    store,
                    at,
                    &key,
                    TrackingCommand::StartEnrollment {
                        unit_ids,
                        port_id: port,
                        borrow,
                        purpose,
                        ttl_seconds: 900,
                    },
                )
                .await?;
                captures.insert(name, result.session.ok_or(Error::Invalid)?.id);
                continue;
            }
            Step::Confirm {
                capture,
                epc,
                baseline_area,
                ..
            } => {
                let current = session(store, at, &captures, &capture).await?;
                let unit_id = *current.queue.get(current.cursor).ok_or(Error::Invalid)?;
                TrackingCommand::SelectCandidate {
                    session_id: current.id,
                    capture_version: current.capture_version,
                    unit_id,
                    epc,
                    baseline_area,
                }
            }
            Step::Skip { capture, .. } => {
                let current = session(store, at, &captures, &capture).await?;
                TrackingCommand::Skip {
                    session_id: current.id,
                    capture_version: current.capture_version,
                    unit_id: *current.queue.get(current.cursor).ok_or(Error::Invalid)?,
                }
            }
            Step::Finish { capture, .. } => TrackingCommand::AbortEnrollment {
                session_id: session(store, at, &captures, &capture).await?.id,
            },
            Step::Baseline { unit, area, .. } => TrackingCommand::Baseline {
                unit_id: unit_id(store, at, &unit).await?,
                area_id: area,
            },
            Step::Correct {
                unit, status, note, ..
            } => TrackingCommand::Correct {
                unit_id: unit_id(store, at, &unit).await?,
                status,
                note,
            },
            Step::Installation {
                unit, installation, ..
            } => TrackingCommand::Installation {
                unit_id: unit_id(store, at, &unit).await?,
                installation,
            },
            Step::Retire { unit, note, .. } => TrackingCommand::Retire {
                unit_id: unit_id(store, at, &unit).await?,
                note,
            },
            Step::Unbind {
                unit,
                note,
                mistaken,
                ..
            } => TrackingCommand::Unbind {
                unit_id: unit_id(store, at, &unit).await?,
                note,
                mistaken,
            },
            Step::Pause { unit, paused, .. } => TrackingCommand::Pause {
                unit_id: unit_id(store, at, &unit).await?,
                paused,
            },
        };
        mutate(store, at, &key, command).await?;
    }
    let export = store.tracking_export(now).await?;
    let mut units: Vec<_> = export
        .units
        .iter()
        .map(|stored| UnitState {
            catalog_id: stored.unit.catalog_id.clone(),
            ordinal: stored.unit.ordinal,
            status: stored.unit.status,
            ready_to_wear: stored.unit.ready_to_wear(),
            last_confirmed_area: stored.unit.last_confirmed_area.clone(),
            retired: stored.unit.retired,
        })
        .collect();
    units.sort_by(|a, b| (&a.catalog_id, a.ordinal).cmp(&(&b.catalog_id, b.ordinal)));
    let mut transitions: Vec<_> = export
        .history
        .iter()
        .map(|event| Transition {
            catalog_id: event.after.unit.catalog_id.clone(),
            ordinal: event.after.unit.ordinal,
            at: event.at,
            previous: event.before.as_ref().map(|before| before.unit.status),
            next: event.after.unit.status,
            reason: event.reason,
            automatic: event.automatic,
            configuration_version: event.configuration_version,
            source_area: event.source_area.clone(),
        })
        .collect();
    // Database-generated UUIDs are intentionally absent from semantic replay
    // output. Stable catalog ID + non-renumbered ordinal identify each unit.
    transitions.sort_by(|a, b| {
        (a.at, &a.catalog_id, a.ordinal, a.next.as_str()).cmp(&(
            b.at,
            &b.catalog_id,
            b.ordinal,
            b.next.as_str(),
        ))
    });
    Ok(Report {
        schema_version: 1,
        simulated: true,
        steps,
        catalog_revision: store.counts().await?.0,
        units,
        transitions,
    })
}
