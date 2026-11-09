//! Safety use survives the real wire/storage path and closes every Today gate.
use std::{fs, sync::Arc};

use chrono::{Duration, Utc};
use closet::{
    api,
    models::{
        CatalogItem, Freshness, GenerationStatus, Item, ItemsPage, SafetyRequirement, Suggestion,
        SyncReport, SyncRequest, TodayRequest, TodayResult, Usage, parse_catalog,
    },
    store::{GenerationBasis, Store, styling_hash},
};
use reqwest::{Client, StatusCode};

const OWNER: &str = "fictional-safety-use-test-credential";

fn piece(id: &str, category: &str) -> CatalogItem {
    let mut item = parse_catalog(include_bytes!("fixtures/catalog-crop.json"))
        .unwrap()
        .into_items()
        .remove(0);
    item.id = id.into();
    item.category = category.into();
    item
}
fn restricted() -> Usage {
    Usage::SafetyWorkOnly {
        requirement: SafetyRequirement::SteelToe,
    }
}
fn sync(items: Vec<CatalogItem>) -> SyncRequest {
    SyncRequest {
        items,
        dry_run: false,
        allow_empty: false,
    }
}
fn outfit(id: &str, inputs: TodayRequest, now: chrono::DateTime<Utc>) -> TodayResult {
    TodayResult {
        generation_id: id.into(),
        inputs,
        generated_at: now,
        expires_at: now + Duration::hours(1),
        freshness: Freshness::Ready,
        stale_reasons: Vec::new(),
        weather: None,
        suggestions: vec![Suggestion {
            occasion: None,
            label: "Example outfit".into(),
            item_ids: vec!["example-dress".into(), "example-boots".into()],
            advisory_reason: "Fictional pieces".into(),
            warnings: Vec::new(),
        }],
    }
}

#[test]
fn old_json_and_hashes_stay_identical_and_invalid_usage_is_rejected() {
    let item = piece("example-boots", "footwear.boots");
    assert_eq!(item.usage, Usage::Everyday);
    let legacy = serde_json::to_string(&vec![item.clone()]).unwrap();
    assert!(!legacy.contains("usage"));
    let explicit = legacy.replacen("\"id\":", "\"usage\":{\"kind\":\"everyday\"},\"id\":", 1);
    let everyday = parse_catalog(explicit.as_bytes())
        .unwrap()
        .into_items()
        .remove(0);
    assert_eq!(
        serde_json::to_string(&vec![everyday.clone()]).unwrap(),
        legacy
    );
    assert_eq!(
        styling_hash(&item).unwrap(),
        styling_hash(&everyday).unwrap()
    );
    let mut safety = item;
    safety.usage = restricted();
    assert_ne!(
        styling_hash(&safety).unwrap(),
        styling_hash(&everyday).unwrap()
    );
    for usage in [
        r#"{"kind":"safety_work_only"}"#,
        r#"{"kind":"safety_work_only","requirement":"office"}"#,
        r#"{"kind":"safety_work_only","requirement":"steel_toe","certified":true}"#,
        r#"{"kind":"unknown"}"#,
        "null",
    ] {
        let wire = legacy.replacen("\"id\":", &format!("\"usage\":{usage},\"id\":"), 1);
        assert!(parse_catalog(wire.as_bytes()).is_err(), "accepted {usage}");
    }
}

