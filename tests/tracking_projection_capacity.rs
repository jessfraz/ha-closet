use std::{collections::BTreeSet, sync::Arc};

use chrono::Utc;
use closet::{
    api,
    models::{CatalogItem, SyncReport, SyncRequest},
    store::Store,
    tracking::models::{
        CommandResult, StateEvidence, TrackingCommand, TrackingConfig, TrackingCounts, UnitId,
        UnitStatus,
    },
};
use reqwest::{Client, Response, StatusCode};
use serde::Deserialize;

const OWNER: &str = "fictional-owner-for-projection-capacity-tests";
const INTEGRATION: &str = "fictional-ha-for-projection-capacity-tests";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectedUnit {
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
    tracking_revision: i64,
    counts: TrackingCounts,
    units: Vec<ProjectedUnit>,
    certified_ready_unit_ids: Vec<UnitId>,
    next_page: Option<String>,
}
#[derive(Deserialize)]
struct ErrorCode {
    error_code: String,
}
struct App {
    _directory: tempfile::TempDir,
    store: Store,
    context: Arc<api::Context>,
    server: dropshot::HttpServer<Arc<api::Context>>,
    origin: String,
    client: Client,
    catalog_revision: i64,
}
impl App {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("closet.sqlite3"))
            .await
            .unwrap();
        let context =
            Arc::new(api::Context::new(store.clone(), OWNER).with_integration(INTEGRATION));
        let server = api::start("127.0.0.1:0".parse().unwrap(), context.clone()).unwrap();
        let origin = format!("http://{}", server.local_addr());
        Self {
            _directory: directory,
            store,
            context,
            server,
            origin,
            client: Client::new(),
            catalog_revision: 0,
        }
    }
    async fn sync(&mut self, items: Vec<CatalogItem>, key: &str) {
        let response = self
            .client
            .post(format!("{}/v1/inventory/sync", self.origin))
            .bearer_auth(OWNER)
            .header("If-Match", format!("\"{}\"", self.catalog_revision))
            .header("Idempotency-Key", key)
            .json(&SyncRequest {
                items,
                dry_run: false,
                allow_empty: false,
            })
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<SyncReport>()
            .await
            .unwrap();
        self.catalog_revision = response.catalog_revision;
    }
    async fn page(&self, limit: Option<u16>, token: Option<&str>) -> Response {
        let mut request = self
            .client
            .get(format!("{}/v1/tracking/projection", self.origin))
            .bearer_auth(INTEGRATION);
        if let Some(limit) = limit {
            request = request.query(&[("limit", limit)]);
        }
        if let Some(token) = token {
            request = request.query(&[("page_token", token)]);
        }
        request.send().await.unwrap()
    }
    async fn command(&self, key: &str, command: TrackingCommand) {
        let revision = self
            .store
            .tracking_snapshot(Utc::now())
            .await
            .unwrap()
            .tracking_revision;
        self.client
            .post(format!("{}/v1/tracking/commands", self.origin))
            .bearer_auth(OWNER)
            .header("If-Match", format!("\"{revision}\""))
            .header("Idempotency-Key", key)
            .json(&command)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<CommandResult>()
            .await
            .unwrap();
    }
    async fn close(self) {
        self.server.close().await.unwrap();
        self.context.shutdown().await.unwrap();
        self.store.close().await;
    }
}
fn cohort(number: u8) -> Vec<CatalogItem> {
    let template: Vec<CatalogItem> =
        serde_json::from_str(include_str!("../config/catalog.example.json")).unwrap();
    (0..20)
        .map(|index| {
            let mut item = template[0].clone();
            item.id = format!("example-cohort-{number}-{index:02}");
            item.quantity = Some(1000);
            item
        })
        .collect()
}

