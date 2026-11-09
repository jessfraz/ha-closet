//! Typed tracking commands on the same authenticated transport as the catalog.

use std::{
    fs::OpenOptions,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use clap::{Args, Subcommand};
use closet::{
    client::{ApiClient, Failure, Output, response},
    tracking::analytics::AnalyticsReport,
    tracking::models::{
        AreaId, CommandResult, CoverageHistoryPage, EnrollmentPurpose, Epc, HistoryPage,
        IngestResult, ObservationBatch, PortId, SessionId, TrackingCommand, TrackingConfig,
        TrackingExport, TrackingSnapshot, UnitId, UnitStatus,
    },
};
use reqwest::Method;
use serde::{Serialize, de::DeserializeOwned};

type Result<T> = std::result::Result<T, Failure>;
const MAX_INPUT_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Args)]
pub struct Mutation {
    /// Quoted tracking ETag, independent of the catalog revision.
    #[arg(long, value_parser = crate::parse_revision)]
    if_match: String,
    #[arg(long, value_parser = crate::parse_key)]
    idempotency_key: String,
}

#[derive(Subcommand)]
pub enum Command {
    /// Read unit statuses, enrollment progress and tracking health.
    Status,
    #[command(subcommand)]
    Config(Config),
    /// Read one bounded history page; raw bindings are private.
    History {
        #[arg(long)]
        unit: Option<UnitId>,
        #[arg(long)]
        before: Option<i64>,
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u16).range(1..=200))]
        limit: u16,
    },
    /// Read verified observation intervals; gaps are not evidence of neglect.
    Coverage {
        #[arg(long)]
        before: Option<i64>,
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u16).range(1..=200))]
        limit: u16,
    },
    /// Read observed wear, trends and eligible overlooked pieces.
    Analytics {
        #[arg(long, default_value_t = 90, value_parser = clap::value_parser!(u16).range(30..=365))]
        days: u16,
        #[arg(long, default_value_t = 0)]
        pick_seed: u64,
    },
    /// Write a new owner-only export file. Never overwrites an existing path.
    Export {
        #[arg(long)]
        output: PathBuf,
    },
    /// Restore a validated operational export, not catalog metadata.
    Import {
        #[arg(long)]
        input: PathBuf,
        #[command(flatten)]
        mutation: Mutation,
    },
    /// Apply any typed tracking command, including installation, undo and pause.
    Apply {
        #[arg(long)]
        input: PathBuf,
        #[command(flatten)]
        mutation: Mutation,
    },
    /// Submit a bounded normalized live observation batch; sequence deduplicates.
    Observe {
        #[arg(long)]
        input: PathBuf,
    },
    #[command(subcommand)]
    Enroll(Enroll),
    /// Open an isolated scan-to-identify session, without pairing or movement.
    Lookup {
        #[arg(long)]
        port: PortId,
        #[arg(long)]
        borrow: bool,
        #[command(flatten)]
        mutation: Mutation,
    },
    /// Establish a physical-area baseline and resume with fresh evidence.
    Baseline {
        unit: UnitId,
        #[arg(long)]
        area: AreaId,
        #[command(flatten)]
        mutation: Mutation,
    },
    /// Correct status without pretending that a radio observation occurred.
    Correct {
        unit: UnitId,
        #[arg(long, value_parser = parse_status)]
        status: UnitStatus,
        #[arg(long)]
        note: String,
        #[command(flatten)]
        mutation: Mutation,
    },
    /// Remove a tag; mark a mistaken pairing to correct its analytics attribution.
    Unbind {
        unit: UnitId,
        #[arg(long)]
        note: String,
        #[arg(long)]
        mistaken: bool,
        #[command(flatten)]
        mutation: Mutation,
    },
    Retire {
        unit: UnitId,
        #[arg(long)]
        note: String,
        #[command(flatten)]
        mutation: Mutation,
    },
    Undo {
        event: i64,
        #[arg(long)]
        note: String,
        #[command(flatten)]
        mutation: Mutation,
    },
    /// Replay in a new in-memory simulator. No URL, credentials or production DB.
    Replay {
        #[arg(long)]
        input: PathBuf,
    },
}

#[derive(Subcommand)]
pub enum Config {
    /// Structural validation only; RF capabilities require reader commissioning.
    Validate {
        #[arg(long)]
        input: PathBuf,
    },
    Apply {
        #[arg(long)]
        input: PathBuf,
        #[command(flatten)]
        mutation: Mutation,
    },
}

