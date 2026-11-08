//! Fictional garments, real SQLite, and the same normalized reducer as readers.
use chrono::{DateTime, Duration, Utc};
use closet::{
    models::{CatalogItem, SyncRequest},
    store::Store,
    tracking::models::*,
};
use uuid::Uuid;

fn t(ms: i64) -> DateTime<Utc> {
    DateTime::<Utc>::UNIX_EPOCH + Duration::milliseconds(ms)
}
fn id<T: std::str::FromStr>(s: &str) -> T
where
    T::Err: std::fmt::Debug,
{
    s.parse().unwrap()
}
fn catalog(quantity: u32) -> CatalogItem {
    CatalogItem {
        id: "fictional-shirt".into(),
        brand: Some("Example studio".into()),
        name: "Cotton shirt".into(),
        category: "clothing.top.shirt".into(),
        color: Some("Blue".into()),
        size: Some("M".into()),
        quantity: Some(quantity),
        material: Some("Cotton".into()),
        sku: None,
        purchase_source: Some("self_bought".into()),
        images: vec![],
        notes: vec![],
        price: None,
        shop: None,
    }
}
fn config() -> TrackingConfig {
    let areas = [
        ("closet", AreaKind::Closet),
        ("rewear", AreaKind::Rewear),
        ("hamper", AreaKind::Hamper),
        ("drybag", AreaKind::DryCleaningBag),
    ]
    .into_iter()
    .map(|(name, kind)| AreaConfig {
        id: id(name),
        label: name.into(),
        kind,
        enabled: true,
        thresholds: AreaThresholds::default(),
    })
    .collect::<Vec<_>>();
    let mut ports = areas
        .iter()
        .enumerate()
        .map(|(n, a)| PortConfig {
            id: id(&format!("port-{}", n + 1)),
            reader_id: id("reader"),
            antenna: (n + 1) as u16,
            area_id: Some(a.id.clone()),
            role: PortRole::Monitor,
            enabled: true,
            required_for_absence: true,
            commissioned: true,
            transmit_power_cdbm: None,
            dwell_ms: None,
        })
        .collect::<Vec<_>>();
    for n in 5..=6 {
        ports.push(PortConfig {
            id: id(&format!("port-{n}")),
            reader_id: id("reader"),
            antenna: n,
            area_id: None,
            role: PortRole::Enrollment,
            enabled: true,
            required_for_absence: false,
            commissioned: false,
            transmit_power_cdbm: None,
            dwell_ms: None,
        });
    }
    TrackingConfig {
        schema_version: 1,
        enabled: true,
        areas,
        readers: vec![ReaderConfig {
            id: id("reader"),
            label: "Fictional reader".into(),
            enabled: true,
            connection: ReaderConnection::Simulator,
        }],
        ports,
        thresholds: Thresholds {
            positive_dwell_ms: 100,
            positive_min_reads: 2,
            movement_hysteresis_ms: 100,
            absence_ms: 1_000,
            warmup_ms: 100,
            coverage_max_gap_ms: 1_000,
            candidate_fresh_ms: 500,
            reader_event_max_age_ms: 10_000,
        },
    }
}
struct Rig {
    store: Store,
    now: i64,
    sequence: u64,
    operation: u64,
    epoch: Uuid,
}
impl Rig {
    async fn new(quantity: u32) -> Self {
        let store = Store::open_simulator(t(0)).await.unwrap();
        store
            .sync_at(
                SyncRequest {
                    items: vec![catalog(quantity)],
                    dry_run: false,
                    allow_empty: false,
                },
                Some(0),
                "fixture",
                Some("seed"),
                t(0),
            )
            .await
            .unwrap();
        let mut rig = Self {
            store,
            now: 0,
            sequence: 0,
            operation: 0,
            epoch: Uuid::from_u128(1),
        };
        rig.command(TrackingCommand::Configure { config: config() })
            .await
            .unwrap();
        rig.batch(ObservationPurpose::Inventory, None, vec![], true)
            .await
            .unwrap();
        rig
    }
    async fn snapshot(&self) -> TrackingSnapshot {
        self.store.tracking_snapshot(t(self.now)).await.unwrap()
    }
    async fn command(&mut self, command: TrackingCommand) -> Result<CommandResult> {
        self.operation += 1;
        let revision = self.snapshot().await.tracking_revision;
        self.store
            .tracking_command(
                "fixture-operator",
                &format!("command-{}", self.operation),
                revision,
                command,
                t(self.now),
            )
            .await
    }
    async fn batch(
        &mut self,
        purpose: ObservationPurpose,
        session: Option<SessionId>,
        observations: Vec<TagObservation>,
        healthy: bool,
    ) -> Result<IngestResult> {
        self.sequence += 1;
        let coverage = if purpose == ObservationPurpose::Inventory {
            (1..=4)
                .map(|n| PortCoverage {
                    port_id: id(&format!("port-{n}")),
                    started_at: t((self.now - 100).max(0)),
                    ended_at: t(self.now),
                    proof: if healthy {
                        CoverageProof::VerifiedInventory
                    } else {
                        CoverageProof::TransportOnly
                    },
                })
                .collect()
        } else {
            vec![]
        };
        self.store
            .tracking_ingest(
                ObservationBatch {
                    reader_id: id("reader"),
                    purpose,
                    data_source: DataSource::Simulation,
                    capture_session: session,
                    session: self.epoch,
                    sequence: self.sequence,
                    observed_at: t(self.now),
                    connected: true,
                    coverage,
                    observations,
                    readback: None,
                },
                t(self.now),
            )
            .await
    }
    fn observation(&self, epc: &str, port: u16, span: i64, count: u32) -> TagObservation {
        TagObservation {
            epc: id(epc),
            port_id: id(&format!("port-{port}")),
            first_seen_at: t(self.now - span),
            last_seen_at: t(self.now),
            read_count: count,
            tid: None,
            peak_rssi_cdbm: Some(-5_000),
            event_ref: None,
        }
    }
    async fn start(
        &mut self,
        unit: UnitId,
        port: u16,
        purpose: EnrollmentPurpose,
    ) -> EnrollmentSession {
        self.now += 10;
        self.command(TrackingCommand::StartEnrollment {
            unit_ids: vec![unit],
            port_id: id(&format!("port-{port}")),
            borrow: false,
            purpose,
            ttl_seconds: 600,
        })
        .await
        .unwrap()
        .session
        .unwrap()
    }
    async fn bind(&mut self, unit: UnitId, epc: &str) {
        let session = self.start(unit, 5, EnrollmentPurpose::Bind).await;
        self.now += 201;
        let observation = self.observation(epc, 5, 200, 3);
        self.batch(
            ObservationPurpose::Enrollment,
            Some(session.id),
            vec![observation],
            true,
        )
        .await
        .unwrap();
        let session = self
            .snapshot()
            .await
            .sessions
            .into_iter()
            .find(|s| s.id == session.id)
            .unwrap();
        assert!(session.candidates[0].selectable);
        self.now += 1;
        self.command(TrackingCommand::SelectCandidate {
            session_id: session.id,
            capture_version: session.capture_version,
            unit_id: unit,
            epc: id(epc),
            baseline_area: None,
        })
        .await
        .unwrap();
    }
    async fn area(&mut self, epc: &str, port: u16) {
        self.batch(ObservationPurpose::Inventory, None, vec![], true)
            .await
            .unwrap();
        self.now += 400;
        let observation = self.observation(epc, port, 300, 3);
        self.batch(ObservationPurpose::Inventory, None, vec![observation], true)
            .await
            .unwrap();
    }
    async fn absent(&mut self, duration: i64, healthy: bool) {
        let end = self.now + duration;
        self.store.tracking_tick(t(self.now)).await.unwrap();
        while self.now < end {
            self.now = (self.now + 100).min(end);
            self.batch(ObservationPurpose::Inventory, None, vec![], healthy)
                .await
                .unwrap();
            self.store.tracking_tick(t(self.now)).await.unwrap();
        }
    }
    async fn unit(&self, id: UnitId) -> PhysicalUnit {
        self.snapshot()
            .await
            .units
            .into_iter()
            .find(|u| u.id == id)
            .unwrap()
    }
}