#[tokio::test]
async fn http_sqlite_revisions_retry_backup_and_cached_outfits_respect_safety_use() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("closet.sqlite3");
    let store = Store::open(&path).await.unwrap();
    let server = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(api::Context::new(store.clone(), OWNER)),
    )
    .unwrap();
    let origin = format!("http://{}", server.local_addr());
    let client = Client::new();
    let mut catalog = vec![
        piece("example-dress", "clothing.dress"),
        piece("example-boots", "footwear.boots"),
    ];
    let initial = client
        .post(format!("{origin}/v1/inventory/sync"))
        .bearer_auth(OWNER)
        .header("If-Match", "\"0\"")
        .header("Idempotency-Key", "initial")
        .json(&sync(catalog.clone()))
        .send()
        .await
        .unwrap();
    assert_eq!(initial.status(), StatusCode::OK);
    let initial: SyncReport = initial.json().await.unwrap();
    let now = Utc::now();
    let date = now.date_naive().to_string();
    let basis = || GenerationBasis {
        date: date.clone(),
        timezone: "UTC".into(),
        config_hash: "example".into(),
        items: catalog.clone(),
    };
    let first = store
        .reserve_generation(TodayRequest::default(), basis(), "owner", "first")
        .await
        .unwrap()
        .generation;
    store
        .finish_generation(&first.id, Ok(outfit(&first.id, first.inputs, now)))
        .await
        .unwrap();
    assert!(
        store
            .today(&date, "UTC", "example", now, None)
            .await
            .unwrap()
            .result
            .is_some()
    );
    let racing = store
        .reserve_generation(TodayRequest::default(), basis(), "owner", "racing")
        .await
        .unwrap()
        .generation;
    catalog[1].usage = restricted();
    let apply = || {
        client
            .post(format!("{origin}/v1/inventory/sync"))
            .bearer_auth(OWNER)
            .header("If-Match", "\"1\"")
            .header("Idempotency-Key", "safety")
            .json(&sync(catalog.clone()))
    };
    let response = apply().send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let report: SyncReport = response.json().await.unwrap();
    assert_eq!((report.updated, report.catalog_revision), (1, 2));
    assert_ne!(report.source_hash, initial.source_hash);
    assert_eq!(
        apply()
            .send()
            .await
            .unwrap()
            .json::<SyncReport>()
            .await
            .unwrap(),
        report
    );
    let page = client
        .get(format!("{origin}/v1/items"))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .json::<ItemsPage>()
        .await
        .unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(
        page.items
            .iter()
            .find(|item| item.catalog.id == "example-boots")
            .unwrap()
            .catalog
            .usage,
        restricted()
    );
    let boot = client
        .get(format!("{origin}/v1/items/example-boots"))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .json::<Item>()
        .await
        .unwrap();
    assert_eq!(boot.revision, 2);
    assert_eq!(boot.catalog.usage, restricted());
    assert!(
        store
            .today(&date, "UTC", "example", now, None)
            .await
            .unwrap()
            .result
            .is_none()
    );
    store
        .finish_generation(&racing.id, Ok(outfit(&racing.id, racing.inputs, now)))
        .await
        .unwrap();
    let response = store
        .today(&date, "UTC", "example", now, None)
        .await
        .unwrap();
    assert_eq!(
        response.generation.unwrap().status,
        GenerationStatus::Failed
    );
    let direct = store
        .reserve_generation(
            TodayRequest::default(),
            GenerationBasis {
                date: date.clone(),
                timezone: "UTC".into(),
                config_hash: "example".into(),
                items: catalog.clone(),
            },
            "owner",
            "direct",
        )
        .await
        .unwrap()
        .generation;
    store
        .finish_generation(&direct.id, Ok(outfit(&direct.id, direct.inputs, now)))
        .await
        .unwrap();
    let attempt = store
        .today(&date, "UTC", "example", now, None)
        .await
        .unwrap()
        .generation
        .unwrap();
    assert_eq!(attempt.status, GenerationStatus::Failed);
    assert_eq!(attempt.error_code.as_deref(), Some("invalid_model_output"));
    let required = TodayRequest {
        occasion: closet::models::Occasion::Work,
        weather_mode: closet::models::WeatherMode::Ignored,
        footwear_requirement: Some(SafetyRequirement::SteelToe),
    };
    let required_basis = GenerationBasis {
        date: date.clone(),
        timezone: "UTC".into(),
        config_hash: "example".into(),
        items: catalog.clone(),
    };
    let safety = store
        .reserve_generation(
            required.clone(),
            required_basis.clone(),
            "owner",
            "required",
        )
        .await
        .unwrap()
        .generation;
    store
        .finish_generation(&safety.id, Ok(outfit(&safety.id, required.clone(), now)))
        .await
        .unwrap();
    assert!(
        store
            .today(
                &date,
                "UTC",
                "example",
                now,
                Some(SafetyRequirement::SteelToe)
            )
            .await
            .unwrap()
            .result
            .is_some()
    );
    let normal = store
        .today(&date, "UTC", "example", now, None)
        .await
        .unwrap();
    assert!(normal.result.is_none());
    assert!(normal.generation.is_none());
    let conflict = store
        .reserve_generation(TodayRequest::default(), required_basis, "owner", "required")
        .await
        .err()
        .unwrap();
    assert_eq!(conflict.code(), "idempotency_conflict");
    server.close().await.unwrap();
    store.close().await;
    // Home Assistant uses a cold app backup; copy only after the service closes.
    let backup = temp.path().join("restored.sqlite3");
    fs::copy(&path, &backup).unwrap();
    let restored = Store::open(&backup).await.unwrap();
    assert_eq!(
        restored.item("example-boots").await.unwrap().catalog.usage,
        restricted()
    );
    assert!(
        restored
            .today(&date, "UTC", "example", now, None)
            .await
            .unwrap()
            .result
            .is_none()
    );
    restored.close().await;
}
