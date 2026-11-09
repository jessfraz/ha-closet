//! Groups remain complete even when a flat page or filter selects one purchase.
use chrono::Utc;
use closet::{
    api,
    models::{
        CatalogIdentity, CatalogItem, ItemsPage, ListQuery, Shop, Source, SyncRequest,
        parse_catalog,
    },
    store::Store,
};
use reqwest::Client;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::sync::Arc;

fn item(id: &str, variant: Option<&str>) -> CatalogItem {
    let mut item = parse_catalog(include_bytes!("fixtures/catalog-crop.json"))
        .unwrap()
        .into_items()
        .remove(0);
    item.id = id.into();
    item.name = "Same visible name".into();
    item.identity = variant.map(|variant| CatalogIdentity {
        style_id: "example-style".into(),
        variant_id: variant.into(),
    });
    item.purchase_source = Some("self_bought".into());
    item.shop = None;
    item
}
fn sync(items: Vec<CatalogItem>) -> SyncRequest {
    SyncRequest {
        items,
        dry_run: false,
        allow_empty: false,
    }
}

#[tokio::test]
async fn grouped_summaries_span_pages_and_filters_without_merging_purchase_or_unit_ids() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("groups.sqlite3"))
        .await
        .unwrap();
    let mut a = item("example-a", Some("shared-variant"));
    a.size = Some("XS".into());
    a.quantity = Some(2);
    let mut b = item("example-b", Some("shared-variant"));
    b.size = Some("S".into());
    b.quantity = Some(1);
    b.shop = Some(Shop {
        id: "example-shop".into(),
        label: "Example shop".into(),
        season: None,
        years: Vec::new(),
    });
    let c = item("example-c", Some("distinct-variant"));
    let legacy = item("example-legacy", None);
    let mut catalog = vec![a, b, c, legacy];
    store
        .sync(sync(catalog.clone()), Some(0), "owner", Some("initial"))
        .await
        .unwrap();
    let units = store.tracking_snapshot(Utc::now()).await.unwrap().units;
    let server = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(api::Context::new(
            store.clone(),
            "fictional-group-owner-credential",
        )),
    )
    .unwrap();
    let origin = format!("http://{}", server.local_addr());
    let client = Client::new();
    let first = client
        .get(format!("{origin}/v1/items?limit=1"))
        .bearer_auth("fictional-group-owner-credential")
        .send()
        .await
        .unwrap()
        .json::<ItemsPage>()
        .await
        .unwrap();
    assert_eq!(first.items.len(), 1);
    assert_eq!(first.variant_groups.len(), 1);
    let group = &first.variant_groups[0];
    assert_eq!(group.quantity, Some(3));
    assert_eq!(
        group
            .copies
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        ["example-a", "example-b"]
    );
    assert_eq!(
        group
            .copies
            .iter()
            .map(|item| item.size.as_deref())
            .collect::<Vec<_>>(),
        [Some("XS"), Some("S")]
    );
    let second = store
        .list(ListQuery {
            page_token: first.next_page.clone(),
            limit: Some(1),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.variant_groups[0].copies.len(), 2);
    let filtered = store
        .list(ListQuery {
            source: Some(Source::Shop("example-shop".into())),
            limit: Some(1),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(filtered.total, 1);
    assert_eq!(filtered.variant_groups[0].quantity, Some(3));
    assert_eq!(filtered.variant_groups[0].copies.len(), 2);
    let all = store.list(ListQuery::default()).await.unwrap();
    assert_eq!(all.items.len(), 4);
    assert_eq!(all.variant_groups.len(), 2); // Identical names/images never group distinct or legacy variants.
    catalog[0].size = Some("XXS".into());
    store
        .sync(sync(catalog.clone()), Some(1), "owner", Some("metadata"))
        .await
        .unwrap();
    assert_eq!(
        store
            .list(ListQuery {
                page_token: first.next_page,
                ..Default::default()
            })
            .await
            .unwrap_err()
            .code(),
        "page_invalidated"
    );
    let after = store.tracking_snapshot(Utc::now()).await.unwrap().units;
    assert_eq!(
        units.iter().map(|unit| &unit.id).collect::<Vec<_>>(),
        after.iter().map(|unit| &unit.id).collect::<Vec<_>>()
    );
    catalog.remove(1);
    store
        .sync(sync(catalog), Some(2), "owner", Some("archive"))
        .await
        .unwrap();
    let page = store
        .list(ListQuery {
            limit: Some(1),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.variant_groups[0].quantity, Some(2));
    assert_eq!(page.variant_groups[0].copies.len(), 1);
    assert_eq!(
        page.variant_groups[0].copies[0].size.as_deref(),
        Some("XXS")
    );
    let expected_counts = store.counts().await.unwrap();
    let expected_units = store.tracking_snapshot(Utc::now()).await.unwrap().units;
    server.close().await.unwrap();
    store.close().await;
    // Version 6 differs only by this index: restore its exact schema, then
    // exercise the real startup migrator without changing catalog/runtime data.
    let path = temp.path().join("groups.sqlite3");
    let mut old = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::query("DROP INDEX items_active_variant")
        .execute(&mut old)
        .await
        .unwrap();
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version=7")
        .execute(&mut old)
        .await
        .unwrap();
    old.close().await.unwrap();
    let upgraded = Store::open(&path).await.unwrap();
    assert_eq!(upgraded.counts().await.unwrap(), expected_counts);
    let actual_units = upgraded.tracking_snapshot(Utc::now()).await.unwrap().units;
    assert_eq!(
        actual_units.iter().map(|unit| &unit.id).collect::<Vec<_>>(),
        expected_units
            .iter()
            .map(|unit| &unit.id)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        upgraded
            .list(ListQuery {
                limit: Some(1),
                ..Default::default()
            })
            .await
            .unwrap()
            .variant_groups[0]
            .quantity,
        Some(2)
    );
    upgraded.close().await;
}

#[tokio::test]
async fn http_rejects_conflicting_shared_variants_and_keeps_purchase_overrides() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("variant-validation.sqlite3"))
        .await
        .unwrap();
    let server = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(api::Context::new(
            store.clone(),
            "fictional-group-validation-credential",
        )),
    )
    .unwrap();
    let endpoint = format!("http://{}/v1/inventory/sync", server.local_addr());
    let client = Client::new();
    let mut a = item("example-shared-a", Some("example-shared-variant"));
    a.color = Some("Blue".into());
    a.material = Some("Cotton".into());
    a.price = Some(closet::models::Price {
        amount: 42.5,
        currency: "USD".into(),
        basis: "receipt".into(),
        quantity_basis: "unit".into(),
    });
    let mut b = a.clone();
    b.id = "example-shared-b".into();
    b.name = "An allowed purchase-specific name".into();
    b.size = Some("L".into());
    b.quantity = Some(2);
    b.purchase_source = Some("Example gift".into());
    b.price = None;
    b.images.clear();
    b.usage = closet::models::Usage::SafetyWorkOnly {
        requirement: closet::models::SafetyRequirement::SteelToe,
    };
    let valid = vec![a, b];
    let initial = client
        .post(&endpoint)
        .bearer_auth("fictional-group-validation-credential")
        .header("If-Match", "\"0\"")
        .header("Idempotency-Key", "valid-overrides")
        .json(&sync(valid.clone()))
        .send()
        .await
        .unwrap();
    assert_eq!(initial.status(), reqwest::StatusCode::OK);
    for field in ["style", "brand", "category", "color", "material"] {
        let mut invalid = valid.clone();
        match field {
            "style" => invalid[1].identity.as_mut().unwrap().style_id = "different-style".into(),
            "brand" => invalid[1].brand = Some("Different brand".into()),
            "category" => invalid[1].category = "footwear.boots".into(),
            "color" => invalid[1].color = Some("Red".into()),
            "material" => invalid[1].material = Some("Linen".into()),
            _ => unreachable!(),
        }
        let response = client
            .post(&endpoint)
            .bearer_auth("fictional-group-validation-credential")
            .header("If-Match", "\"1\"")
            .header("Idempotency-Key", field)
            .json(&sync(invalid))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::BAD_REQUEST,
            "{field}"
        );
        assert_eq!(store.counts().await.unwrap(), (1, 2, 0));
        // Reusing the rejected key with valid input proves no write or retry was reserved.
        let retry = client
            .post(&endpoint)
            .bearer_auth("fictional-group-validation-credential")
            .header("If-Match", "\"1\"")
            .header("Idempotency-Key", field)
            .json(&sync(valid.clone()))
            .send()
            .await
            .unwrap();
        assert_eq!(retry.status(), reqwest::StatusCode::OK);
    }
    let retained = store.item("example-shared-b").await.unwrap().catalog;
    assert_eq!(retained.name, "An allowed purchase-specific name");
    assert_eq!(retained.quantity, Some(2));
    assert_eq!(retained.price, None);
    assert!(retained.images.is_empty());
    assert!(!retained.usage.is_everyday());
    server.close().await.unwrap();
    store.close().await;
}
