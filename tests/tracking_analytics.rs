//! Fictional inventory through the actual reducer, operation journal and SQLite.

use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Utc};
use closet::{
    models::{CatalogItem, SyncRequest},
    store::Store,
    tracking::{analytics::AnalyticsReport, models::*},
};
use uuid::Uuid;

fn at(value: &str) -> DateTime<Utc> {
    value.parse().unwrap()
}

fn id<T: std::str::FromStr>(value: &str) -> T
where
    T::Err: std::fmt::Debug,
{
    value.parse().unwrap()
}

fn item(name: &str, quantity: u32) -> CatalogItem {
    CatalogItem {
        id: name.into(),
        brand: Some("Fictional studio".into()),
        name: name.replace('-', " "),
        category: "clothing.top.shirt".into(),
        color: Some("Blue".into()),
        size: Some("M".into()),
        quantity: Some(quantity),
        material: Some("Cotton".into()),
        sku: None,
        purchase_source: Some("self_bought".into()),
        images: Vec::new(),
        notes: Vec::new(),
        price: None,
        shop: None,
    }
}

fn config() -> TrackingConfig {
    let mut config: TrackingConfig =
        serde_json::from_str(include_str!("../config/tracking.example.json")).unwrap();
    config.enabled = true;
    config.readers.truncate(1);
    config.readers[0].connection = ReaderConnection::Simulator;
    for port in &mut config.ports {
        port.reader_id = config.readers[0].id.clone();
        port.commissioned = true;
        if port.role == PortRole::Enrollment {
            port.antenna = 5;
        }
    }
    config.thresholds = Thresholds {
        positive_dwell_ms: 100,
        positive_min_reads: 2,
        movement_hysteresis_ms: 100,
        absence_ms: 1_000,
        warmup_ms: 100,
        coverage_max_gap_ms: 1_000,
        candidate_fresh_ms: 500,
        reader_event_max_age_ms: 10_000,
    };
    config
}

struct Rig {
    store: Store,
    now: DateTime<Utc>,
    sequence: u64,
    operation: u64,
    items: Vec<CatalogItem>,
    visible: BTreeSet<String>,
}

impl Rig {
    async fn new(now: DateTime<Utc>, items: Vec<CatalogItem>) -> Self {
        let store = Store::open_simulator(now).await.unwrap();
        store
            .sync_at(
                SyncRequest {
                    items: items.clone(),
                    dry_run: false,
                    allow_empty: false,
                },
                Some(0),
                "analytics-fixture",
                Some("seed"),
                now,
            )
            .await
            .unwrap();
        let mut rig = Self {
            store,
            now,
            sequence: 0,
            operation: 0,
            items,
            visible: BTreeSet::new(),
        };
        rig.command(TrackingCommand::Configure { config: config() })
            .await;
        rig
    }

    async fn snapshot(&self) -> TrackingSnapshot {
        self.store.tracking_snapshot(self.now).await.unwrap()
    }

    async fn command(&mut self, command: TrackingCommand) -> CommandResult {
        self.operation += 1;
        let revision = self.snapshot().await.tracking_revision;
        self.store
            .tracking_command(
                "analytics-fixture",
                &format!("action-{}", self.operation),
                revision,
                command,
                self.now,
            )
            .await
            .unwrap()
    }

    async fn batch(
        &mut self,
        mut observations: Vec<TagObservation>,
        enrollment: Option<SessionId>,
    ) {
        self.sequence += 1;
        if enrollment.is_none() {
            for epc in &self.visible {
                if !observations.iter().any(|read| read.epc == id(epc)) {
                    observations.push(self.observation(epc, "example-closet-port"));
                }
            }
        }
        self.store
            .tracking_ingest(
                ObservationBatch {
                    reader_id: id("example-reader"),
                    purpose: if enrollment.is_some() {
                        ObservationPurpose::Enrollment
                    } else {
                        ObservationPurpose::Inventory
                    },
                    data_source: DataSource::Simulation,
                    capture_session: enrollment,
                    session: Uuid::from_u128(1),
                    sequence: self.sequence,
                    observed_at: self.now,
                    connected: true,
                    coverage: if enrollment.is_none() {
                        [
                            "example-closet-port",
                            "example-rewear-port",
                            "example-hamper-port",
                            "example-dry-bag-port",
                        ]
                        .map(|port| PortCoverage {
                            port_id: id(port),
                            started_at: self.now - Duration::milliseconds(100),
                            ended_at: self.now,
                            proof: CoverageProof::VerifiedInventory,
                        })
                        .to_vec()
                    } else {
                        Vec::new()
                    },
                    observations,
                    readback: None,
                },
                self.now,
            )
            .await
            .unwrap();
    }

