use std::{collections::BTreeSet, net::SocketAddr, sync::Arc};

use closet::{
    api,
    models::{
        CatalogItem, FieldError, Generation, GenerationStatus, ImageConfidence, ImageCrop,
        ImageReference, Item, ListQuery, Occasion, Price, Shop, SyncReport, SyncRequest,
        TodayRequest, TodayResponse, parse_catalog,
    },
    store::{GenerationBasis, Store, StoreError},
};
use reqwest::{Client, StatusCode};
use tempfile::TempDir;

const OWNER: &str = "example-owner-credential-for-tests-only";
const INTEGRATION: &str = "example-adapter-credential-for-tests-only";

fn piece(id: &str, name: &str, category: &str) -> CatalogItem {
    CatalogItem {
        id: id.into(),
        brand: Some("Example label".into()),
        name: name.into(),
        category: category.into(),
        color: Some("Navy".into()),
        size: None,
        quantity: Some(1),
        usage: closet::models::Usage::Everyday,
        identity: None,
        material: None,
        sku: None,
        purchase_source: Some("example".into()),
        images: vec![ImageReference {
            url: "https://images.example.invalid/reference.jpg".into(),
            source_url: Some("https://example.invalid/product".into()),
            kind: "product_reference".into(),
            confidence: ImageConfidence::Medium,
            notes: Some("Example reference, not an exact variant".into()),
            crop: None,
        }],
        notes: vec!["An invented catalog record".into()],
        price: Some(Price {
            amount: 25.5,
            basis: "item".into(),
            currency: "USD".into(),
            quantity_basis: "one".into(),
        }),
        shop: Some(Shop {
            id: "example-shop".into(),
            label: "Example shop".into(),
            season: None,
            years: vec![2026],
        }),
    }
}
fn catalog() -> Vec<CatalogItem> {
    vec![
        piece("example-dress", "Jersey dress", "clothing.dress"),
        piece("example-shoes", "Leather flats", "footwear.shoes"),
        piece("example-jumpsuit", "Cotton jumpsuit", "clothing.jumpsuit"),
    ]
}
fn sync(items: Vec<CatalogItem>, dry_run: bool) -> SyncRequest {
    SyncRequest {
        items,
        dry_run,
        allow_empty: false,
    }
}

struct App {
    _directory: TempDir,
    store: Store,
    context: Arc<api::Context>,
    server: dropshot::HttpServer<Arc<api::Context>>,
    client: Client,
    origin: String,
}
impl App {
    async fn new() -> Self {
        Self::with_today(None).await
    }
    async fn with_today(today: Option<closet::today::TodayConfig>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("catalog.sqlite3"))
            .await
            .unwrap();
        let mut context = api::Context::new(store.clone(), OWNER).with_integration(INTEGRATION);
        if let Some(today) = today {
            context = context.with_today(today);
        }
        let context = Arc::new(context);
        let server = api::start(
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            context.clone(),
        )
        .unwrap();
        let origin = format!("http://{}", server.local_addr());
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        Self {
            _directory: directory,
            store,
            context,
            server,
            client,
            origin,
        }
    }
    async fn close(self) {
        self.server.close().await.unwrap();
        self.context.shutdown().await.unwrap();
        self.store.close().await;
    }
}

#[tokio::test]
async fn system_exposes_the_calendar_timezone_without_requiring_today_generation() {
    #[derive(serde::Deserialize)]
    struct CalendarSettings {
        timezone: String,
        today_configured: bool,
    }
    for (today, expected) in [
        (None, "UTC"),
        (
            Some(
                closet::today::TodayConfig::new(
                    "http://127.0.0.1:1".into(),
                    None,
                    "Pacific/Kiritimati",
                    None,
                    None,
                )
                .unwrap(),
            ),
            "Pacific/Kiritimati",
        ),
    ] {
        let app = App::with_today(today).await;
        let calendar = app
            .client
            .get(format!("{}/v1/system", app.origin))
            .bearer_auth(OWNER)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<CalendarSettings>()
            .await
            .unwrap();
        assert_eq!(calendar.timezone, expected);
        assert!(!calendar.today_configured);
        app.close().await;
    }
}