#[derive(Subcommand)]
pub enum Enroll {
    Start {
        #[arg(required = true, num_args = 1..)]
        units: Vec<UnitId>,
        #[arg(long)]
        port: PortId,
        #[arg(long)]
        borrow: bool,
        #[arg(long)]
        replace: bool,
        #[arg(long, default_value_t = 900)]
        ttl_seconds: u32,
        #[command(flatten)]
        mutation: Mutation,
    },
    /// Discard old observations and start a fresh capture cycle.
    Capture {
        session: SessionId,
        #[command(flatten)]
        mutation: Mutation,
    },
    /// Resume the saved queue with a new isolated capture, never old candidates.
    Resume {
        session: SessionId,
        #[arg(long, default_value_t = 900)]
        ttl_seconds: u32,
        #[command(flatten)]
        mutation: Mutation,
    },
    /// Confirm only the server's fresh isolated candidate and selected unit.
    Confirm {
        session: SessionId,
        unit: UnitId,
        epc: Epc,
        #[arg(long)]
        capture_version: u32,
        #[arg(long)]
        baseline_area: Option<AreaId>,
        #[command(flatten)]
        mutation: Mutation,
    },
    Skip {
        session: SessionId,
        unit: UnitId,
        #[arg(long)]
        capture_version: u32,
        #[command(flatten)]
        mutation: Mutation,
    },
    Finish {
        session: SessionId,
        #[command(flatten)]
        mutation: Mutation,
    },
}

pub enum Prepared {
    Status,
    Coverage {
        before: Option<i64>,
        limit: u16,
    },
    Analytics {
        days: u16,
        pick_seed: u64,
    },
    History {
        unit: Option<UnitId>,
        before: Option<i64>,
        limit: u16,
    },
    Export(PathBuf),
    Observe(Box<ObservationBatch>),
    Mutate {
        command: Box<TrackingCommand>,
        mutation: Mutation,
    },
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let mut bytes = Vec::new();
    if path.as_os_str() == "-" {
        std::io::stdin()
            .take(MAX_INPUT_BYTES + 1)
            .read_to_end(&mut bytes)
    } else {
        std::fs::File::open(path)
            .map_err(|_| Failure::local("tracking_input_read_failed"))?
            .take(MAX_INPUT_BYTES + 1)
            .read_to_end(&mut bytes)
    }
    .map_err(|_| Failure::local("tracking_input_read_failed"))?;
    if bytes.len() as u64 > MAX_INPUT_BYTES {
        return Err(Failure::local("tracking_input_too_large"));
    }
    serde_json::from_slice(&bytes).map_err(|_| Failure::local("invalid_tracking_input"))
}

pub fn parse_status(value: &str) -> std::result::Result<UnitStatus, &'static str> {
    match value {
        "unknown" => Ok(UnitStatus::Unknown),
        "clean_ready" => Ok(UnitStatus::CleanReady),
        "worn_ready" => Ok(UnitStatus::WornReady),
        "needs_laundry" => Ok(UnitStatus::NeedsLaundry),
        "queued_dry_cleaning" => Ok(UnitStatus::QueuedDryCleaning),
        "being_worn" => Ok(UnitStatus::BeingWorn),
        "in_wash" => Ok(UnitStatus::InWash),
        "at_cleaners" => Ok(UnitStatus::AtCleaners),
        _ => Err("expected a documented garment status"),
    }
}

pub async fn offline(command: &Command) -> Option<Result<()>> {
    match command {
        Command::Config(Config::Validate { input }) => Some((|| {
            let config: TrackingConfig = read_json(input)?;
            closet::tracking::engine::validate_config(&config)
                .map_err(|_| Failure::local("invalid_tracking_config"))?;
            #[derive(Serialize)]
            struct Validated {
                valid: bool,
                schema_version: u16,
                readers: usize,
                areas: usize,
                ports: usize,
                rf_commissioning_required: bool,
            }
            crate::write_json(
                std::io::stdout().lock(),
                &Output {
                    status: None,
                    etag: None,
                    data: Validated {
                        valid: true,
                        schema_version: config.schema_version,
                        readers: config.readers.len(),
                        areas: config.areas.len(),
                        ports: config.ports.len(),
                        rf_commissioning_required: true,
                    },
                },
            )
        })()),
        Command::Replay { input } => Some(replay(input).await),
        _ => None,
    }
}

