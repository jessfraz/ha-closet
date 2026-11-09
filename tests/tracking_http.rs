use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use closet::{
    api,
    clock::Clock,
    models::{CatalogItem, ItemsPage, ListQuery, SyncRequest},
    store::Store,
    tracking::models::*,
};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

const OWNER: &str = "example-owner-credential-for-tracking-tests";
const INTEGRATION: &str = "example-integration-credential-for-tracking-tests";

struct TestClock(Mutex<DateTime<Utc>>);
impl Clock for TestClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}
impl TestClock {
    fn advance(&self) {
        *self.0.lock().unwrap() += Duration::milliseconds(10);
    }
}
struct App {
    directory: tempfile::TempDir,
    store: Store,
    clock: Arc<TestClock>,
    server: dropshot::HttpServer<Arc<api::Context>>,
    context: Arc<api::Context>,
    origin: String,
    client: Client,
}
impl App {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("closet.sqlite3"))
            .await
            .unwrap();
        let clock = Arc::new(TestClock(Mutex::new(Utc::now() + Duration::seconds(1))));
        let mut items: Vec<CatalogItem> =
            serde_json::from_str(include_str!("../config/catalog.example.json")).unwrap();
        items[0].quantity = Some(2);
        store
            .sync_at(
                SyncRequest {
                    items,
                    dry_run: false,
                    allow_empty: false,
                },
                Some(0),
                "test",
                Some("seed"),
                clock.now(),
            )
            .await
            .unwrap();
        let context = Arc::new(
            api::Context::new(store.clone(), OWNER)
                .with_integration(INTEGRATION)
                .with_clock(clock.clone()),
        );
        let server = api::start("127.0.0.1:0".parse().unwrap(), context.clone()).unwrap();
        let origin = format!("http://{}", server.local_addr());
        Self {
            directory,
            store,
            clock,
            server,
            context,
            origin,
            client: Client::new(),
        }
    }
    async fn snapshot(&self) -> TrackingSnapshot {
        self.client
            .get(format!("{}/v1/tracking", self.origin))
            .bearer_auth(OWNER)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    async fn command(
        &self,
        key: &str,
        revision: i64,
        command: &TrackingCommand,
    ) -> reqwest::Response {
        self.client
            .post(format!("{}/v1/tracking/commands", self.origin))
            .bearer_auth(OWNER)
            .header("If-Match", format!("\"{revision}\""))
            .header("Idempotency-Key", key)
            .json(command)
            .send()
            .await
            .unwrap()
    }
    async fn mutate(&self, key: &str, command: TrackingCommand) -> CommandResult {
        let revision = self.snapshot().await.tracking_revision;
        let response = self.command(key, revision, &command).await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{}",
            response.text().await.unwrap_or_default()
        );
        // Read through the actual retry path too: the original revision may now
        // be stale but an exact idempotency replay still returns its receipt.
        self.command(key, revision, &command)
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    async fn configure(&self) {
        let mut config: TrackingConfig =
            serde_json::from_str(include_str!("../config/tracking.example.json")).unwrap();
        config.thresholds.positive_dwell_ms = 1;
        self.mutate("configure", TrackingCommand::Configure { config })
            .await;
    }
    async fn close(self) {
        self.server.close().await.unwrap();
        self.context.shutdown().await.unwrap();
        self.store.close().await;
    }
    async fn cold_backup(self, path: &std::path::Path) {
        self.server.close().await.unwrap();
        self.context.shutdown().await.unwrap();
        self.store.close().await;
        std::fs::copy(self.directory.path().join("closet.sqlite3"), path).unwrap();
    }
}