    fn observation(&self, epc: &str, port: &str) -> TagObservation {
        TagObservation {
            epc: id(epc),
            port_id: id(port),
            first_seen_at: self.now - Duration::milliseconds(300),
            last_seen_at: self.now,
            read_count: 3,
            tid: None,
            peak_rssi_cdbm: Some(-5_000),
            event_ref: None,
        }
    }

    async fn bind(&mut self, unit: UnitId, epc: &str) {
        // Establish the real reader epoch before starting an isolated capture.
        self.batch(Vec::new(), None).await;
        self.now += Duration::milliseconds(10);
        let session = self
            .command(TrackingCommand::StartEnrollment {
                unit_ids: vec![unit],
                port_id: id("example-enroll"),
                borrow: false,
                purpose: EnrollmentPurpose::Bind,
                ttl_seconds: 600,
            })
            .await
            .session
            .unwrap();
        self.now += Duration::milliseconds(400);
        self.batch(
            vec![self.observation(epc, "example-enroll")],
            Some(session.id),
        )
        .await;
        let session = self
            .snapshot()
            .await
            .sessions
            .into_iter()
            .find(|candidate| candidate.id == session.id)
            .unwrap();
        self.now += Duration::milliseconds(1);
        self.command(TrackingCommand::SelectCandidate {
            session_id: session.id,
            capture_version: session.capture_version,
            unit_id: unit,
            epc: id(epc),
            baseline_area: Some(id("example-closet")),
        })
        .await;
        self.visible.insert(epc.into());
    }

    async fn present(&mut self, epc: &str) {
        self.visible.insert(epc.into());
        self.batch(Vec::new(), None).await;
        self.now += Duration::milliseconds(400);
        self.batch(vec![self.observation(epc, "example-closet-port")], None)
            .await;
    }

    async fn absent(&mut self, epc: &str) {
        self.visible.remove(epc);
        for _ in 0..16 {
            self.now += Duration::milliseconds(100);
            self.batch(Vec::new(), None).await;
            self.store.tracking_tick(self.now).await.unwrap();
        }
    }

    async fn analytics(&self, days: u16) -> AnalyticsReport {
        self.store
            .tracking_analytics_clocked(days, 0, chrono_tz::America::Los_Angeles, || self.now)
            .await
            .unwrap()
    }

    async fn restored(&self, coverage: Vec<CoverageInterval>) -> Store {
        let mut data = self.store.tracking_export(self.now).await.unwrap();
        data.coverage = coverage;
        let encoded = serde_json::to_vec(&data).unwrap();
        let data: TrackingExport = serde_json::from_slice(&encoded).unwrap();
        let destination = Store::open_simulator(self.now).await.unwrap();
        destination
            .sync_at(
                SyncRequest {
                    items: self.items.clone(),
                    dry_run: false,
                    allow_empty: false,
                },
                Some(0),
                "analytics-restore",
                Some("seed"),
                self.now,
            )
            .await
            .unwrap();
        let revision = destination
            .tracking_snapshot(self.now)
            .await
            .unwrap()
            .tracking_revision;
        destination
            .tracking_command(
                "analytics-restore",
                "restore",
                revision,
                TrackingCommand::Import {
                    data: Box::new(data),
                },
                self.now,
            )
            .await
            .unwrap();
        destination
    }
}

#[tokio::test]
async fn no_rf_history_is_not_neglect_and_windows_are_explicit() {
    let now = at("2026-11-08T12:00:00Z");
    let rig = Rig::new(now, vec![item("fictional-shirt", 2)]).await;
    let report = rig.analytics(30).await;
    assert_eq!(report.as_of, now);
    assert_eq!(report.days, 30);
    assert_eq!(report.daily.len(), 30);
    assert_eq!(report.total_units, 2);
    assert_eq!(report.tracked_units, 0);
    assert_eq!(report.healthy_observation_ms, 0);
    assert_eq!(report.inferred_wears, 0);
    assert!(report.most_worn.is_empty());
    assert!(report.overlooked.is_empty());
    assert!(report.pick.is_none());
    let failure = rig
        .store
        .tracking_analytics_clocked(31, 0, chrono_tz::UTC, || now)
        .await
        .unwrap_err();
    assert_eq!(failure.code(), "invalid_analytics_window");
}