#[test]
fn epcs_are_variable_length_canonical_bytes_not_numbers() {
    let a: Epc = "0000ab".parse().unwrap();
    assert_eq!(a.as_str(), "0000AB");
    assert_ne!(a, "00ab".parse().unwrap());
    assert!("abc".parse::<Epc>().is_err());
    assert!("0x0001".parse::<Epc>().is_err());
    assert!("gg".parse::<Epc>().is_err());
    let json = serde_json::to_vec(&a).unwrap();
    assert_eq!(serde_json::from_slice::<Epc>(&json).unwrap(), a);
}

#[tokio::test]
async fn exact_area_absence_rules_are_stable_and_new_physical_context_replaces_cleaning() {
    for (port, present, absent) in [
        (1, UnitStatus::CleanReady, UnitStatus::BeingWorn),
        (2, UnitStatus::WornReady, UnitStatus::BeingWorn),
        (3, UnitStatus::NeedsLaundry, UnitStatus::InWash),
        (4, UnitStatus::QueuedDryCleaning, UnitStatus::AtCleaners),
    ] {
        let mut rig = Rig::new(1).await;
        let unit = rig.snapshot().await.units[0].id;
        rig.bind(unit, "0001").await;
        assert_eq!(rig.unit(unit).await.status, UnitStatus::Unknown);
        rig.area("0001", port).await;
        assert_eq!(rig.unit(unit).await.status, present);
        let area = rig.unit(unit).await.last_confirmed_area;
        rig.absent(1_100, true).await;
        assert_eq!(rig.unit(unit).await.status, absent);
        assert_eq!(rig.unit(unit).await.last_confirmed_area, area);
        assert_eq!(rig.unit(unit).await.current_area, None);
        let revision = rig.snapshot().await.tracking_revision;
        rig.absent(1_100, true).await;
        assert_eq!(rig.unit(unit).await.status, absent);
        assert_eq!(
            rig.snapshot().await.tracking_revision,
            revision,
            "continued absence must not rewrite status/history"
        );
        rig.area("0001", 1).await;
        assert_eq!(rig.unit(unit).await.status, UnitStatus::CleanReady);
        rig.absent(1_100, true).await;
        assert_eq!(rig.unit(unit).await.status, UnitStatus::BeingWorn);
        assert_eq!(rig.unit(unit).await.last_confirmed_area, Some(id("closet")));
        let history = rig
            .store
            .tracking_history(Some(unit), None, 100)
            .await
            .unwrap();
        assert!(
            history
                .events
                .iter()
                .any(|e| e.reason == DecisionReason::ConfirmedAbsence)
        );
        assert!(
            history
                .events
                .iter()
                .filter(|e| e.reason == DecisionReason::ConfirmedAbsence)
                .all(|e| e.configuration_version == 1
                    && e.evidence.len() == 4
                    && e.evidence
                        .iter()
                        .all(|r| r.event_ref.as_deref() == Some("inventory_coverage")))
        );
        assert!(
            history
                .events
                .iter()
                .filter(|e| e.reason == DecisionReason::StableArea)
                .all(|e| e.configuration_version == 1 && !e.evidence.is_empty() && e.automatic)
        );
        rig.store.close().await;
    }
}

#[tokio::test]
async fn single_misses_strays_ambiguity_and_bad_coverage_never_invent_movement() {
    let mut rig = Rig::new(1).await;
    let unit = rig.snapshot().await.units[0].id;
    rig.bind(unit, "0001").await;
    rig.area("0001", 3).await;
    rig.absent(100, true).await;
    assert_eq!(rig.unit(unit).await.status, UnitStatus::NeedsLaundry);
    rig.now += 20;
    let stray = rig.observation("0001", 1, 0, 1);
    rig.batch(ObservationPurpose::Inventory, None, vec![stray], true)
        .await
        .unwrap();
    assert_eq!(rig.unit(unit).await.status, UnitStatus::NeedsLaundry);
    rig.now += 100;
    let mut closet = rig.observation("0001", 1, 100, 30);
    closet.peak_rssi_cdbm = Some(-1_000);
    let hamper = rig.observation("0001", 3, 100, 2);
    rig.batch(
        ObservationPurpose::Inventory,
        None,
        vec![closet, hamper],
        true,
    )
    .await
    .unwrap();
    assert_eq!(rig.unit(unit).await.status, UnitStatus::NeedsLaundry);
    assert!(
        rig.snapshot()
            .await
            .diagnostics
            .decisions
            .iter()
            .any(|d| d.reason == DecisionReason::AmbiguousEvidence)
    );
    rig.absent(3_000, false).await;
    assert_eq!(rig.unit(unit).await.status, UnitStatus::NeedsLaundry);
    assert!(!rig.snapshot().await.health.absence_coverage_healthy);
    rig.now += 400;
    let positive = rig.observation("0001", 1, 300, 3);
    rig.batch(ObservationPurpose::Inventory, None, vec![positive], false)
        .await
        .unwrap();
    assert_eq!(
        rig.unit(unit).await.status,
        UnitStatus::CleanReady,
        "positive evidence does not require healthy negative coverage"
    );
    rig.store.close().await;
}

