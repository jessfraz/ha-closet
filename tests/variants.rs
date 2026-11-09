//! Complete grouped detail keeps the original acquisitions and catalog snapshot.
use std::sync::Arc;

use closet::{
    api,
    models::{
        CatalogIdentity, CatalogItem, Item, ItemsPage, Price, Shop, SyncRequest, VariantItems,
        parse_catalog,
    },
    store::Store,
};
use reqwest::{Client, StatusCode};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};

const OWNER: &str = "fictional-variant-owner-credential";
const INTEGRATION: &str = "fictional-variant-integration-credential";

fn purchase(index: usize) -> CatalogItem {
    let mut item = parse_catalog(include_bytes!("fixtures/catalog-crop.json"))
        .unwrap()
        .into_items()
        .remove(0);
    item.id = format!("example-purchase-{index:03}");
    item.name = format!("Example acquisition {:03}", 102 - index);
    item.identity = Some(CatalogIdentity {
        style_id: "example-style".into(),
        variant_id: "example-variant".into(),
    });
    item.size = Some(if index.is_multiple_of(2) { "M" } else { "S" }.into());
    item.quantity = Some(if index == 0 { 3 } else { 1 });
    item.price = Some(Price {
        amount: 40.0 + index as f64,
        currency: "USD".into(),
        basis: "receipt".into(),
        quantity_basis: "per_item".into(),
    });
    item.shop = Some(Shop {
        id: format!("example-shop-{index}"),
        label: format!("Example shop {index}"),
        season: None,
        years: vec![],
    });
    item.notes = vec![format!("Fictional acquisition note {index}")];
    item
}

fn snapshot(items: Vec<CatalogItem>) -> SyncRequest {
    SyncRequest {
        items,
        dry_run: false,
        allow_empty: false,
    }
}