#[tokio::test]
async fn automatic_episodes_fold_undo_corrections_returns_and_import() {
    let mut rig = Rig::new(at("2026-11-01T08:59:50Z"), vec![item("fictional-shirt", 2)]).await;
    let unit = rig.snapshot().await.units[0].id;
    rig.bind(unit, "0001").await;
    rig.present("0001").await;
    rig.absent("0001").await;
    let first = rig.analytics(90).await;
    assert_eq!(first.inferred_wears, 1);
    assert_eq!(first.distinct_items_worn, 1);
    assert_eq!(first.most_worn[0].unit_count, 2);
    assert_eq!(first.most_worn[0].tracked_units, 1);
    rig.absent("0001").await;
    assert_eq!(rig.analytics(90).await.inferred_wears, 1);

    let event = rig.snapshot().await.units[0].last_event_id.unwrap();
    let undo = rig
        .command(TrackingCommand::Undo {
            event_id: event,
            note: "Undo the fictional inference".into(),
        })
        .await;
    assert_eq!(rig.analytics(90).await.inferred_wears, 0);
    rig.command(TrackingCommand::Undo {
        event_id: undo.event_ids[0],
        note: "Restore the fictional inference".into(),
    })
    .await;
    assert_eq!(rig.analytics(90).await.inferred_wears, 1);

    let corrected = rig
        .command(TrackingCommand::Correct {
            unit_id: unit,
            status: UnitStatus::CleanReady,
            note: "The inferred removal was incorrect".into(),
        })
        .await;
    assert_eq!(rig.analytics(90).await.inferred_wears, 0);
    rig.command(TrackingCommand::Undo {
        event_id: corrected.event_ids[0],
        note: "Restore the prior state".into(),
    })
    .await;
    assert_eq!(rig.analytics(90).await.inferred_wears, 1);
    rig.present("0001").await;
    rig.command(TrackingCommand::Correct {
        unit_id: unit,
        status: UnitStatus::CleanReady,
        note: "Unrelated correction after the actual return".into(),
    })
    .await;
    assert_eq!(rig.analytics(90).await.inferred_wears, 1);

    let mut disabled = config();
    disabled.enabled = false;
    rig.command(TrackingCommand::Configure { config: disabled })
        .await;
    let restored = rig.restored(Vec::new()).await;
    let report = restored
        .tracking_analytics_clocked(90, 0, chrono_tz::America::Los_Angeles, || rig.now)
        .await
        .unwrap();
    assert!(!report.tracking_enabled);
    assert_eq!(report.inferred_wears, 1);
    assert_eq!(
        report
            .daily
            .iter()
            .map(|day| day.inferred_wears)
            .sum::<u64>(),
        1
    );
    assert!(report.overlooked.is_empty());
}

#[tokio::test]
async fn coverage_intersects_unit_lifetimes_and_never_credits_untagged_copies() {
    let start = at("2026-10-20T07:00:00Z");
    let mut rig = Rig::new(start, vec![item("fictional-shirt", 4)]).await;
    let units = rig.snapshot().await.units;
    rig.bind(units[0].id, "0001").await;
    rig.bind(units[1].id, "0002").await;
    rig.bind(units[2].id, "0003").await;
    rig.command(TrackingCommand::Baseline {
        unit_id: units[3].id,
        area_id: id("example-closet"),
    })
    .await;
    rig.now = start + Duration::days(1);
    rig.command(TrackingCommand::Installation {
        unit_id: units[2].id,
        installation: Installation {
            state: InstallationState::Exempt,
            no_tag_reason: Some("Fictional delicate fabric".into()),
            ..Installation::default()
        },
    })
    .await;
    rig.now = start + Duration::days(4);
    rig.command(TrackingCommand::Pause {
        unit_id: units[0].id,
        paused: true,
    })
    .await;
    rig.now = start + Duration::days(10);
    rig.command(TrackingCommand::Pause {
        unit_id: units[0].id,
        paused: false,
    })
    .await;
    rig.now = start + Duration::days(12);
    let version = rig.snapshot().await.configuration_version;
    let restored = rig
        .restored(vec![CoverageInterval {
            id: 1,
            started_at: start,
            verified_through: rig.now,
            ended_at: Some(rig.now),
            end_reason: Some(CoverageEndReason::Import),
            configuration_version: version,
        }])
        .await;
    let report = restored
        .tracking_analytics_clocked(90, 0, chrono_tz::America::Los_Angeles, || rig.now)
        .await
        .unwrap();
    assert_eq!(report.healthy_observation_ms, 12 * 86_400_000);
    assert_eq!(report.total_units, 4);
    assert_eq!(report.tracked_units, 2);
    assert_eq!(report.overlooked.len(), 1);
    assert_eq!(report.overlooked[0].unit_id, units[1].id);
    assert_eq!(report.overlooked[0].ordinal, units[1].ordinal);
    assert_eq!(report.overlooked[0].inferred_wears, 0);
    assert!(report.overlooked[0].observed_ms > 11 * 86_400_000);
    assert_eq!(report.pick.as_ref().unwrap().unit_id, units[1].id);
}