#[tokio::test]
async fn reconnect_replay_and_uncovered_enabled_areas_reset_absence_not_garment_state() {
    let mut rig = Rig::new(1).await;
    let unit = rig.snapshot().await.units[0].id;
    rig.bind(unit, "0001").await;
    rig.area("0001", 4).await;
    rig.absent(600, true).await;
    let old_epoch = rig.epoch;
    rig.epoch = Uuid::from_u128(2);
    rig.now += 100;
    rig.batch(ObservationPurpose::Inventory, None, vec![], true)
        .await
        .unwrap();
    rig.absent(600, true).await;
    assert_eq!(
        rig.unit(unit).await.status,
        UnitStatus::QueuedDryCleaning,
        "reconnect downtime/old absence time must not accumulate"
    );
    rig.sequence += 1;
    let old = ObservationBatch {
        reader_id: id("reader"),
        purpose: ObservationPurpose::Inventory,
        data_source: DataSource::Simulation,
        capture_session: None,
        session: old_epoch,
        sequence: rig.sequence,
        observed_at: t(rig.now),
        connected: true,
        coverage: vec![],
        observations: vec![],
        readback: None,
    };
    assert_eq!(
        rig.store
            .tracking_ingest(old, t(rig.now))
            .await
            .unwrap_err()
            .code(),
        "retired_reader_session"
    );
    let mut config = config();
    config.ports[2].required_for_absence = false;
    rig.command(TrackingCommand::Configure { config })
        .await
        .unwrap();
    rig.absent(3_000, true).await;
    assert_eq!(rig.unit(unit).await.status, UnitStatus::QueuedDryCleaning);
    assert!(!rig.snapshot().await.health.absence_coverage_healthy);
    rig.store.close().await;
}

#[tokio::test]
async fn enrollment_requires_a_fresh_isolated_capture_and_replays_atomically() {
    let mut rig = Rig::new(2).await;
    let mut units = rig.snapshot().await.units;
    units.sort_by_key(|u| u.ordinal);
    let first = units[0].id;
    let second = units[1].id;
    let session = rig
        .command(TrackingCommand::StartEnrollment {
            unit_ids: vec![first, second],
            port_id: id("port-5"),
            borrow: false,
            purpose: EnrollmentPurpose::Bind,
            ttl_seconds: 600,
        })
        .await
        .unwrap()
        .session
        .unwrap();
    rig.now += 201;
    let a = rig.observation("0001", 5, 200, 3);
    let b = rig.observation("0002", 5, 200, 3);
    rig.batch(
        ObservationPurpose::Enrollment,
        Some(session.id),
        vec![a, b],
        true,
    )
    .await
    .unwrap();
    let view = rig
        .snapshot()
        .await
        .sessions
        .into_iter()
        .find(|s| s.id == session.id)
        .unwrap();
    assert_eq!(view.candidates.len(), 2);
    assert!(view.candidates.iter().all(|c| !c.selectable));
    assert_ne!(view.capture_version, session.capture_version);
    let error = rig
        .command(TrackingCommand::SelectCandidate {
            session_id: session.id,
            capture_version: view.capture_version,
            unit_id: first,
            epc: id("0001"),
            baseline_area: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), "enrollment_not_isolated_fresh");
    let reset = rig
        .command(TrackingCommand::ResetCapture {
            session_id: session.id,
        })
        .await
        .unwrap()
        .session
        .unwrap();
    let stale = rig.observation("0001", 5, 200, 3);
    rig.batch(
        ObservationPurpose::Enrollment,
        Some(session.id),
        vec![stale],
        true,
    )
    .await
    .unwrap();
    assert!(
        rig.snapshot()
            .await
            .sessions
            .iter()
            .find(|s| s.id == session.id)
            .unwrap()
            .candidates
            .is_empty()
    );
    rig.now += 201;
    let fresh = rig.observation("0001", 5, 200, 3);
    rig.batch(
        ObservationPurpose::Enrollment,
        Some(session.id),
        vec![fresh],
        true,
    )
    .await
    .unwrap();
    let revision = rig.snapshot().await.tracking_revision;
    let command = TrackingCommand::SelectCandidate {
        session_id: session.id,
        capture_version: reset.capture_version,
        unit_id: first,
        epc: id("0001"),
        baseline_area: Some(id("closet")),
    };
    let result = rig
        .store
        .tracking_command(
            "operator",
            "pair-once",
            revision,
            command.clone(),
            t(rig.now),
        )
        .await
        .unwrap();
    let replay = rig
        .store
        .tracking_command("operator", "pair-once", revision, command, t(rig.now + 1))
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&result).unwrap(),
        serde_json::to_vec(&replay).unwrap()
    );
    let advanced = result.session.unwrap();
    assert_eq!(advanced.cursor, 1);
    assert!(advanced.candidates.is_empty());
    assert!(advanced.capture_version > reset.capture_version);
    let old = rig.observation("0001", 5, 200, 3);
    rig.batch(
        ObservationPurpose::Enrollment,
        Some(session.id),
        vec![old],
        true,
    )
    .await
    .unwrap();
    assert!(
        rig.snapshot()
            .await
            .sessions
            .iter()
            .find(|s| s.id == session.id)
            .unwrap()
            .candidates
            .is_empty()
    );
    assert_eq!(rig.unit(first).await.status, UnitStatus::CleanReady);
    assert_eq!(rig.unit(second).await.status, UnitStatus::Unknown);
    let history = rig
        .store
        .tracking_history(Some(first), None, 100)
        .await
        .unwrap();
    assert_eq!(
        history
            .events
            .iter()
            .filter(|e| matches!(e.kind, HistoryKind::Bind))
            .count(),
        1
    );
    rig.store.close().await;
}