#[tokio::test]
async fn protected_runtime_api_requires_admin_own_revision_and_idempotency() {
    let app = App::new().await;
    assert_eq!(
        app.client
            .get(format!("{}/v1/tracking", app.origin))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let command = TrackingCommand::Tick;
    let response = app
        .client
        .post(format!("{}/v1/tracking/commands", app.origin))
        .bearer_auth(OWNER)
        .header("Idempotency-Key", "no-revision")
        .json(&command)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PRECONDITION_REQUIRED);
    let response = app
        .client
        .post(format!("{}/v1/tracking/commands", app.origin))
        .bearer_auth(INTEGRATION)
        .header("If-Match", "\"0\"")
        .header("Idempotency-Key", "not-admin")
        .json(&command)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    app.configure().await;
    let snapshot = app.snapshot().await;
    assert_eq!(snapshot.units.len(), 5);
    assert_eq!(app.store.counts().await.unwrap().0, 1);
    let revision = snapshot.tracking_revision;
    let unit = snapshot.units[0].id;
    app.mutate(
        "baseline",
        TrackingCommand::Baseline {
            unit_id: unit,
            area_id: "example-closet".parse().unwrap(),
        },
    )
    .await;
    assert_eq!(
        app.command(
            "stale",
            revision,
            &TrackingCommand::Pause {
                unit_id: unit,
                paused: true
            }
        )
        .await
        .status(),
        StatusCode::PRECONDITION_FAILED
    );
    assert_eq!(
        app.store.counts().await.unwrap().0,
        1,
        "runtime writes cannot change catalog revision"
    );
    assert_eq!(
        app.command(
            "baseline",
            revision,
            &TrackingCommand::Retire {
                unit_id: unit,
                note: "Different body".into()
            }
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    for path in [
        "/v1/tracking/export",
        "/v1/tracking/history",
        "/v1/tracking/coverage",
        "/v1/tracking/analytics",
    ] {
        assert_eq!(
            app.client
                .get(format!("{}{path}", app.origin))
                .bearer_auth(INTEGRATION)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    app.close().await;
}

#[tokio::test]
async fn pairing_requires_isolation_fresh_capture_and_atomic_unique_binding() {
    let app = App::new().await;
    app.configure().await;
    let units = app.snapshot().await.units;
    let queue = vec![units[0].id, units[1].id];
    let session = app
        .mutate(
            "start",
            TrackingCommand::StartEnrollment {
                unit_ids: queue.clone(),
                port_id: "example-enroll".parse().unwrap(),
                borrow: false,
                purpose: EnrollmentPurpose::Bind,
                ttl_seconds: 900,
            },
        )
        .await
        .session
        .unwrap();
    let radio_session = uuid::Uuid::now_v7();
    // Establish a new reader epoch before starting the capture. Disconnects
    // invalidate pending confirmations rather than treating reconnect as fresh.
    let ingest = |sequence, observations| ObservationBatch {
        reader_id: "example-enrollment-reader".parse().unwrap(),
        purpose: ObservationPurpose::Enrollment,
        data_source: DataSource::Live,
        capture_session: Some(session.id),
        session: radio_session,
        sequence,
        observed_at: app.clock.now(),
        connected: true,
        coverage: Vec::new(),
        observations,
        readback: None,
    };
    app.store
        .tracking_ingest(ingest(0, Vec::new()), app.clock.now())
        .await
        .unwrap();
    app.clock.advance();
    let at = app.clock.now();
    let observation = |epc: &str| TagObservation {
        epc: epc.parse().unwrap(),
        port_id: "example-enroll".parse().unwrap(),
        first_seen_at: at - Duration::milliseconds(1),
        last_seen_at: at,
        read_count: 5,
        tid: None,
        peak_rssi_cdbm: None,
        event_ref: None,
    };
    app.store
        .tracking_ingest(
            ingest(1, vec![observation("00AA"), observation("00BB")]),
            at,
        )
        .await
        .unwrap();
    let current = app
        .snapshot()
        .await
        .sessions
        .into_iter()
        .find(|s| s.id == session.id)
        .unwrap();
    assert_eq!(current.candidates.len(), 2);
    let confirm = TrackingCommand::SelectCandidate {
        session_id: session.id,
        capture_version: current.capture_version,
        unit_id: queue[0],
        epc: "00AA".parse().unwrap(),
        baseline_area: None,
    };
    assert_eq!(
        app.command(
            "ambiguous",
            app.snapshot().await.tracking_revision,
            &confirm
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    app.mutate(
        "reset",
        TrackingCommand::ResetCapture {
            session_id: session.id,
        },
    )
    .await;
    app.clock.advance();
    let at = app.clock.now();
    let mut observation = observation("00AA");
    observation.first_seen_at = at - Duration::milliseconds(1);
    observation.last_seen_at = at;
    app.store
        .tracking_ingest(ingest(2, vec![observation]), at)
        .await
        .unwrap();
    let snap = app.snapshot().await;
    let capture = snap.sessions.iter().find(|s| s.id == session.id).unwrap();
    assert!(capture.candidates[0].selectable);
    let confirm = TrackingCommand::SelectCandidate {
        session_id: session.id,
        capture_version: capture.capture_version,
        unit_id: queue[0],
        epc: "00aa".parse().unwrap(),
        baseline_area: None,
    };
    let (first, second) = tokio::join!(
        app.command("pair", snap.tracking_revision, &confirm),
        app.command("pair", snap.tracking_revision, &confirm)
    );
    let first: CommandResult = first.error_for_status().unwrap().json().await.unwrap();
    let second: CommandResult = second.error_for_status().unwrap().json().await.unwrap();
    assert_eq!(first.event_ids, second.event_ids);
    let snap = app.snapshot().await;
    assert_eq!(snap.counts.tagged_units, 1);
    assert_eq!(snap.counts.being_worn, 0);
    let capture = snap.sessions.iter().find(|s| s.id == session.id).unwrap();
    assert!(capture.candidates.is_empty());
    assert_eq!(capture.cursor, 1);
    let ordinary: TrackingSnapshot = app
        .client
        .get(format!("{}/v1/tracking", app.origin))
        .bearer_auth(INTEGRATION)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(ordinary.sessions.is_empty());
    assert!(ordinary.diagnostics.recent_reads.is_empty());
    #[derive(Deserialize)]
    struct ErrorCode {
        error_code: String,
    }
    let stale = app
        .command("old-candidate", snap.tracking_revision, &confirm)
        .await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(
        stale.json::<ErrorCode>().await.unwrap().error_code,
        "enrollment_selection_changed"
    );
    let backup = tempfile::tempdir().unwrap();
    let path = backup.path().join("restored.sqlite3");
    let before = app.store.tracking_export(app.clock.now()).await.unwrap();
    app.cold_backup(&path).await;
    let restored = Store::open(&path).await.unwrap();
    let after = restored.tracking_export(Utc::now()).await.unwrap();
    assert_eq!(
        serde_json::to_vec(&after.bindings).unwrap(),
        serde_json::to_vec(&before.bindings).unwrap()
    );
    assert_eq!(after.history.len(), before.history.len());
    assert_eq!(
        after
            .units
            .iter()
            .map(|unit| unit.unit.id)
            .collect::<std::collections::BTreeSet<_>>(),
        before.units.iter().map(|unit| unit.unit.id).collect()
    );
    let snapshot = restored.tracking_snapshot(Utc::now()).await.unwrap();
    assert!(!snapshot.health.absence_coverage_healthy);
    assert!(
        snapshot
            .sessions
            .iter()
            .all(|session| session.candidates.is_empty())
    );
    restored.close().await;
}

#[tokio::test]
async fn ready_filter_and_pagination_track_runtime_without_invalidating_plain_catalog_pages() {
    let app = App::new().await;
    app.configure().await;
    let units = app.snapshot().await.units;
    for (index, unit) in units.iter().enumerate() {
        app.mutate(
            &format!("baseline-{index}"),
            TrackingCommand::Baseline {
                unit_id: unit.id,
                area_id: "example-closet".parse().unwrap(),
            },
        )
        .await;
    }
    let plain = app
        .store
        .list(ListQuery {
            limit: Some(1),
            ..Default::default()
        })
        .await
        .unwrap();
    let filtered: ItemsPage = app
        .client
        .get(format!(
            "{}/v1/items?ready_to_wear=true&limit=1",
            app.origin
        ))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    app.mutate(
        "worn",
        TrackingCommand::Correct {
            unit_id: units[0].id,
            status: UnitStatus::BeingWorn,
            note: "Manual test baseline".into(),
        },
    )
    .await;
    assert!(
        app.store
            .list(ListQuery {
                page_token: plain.next_page,
                ..Default::default()
            })
            .await
            .is_ok()
    );
    let response = app
        .client
        .get(format!("{}/v1/items", app.origin))
        .bearer_auth(OWNER)
        .query(&[("page_token", filtered.next_page.unwrap())])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let ready = app
        .store
        .list(ListQuery {
            ready_to_wear: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(ready.total, 3);
    let status = app
        .store
        .list(ListQuery {
            status: Some(UnitStatus::BeingWorn),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(status.total, 1);
    app.close().await;
}

#[tokio::test]
async fn today_rechecks_availability_at_publication_and_marks_saved_results_stale() {
    use closet::{
        models::{Freshness, GenerationStatus, Suggestion, TodayRequest, TodayResult},
        store::GenerationBasis,
    };
    let app = App::new().await;
    app.configure().await;
    let now = app.clock.now();
    let date = now.date_naive().to_string();
    let basis = GenerationBasis {
        date: date.clone(),
        timezone: "UTC".into(),
        config_hash: "example-config".into(),
        items: app.store.active_items().await.unwrap(),
    };
    let first = app
        .store
        .reserve_generation(TodayRequest::default(), basis.clone(), "owner", "first")
        .await
        .unwrap();
    let result = TodayResult {
        generation_id: first.generation.id.clone(),
        inputs: TodayRequest::default(),
        generated_at: now,
        expires_at: now + Duration::hours(1),
        freshness: Freshness::Ready,
        stale_reasons: Vec::new(),
        weather: None,
        suggestions: vec![Suggestion {
            occasion: None,
            label: "Example combination".into(),
            item_ids: vec![
                "example-tee".into(),
                "example-denim".into(),
                "example-flats".into(),
            ],
            advisory_reason: "Fictional outfit".into(),
            warnings: Vec::new(),
        }],
    };
    app.store
        .finish_generation(&first.generation.id, Ok(result.clone()))
        .await
        .unwrap();
    let second = app
        .store
        .reserve_generation(TodayRequest::default(), basis, "owner", "second")
        .await
        .unwrap();
    for unit in app
        .snapshot()
        .await
        .units
        .into_iter()
        .filter(|unit| unit.catalog_id == "example-tee")
    {
        app.mutate(
            &format!("unavailable-{}", unit.ordinal),
            TrackingCommand::Correct {
                unit_id: unit.id,
                status: UnitStatus::InWash,
                note: "Manual current baseline".into(),
            },
        )
        .await;
    }
    let availability = app.store.tracking_availability(now).await.unwrap();
    assert!(
        !availability
            .eligible_catalog_ids
            .contains(&"example-tee".into())
    );
    assert!(
        availability
            .eligible_catalog_ids
            .contains(&"example-denim".into()),
        "untagged inventory retains previous behavior"
    );
    let mut refreshed = result;
    refreshed.generation_id = second.generation.id.clone();
    app.store
        .finish_generation(&second.generation.id, Ok(refreshed))
        .await
        .unwrap();
    let today = app
        .store
        .today(&date, "UTC", "example-config", now, None)
        .await
        .unwrap();
    let generation = today.generation.unwrap();
    assert_eq!(generation.status, GenerationStatus::Failed);
    assert_eq!(
        generation.error_code.as_deref(),
        Some("referenced_items_unavailable")
    );
    let saved = today.result.unwrap();
    assert_eq!(saved.generation_id, first.generation.id);
    assert!(matches!(saved.freshness, Freshness::Stale));
    assert!(
        saved
            .stale_reasons
            .contains(&"referenced_items_unavailable".into())
    );
    app.close().await;
}

#[tokio::test]
async fn combined_tracking_filters_must_match_the_same_physical_copy() {
    let app = App::new().await;
    app.configure().await;
    let mut copies: Vec<_> = app
        .snapshot()
        .await
        .units
        .into_iter()
        .filter(|unit| unit.catalog_id == "example-tee")
        .collect();
    copies.sort_by_key(|unit| unit.ordinal);
    app.mutate(
        "ready-copy",
        TrackingCommand::Baseline {
            unit_id: copies[0].id,
            area_id: "example-closet".parse().unwrap(),
        },
    )
    .await;
    app.mutate(
        "dirty-copy",
        TrackingCommand::Baseline {
            unit_id: copies[1].id,
            area_id: "example-hamper".parse().unwrap(),
        },
    )
    .await;
    for query in [
        "ready_to_wear=true&area=example-hamper",
        "ready_to_wear=true&status=needs_laundry&area=example-hamper",
    ] {
        let page: ItemsPage = app
            .client
            .get(format!("{}/v1/items?{query}", app.origin))
            .bearer_auth(OWNER)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            page.total, 0,
            "{query} must not combine evidence from different copies"
        );
    }
    let ready = app
        .store
        .list(ListQuery {
            ready_to_wear: Some(true),
            area: Some("example-closet".parse().unwrap()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(ready.total, 1);
    // False readiness is catalog-level: a card is not unavailable when another
    // copy is ready elsewhere, even if the area filter matches a dirty copy.
    let not_ready = app
        .store
        .list(ListQuery {
            ready_to_wear: Some(false),
            area: Some("example-hamper".parse().unwrap()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(not_ready.total, 0);
    app.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_cli_exports_private_complete_json_without_clobbering() {
    use std::os::unix::fs::PermissionsExt;
    let app = App::new().await;
    app.configure().await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tracking.json");
    let call = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_ha-closet"))
            .args(["--transport", "native", "tracking", "export", "--output"])
            .arg(&path)
            .env("CLOSET_API_URL", &app.origin)
            .env("CLOSET_API_TOKEN", OWNER)
            .output()
            .unwrap()
    };
    let first = call();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let bytes = std::fs::read(&path).unwrap();
    let export: TrackingExport = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(export.units.len(), 5);
    assert!(export.coverage.is_empty());
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!call().status.success());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(
        std::fs::read_dir(directory.path()).unwrap().count(),
        1,
        "no abandoned temporary exports"
    );
    let coverage = std::process::Command::new(env!("CARGO_BIN_EXE_ha-closet"))
        .args([
            "--transport",
            "native",
            "tracking",
            "coverage",
            "--limit",
            "1",
        ])
        .env("CLOSET_API_URL", &app.origin)
        .env("CLOSET_API_TOKEN", OWNER)
        .output()
        .unwrap();
    assert!(
        coverage.status.success(),
        "{}",
        String::from_utf8_lossy(&coverage.stderr)
    );
    #[derive(Deserialize)]
    struct CoverageOutput {
        data: CoverageHistoryPage,
    }
    let coverage: CoverageOutput = serde_json::from_slice(&coverage.stdout).unwrap();
    assert!(coverage.data.intervals.is_empty());
    assert!(coverage.data.next_before.is_none());
    let analytics = std::process::Command::new(env!("CARGO_BIN_EXE_ha-closet"))
        .args([
            "--transport",
            "native",
            "tracking",
            "analytics",
            "--days",
            "30",
            "--pick-seed",
            "8",
        ])
        .env("CLOSET_API_URL", &app.origin)
        .env("CLOSET_API_TOKEN", OWNER)
        .output()
        .unwrap();
    assert!(
        analytics.status.success(),
        "{}",
        String::from_utf8_lossy(&analytics.stderr)
    );
    #[derive(Deserialize)]
    struct AnalyticsOutput {
        data: closet::tracking::analytics::AnalyticsReport,
    }
    let analytics: AnalyticsOutput = serde_json::from_slice(&analytics.stdout).unwrap();
    assert_eq!(analytics.data.days, 30);
    assert_eq!(analytics.data.inferred_wears, 0);
    assert!(analytics.data.most_worn.is_empty());
    assert!(analytics.data.overlooked.is_empty());
    assert!(analytics.data.pick.is_none());
    assert_eq!(
        app.client
            .get(format!("{}/v1/tracking/coverage?limit=0", app.origin))
            .bearer_auth(OWNER)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    app.close().await;
}

#[tokio::test]
async fn tracking_capacity_cannot_reject_catalog_sync_or_overflow_the_ha_projection() {
    let app = App::new().await;
    app.configure().await;
    let template: Vec<CatalogItem> =
        serde_json::from_str(include_str!("../config/catalog.example.json")).unwrap();
    let records = (0..21)
        .map(|index| {
            let mut item = template[0].clone();
            item.id = format!("example-bulk-{index:02}");
            item.quantity = Some(1000);
            item
        })
        .collect();
    let response = app
        .client
        .post(format!("{}/v1/inventory/sync", app.origin))
        .bearer_auth(OWNER)
        .header("If-Match", "\"1\"")
        .header("Idempotency-Key", "large-valid-catalog")
        .json(&SyncRequest {
            items: records,
            dry_run: false,
            allow_empty: false,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        response.text().await.unwrap_or_default()
    );
    let inventory = app.store.list(ListQuery::default()).await.unwrap();
    assert_eq!(inventory.total, 21);
    assert_eq!(inventory.catalog_revision, 2);
    let response = app
        .client
        .get(format!("{}/v1/tracking/projection", app.origin))
        .bearer_auth(INTEGRATION)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let bytes = response.bytes().await.unwrap();
    assert!(
        bytes.len() < 8 * 1024 * 1024,
        "HA projection exceeds its actual transport bound"
    );
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Unit {
        id: UnitId,
        ordinal: u32,
        status: UnitStatus,
        paused: bool,
        retired: bool,
        archived: bool,
        evidence: StateEvidence,
    }
    #[derive(Deserialize)]
    struct Projection {
        units: Vec<Unit>,
        counts: TrackingCounts,
        next_page: Option<String>,
    }
    let projection: Projection = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(projection.units.len(), 500);
    assert!(projection.next_page.is_some());
    assert_eq!(projection.counts.unallocated_units, 1000);
    assert_eq!(projection.counts.capacity_limited_items, 1);
    let ids: std::collections::BTreeSet<_> = projection.units.iter().map(|unit| unit.id).collect();
    assert_eq!(ids.len(), 500);
    assert!(projection.units.iter().all(|unit| unit.ordinal > 0
        && unit.status == UnitStatus::Unknown
        && !unit.paused
        && !unit.retired
        && !unit.archived
        && unit.evidence == StateEvidence::Unknown));
    assert_eq!(projection.counts.tagged_units, 0);
    app.close().await;
}

// A released configuration request, including the obsolete activation bit.
// Keep this producer independent of the new serializer to exercise old clients.
#[derive(Clone, Serialize, Deserialize)]
struct ReleasedConfig {
    schema_version: u16,
    #[serde(default)]
    enabled: bool,
    areas: Vec<AreaConfig>,
    readers: Vec<ReaderConfig>,
    ports: Vec<PortConfig>,
    thresholds: Thresholds,
}
#[derive(Serialize)]
struct ReleasedConfigure<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    config: &'a ReleasedConfig,
}
#[derive(Deserialize)]
struct ConfigActivity {
    enabled: bool,
}
fn config_activity(config: &TrackingConfig) -> bool {
    serde_json::from_slice::<ConfigActivity>(&serde_json::to_vec(config).unwrap())
        .unwrap()
        .enabled
}
async fn released_configure(app: &App, key: &str, config: &ReleasedConfig) -> CommandResult {
    let revision = app.snapshot().await.tracking_revision;
    let mut receipt = None;
    for _ in 0..2 {
        let result = app
            .client
            .post(format!("{}/v1/tracking/commands", app.origin))
            .bearer_auth(OWNER)
            .header("If-Match", format!("\"{revision}\""))
            .header("Idempotency-Key", key)
            .json(&ReleasedConfigure {
                kind: "configure",
                config,
            })
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<CommandResult>()
            .await
            .unwrap();
        if let Some(previous) = &receipt {
            assert_eq!(
                serde_json::to_vec(previous).unwrap(),
                serde_json::to_vec(&result).unwrap()
            );
        }
        receipt = Some(result);
    }
    receipt.unwrap()
}
#[tokio::test]
async fn area_presence_controls_tracking_and_survives_backup_restore() {
    let app = App::new().await;
    assert!(!config_activity(&app.snapshot().await.config));
    let mut config: ReleasedConfig =
        serde_json::from_str(include_str!("../config/tracking.example.json")).unwrap();
    config.enabled = false;
    config.areas.truncate(1);
    let area = config.areas[0].id.clone();
    config
        .ports
        .retain(|port| port.area_id.is_none() || port.area_id.as_ref() == Some(&area));
    released_configure(&app, "old-client-add-area", &config).await;
    let configured = app.snapshot().await;
    assert!(
        config_activity(&configured.config),
        "an area must enable tracking even when an old client sends enabled=false"
    );
    assert!(
        !configured.health.absence_coverage_healthy,
        "configured areas still need actual coverage"
    );
    let unit = configured.units[0].id;
    app.mutate(
        "manual-baseline",
        TrackingCommand::Baseline {
            unit_id: unit,
            area_id: area,
        },
    )
    .await;
    let baseline = app.snapshot().await.units;
    config.areas[0].enabled = false;
    released_configure(&app, "disable-last-area", &config).await;
    let disabled = app.snapshot().await;
    assert!(!config_activity(&disabled.config));
    assert_eq!(disabled.units, baseline);
    config.areas[0].enabled = true;
    released_configure(&app, "restore-last-area", &config).await;
    assert!(config_activity(&app.snapshot().await.config));
    let copy = tempfile::tempdir().unwrap();
    let database = copy.path().join("backup.sqlite3");
    app.cold_backup(&database).await;
    let store = Store::open(&database).await.unwrap();
    let now = Utc::now() + Duration::seconds(3);
    let restored = store.tracking_snapshot(now).await.unwrap();
    assert!(config_activity(&restored.config));
    assert!(!restored.health.absence_coverage_healthy);
    assert_eq!(restored.units, baseline);
    let mut no_areas = restored.config;
    no_areas.areas.clear();
    no_areas.ports.retain(|port| port.area_id.is_none());
    store
        .tracking_command(
            "restore-test",
            "remove-last-area",
            restored.tracking_revision,
            TrackingCommand::Configure { config: no_areas },
            now,
        )
        .await
        .unwrap();
    let stopped = store.tracking_snapshot(now).await.unwrap();
    assert!(!config_activity(&stopped.config));
    assert_eq!(stopped.units, baseline);
    store.close().await;
}