#[tokio::test]
async fn undoing_a_wrong_binding_does_not_age_its_replacement() {
    let start = at("2026-10-20T07:00:00Z");
    let mut rig = Rig::new(start, vec![item("fictional-shirt", 1)]).await;
    let unit = rig.snapshot().await.units[0].id;
    rig.bind(unit, "0001").await;
    let binding = rig.snapshot().await.units[0].last_event_id.unwrap();
    rig.now = start + Duration::days(8);
    rig.command(TrackingCommand::Undo {
        event_id: binding,
        note: "The original tag belonged to another fictional piece".into(),
    })
    .await;
    rig.bind(unit, "0002").await;
    rig.now += Duration::hours(1);
    let version = rig.snapshot().await.configuration_version;
    let restored = rig
        .restored(vec![CoverageInterval {
            id: 1,
            started_at: start,
            verified_through: rig.now,
            ended_at: Some(rig.now),
            end_reason: Some(CoverageEndReason::Import),
            configuration_version: version,
        }])
        .await;
    let report = restored
        .tracking_analytics_clocked(90, 0, chrono_tz::America::Los_Angeles, || rig.now)
        .await
        .unwrap();
    assert!(report.healthy_observation_ms > 8 * 86_400_000);
    assert_eq!(report.tracked_units, 1);
    assert!(report.overlooked.is_empty());
    assert!(report.pick.is_none());
}

#[tokio::test]
async fn mistaken_pairing_excludes_only_its_binding_era_and_undo_restores_it() {
    let start = at("2026-10-20T07:00:00Z");
    let mut rig = Rig::new(start, vec![item("fictional-shirt", 1)]).await;
    let unit = rig.snapshot().await.units[0].id;
    rig.bind(unit, "0001").await;
    rig.present("0001").await;
    rig.absent("0001").await;
    rig.present("0001").await;
    rig.command(TrackingCommand::Unbind {
        unit_id: unit,
        note: "Remove a failed tag without discarding valid wear history".into(),
        mistaken: false,
    })
    .await;
    assert_eq!(rig.analytics(90).await.inferred_wears, 1);

    rig.bind(unit, "0002").await;
    rig.present("0002").await;
    rig.absent("0002").await;
    rig.present("0002").await;
    rig.now += Duration::days(8);
    let wrong = rig
        .command(TrackingCommand::Unbind {
            unit_id: unit,
            note: "This tag belonged to a different fictional piece".into(),
            mistaken: true,
        })
        .await;
    assert_eq!(rig.analytics(90).await.inferred_wears, 1);
    let undo = rig
        .command(TrackingCommand::Undo {
            event_id: wrong.event_ids[0],
            note: "The original association was correct".into(),
        })
        .await;
    assert_eq!(rig.analytics(90).await.inferred_wears, 2);
    rig.command(TrackingCommand::Undo {
        event_id: undo.event_ids[0],
        note: "Restore the pairing correction".into(),
    })
    .await;
    assert_eq!(rig.analytics(90).await.inferred_wears, 1);

    rig.bind(unit, "0003").await;
    rig.present("0003").await;
    rig.now += Duration::hours(1);
    let version = rig.snapshot().await.configuration_version;
    let restored = rig
        .restored(vec![CoverageInterval {
            id: 1,
            started_at: start,
            verified_through: rig.now,
            ended_at: Some(rig.now),
            end_reason: Some(CoverageEndReason::Import),
            configuration_version: version,
        }])
        .await;
    let report = restored
        .tracking_analytics_clocked(90, 0, chrono_tz::UTC, || rig.now)
        .await
        .unwrap();
    assert_eq!(report.inferred_wears, 1);
    assert!(
        report.overlooked.is_empty(),
        "the wrong tag's eight days must not age its replacement"
    );
    let history = restored.tracking_export(rig.now).await.unwrap().history;
    assert!(
        history
            .iter()
            .any(|event| event.reason == DecisionReason::MistakenPairing)
    );
    assert_eq!(
        history
            .iter()
            .filter(|event| event.kind == HistoryKind::Absence)
            .count(),
        2
    );
}