#[tokio::test]
async fn bound_ambient_tags_and_concurrent_sessions_cannot_pair_the_same_epc() {
    let mut rig = Rig::new(2).await;
    let units = rig.snapshot().await.units;
    let one = units[0].id;
    let two = units[1].id;
    let a = rig.start(one, 5, EnrollmentPurpose::Bind).await;
    let b = rig.start(two, 6, EnrollmentPurpose::Bind).await;
    rig.now += 201;
    let observation = rig.observation("0001", 5, 200, 3);
    rig.batch(
        ObservationPurpose::Enrollment,
        Some(a.id),
        vec![observation],
        true,
    )
    .await
    .unwrap();
    let observation = rig.observation("0001", 6, 200, 3);
    rig.batch(
        ObservationPurpose::Enrollment,
        Some(b.id),
        vec![observation],
        true,
    )
    .await
    .unwrap();
    let snapshot = rig.snapshot().await;
    let av = snapshot.sessions.iter().find(|s| s.id == a.id).unwrap();
    let bv = snapshot.sessions.iter().find(|s| s.id == b.id).unwrap();
    let ca = TrackingCommand::SelectCandidate {
        session_id: a.id,
        capture_version: av.capture_version,
        unit_id: one,
        epc: id("0001"),
        baseline_area: None,
    };
    let cb = TrackingCommand::SelectCandidate {
        session_id: b.id,
        capture_version: bv.capture_version,
        unit_id: two,
        epc: id("0001"),
        baseline_area: None,
    };
    let (a_result, b_result) = tokio::join!(
        rig.store.tracking_command(
            "operator",
            "concurrent-a",
            snapshot.tracking_revision,
            ca,
            t(rig.now)
        ),
        rig.store.tracking_command(
            "operator",
            "concurrent-b",
            snapshot.tracking_revision,
            cb.clone(),
            t(rig.now)
        )
    );
    assert!(a_result.is_ok());
    assert_eq!(b_result.unwrap_err().code(), "tracking_revision_conflict");
    let after = rig.snapshot().await;
    let candidate = &after
        .sessions
        .iter()
        .find(|s| s.id == b.id)
        .unwrap()
        .candidates[0];
    assert_eq!(candidate.bound_unit_id, Some(one));
    assert!(!candidate.selectable);
    let error = rig
        .store
        .tracking_command(
            "operator",
            "concurrent-b-rechecked",
            after.tracking_revision,
            cb,
            t(rig.now),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), "enrollment_not_isolated_fresh");
    assert_eq!(
        rig.store
            .tracking_export(t(rig.now))
            .await
            .unwrap()
            .bindings
            .iter()
            .filter(|b| b.retired_at.is_none())
            .count(),
        1
    );
    rig.store.close().await;
}

#[tokio::test]
async fn quantities_retirement_metadata_archive_and_restore_preserve_unit_identity() {
    let mut rig = Rig::new(2).await;
    let mut original = rig.snapshot().await.units;
    original.sort_by_key(|u| u.ordinal);
    let retired = original[0].id;
    let survivor = original[1].id;
    rig.bind(retired, "0001").await;
    rig.bind(survivor, "0002").await;
    rig.area("0001", 1).await;
    rig.area("0002", 3).await;
    assert_eq!(rig.unit(retired).await.status, UnitStatus::CleanReady);
    assert_eq!(rig.unit(survivor).await.status, UnitStatus::NeedsLaundry);
    let before = rig.snapshot().await.tracking_revision;
    let mut changed = catalog(2);
    changed.name = "Renamed fictional shirt".into();
    rig.store
        .sync_at(
            SyncRequest {
                items: vec![changed],
                dry_run: false,
                allow_empty: false,
            },
            Some(1),
            "fixture",
            Some("metadata"),
            t(rig.now),
        )
        .await
        .unwrap();
    assert_eq!(rig.snapshot().await.tracking_revision, before);
    assert_eq!(
        rig.snapshot()
            .await
            .units
            .iter()
            .map(|u| u.id)
            .collect::<std::collections::BTreeSet<_>>(),
        original.iter().map(|u| u.id).collect()
    );
    rig.store
        .sync_at(
            SyncRequest {
                items: vec![catalog(1)],
                dry_run: false,
                allow_empty: false,
            },
            Some(2),
            "fixture",
            Some("quantity-down"),
            t(rig.now),
        )
        .await
        .unwrap();
    assert!(rig.snapshot().await.inventory[0].needs_reconciliation);
    assert_eq!(
        rig.snapshot()
            .await
            .units
            .iter()
            .filter(|u| !u.retired)
            .count(),
        2
    );
    rig.command(TrackingCommand::Retire {
        unit_id: retired,
        note: "One physical unit removed".into(),
    })
    .await
    .unwrap();
    let after = rig.snapshot().await;
    assert!(!after.inventory[0].needs_reconciliation);
    assert_eq!(after.units.iter().filter(|u| !u.retired).count(), 1);
    assert_eq!(rig.unit(survivor).await.ordinal, 2);
    rig.store
        .sync_at(
            SyncRequest {
                items: vec![],
                dry_run: false,
                allow_empty: true,
            },
            Some(3),
            "fixture",
            Some("archive"),
            t(rig.now),
        )
        .await
        .unwrap();
    assert!(rig.unit(survivor).await.archived);
    rig.store
        .sync_at(
            SyncRequest {
                items: vec![catalog(1)],
                dry_run: false,
                allow_empty: false,
            },
            Some(4),
            "fixture",
            Some("restore"),
            t(rig.now),
        )
        .await
        .unwrap();
    let restored = rig.unit(survivor).await;
    assert!(!restored.archived);
    assert!(restored.has_binding);
    assert!(restored.baseline_at.is_none());
    assert_eq!(restored.id, survivor);
    rig.area("0002", 1).await;
    assert_eq!(rig.unit(survivor).await.status, UnitStatus::CleanReady);
    rig.store
        .sync_at(
            SyncRequest {
                items: vec![catalog(2)],
                dry_run: false,
                allow_empty: false,
            },
            Some(5),
            "fixture",
            Some("quantity-up"),
            t(rig.now),
        )
        .await
        .unwrap();
    let expanded = rig.snapshot().await;
    assert!(
        expanded
            .units
            .iter()
            .any(|u| u.ordinal == 3 && u.id != retired)
    );
    assert!(expanded.units.iter().any(|u| u.id == retired && u.retired));
    rig.store.close().await;
}