pub fn prepare(command: &Command) -> Result<Prepared> {
    let mutate = |command, mutation: &Mutation| {
        Ok(Prepared::Mutate {
            command: Box::new(command),
            mutation: mutation.clone(),
        })
    };
    match command {
        Command::Status => Ok(Prepared::Status),
        Command::Coverage { before, limit } => Ok(Prepared::Coverage {
            before: *before,
            limit: *limit,
        }),
        Command::Analytics { days, pick_seed } => Ok(Prepared::Analytics {
            days: *days,
            pick_seed: *pick_seed,
        }),
        Command::History {
            unit,
            before,
            limit,
        } => Ok(Prepared::History {
            unit: *unit,
            before: *before,
            limit: *limit,
        }),
        Command::Export { output } => Ok(Prepared::Export(output.clone())),
        Command::Observe { input } => Ok(Prepared::Observe(read_json(input)?)),
        Command::Import { input, mutation } => mutate(
            TrackingCommand::Import {
                data: Box::new(read_json(input)?),
            },
            mutation,
        ),
        Command::Apply { input, mutation } => mutate(read_json(input)?, mutation),
        Command::Config(Config::Apply { input, mutation }) => {
            let config = read_json(input)?;
            closet::tracking::engine::validate_config(&config)
                .map_err(|_| Failure::local("invalid_tracking_config"))?;
            mutate(TrackingCommand::Configure { config }, mutation)
        }
        Command::Enroll(enroll) => match enroll {
            Enroll::Start {
                units,
                port,
                borrow,
                replace,
                ttl_seconds,
                mutation,
            } => mutate(
                TrackingCommand::StartEnrollment {
                    unit_ids: units.clone(),
                    port_id: port.clone(),
                    borrow: *borrow,
                    purpose: if *replace {
                        EnrollmentPurpose::Replace
                    } else {
                        EnrollmentPurpose::Bind
                    },
                    ttl_seconds: *ttl_seconds,
                },
                mutation,
            ),
            Enroll::Capture { session, mutation } => mutate(
                TrackingCommand::ResetCapture {
                    session_id: *session,
                },
                mutation,
            ),
            Enroll::Resume {
                session,
                ttl_seconds,
                mutation,
            } => mutate(
                TrackingCommand::ResumeEnrollment {
                    session_id: *session,
                    ttl_seconds: *ttl_seconds,
                },
                mutation,
            ),
            Enroll::Confirm {
                session,
                unit,
                epc,
                capture_version,
                baseline_area,
                mutation,
            } => mutate(
                TrackingCommand::SelectCandidate {
                    session_id: *session,
                    capture_version: *capture_version,
                    unit_id: *unit,
                    epc: epc.clone(),
                    baseline_area: baseline_area.clone(),
                },
                mutation,
            ),
            Enroll::Skip {
                session,
                unit,
                capture_version,
                mutation,
            } => mutate(
                TrackingCommand::Skip {
                    session_id: *session,
                    capture_version: *capture_version,
                    unit_id: *unit,
                },
                mutation,
            ),
            Enroll::Finish { session, mutation } => mutate(
                TrackingCommand::AbortEnrollment {
                    session_id: *session,
                },
                mutation,
            ),
        },
        Command::Lookup {
            port,
            borrow,
            mutation,
        } => mutate(
            TrackingCommand::StartEnrollment {
                unit_ids: Vec::new(),
                port_id: port.clone(),
                borrow: *borrow,
                purpose: EnrollmentPurpose::Lookup,
                ttl_seconds: 600,
            },
            mutation,
        ),
        Command::Baseline {
            unit,
            area,
            mutation,
        } => mutate(
            TrackingCommand::Baseline {
                unit_id: *unit,
                area_id: area.clone(),
            },
            mutation,
        ),
        Command::Correct {
            unit,
            status,
            note,
            mutation,
        } => mutate(
            TrackingCommand::Correct {
                unit_id: *unit,
                status: *status,
                note: note.clone(),
            },
            mutation,
        ),
        Command::Unbind {
            unit,
            note,
            mistaken,
            mutation,
        } => mutate(
            TrackingCommand::Unbind {
                unit_id: *unit,
                note: note.clone(),
                mistaken: *mistaken,
            },
            mutation,
        ),
        Command::Retire {
            unit,
            note,
            mutation,
        } => mutate(
            TrackingCommand::Retire {
                unit_id: *unit,
                note: note.clone(),
            },
            mutation,
        ),
        Command::Undo {
            event,
            note,
            mutation,
        } => mutate(
            TrackingCommand::Undo {
                event_id: *event,
                note: note.clone(),
            },
            mutation,
        ),
        Command::Config(Config::Validate { .. }) | Command::Replay { .. } => {
            Err(Failure::local("invalid_arguments"))
        }
    }
}