#[tokio::test]
async fn seven_observed_days_exclude_outages_and_pick_is_deterministic() {
    let start = at("2026-10-20T07:00:00Z");
    let mut rig = Rig::new(start, vec![item("fictional-shirt", 2)]).await;
    let units = rig.snapshot().await.units;
    rig.bind(units[0].id, "0001").await;
    rig.bind(units[1].id, "0002").await;
    let observed_from = rig.now;
    rig.now += Duration::days(21);
    let version = rig.snapshot().await.configuration_version;
    let intervals = [
        (observed_from, observed_from + Duration::days(3)),
        (
            observed_from + Duration::days(17),
            observed_from + Duration::days(21),
        ),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (start, end))| CoverageInterval {
        id: i64::try_from(index + 1).unwrap(),
        started_at: start,
        verified_through: end,
        ended_at: Some(end),
        end_reason: Some(CoverageEndReason::CoverageUnavailable),
        configuration_version: version,
    })
    .collect();
    let restored = rig.restored(intervals).await;
    let first = restored
        .tracking_analytics_clocked(30, 42, chrono_tz::America::Los_Angeles, || rig.now)
        .await
        .unwrap();
    let second = restored
        .tracking_analytics_clocked(30, 42, chrono_tz::America::Los_Angeles, || rig.now)
        .await
        .unwrap();
    assert_eq!(first.healthy_observation_ms, 7 * 86_400_000);
    assert_eq!(first.overlooked.len(), 2);
    assert!(
        first
            .overlooked
            .iter()
            .all(|unit| unit.observed_ms == first.minimum_observation_ms)
    );
    assert_eq!(
        first.pick.as_ref().unwrap().unit_id,
        second.pick.as_ref().unwrap().unit_id
    );
    assert!(first.pick.as_ref().unwrap().ready_to_wear);
}

#[tokio::test]
async fn administrative_import_cancels_only_the_current_inferred_episode() {
    let mut rig = Rig::new(at("2026-11-01T08:59:50Z"), vec![item("fictional-shirt", 1)]).await;
    let unit = rig.snapshot().await.units[0].id;
    rig.bind(unit, "0001").await;
    rig.present("0001").await;
    rig.absent("0001").await;
    assert_eq!(rig.analytics(90).await.inferred_wears, 1);
    let mut data = rig.store.tracking_export(rig.now).await.unwrap();
    data.units[0].unit.status = UnitStatus::CleanReady;
    data.units[0].unit.evidence = StateEvidence::Manual;
    rig.command(TrackingCommand::Import {
        data: Box::new(data),
    })
    .await;
    assert_eq!(rig.analytics(90).await.inferred_wears, 0);

    rig.present("0001").await;
    rig.absent("0001").await;
    rig.present("0001").await;
    assert_eq!(rig.analytics(90).await.inferred_wears, 1);
    let data = rig.store.tracking_export(rig.now).await.unwrap();
    rig.command(TrackingCommand::Import {
        data: Box::new(data),
    })
    .await;
    assert_eq!(rig.analytics(90).await.inferred_wears, 1);
}

#[tokio::test]
async fn journal_keyset_pages_do_not_drop_later_episodes() {
    let mut rig = Rig::new(at("2026-11-01T08:59:50Z"), vec![item("fictional-shirt", 1)]).await;
    let unit = rig.snapshot().await.units[0].id;
    rig.bind(unit, "0001").await;
    for index in 0..501 {
        rig.now += Duration::milliseconds(1);
        rig.command(TrackingCommand::Installation {
            unit_id: unit,
            installation: Installation {
                state: InstallationState::Installed,
                notes: Some(format!("Fictional installation note {index}")),
                ..Installation::default()
            },
        })
        .await;
    }
    rig.present("0001").await;
    rig.absent("0001").await;
    assert_eq!(rig.analytics(90).await.inferred_wears, 1);
    let event = rig.snapshot().await.units[0].last_event_id.unwrap();
    rig.command(TrackingCommand::Undo {
        event_id: event,
        note: "Cancel the inference beyond the first journal page".into(),
    })
    .await;
    assert_eq!(rig.analytics(90).await.inferred_wears, 0);
}