#[test]
fn the_actual_catalog_parser_preserves_details_and_rejects_unsafe_shapes() {
    let input = catalog();
    let bytes = serde_json::to_vec(&input).unwrap();
    let parsed = parse_catalog(&bytes).unwrap();
    for item in &input {
        assert_eq!(
            parsed.items().iter().find(|other| other.id == item.id),
            Some(item)
        );
    }
    assert_eq!(parsed.report().count, 3);
    let mut reversed = input.clone();
    reversed.reverse();
    assert_eq!(
        parse_catalog(&serde_json::to_vec(&reversed).unwrap())
            .unwrap()
            .report()
            .source_hash,
        parsed.report().source_hash
    );
    let mut duplicate = input.clone();
    duplicate.push(input[0].clone());
    assert_eq!(
        parse_catalog(&serde_json::to_vec(&duplicate).unwrap())
            .unwrap_err()
            .errors[0]
            .code,
        "duplicate_id"
    );
    let mut missing = input.clone();
    missing[0].id = "..".into();
    assert_eq!(
        parse_catalog(&serde_json::to_vec(&missing).unwrap())
            .unwrap_err()
            .errors[0]
            .code,
        "invalid_id"
    );
    let mut invalid = input.clone();
    invalid[0].quantity = Some(0);
    assert_eq!(
        parse_catalog(&serde_json::to_vec(&invalid).unwrap())
            .unwrap_err()
            .errors[0]
            .code,
        "quantity_must_be_positive"
    );
    let unknown =
        String::from_utf8(bytes)
            .unwrap()
            .replacen("\"id\":", "\"unknown_field\":true,\"id\":", 1);
    let error = parse_catalog(unknown.as_bytes()).unwrap_err();
    assert_eq!(error.code, "invalid_catalog_json");
    assert!(!error.to_string().contains("unknown_field"));
}

#[tokio::test]
async fn removed_condition_rejects_the_whole_http_snapshot_without_reserving_its_key() {
    let app = App::new().await;
    let initial = sync(catalog(), false);
    let url = format!("{}/v1/inventory/sync", app.origin);
    let response = app
        .client
        .post(&url)
        .bearer_auth(OWNER)
        .header("if-match", "\"0\"")
        .header("idempotency-key", "initial")
        .json(&initial)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let report = response.json::<SyncReport>().await.unwrap();
    for (index, condition) in ["\"Lightly Worn\"", "null"].iter().enumerate() {
        let mut changed = catalog();
        changed[0].name = "A change that must not persist".into();
        let legacy = serde_json::to_string(&sync(changed, false))
            .unwrap()
            .replacen(
                "\"images\":",
                &format!("\"condition_at_purchase\":{condition},\"images\":"),
                1,
            );
        let key = format!("rejected-condition-{index}");
        let rejected = app
            .client
            .post(&url)
            .bearer_auth(OWNER)
            .header("if-match", "\"1\"")
            .header("idempotency-key", &key)
            .header("content-type", "application/json")
            .body(legacy)
            .send()
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        assert!(!rejected.text().await.unwrap().contains("Lightly Worn"));
        let item = app
            .client
            .get(format!("{}/v1/items/example-dress", app.origin))
            .bearer_auth(OWNER)
            .send()
            .await
            .unwrap()
            .json::<Item>()
            .await
            .unwrap();
        assert_eq!(item.catalog, catalog().remove(0));
        let retry = app
            .client
            .post(&url)
            .bearer_auth(OWNER)
            .header("if-match", "\"1\"")
            .header("idempotency-key", &key)
            .json(&initial)
            .send()
            .await
            .unwrap();
        assert_eq!(retry.status(), StatusCode::OK);
        let unchanged = retry.json::<SyncReport>().await.unwrap();
        assert_eq!(unchanged.catalog_revision, 1);
        assert_eq!(unchanged.source_hash, report.source_hash);
        assert_eq!(unchanged.unchanged, 3);
    }
    app.close().await;
}

#[test]
fn catalog_membership_needs_no_confirmation_field() {
    let catalog = parse_catalog(include_bytes!("fixtures/catalog-seasons.json")).unwrap();
    assert_eq!(catalog.report().count, 1);
}