#[tokio::test]
async fn complete_variant_http_detail_preserves_acquisitions_and_archival_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("closet.sqlite3"))
        .await
        .unwrap();
    let server = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(api::Context::new(store.clone(), OWNER).with_integration(INTEGRATION)),
    )
    .unwrap();
    let origin = format!("http://{}", server.local_addr());
    let url = format!("{origin}/v1/variants/example-variant/items");
    let client = Client::new();
    let mut expected: Vec<_> = (0..103).map(purchase).collect();
    let mut legacy = purchase(0);
    legacy.id = "example-legacy".into();
    legacy.identity = None;
    legacy.shop = None;
    let mut catalog = expected.clone();
    catalog.push(legacy.clone());
    let applied = client
        .post(format!("{origin}/v1/inventory/sync"))
        .bearer_auth(OWNER)
        .header("If-Match", "\"0\"")
        .header("Idempotency-Key", "variant-initial")
        .json(&snapshot(catalog))
        .send()
        .await
        .unwrap();
    assert_eq!(applied.status(), StatusCode::OK);
    assert_eq!(
        client.get(&url).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let response = client
        .get(&url)
        .bearer_auth(INTEGRATION)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["etag"], "\"1\"");
    assert_eq!(response.headers()["cache-control"], "no-store");
    let detail: VariantItems = response.json().await.unwrap();
    assert_eq!(detail.catalog_revision, 1);
    assert_eq!(detail.identity, expected[0].identity.clone().unwrap());
    assert_eq!(detail.quantity, Some(105));
    expected.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
    assert_eq!(detail.items.len(), 103);
    for (item, original) in detail.items.iter().zip(&expected) {
        assert_eq!(&item.catalog, original);
        assert!(!item.archived);
        assert_eq!(item.revision, 1);
    }
    let page: ItemsPage = client
        .get(format!(
            "{origin}/v1/items?limit=1&source=shop:example-shop-0"
        ))
        .bearer_auth(INTEGRATION)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.variant_groups[0].copies.len(), 103);
    assert_eq!(page.items[0].catalog.id, "example-purchase-000");
    for id in ["example-missing", "example-legacy"] {
        assert_eq!(
            client
                .get(format!("{origin}/v1/variants/{id}/items"))
                .bearer_auth(OWNER)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        client
            .get(format!("{origin}/v1/variants/invalid%20id/items"))
            .bearer_auth(OWNER)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let retained = purchase(0);
    let applied = client
        .post(format!("{origin}/v1/inventory/sync"))
        .bearer_auth(OWNER)
        .header("If-Match", "\"1\"")
        .header("Idempotency-Key", "variant-archive-most")
        .json(&snapshot(vec![retained.clone(), legacy.clone()]))
        .send()
        .await
        .unwrap();
    assert_eq!(applied.status(), StatusCode::OK);
    let response = client.get(&url).bearer_auth(OWNER).send().await.unwrap();
    assert_eq!(response.headers()["etag"], "\"2\"");
    let detail: VariantItems = response.json().await.unwrap();
    assert_eq!(detail.catalog_revision, 2);
    assert_eq!(detail.quantity, Some(3));
    assert_eq!(detail.items.len(), 1);
    assert_eq!(detail.items[0].catalog, retained);
    assert_eq!(detail.items[0].revision, 1);
    let applied = client
        .post(format!("{origin}/v1/inventory/sync"))
        .bearer_auth(OWNER)
        .header("If-Match", "\"2\"")
        .header("Idempotency-Key", "variant-archive-all")
        .json(&snapshot(vec![legacy]))
        .send()
        .await
        .unwrap();
    assert_eq!(applied.status(), StatusCode::OK);
    assert_eq!(
        client
            .get(&url)
            .bearer_auth(OWNER)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let archived: Item = client
        .get(format!("{origin}/v1/items/example-purchase-000"))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(archived.archived);
    server.close().await.unwrap();
    store.close().await;
}

#[tokio::test]
async fn historical_unknown_quantity_is_not_replaced_by_a_count_of_acquisitions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy.sqlite3");
    let store = Store::open(&path).await.unwrap();
    store
        .sync(
            snapshot(vec![purchase(0), purchase(1)]),
            Some(0),
            OWNER,
            Some("seed"),
        )
        .await
        .unwrap();
    // A historical nullable catalog body, as retained by compatible database upgrades.
    let mut historical = purchase(1);
    historical.quantity = None;
    let mut connection =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
    sqlx::query("UPDATE items SET body=? WHERE id=?")
        .bind(serde_json::to_string(&historical).unwrap())
        .bind(&historical.id)
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
    let detail = store.variant_items("example-variant").await.unwrap();
    assert_eq!(detail.quantity, None);
    assert_eq!(detail.items.len(), 2);
    assert_eq!(
        detail
            .items
            .iter()
            .find(|item| item.catalog.id == historical.id)
            .unwrap()
            .catalog,
        historical
    );
    store.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn variant_revision_and_acquisitions_share_one_snapshot_during_sync() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("concurrent.sqlite3"))
        .await
        .unwrap();
    let records = |revision| {
        (0..2)
            .map(|index| {
                let mut item = purchase(index);
                item.name = format!("Example revision {revision}");
                item
            })
            .collect()
    };
    store
        .sync(snapshot(records(1)), Some(0), OWNER, Some("initial"))
        .await
        .unwrap();
    let writer = store.clone();
    let task = tokio::spawn(async move {
        for revision in 2..=12 {
            writer
                .sync(
                    snapshot(records(revision)),
                    Some(revision - 1),
                    OWNER,
                    Some(&format!("revision-{revision}")),
                )
                .await
                .unwrap();
        }
    });
    for _ in 0..100 {
        let detail = store.variant_items("example-variant").await.unwrap();
        assert_eq!(detail.items.len(), 2);
        assert_eq!(detail.quantity, Some(4));
        for item in detail.items {
            assert_eq!(
                item.catalog.name,
                format!("Example revision {}", detail.catalog_revision)
            );
            assert_eq!(item.revision, detail.catalog_revision);
        }
    }
    task.await.unwrap();
    store.close().await;
}