#[tokio::test]
async fn local_days_include_fall_dst_and_window_boundaries() {
    let start = at("2026-11-01T07:00:00Z");
    let mut rig = Rig::new(start, vec![item("fictional-shirt", 1)]).await;
    let unit = rig.snapshot().await.units[0].id;
    rig.bind(unit, "0001").await;
    rig.present("0001").await;
    rig.absent("0001").await;
    rig.present("0001").await;
    rig.now = at("2026-11-01T09:00:00Z");
    rig.present("0001").await;
    rig.absent("0001").await;
    rig.present("0001").await;
    let end = at("2026-11-02T08:00:00Z");
    rig.now = end + Duration::hours(1);
    let version = rig.snapshot().await.configuration_version;
    let restored = rig
        .restored(vec![CoverageInterval {
            id: 1,
            started_at: start,
            verified_through: end,
            ended_at: Some(end),
            end_reason: Some(CoverageEndReason::CoverageUnavailable),
            configuration_version: version,
        }])
        .await;
    let report = restored
        .tracking_analytics_clocked(30, 0, chrono_tz::America::Los_Angeles, || rig.now)
        .await
        .unwrap();
    let day = report
        .daily
        .iter()
        .find(|day| day.date.to_string() == "2026-11-01")
        .unwrap();
    assert_eq!(day.observed_ms, 25 * 3_600_000);
    assert_eq!(day.inferred_wears, 2);
    assert_eq!(report.inferred_wears, 2);
    let later = at("2026-12-01T08:00:00Z");
    let outside = restored
        .tracking_analytics_clocked(30, 0, chrono_tz::America::Los_Angeles, || later)
        .await
        .unwrap();
    assert_eq!(outside.from, at("2026-11-02T08:00:00Z"));
    assert_eq!(outside.inferred_wears, 0);
    assert_eq!(outside.healthy_observation_ms, 0);
    assert!(outside.most_worn.is_empty());
}

/// Produces actual reducer/export serialization for a disposable browser DB.
/// No generated fixture or API response mock is checked into the repository.
#[tokio::test]
#[ignore = "manual producer for the fictional Analytics browser fixture"]
async fn write_browser_fixture() {
    #[derive(serde::Serialize)]
    struct Fixture {
        catalog: Vec<CatalogItem>,
        tracking: TrackingExport,
    }
    let output = std::env::var("CLOSET_ANALYTICS_FIXTURE").expect("fixture output path");
    let start = Utc::now() - Duration::days(14);
    let mut rig = Rig::new(
        start,
        vec![
            item("fictional-blue-shirt", 2),
            item("fictional-green-dress", 1),
            item("fictional-grey-knit", 1),
        ],
    )
    .await;
    let units = rig.snapshot().await.units;
    let mut epc = 1_u16;
    for unit in &units {
        if unit.ordinal == 1 {
            let tag = format!("{epc:04X}");
            rig.bind(unit.id, &tag).await;
            rig.present(&tag).await;
            let wears = if unit.catalog_id == "fictional-blue-shirt" {
                3
            } else if unit.catalog_id == "fictional-grey-knit" {
                1
            } else {
                0
            };
            for _ in 0..wears {
                rig.absent(&tag).await;
                rig.present(&tag).await;
            }
            epc += 1;
        }
    }
    rig.now = Utc::now();
    let mut tracking = rig.store.tracking_export(rig.now).await.unwrap();
    tracking.coverage = vec![CoverageInterval {
        id: 1,
        started_at: start,
        verified_through: rig.now,
        ended_at: Some(rig.now),
        end_reason: Some(CoverageEndReason::Import),
        configuration_version: tracking.configuration_version,
    }];
    let restored = rig.restored(tracking.coverage.clone()).await;
    let report = restored
        .tracking_analytics_clocked(90, 0, chrono_tz::America::Los_Angeles, || rig.now)
        .await
        .unwrap();
    assert_eq!(report.inferred_wears, 4);
    assert_eq!(report.overlooked.len(), 3);
    std::fs::write(
        output,
        serde_json::to_vec_pretty(&Fixture {
            catalog: rig.items,
            tracking,
        })
        .unwrap(),
    )
    .unwrap();
}