#[tokio::test]
async fn manual_no_tag_baselines_are_ready_and_known_nonready_does_not_fallback_on_diagnostics() {
    let mut rig = Rig::new(1).await;
    let unit = rig.snapshot().await.units[0].id;
    assert!(rig.snapshot().await.certified_ready_unit_ids.is_empty());
    assert_eq!(
        rig.store
            .tracking_availability(t(rig.now))
            .await
            .unwrap()
            .eligible_catalog_ids,
        vec!["fictional-shirt"]
    );
    rig.command(TrackingCommand::Baseline {
        unit_id: unit,
        area_id: id("closet"),
    })
    .await
    .unwrap();
    assert!(rig.unit(unit).await.ready_to_wear());
    assert!(!rig.unit(unit).await.has_binding);
    rig.command(TrackingCommand::Pause {
        unit_id: unit,
        paused: true,
    })
    .await
    .unwrap();
    assert!(rig.unit(unit).await.ready_to_wear());
    rig.command(TrackingCommand::Pause {
        unit_id: unit,
        paused: false,
    })
    .await
    .unwrap();
    rig.command(TrackingCommand::Correct {
        unit_id: unit,
        status: UnitStatus::InWash,
        note: "Explicit manual laundry state".into(),
    })
    .await
    .unwrap();
    rig.command(TrackingCommand::Pause {
        unit_id: unit,
        paused: true,
    })
    .await
    .unwrap();
    assert!(
        rig.store
            .tracking_availability(t(rig.now))
            .await
            .unwrap()
            .eligible_catalog_ids
            .is_empty(),
        "pause must not make a known nonready unit available"
    );
    rig.command(TrackingCommand::Installation {
        unit_id: unit,
        installation: Installation {
            state: InstallationState::Exempt,
            no_tag_reason: Some("Fictional comfort exception".into()),
            ..Installation::default()
        },
    })
    .await
    .unwrap();
    assert!(
        rig.store
            .tracking_availability(t(rig.now))
            .await
            .unwrap()
            .eligible_catalog_ids
            .is_empty(),
        "explicit manual nonready status overrides no-tag fallback"
    );
    rig.command(TrackingCommand::Correct {
        unit_id: unit,
        status: UnitStatus::Unknown,
        note: "Return to untracked catalog behavior".into(),
    })
    .await
    .unwrap();
    assert_eq!(
        rig.store
            .tracking_availability(t(rig.now))
            .await
            .unwrap()
            .eligible_catalog_ids,
        vec!["fictional-shirt"]
    );
    rig.store.close().await;
}

#[tokio::test]
async fn replacement_and_undo_keep_history_and_old_epcs_cannot_drive_the_unit() {
    let mut rig = Rig::new(1).await;
    let unit = rig.snapshot().await.units[0].id;
    rig.bind(unit, "0001").await;
    rig.area("0001", 3).await;
    let replacement = rig.start(unit, 5, EnrollmentPurpose::Replace).await;
    rig.now += 201;
    let fresh = rig.observation("0002", 5, 200, 3);
    rig.batch(
        ObservationPurpose::Enrollment,
        Some(replacement.id),
        vec![fresh],
        true,
    )
    .await
    .unwrap();
    let view = rig
        .snapshot()
        .await
        .sessions
        .into_iter()
        .find(|s| s.id == replacement.id)
        .unwrap();
    let selected = rig
        .command(TrackingCommand::SelectCandidate {
            session_id: replacement.id,
            capture_version: view.capture_version,
            unit_id: unit,
            epc: id("0002"),
            baseline_area: Some(id("rewear")),
        })
        .await
        .unwrap();
    assert_eq!(rig.unit(unit).await.status, UnitStatus::WornReady);
    rig.area("0001", 1).await;
    assert_eq!(
        rig.unit(unit).await.status,
        UnitStatus::WornReady,
        "retired binding must not clean the unit"
    );
    let export = rig.store.tracking_export(t(rig.now)).await.unwrap();
    assert_eq!(export.bindings.len(), 2);
    assert!(
        export
            .bindings
            .iter()
            .any(|b| b.epc == id("0001") && b.retired_at.is_some())
    );
    let event = selected.event_ids[0];
    rig.command(TrackingCommand::Undo {
        event_id: event,
        note: "Correct a fictional pairing mistake".into(),
    })
    .await
    .unwrap();
    assert_eq!(rig.unit(unit).await.status, UnitStatus::NeedsLaundry);
    rig.area("0002", 1).await;
    assert_eq!(rig.unit(unit).await.status, UnitStatus::NeedsLaundry);
    rig.area("0001", 1).await;
    assert_eq!(rig.unit(unit).await.status, UnitStatus::CleanReady);
    assert!(
        rig.store
            .tracking_history(Some(unit), None, 100)
            .await
            .unwrap()
            .events
            .iter()
            .any(|e| matches!(e.kind, HistoryKind::Undo))
    );
    rig.store.close().await;
}

#[tokio::test]
async fn export_import_preserves_bindings_history_replay_and_rejects_simulation_promotion() {
    let mut rig = Rig::new(1).await;
    let unit = rig.snapshot().await.units[0].id;
    rig.bind(unit, "0001").await;
    rig.area("0001", 4).await;
    let export = rig.store.tracking_export(t(rig.now)).await.unwrap();
    assert_eq!(export.data_source, DataSource::Simulation);
    let serialized = serde_json::to_vec(&export).unwrap();
    let export: TrackingExport = serde_json::from_slice(&serialized).unwrap();
    let destination = Store::open_simulator(t(rig.now)).await.unwrap();
    destination
        .sync_at(
            SyncRequest {
                items: vec![catalog(1)],
                dry_run: false,
                allow_empty: false,
            },
            Some(0),
            "fixture",
            Some("seed"),
            t(rig.now),
        )
        .await
        .unwrap();
    let revision = destination
        .tracking_snapshot(t(rig.now))
        .await
        .unwrap()
        .tracking_revision;
    destination
        .tracking_command(
            "operator",
            "restore",
            revision,
            TrackingCommand::Import {
                data: Box::new(export.clone()),
            },
            t(rig.now),
        )
        .await
        .unwrap();
    let restored = destination.tracking_export(t(rig.now)).await.unwrap();
    assert_eq!(restored.units[0].unit.id, unit);
    assert_eq!(restored.units[0].epc, Some(id("0001")));
    assert_eq!(restored.history.len(), export.history.len());
    assert_eq!(restored.bindings.len(), export.bindings.len());
    assert_eq!(restored.units[0].unit.status, UnitStatus::QueuedDryCleaning);
    assert!(
        !destination
            .tracking_snapshot(t(rig.now))
            .await
            .unwrap()
            .health
            .absence_coverage_healthy
    );
    let directory = tempfile::tempdir().unwrap();
    let live = Store::open(directory.path().join("live.sqlite"))
        .await
        .unwrap();
    live.sync_at(
        SyncRequest {
            items: vec![catalog(1)],
            dry_run: false,
            allow_empty: false,
        },
        Some(0),
        "fixture",
        Some("seed"),
        t(rig.now),
    )
    .await
    .unwrap();
    let before = live.tracking_snapshot(t(rig.now)).await.unwrap();
    let error = live
        .tracking_command(
            "operator",
            "no-promote",
            before.tracking_revision,
            TrackingCommand::Import {
                data: Box::new(export),
            },
            t(rig.now),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), "simulation_isolation");
    assert_eq!(
        live.tracking_snapshot(t(rig.now)).await.unwrap().units[0].id,
        before.units[0].id
    );
    live.close().await;
    destination.close().await;
    rig.store.close().await;
}