pub async fn send(api: &ApiClient, prepared: Prepared) -> Result<()> {
    match prepared {
        Prepared::Status => {
            crate::send::<TrackingSnapshot>(api.request(Method::GET, "/tracking")?).await
        }
        Prepared::History {
            unit,
            before,
            limit,
        } => {
            let path = unit.map_or_else(
                || "/tracking/history".to_owned(),
                |id| format!("/tracking/units/{id}/history"),
            );
            let mut query = vec![("limit", limit.to_string())];
            if let Some(before) = before {
                query.push(("before", before.to_string()));
            }
            crate::send::<HistoryPage>(api.request(Method::GET, &path)?.query(&query)).await
        }
        Prepared::Coverage { before, limit } => {
            let mut query = vec![("limit", limit.to_string())];
            if let Some(before) = before {
                query.push(("before", before.to_string()));
            }
            crate::send::<CoverageHistoryPage>(
                api.request(Method::GET, "/tracking/coverage")?
                    .query(&query),
            )
            .await
        }
        Prepared::Analytics { days, pick_seed } => {
            crate::send::<AnalyticsReport>(api.request(Method::GET, "/tracking/analytics")?.query(
                &[
                    ("days", days.to_string()),
                    ("pick_seed", pick_seed.to_string()),
                ],
            ))
            .await
        }
        Prepared::Observe(batch) => {
            crate::send::<IngestResult>(
                api.request(Method::POST, "/tracking/observations")?
                    .json(&batch),
            )
            .await
        }
        Prepared::Mutate { command, mutation } => {
            crate::send::<CommandResult>(
                api.request(Method::POST, "/tracking/commands")?
                    .header("If-Match", mutation.if_match)
                    .header("Idempotency-Key", mutation.idempotency_key)
                    .json(&command),
            )
            .await
        }
        Prepared::Export(output) => {
            let result =
                response::<TrackingExport>(api.request(Method::GET, "/tracking/export")?).await?;
            export_file(&output, &result.data)?;
            #[derive(Serialize)]
            struct Exported {
                exported: bool,
                schema_version: u16,
                tracking_revision: i64,
                units: usize,
            }
            crate::write_json(
                std::io::stdout().lock(),
                &Output {
                    status: result.status,
                    etag: result.etag,
                    data: Exported {
                        exported: true,
                        schema_version: result.data.schema_version,
                        tracking_revision: result.data.tracking_revision,
                        units: result.data.units.len(),
                    },
                },
            )
        }
    }
}

fn export_file(output: &Path, data: &TrackingExport) -> Result<()> {
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(".closet-export-{}.tmp", uuid::Uuid::now_v7()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|_| Failure::local("tracking_export_create_failed"))?;
    let result = (|| {
        serde_json::to_writer(&mut file, data)
            .map_err(|_| Failure::local("tracking_export_write_failed"))?;
        file.write_all(b"\n")
            .and_then(|()| file.sync_all())
            .map_err(|_| Failure::local("tracking_export_write_failed"))?;
        // Publish only complete bytes, atomically and without clobbering an
        // existing export or following a destination symlink.
        std::fs::hard_link(&temporary, output)
            .map_err(|_| Failure::local("tracking_export_create_failed"))?;
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| Failure::local("tracking_export_write_failed"))
    })();
    let cleanup = std::fs::remove_file(&temporary)
        .map_err(|_| Failure::local("tracking_export_cleanup_failed"));
    result.and(cleanup)
}

async fn replay(input: &Path) -> Result<()> {
    let scenario = read_json(input)?;
    let report = closet::replay::run(scenario)
        .await
        .map_err(|_| Failure::local("tracking_replay_failed"))?;
    crate::write_json(
        std::io::stdout().lock(),
        &Output {
            status: None,
            etag: None,
            data: report,
        },
    )
}