#[tokio::test]
async fn required_quantities_reject_whole_snapshots_and_preserve_separate_purchases() {
    let app = App::new().await;
    let initial = catalog();
    app.store
        .sync(
            sync(initial.clone(), false),
            Some(0),
            "owner",
            Some("initial"),
        )
        .await
        .unwrap();
    let before = app.store.counts().await.unwrap();
    let endpoint = format!("{}/v1/inventory/sync", app.origin);
    for (index, quantity) in [None, None, Some(0)].into_iter().enumerate() {
        let mut changed = initial[..2].to_vec();
        changed[0].name = "An edit that must not persist".into();
        changed[1].quantity = quantity;
        let mut body = serde_json::to_string(&sync(changed, false)).unwrap();
        if index == 1 {
            body = body.replacen("\"quantity\":null,", "", 1);
        }
        let response = app
            .client
            .post(&endpoint)
            .bearer_auth(OWNER)
            .header("if-match", "\"1\"")
            .header("idempotency-key", format!("quantity-correction-{index}"))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.json::<HttpFailure>().await.unwrap().error_code,
            "invalid_catalog"
        );
        assert_eq!(app.store.counts().await.unwrap(), before);
        for item in &initial {
            assert_eq!(app.store.item(&item.id).await.unwrap().catalog, *item);
        }
    }
    let fitting = piece("example-fitting-purchase", "Crew tee", "clothing.top");
    let mut direct = fitting.clone();
    direct.id = "example-direct-purchase".into();
    direct.shop = None;
    direct.purchase_source = Some("self_bought".into());
    let purchases = vec![fitting, direct];
    let response = app
        .client
        .post(&endpoint)
        .bearer_auth(OWNER)
        .header("if-match", "\"1\"")
        .header("idempotency-key", "quantity-correction-0")
        .json(&sync(purchases.clone(), false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let report = response.json::<SyncReport>().await.unwrap();
    assert_eq!(report.created, 2);
    assert_eq!(report.archived, 3);
    for purchase in &purchases {
        let saved = app.store.item(&purchase.id).await.unwrap();
        assert_eq!(saved.catalog, *purchase);
        assert_eq!(saved.catalog.quantity, Some(1));
    }
    app.close().await;
}

#[derive(serde::Deserialize)]
struct RemovedItemField {
    current_ownership_confirmed: Option<bool>,
}
#[derive(serde::Deserialize)]
struct RemovedSystemField {
    confirmed_count: Option<i64>,
}
#[derive(serde::Serialize)]
struct RemovedTodayInput {
    ownership_scope: &'static str,
}

#[tokio::test]
async fn ownership_controls_are_absent_from_real_http_inventory_and_today() {
    let app = App::new().await;
    app.store
        .sync(sync(catalog(), false), Some(0), "owner", Some("initial"))
        .await
        .unwrap();
    let detail = app
        .client
        .get(format!("{}/v1/items/example-dress", app.origin))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<RemovedItemField>()
        .await
        .unwrap();
    assert!(detail.current_ownership_confirmed.is_none());
    let system = app
        .client
        .get(format!("{}/v1/system", app.origin))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<RemovedSystemField>()
        .await
        .unwrap();
    assert!(system.confirmed_count.is_none());
    for confirmed in ["true", "false"] {
        let response = app
            .client
            .get(format!("{}/v1/items", app.origin))
            .bearer_auth(OWNER)
            .query(&[("confirmed_only", confirmed)])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let response = app
        .client
        .post(format!("{}/v1/today/generate", app.origin))
        .bearer_auth(OWNER)
        .header("idempotency-key", "removed-input")
        .json(&RemovedTodayInput {
            ownership_scope: "confirmed_only",
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    app.close().await;
}

#[test]
fn required_source_validation_preserves_known_catalog_shape_and_rejects_missing_sources() {
    let mut known = catalog();
    known[0].purchase_source = None;
    known[1].shop = None;
    let bytes = serde_json::to_vec(&known).unwrap();
    let parsed = parse_catalog(&bytes).unwrap();
    for item in &known {
        assert_eq!(
            parsed.items().iter().find(|other| other.id == item.id),
            Some(item)
        );
    }

    let mut missing = known.clone();
    missing[0].shop = None;
    missing[2].shop = None;
    missing[2].purchase_source = None;
    let error = parse_catalog(&serde_json::to_vec(&missing).unwrap()).unwrap_err();
    assert_eq!(error.code, "invalid_catalog");
    assert_eq!(
        error.errors,
        [
            FieldError {
                index: Some(0),
                field: "purchase_source".into(),
                code: "source_required".into(),
            },
            FieldError {
                index: Some(2),
                field: "purchase_source".into(),
                code: "source_required".into(),
            },
        ]
    );
    // A malformed explicit source is still an error, never a default or guess.
    missing[0].purchase_source = Some(" ".into());
    let error = parse_catalog(&serde_json::to_vec(&missing).unwrap()).unwrap_err();
    assert!(error.errors.contains(&FieldError {
        index: Some(0),
        field: "purchase_source".into(),
        code: "invalid_text".into(),
    }));
}

#[derive(serde::Deserialize)]
struct HttpFailure {
    error_code: String,
}

#[tokio::test]
async fn required_source_http_rejection_is_atomic_and_does_not_reserve_idempotency_keys() {
    let app = App::new().await;
    let initial = catalog();
    app.store
        .sync(
            sync(initial.clone(), false),
            Some(0),
            "owner",
            Some("initial"),
        )
        .await
        .unwrap();
    let before_counts = app.store.counts().await.unwrap();
    let mut changed = initial[..2].to_vec();
    changed[0].name = "This edit must not survive rejection".into();
    changed[1].shop = None;
    changed[1].purchase_source = None;
    changed.push(piece("example-new", "New piece", "clothing.top"));
    let endpoint = format!("{}/v1/inventory/sync", app.origin);
    for dry_run in [true, false] {
        let response = app
            .client
            .post(&endpoint)
            .bearer_auth(OWNER)
            .header("if-match", "\"1\"")
            .header("idempotency-key", "source-correction")
            .json(&sync(changed.clone(), dry_run))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.json::<HttpFailure>().await.unwrap().error_code,
            "invalid_catalog"
        );
        assert_eq!(app.store.counts().await.unwrap(), before_counts);
        for item in &initial {
            let persisted = app.store.item(&item.id).await.unwrap();
            assert_eq!(persisted.catalog, *item);
            assert!(!persisted.archived);
            assert_eq!(persisted.revision, 1);
        }
        assert!(matches!(
            app.store.item("example-new").await,
            Err(StoreError::NotFound)
        ));
    }

    changed[1].purchase_source = Some("Example exchange".into());
    let response = app
        .client
        .post(&endpoint)
        .bearer_auth(OWNER)
        .header("if-match", "\"1\"")
        .header("idempotency-key", "source-correction")
        .json(&sync(changed, false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let report = response.json::<SyncReport>().await.unwrap();
    assert_eq!(
        (
            report.created,
            report.updated,
            report.archived,
            report.catalog_revision
        ),
        (1, 2, 1, 2)
    );
    app.close().await;
}

#[tokio::test]
async fn sync_is_atomic_revision_checked_and_replays_across_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("catalog.sqlite3");
    let store = Store::open(&path).await.unwrap();
    let input = catalog();
    let preview = store
        .sync(sync(input.clone(), true), None, "owner", None)
        .await
        .unwrap();
    assert_eq!((preview.created, preview.catalog_revision), (3, 0));
    assert_eq!(store.counts().await.unwrap(), (0, 0, 0));
    let first = store
        .sync(
            sync(input.clone(), false),
            Some(0),
            "owner",
            Some("initial"),
        )
        .await
        .unwrap();
    assert_eq!((first.created, first.catalog_revision), (3, 1));
    let replay = store
        .sync(
            sync(input.clone(), false),
            Some(0),
            "owner",
            Some("initial"),
        )
        .await
        .unwrap();
    assert_eq!(replay, first);
    let mut changed = input.clone();
    changed[0].name = "Different dress".into();
    assert!(matches!(
        store
            .sync(sync(changed, false), Some(1), "owner", Some("initial"))
            .await,
        Err(StoreError::Conflict("idempotency_conflict"))
    ));
    let mut invalid = input.clone();
    invalid[1].quantity = Some(0);
    invalid[0].name = "Must not be saved".into();
    assert!(
        store
            .sync(sync(invalid, false), Some(1), "owner", Some("invalid"))
            .await
            .is_err()
    );
    assert_eq!(
        store.item("example-dress").await.unwrap().catalog.name,
        "Jersey dress"
    );
    assert!(
        store
            .sync(sync(Vec::new(), false), Some(1), "owner", Some("empty"))
            .await
            .is_err()
    );
    let reduced = vec![input[0].clone(), input[1].clone()];
    let removed = store
        .sync(sync(reduced, false), Some(1), "owner", Some("remove"))
        .await
        .unwrap();
    assert_eq!((removed.archived, removed.catalog_revision), (1, 2));
    assert!(store.item("example-jumpsuit").await.unwrap().archived);
    let restored = store
        .sync(
            sync(input.clone(), false),
            Some(2),
            "owner",
            Some("restore"),
        )
        .await
        .unwrap();
    assert_eq!((restored.updated, restored.catalog_revision), (1, 3));
    let restored_item = store.item("example-jumpsuit").await.unwrap();
    assert!(!restored_item.archived);
    assert_eq!(restored_item.revision, 3);
    store.close().await;
    let reopened = Store::open(&path).await.unwrap();
    assert_eq!(
        reopened
            .sync(sync(input, false), Some(2), "owner", Some("restore"))
            .await
            .unwrap(),
        restored
    );
    assert_eq!(
        reopened
            .item("example-dress")
            .await
            .unwrap()
            .catalog
            .price
            .as_ref()
            .unwrap()
            .amount,
        25.5
    );
    reopened.close().await;
}

#[tokio::test]
async fn image_crops_survive_http_sync_read_replay_and_atomic_rejection() {
    let app = App::new().await;
    let mut input = catalog();
    let crop = ImageCrop {
        x: 2500,
        y: 1250,
        width: 5000,
        height: 7500,
    };
    input[0].images[0].crop = Some(crop);
    let url = format!("{}/v1/inventory/sync", app.origin);
    let request = sync(input.clone(), false);
    let response = app
        .client
        .post(&url)
        .bearer_auth(OWNER)
        .header("if-match", "\"0\"")
        .header("idempotency-key", "example-crop")
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let applied = response.json::<SyncReport>().await.unwrap();
    assert_eq!(applied.catalog_revision, 1);
    let response = app
        .client
        .get(format!("{}/v1/items/example-dress", app.origin))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let item = response.json::<Item>().await.unwrap();
    assert_eq!(item.catalog, input[0]);
    assert_eq!(item.catalog.images[0].crop, Some(crop));
    let response = app
        .client
        .post(&url)
        .bearer_auth(OWNER)
        .header("if-match", "\"0\"")
        .header("idempotency-key", "example-crop")
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.json::<SyncReport>().await.unwrap(), applied);

    let mut changed_crop = input.clone();
    changed_crop[0].images[0].crop.as_mut().unwrap().x += 1;
    let response = app
        .client
        .post(&url)
        .bearer_auth(OWNER)
        .header("if-match", "\"1\"")
        .header("idempotency-key", "example-crop")
        .json(&sync(changed_crop, false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);

    let mut invalid = input.clone();
    invalid[0].name = "Must not be saved".into();
    invalid[1].images[0].crop = Some(ImageCrop {
        x: 9999,
        y: 0,
        width: 2,
        height: 1,
    });
    let response = app
        .client
        .post(&url)
        .bearer_auth(OWNER)
        .header("if-match", "\"1\"")
        .header("idempotency-key", "example-invalid-crop")
        .json(&sync(invalid, false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        app.store.item("example-dress").await.unwrap().catalog,
        input[0]
    );
    assert_eq!(app.store.counts().await.unwrap(), (1, 3, 0));
    app.close().await;
}

#[tokio::test]
async fn actual_http_enforces_auth_preconditions_adapter_scope_and_etags() {
    let app = App::new().await;
    let url = format!("{}/v1/inventory/sync", app.origin);
    let input = sync(catalog(), false);
    assert_eq!(
        app.client
            .post(&url)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.client
            .post(&url)
            .bearer_auth(INTEGRATION)
            .header("x-closet-admin", "true")
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.client
            .post(&url)
            .bearer_auth(OWNER)
            .header("idempotency-key", "missing-etag")
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::PRECONDITION_REQUIRED
    );
    let response = app
        .client
        .post(&url)
        .bearer_auth(INTEGRATION)
        .header("x-closet-actor-id", "ha:example-user")
        .header("x-closet-admin", "true")
        .header("if-match", "\"0\"")
        .header("idempotency-key", "adapter-sync")
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["etag"], "\"1\"");
    assert_eq!(response.json::<SyncReport>().await.unwrap().created, 3);
    let response = app
        .client
        .get(format!("{}/v1/items/example-dress", app.origin))
        .bearer_auth(INTEGRATION)
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["etag"], "\"1\"");
    assert_eq!(
        response.json::<Item>().await.unwrap().catalog.name,
        "Jersey dress"
    );
    let response = app
        .client
        .post(&url)
        .bearer_auth(OWNER)
        .header("if-match", "\"0\"")
        .header("idempotency-key", "stale")
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    app.close().await;
}

#[tokio::test]
async fn pagination_filters_and_concurrent_writes_have_real_boundaries() {
    let app = App::new().await;
    let mut input = catalog();
    input[0].sku = Some("ÉTÉ-EXAMPLE".into());
    input.push(piece("example-top", "Top with 100% cotton", "clothing.top"));
    input.push(piece(
        "example-topper",
        "Not a descendant",
        "clothing.topper",
    ));
    app.store
        .sync(sync(input.clone(), false), Some(0), "owner", Some("start"))
        .await
        .unwrap();
    let first = app
        .store
        .list(ListQuery {
            limit: Some(1),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!((first.total, first.scope_total), (5, 5));
    assert!(first.next_page.is_some());
    let mut seen = BTreeSet::new();
    seen.insert(first.items[0].catalog.id.clone());
    let mut next = first.next_page;
    while let Some(page_token) = next {
        let page = app
            .store
            .list(ListQuery {
                limit: Some(1),
                page_token: Some(page_token),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(seen.insert(page.items[0].catalog.id.clone()));
        next = page.next_page;
    }
    assert_eq!(seen.len(), 5);
    let tops = app
        .store
        .list(ListQuery {
            category: Some("clothing.top".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(tops.total, 1);
    let percent = app
        .store
        .list(ListQuery {
            q: Some("%".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(percent.total, 1);
    let unicode_sku = app
        .store
        .list(ListQuery {
            q: Some("été-example".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(unicode_sku.total, 1);
    let old = app
        .store
        .list(ListQuery {
            limit: Some(1),
            ..Default::default()
        })
        .await
        .unwrap()
        .next_page
        .unwrap();
    let mut left = input.clone();
    left[0].name = "First competing edit".into();
    let mut right = input.clone();
    right[0].name = "Second competing edit".into();
    let url = format!("{}/v1/inventory/sync", app.origin);
    let left = app
        .client
        .post(&url)
        .bearer_auth(OWNER)
        .header("if-match", "\"1\"")
        .header("idempotency-key", "left")
        .json(&sync(left, false))
        .send();
    let right = app
        .client
        .post(&url)
        .bearer_auth(OWNER)
        .header("if-match", "\"1\"")
        .header("idempotency-key", "right")
        .json(&sync(right, false))
        .send();
    let (left, right) = tokio::join!(left, right);
    let statuses = BTreeSet::from([
        left.unwrap().status().as_u16(),
        right.unwrap().status().as_u16(),
    ]);
    assert_eq!(statuses, BTreeSet::from([200, 412]));
    assert!(matches!(
        app.store
            .list(ListQuery {
                page_token: Some(old),
                ..Default::default()
            })
            .await,
        Err(StoreError::Conflict("page_invalidated"))
    ));
    app.close().await;
}

#[tokio::test]
async fn durable_generation_reservation_survives_restart_and_does_not_retry() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("catalog.sqlite3");
    let store = Store::open(&path).await.unwrap();
    let basis = GenerationBasis {
        date: "2026-01-02".into(),
        timezone: "UTC".into(),
        config_hash: "test-config".into(),
        items: catalog(),
    };
    let first = store
        .reserve_generation(TodayRequest::default(), basis.clone(), "owner", "attempt")
        .await
        .unwrap();
    assert!(first.fresh);
    let replay = store
        .reserve_generation(TodayRequest::default(), basis.clone(), "owner", "attempt")
        .await
        .unwrap();
    assert!(!replay.fresh);
    assert_eq!(replay.generation.id, first.generation.id);
    assert!(matches!(
        store
            .reserve_generation(TodayRequest::default(), basis.clone(), "owner", "different")
            .await,
        Err(StoreError::Conflict("generation_in_progress"))
    ));
    store.close().await;
    let store = Store::open(&path).await.unwrap();
    let replay = store
        .reserve_generation(TodayRequest::default(), basis, "owner", "attempt")
        .await
        .unwrap();
    assert!(!replay.fresh);
    assert_eq!(replay.generation.status, GenerationStatus::Interrupted);
    store.close().await;
}

#[tokio::test]
async fn today_get_never_generates_and_unconfigured_attempt_is_honest() {
    let app = App::new().await;
    let get = app
        .client
        .get(format!("{}/v1/today", app.origin))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .json::<TodayResponse>()
        .await
        .unwrap();
    assert!(get.result.is_none() && get.generation.is_none());
    let response = app
        .client
        .post(format!("{}/v1/today/generate", app.origin))
        .bearer_auth(INTEGRATION)
        .header("idempotency-key", "not-admin")
        .json(&TodayRequest::default())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = app
        .client
        .post(format!("{}/v1/today/generate", app.origin))
        .bearer_auth(OWNER)
        .header("idempotency-key", "unconfigured")
        .json(&TodayRequest::default())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let generation = response.json::<Generation>().await.unwrap();
    let done = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let result = app
                .client
                .get(format!("{}/v1/today", app.origin))
                .bearer_auth(OWNER)
                .send()
                .await
                .unwrap()
                .json::<TodayResponse>()
                .await
                .unwrap();
            if result
                .generation
                .as_ref()
                .is_some_and(|run| run.status != GenerationStatus::Running)
            {
                break result;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(done.generation.as_ref().unwrap().id, generation.id);
    assert_eq!(
        done.generation.as_ref().unwrap().error_code.as_deref(),
        Some("today_unconfigured")
    );
    assert!(done.result.is_none());
    let changed = TodayRequest {
        occasion: Occasion::Dinner,
        ..Default::default()
    };
    let response = app
        .client
        .post(format!("{}/v1/today/generate", app.origin))
        .bearer_auth(OWNER)
        .header("idempotency-key", "unconfigured")
        .json(&changed)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    app.close().await;
}

#[tokio::test]
async fn ingress_peer_and_request_marker_are_not_native_authentication() {
    let app = App::new().await;
    let response = app
        .client
        .get(format!("{}/v1/items", app.origin))
        .header("x-remote-user-id", "example")
        .header("x-closet-request", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let ingress = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(
            app.context
                .as_ref()
                .clone()
                .for_ingress("127.0.0.1".parse().unwrap()),
        ),
    )
    .unwrap();
    let url = format!("http://{}/v1/inventory/sync", ingress.local_addr());
    let response = app
        .client
        .post(&url)
        .header("x-remote-user-id", "example")
        .json(&sync(catalog(), true))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = app
        .client
        .post(&url)
        .header("x-remote-user-id", "example")
        .header("x-closet-request", "1")
        .json(&sync(catalog(), true))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    ingress.close().await.unwrap();
    app.close().await;
}

#[tokio::test]
async fn today_keeps_the_last_result_and_rechecks_only_referenced_facts() {
    use chrono::{Duration, Utc};
    use closet::models::{Freshness, Suggestion, TodayResult, WeatherMode};

    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("catalog.sqlite3"))
        .await
        .unwrap();
    let pieces = vec![catalog()[0].clone(), catalog()[1].clone()];
    store
        .sync(
            sync(pieces.clone(), false),
            Some(0),
            "owner",
            Some("catalog"),
        )
        .await
        .unwrap();
    let now = Utc::now();
    let date = now.date_naive().to_string();
    let inputs = TodayRequest {
        weather_mode: WeatherMode::Ignored,
        footwear_requirement: None,
        ..Default::default()
    };
    let basis = GenerationBasis {
        date: date.clone(),
        timezone: "UTC".into(),
        config_hash: "example-config".into(),
        items: pieces.clone(),
    };
    let first = store
        .reserve_generation(inputs.clone(), basis.clone(), "owner", "first")
        .await
        .unwrap();
    let result = TodayResult {
        generation_id: first.generation.id.clone(),
        inputs: inputs.clone(),
        generated_at: now,
        expires_at: now + Duration::hours(1),
        freshness: Freshness::Ready,
        stale_reasons: Vec::new(),
        weather: None,
        suggestions: vec![Suggestion {
            occasion: None,
            label: "An example outfit".into(),
            item_ids: vec!["example-dress".into(), "example-shoes".into()],
            advisory_reason: "An optional combination to consider.".into(),
            warnings: Vec::new(),
        }],
    };
    store
        .finish_generation(&first.generation.id, Ok(result.clone()))
        .await
        .unwrap();
    let failed = store
        .reserve_generation(inputs.clone(), basis.clone(), "owner", "failed-refresh")
        .await
        .unwrap();
    store
        .finish_generation(&failed.generation.id, Err("model_unavailable"))
        .await
        .unwrap();
    let current = store
        .today(&date, "UTC", "example-config", now, None)
        .await
        .unwrap();
    assert_eq!(current.result.unwrap().generation_id, first.generation.id);
    assert_eq!(current.generation.unwrap().status, GenerationStatus::Failed);
    let mut expanded = pieces.clone();
    expanded.push(catalog()[2].clone());
    store
        .sync(
            sync(expanded.clone(), false),
            Some(1),
            "owner",
            Some("unrelated-addition"),
        )
        .await
        .unwrap();
    assert!(matches!(
        store
            .today(&date, "UTC", "example-config", now, None)
            .await
            .unwrap()
            .result
            .unwrap()
            .freshness,
        Freshness::Ready
    ));
    let pending = store
        .reserve_generation(inputs, basis, "owner", "changed-while-running")
        .await
        .unwrap();
    expanded[0].color = Some("Grey".into());
    store
        .sync(
            sync(expanded, false),
            Some(2),
            "owner",
            Some("referenced-change"),
        )
        .await
        .unwrap();
    let mut result = result;
    result.generation_id = pending.generation.id.clone();
    store
        .finish_generation(&pending.generation.id, Ok(result))
        .await
        .unwrap();
    let current = store
        .today(&date, "UTC", "example-config", now, None)
        .await
        .unwrap();
    assert_eq!(
        current.generation.unwrap().error_code.as_deref(),
        Some("referenced_items_changed")
    );
    let previous = current.result.unwrap();
    assert_eq!(previous.generation_id, first.generation.id);
    assert!(matches!(previous.freshness, Freshness::Stale));
    assert!(
        previous
            .stale_reasons
            .contains(&"referenced_items_changed".into())
    );
    assert!(
        store
            .today("2099-01-01", "UTC", "example-config", now, None)
            .await
            .unwrap()
            .result
            .is_none()
    );
    store.close().await;
}

#[tokio::test]
async fn bedtime_is_a_typed_http_occasion_with_bound_idempotency() {
    #[derive(serde::Serialize)]
    struct WireRequest {
        occasion: &'static str,
        weather_mode: &'static str,
    }
    let app = App::new().await;
    let response = app
        .client
        .post(format!("{}/v1/today/generate", app.origin))
        .bearer_auth(OWNER)
        .header("idempotency-key", "example-bedtime")
        .json(&WireRequest {
            occasion: "bedtime",
            weather_mode: "ignored",
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let generation = response.json::<Generation>().await.unwrap();
    assert_eq!(generation.inputs.occasion.to_string(), "bedtime");
    let saved = app
        .client
        .get(format!("{}/v1/today", app.origin))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .json::<TodayResponse>()
        .await
        .unwrap();
    assert_eq!(saved.generation.as_ref().unwrap().id, generation.id);
    assert_eq!(
        saved.generation.unwrap().inputs.occasion.to_string(),
        "bedtime"
    );
    let replay = app
        .client
        .post(format!("{}/v1/today/generate", app.origin))
        .bearer_auth(OWNER)
        .header("idempotency-key", "example-bedtime")
        .json(&WireRequest {
            occasion: "bedtime",
            weather_mode: "ignored",
        })
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::ACCEPTED);
    assert_eq!(replay.json::<Generation>().await.unwrap().id, generation.id);
    let changed = app
        .client
        .post(format!("{}/v1/today/generate", app.origin))
        .bearer_auth(OWNER)
        .header("idempotency-key", "example-bedtime")
        .json(&WireRequest {
            occasion: "everyday",
            weather_mode: "ignored",
        })
        .send()
        .await
        .unwrap();
    assert_eq!(changed.status(), StatusCode::CONFLICT);
    app.close().await;
}