#[tokio::test]
async fn replacing_a_tag_preserves_wash_and_cleaner_context_without_a_new_area_claim() {
    for (port, expected) in [(3, UnitStatus::InWash), (4, UnitStatus::AtCleaners)] {
        let mut rig = Rig::new(1).await;
        let unit = rig.snapshot().await.units[0].id;
        rig.bind(unit, "0001").await;
        rig.area("0001", port).await;
        rig.absent(1_100, true).await;
        let previous = rig.unit(unit).await;
        assert_eq!(previous.status, expected);
        let session = rig.start(unit, 5, EnrollmentPurpose::Replace).await;
        rig.now += 201;
        let observation = rig.observation("0002", 5, 200, 3);
        rig.batch(
            ObservationPurpose::Enrollment,
            Some(session.id),
            vec![observation],
            true,
        )
        .await
        .unwrap();
        let view = rig
            .snapshot()
            .await
            .sessions
            .into_iter()
            .find(|s| s.id == session.id)
            .unwrap();
        rig.command(TrackingCommand::SelectCandidate {
            session_id: session.id,
            capture_version: view.capture_version,
            unit_id: unit,
            epc: id("0002"),
            baseline_area: None,
        })
        .await
        .unwrap();
        let replaced = rig.unit(unit).await;
        assert_eq!(replaced.status, expected);
        assert_eq!(replaced.last_confirmed_area, previous.last_confirmed_area);
        assert_eq!(replaced.current_area, previous.current_area);
        assert_eq!(replaced.baseline_at, previous.baseline_at);
        rig.area("0001", 1).await;
        assert_eq!(rig.unit(unit).await.status, expected);
        rig.area("0002", 1).await;
        assert_eq!(rig.unit(unit).await.status, UnitStatus::CleanReady);
        rig.store.close().await;
    }
}

#[tokio::test]
async fn test_reads_and_one_stray_after_absence_cannot_clean_a_garment() {
    let mut rig = Rig::new(1).await;
    let unit = rig.snapshot().await.units[0].id;
    rig.bind(unit, "0001").await;
    rig.area("0001", 3).await;
    rig.now += 400;
    let test = rig.observation("0001", 1, 300, 100);
    rig.batch(ObservationPurpose::Test, None, vec![test], true)
        .await
        .unwrap();
    assert_eq!(rig.unit(unit).await.status, UnitStatus::NeedsLaundry);
    rig.area("0001", 1).await;
    rig.absent(1_100, true).await;
    assert_eq!(rig.unit(unit).await.status, UnitStatus::BeingWorn);
    rig.now += 1;
    let stray = rig.observation("0001", 1, 0, 1);
    rig.batch(ObservationPurpose::Inventory, None, vec![stray], true)
        .await
        .unwrap();
    assert_eq!(rig.unit(unit).await.status, UnitStatus::BeingWorn);
    rig.store.close().await;
}

#[tokio::test]
async fn reader_control_recovery_never_adopts_external_settings_as_its_own() {
    let mut rig = Rig::new(1).await;
    let mut cfg = config();
    cfg.readers[0].connection = ReaderConnection::ImpinjLlrp {
        host: "reader.example.invalid".into(),
        port: 5084,
        owned_rospec_id: 47,
    };
    rig.command(TrackingCommand::Configure { config: cfg })
        .await
        .unwrap();
    let version = rig.snapshot().await.configuration_version;
    let reader = id("reader");
    let original = LlrpControlSettings {
        notifications: vec![
            LlrpEventSetting {
                event_type: 2,
                enabled: false,
            },
            LlrpEventSetting {
                event_type: 9,
                enabled: true,
            },
        ],
        keepalive: LlrpKeepalive {
            trigger: KeepaliveTrigger::None,
            interval_ms: 17,
        },
        hold_events_and_reports_upon_reconnect: true,
    };
    let mut applied = original.clone();
    applied.notifications[0].enabled = true;
    applied.keepalive = LlrpKeepalive {
        trigger: KeepaliveTrigger::Periodic,
        interval_ms: 2_000,
    };
    applied.hold_events_and_reports_upon_reconnect = false;
    let plan = rig
        .store
        .tracking_reader_plan_control(&reader, version, &original, &applied, t(rig.now))
        .await
        .unwrap();
    assert_eq!(plan.action, ReaderPlanAction::Create);
    rig.store
        .tracking_reader_control_applied(&reader, version, &applied, false, t(rig.now))
        .await
        .unwrap();
    let mut external = applied.clone();
    external.notifications[1].enabled = false;
    assert_eq!(
        rig.store
            .tracking_reader_restore_control(&reader, &external, t(rig.now))
            .await
            .unwrap()
            .action,
        ReaderPlanAction::Recover
    );
    for _ in 0..2 {
        let retry = rig
            .store
            .tracking_reader_plan_control(&reader, version, &external, &external, t(rig.now))
            .await
            .unwrap();
        assert_eq!(retry.action, ReaderPlanAction::Recover);
        assert_eq!(retry.operation.original, original);
        assert_eq!(
            retry.operation.applied, applied,
            "unknown state must not become owned on a retry"
        );
    }
    let restore = rig
        .store
        .tracking_reader_restore_control(&reader, &applied, t(rig.now))
        .await
        .unwrap();
    assert_eq!(restore.action, ReaderPlanAction::Replace);
    assert_eq!(restore.target, original);
    rig.store
        .tracking_reader_control_applied(&reader, version, &original, true, t(rig.now))
        .await
        .unwrap();
    rig.store.close().await;
}