#[tokio::test]
async fn restored_identities_over_capacity_remain_complete_in_bounded_revision_bound_pages() {
    let mut app = App::new().await;
    let first = cohort(1);
    let second = cohort(2);
    app.sync(first.clone(), "first-cohort").await;
    let original_ids: BTreeSet<_> = app
        .store
        .tracking_snapshot(Utc::now())
        .await
        .unwrap()
        .units
        .into_iter()
        .map(|u| u.id)
        .collect();
    assert_eq!(original_ids.len(), 20_000);
    app.sync(second.clone(), "archive-first").await;
    let second_ids: BTreeSet<_> = app
        .store
        .tracking_snapshot(Utc::now())
        .await
        .unwrap()
        .units
        .into_iter()
        .filter(|u| !u.archived && !u.retired)
        .map(|u| u.id)
        .collect();
    assert_eq!(second_ids.len(), 20_000);
    assert!(original_ids.is_disjoint(&second_ids));
    app.sync(first.into_iter().chain(second).collect(), "restore-first")
        .await;
    let expected: BTreeSet<_> = original_ids.union(&second_ids).copied().collect();
    let first_id = *expected.first().unwrap();
    let last_id = *expected.last().unwrap();
    let config: TrackingConfig =
        serde_json::from_str(include_str!("../config/tracking.example.json")).unwrap();
    app.command("configure", TrackingCommand::Configure { config })
        .await;
    for (key, unit_id) in [("ready-first", first_id), ("ready-last", last_id)] {
        app.command(
            key,
            TrackingCommand::Baseline {
                unit_id,
                area_id: "example-closet".parse().unwrap(),
            },
        )
        .await;
    }
    let default: Projection = app
        .page(None, None)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(default.units.len(), 500);
    assert_eq!(default.certified_ready_unit_ids, vec![first_id]);
    let stale_token = default.next_page.unwrap();
    let revision = default.tracking_revision;
    let mut token = None;
    let mut seen = BTreeSet::new();
    let mut ready = BTreeSet::new();
    let mut pages = 0;
    loop {
        let response = app
            .page(Some(1000), token.as_deref())
            .await
            .error_for_status()
            .unwrap();
        assert_eq!(response.headers()["cache-control"], "no-store");
        let bytes = response.bytes().await.unwrap();
        assert!(
            bytes.len() < 8 * 1024 * 1024,
            "one HA page exceeds its actual transport bound"
        );
        let page: Projection = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(page.tracking_revision, revision);
        assert_eq!(page.counts.total_units, 40_000);
        assert_eq!(page.counts.certified_ready, 2);
        assert_eq!(page.counts.unallocated_units, 0);
        assert_eq!(page.counts.capacity_limited_items, 0);
        assert!(!page.units.is_empty() && page.units.len() <= 1000);
        let ids: BTreeSet<_> = page.units.iter().map(|u| u.id).collect();
        assert_eq!(ids.len(), page.units.len());
        assert!(seen.is_disjoint(&ids));
        assert!(
            page.certified_ready_unit_ids
                .iter()
                .all(|id| ids.contains(id))
        );
        assert!(page.units.iter().all(|u| u.ordinal > 0
            && !u.paused
            && !u.archived
            && !u.retired
            && if [first_id, last_id].contains(&u.id) {
                u.status == UnitStatus::CleanReady && u.evidence == StateEvidence::Manual
            } else {
                u.status == UnitStatus::Unknown && u.evidence == StateEvidence::Unknown
            }));
        seen.extend(ids);
        ready.extend(page.certified_ready_unit_ids);
        pages += 1;
        token = page.next_page;
        if token.is_none() {
            break;
        }
        assert!(pages < 41, "pagination did not terminate");
    }
    assert_eq!(pages, 40);
    assert_eq!(
        seen, expected,
        "restoration lost or duplicated stable physical identities"
    );
    assert_eq!(ready, BTreeSet::from([first_id, last_id]));
    app.command(
        "manual-change",
        TrackingCommand::Correct {
            unit_id: first_id,
            status: UnitStatus::NeedsLaundry,
            note: "Fictional manual correction".into(),
        },
    )
    .await;
    let stale = app.page(Some(1000), Some(&stale_token)).await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(
        stale.json::<ErrorCode>().await.unwrap().error_code,
        "page_invalidated"
    );
    app.close().await;
}