#[tokio::test]
async fn reader_program_claims_reconcile_lost_replies_but_never_adopt_unknown_programs() {
    let mut rig = Rig::new(1).await;
    let mut cfg = config();
    cfg.readers[0].connection = ReaderConnection::ImpinjLlrp {
        host: "reader.example.invalid".into(),
        port: 5084,
        owned_rospec_id: 47,
    };
    rig.command(TrackingCommand::Configure { config: cfg })
        .await
        .unwrap();
    let version = rig.snapshot().await.configuration_version;
    let reader = id("reader");
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    for _ in 0..2 {
        let unknown = rig
            .store
            .tracking_reader_plan(&reader, version, &a, Some(&a), t(rig.now))
            .await
            .unwrap();
        assert_eq!(unknown.action, ReaderPlanAction::Recover);
        assert!(unknown.operation.known_fingerprints.is_empty());
    }
    let create = rig
        .store
        .tracking_reader_plan(&reader, version, &a, None, t(rig.now))
        .await
        .unwrap();
    assert_eq!(create.action, ReaderPlanAction::Create);
    assert_eq!(create.operation.pending_fingerprint.as_ref(), Some(&a));
    let recovered = rig
        .store
        .tracking_reader_plan(&reader, version, &a, Some(&a), t(rig.now))
        .await
        .unwrap();
    assert_eq!(recovered.action, ReaderPlanAction::Keep);
    assert_eq!(recovered.operation.phase, ReaderOperationPhase::Applied);
    let replacement = rig
        .store
        .tracking_reader_plan(&reader, version, &b, Some(&a), t(rig.now))
        .await
        .unwrap();
    assert_eq!(replacement.action, ReaderPlanAction::Replace);
    rig.store
        .tracking_reader_applied(&reader, version, &b, t(rig.now))
        .await
        .unwrap();
    rig.store
        .tracking_reader_released(&reader, 47, &b, t(rig.now))
        .await
        .unwrap();
    assert_eq!(
        rig.store
            .tracking_reader_plan(&reader, version, &b, Some(&b), t(rig.now))
            .await
            .unwrap()
            .action,
        ReaderPlanAction::Recover,
        "released ownership cannot authorize later deletion of another program"
    );
    rig.store.close().await;
}

#[tokio::test]
async fn duplicate_or_expired_enrollment_evidence_never_reuses_an_old_confirmation() {
    let mut rig = Rig::new(1).await;
    let unit = rig.snapshot().await.units[0].id;
    let session = rig.start(unit, 5, EnrollmentPurpose::Bind).await;
    rig.now += 201;
    let sample = rig.observation("0001", 5, 200, 1);
    rig.batch(
        ObservationPurpose::Enrollment,
        Some(session.id),
        vec![sample.clone()],
        true,
    )
    .await
    .unwrap();
    rig.now += 1;
    rig.batch(
        ObservationPurpose::Enrollment,
        Some(session.id),
        vec![sample],
        true,
    )
    .await
    .unwrap();
    let candidate = rig
        .snapshot()
        .await
        .sessions
        .into_iter()
        .find(|s| s.id == session.id)
        .unwrap();
    assert_eq!(candidate.candidates[0].read_count, 1);
    assert!(!candidate.candidates[0].selectable);
    rig.now += 100;
    let sample = rig.observation("0001", 5, 0, 1);
    rig.batch(
        ObservationPurpose::Enrollment,
        Some(session.id),
        vec![sample],
        true,
    )
    .await
    .unwrap();
    let confirmed = rig
        .snapshot()
        .await
        .sessions
        .into_iter()
        .find(|s| s.id == session.id)
        .unwrap();
    assert!(confirmed.candidates[0].selectable);
    rig.now += 600;
    let sample = rig.observation("0001", 5, 0, 1);
    rig.batch(
        ObservationPurpose::Enrollment,
        Some(session.id),
        vec![sample],
        true,
    )
    .await
    .unwrap();
    let reacquired = rig
        .snapshot()
        .await
        .sessions
        .into_iter()
        .find(|s| s.id == session.id)
        .unwrap();
    assert!(reacquired.capture_version > confirmed.capture_version);
    assert_eq!(reacquired.candidates[0].read_count, 1);
    assert!(!reacquired.candidates[0].selectable);
    rig.store.close().await;
}

#[tokio::test]
async fn claimed_readers_require_cleanup_before_identity_changes_or_import_and_closed_claims_roundtrip()
 {
    for replace in [false, true] {
        let mut rig = Rig::new(1).await;
        let mut cfg = config();
        cfg.readers[0].connection = ReaderConnection::ImpinjLlrp {
            host: "reader.example.invalid".into(),
            port: 5084,
            owned_rospec_id: 47,
        };
        rig.command(TrackingCommand::Configure {
            config: cfg.clone(),
        })
        .await
        .unwrap();
        let completed_capture = rig
            .command(TrackingCommand::StartEnrollment {
                unit_ids: Vec::new(),
                port_id: id("port-5"),
                borrow: false,
                purpose: EnrollmentPurpose::Lookup,
                ttl_seconds: 60,
            })
            .await
            .unwrap()
            .session
            .unwrap();
        let mut removed_station = cfg.clone();
        removed_station.ports.retain(|p| p.id != id("port-5"));
        assert!(matches!(
            rig.command(TrackingCommand::Configure {
                config: removed_station
            })
            .await,
            Err(TrackingError::Conflict("enrollment_abort_required"))
        ));
        rig.command(TrackingCommand::AbortEnrollment {
            session_id: completed_capture.id,
        })
        .await
        .unwrap();
        let clean_export = rig.store.tracking_export(t(rig.now)).await.unwrap();
        let version = rig.snapshot().await.configuration_version;
        let reader = id("reader");
        let fingerprint = "a".repeat(64);
        rig.store
            .tracking_reader_plan(&reader, version, &fingerprint, None, t(rig.now))
            .await
            .unwrap();
        let original = LlrpControlSettings {
            notifications: vec![LlrpEventSetting {
                event_type: 2,
                enabled: false,
            }],
            keepalive: LlrpKeepalive {
                trigger: KeepaliveTrigger::None,
                interval_ms: 0,
            },
            hold_events_and_reports_upon_reconnect: true,
        };
        let mut applied = original.clone();
        applied.notifications[0].enabled = true;
        applied.keepalive = LlrpKeepalive {
            trigger: KeepaliveTrigger::Periodic,
            interval_ms: 2_000,
        };
        applied.hold_events_and_reports_upon_reconnect = false;
        rig.store
            .tracking_reader_plan_control(&reader, version, &original, &applied, t(rig.now))
            .await
            .unwrap();
        let mut removed = cfg.clone();
        removed.readers.clear();
        removed.ports.clear();
        let mut replaced = cfg.clone();
        replaced.readers[0].connection = ReaderConnection::ImpinjLlrp {
            host: "replacement.example.invalid".into(),
            port: 5085,
            owned_rospec_id: 48,
        };
        for attempted in [removed.clone(), replaced.clone()] {
            assert!(matches!(
                rig.command(TrackingCommand::Configure { config: attempted })
                    .await,
                Err(TrackingError::Conflict("reader_cleanup_required"))
            ));
        }
        assert!(matches!(
            rig.command(TrackingCommand::Import {
                data: Box::new(clean_export)
            })
            .await,
            Err(TrackingError::Conflict("reader_cleanup_required"))
        ));
        // Disabling retains the old connection so restart recovery can still reach it.
        let mut disabled = cfg.clone();
        disabled.enabled = false;
        disabled.readers[0].enabled = false;
        rig.command(TrackingCommand::Configure { config: disabled })
            .await
            .unwrap();
        assert_eq!(
            rig.snapshot().await.config.readers[0].connection,
            cfg.readers[0].connection
        );
        // Fixture acknowledgements stand for exact native readback, not RF writes.
        rig.store
            .tracking_reader_applied(&reader, version, &fingerprint, t(rig.now))
            .await
            .unwrap();
        rig.store
            .tracking_reader_control_applied(&reader, version, &applied, false, t(rig.now))
            .await
            .unwrap();
        rig.store
            .tracking_reader_released(&reader, 47, &fingerprint, t(rig.now))
            .await
            .unwrap();
        assert!(
            matches!(
                rig.command(TrackingCommand::Configure {
                    config: removed.clone()
                })
                .await,
                Err(TrackingError::Conflict("reader_cleanup_required"))
            ),
            "global controls still require restoration"
        );
        rig.store
            .tracking_reader_restore_control(&reader, &applied, t(rig.now))
            .await
            .unwrap();
        rig.store
            .tracking_reader_control_applied(&reader, version, &original, true, t(rig.now))
            .await
            .unwrap();
        let mut final_config = if replace { replaced } else { removed };
        final_config.enabled = false;
        rig.command(TrackingCommand::Configure {
            config: final_config.clone(),
        })
        .await
        .unwrap();
        let exported = rig.store.tracking_export(t(rig.now)).await.unwrap();
        assert!(exported.reader_operations.is_empty());
        assert!(exported.reader_controls.is_empty());
        assert!(exported.configurations.iter().any(|c| c.config == cfg));
        let restored = Store::open_simulator(t(rig.now)).await.unwrap();
        restored
            .sync_at(
                SyncRequest {
                    items: vec![catalog(1)],
                    dry_run: false,
                    allow_empty: false,
                },
                Some(0),
                "fixture",
                Some("seed"),
                t(rig.now),
            )
            .await
            .unwrap();
        let revision = restored
            .tracking_snapshot(t(rig.now))
            .await
            .unwrap()
            .tracking_revision;
        restored
            .tracking_command(
                "fixture-operator",
                "restore-cleaned",
                revision,
                TrackingCommand::Import {
                    data: Box::new(exported),
                },
                t(rig.now),
            )
            .await
            .unwrap();
        let snapshot = restored.tracking_snapshot(t(rig.now)).await.unwrap();
        assert_eq!(snapshot.config, final_config);
        assert!(snapshot.reader_operations.is_empty());
        assert!(snapshot.reader_controls.is_empty());
        assert_eq!(snapshot.units[0].id, rig.snapshot().await.units[0].id);
        restored.close().await;
        rig.store.close().await;
    }
}

#[tokio::test]
async fn known_only_reader_ownership_is_not_discarded_and_exact_release_clears_it() {
    let mut rig = Rig::new(1).await;
    let mut cfg = config();
    cfg.readers[0].connection = ReaderConnection::ImpinjLlrp {
        host: "reader.example.invalid".into(),
        port: 5084,
        owned_rospec_id: 47,
    };
    rig.command(TrackingCommand::Configure {
        config: cfg.clone(),
    })
    .await
    .unwrap();
    let version = rig.snapshot().await.configuration_version;
    let reader = id("reader");
    let fingerprint = "a".repeat(64);
    rig.store
        .tracking_reader_plan(&reader, version, &fingerprint, None, t(rig.now))
        .await
        .unwrap();
    let mut recovery = rig.store.tracking_export(t(rig.now)).await.unwrap();
    // This valid retained ownership state can follow a recovery/import: no
    // currently published program, but an exact known fingerprint still exists.
    recovery.reader_operations[0].pending_fingerprint = None;
    recovery.reader_operations[0].applied_fingerprint = None;
    recovery.reader_operations[0].phase = ReaderOperationPhase::RecoveryRequired;
    rig.store
        .tracking_reader_released(&reader, 47, &fingerprint, t(rig.now))
        .await
        .unwrap();
    rig.command(TrackingCommand::Import {
        data: Box::new(recovery),
    })
    .await
    .unwrap();
    assert!(rig.snapshot().await.reader_operations[0].requires_cleanup());
    cfg.readers.clear();
    cfg.ports.clear();
    assert!(matches!(
        rig.command(TrackingCommand::Configure {
            config: cfg.clone()
        })
        .await,
        Err(TrackingError::Conflict("reader_cleanup_required"))
    ));
    let released = rig
        .store
        .tracking_reader_released(&reader, 47, &fingerprint, t(rig.now))
        .await
        .unwrap();
    assert!(!released.requires_cleanup());
    assert!(!rig.snapshot().await.reader_operations[0].requires_cleanup());
    rig.command(TrackingCommand::Configure { config: cfg })
        .await
        .unwrap();
    assert!(
        rig.store
            .tracking_export(t(rig.now))
            .await
            .unwrap()
            .reader_operations
            .is_empty()
    );
    rig.store.close().await;
}
