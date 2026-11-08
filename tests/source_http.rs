//! A wire-level consumer also runs against the previous release for fail-first proof.

use std::{collections::BTreeMap, sync::Arc};

use closet::{
    api,
    models::{CatalogItem, Item, SyncRequest, parse_catalog},
    store::Store,
};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};

const OWNER: &str = "example-source-test-credential";
const SHOP: &str = "shop:example-shop";
const SELF_BOUGHT: &str = "purchase:self_bought";

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct WireSourceFacet {
    source: String,
    label: String,
    count: i64,
}
#[derive(Debug, Deserialize, PartialEq, Eq)]
struct WireFacets {
    sources: Vec<WireSourceFacet>,
}
#[derive(Deserialize)]
struct WirePage {
    items: Vec<Item>,
    next_page: Option<String>,
    total: i64,
    scope_total: i64,
    catalog_revision: i64,
    facets: WireFacets,
}

fn piece(id: &str, name: &str, purchase: Option<&str>, shop: bool) -> CatalogItem {
    let mut item = parse_catalog(include_bytes!("fixtures/catalog-seasons.json"))
        .unwrap()
        .into_items()
        .remove(0);
    item.id = id.into();
    item.name = name.into();
    item.purchase_source = purchase.map(str::to_owned);
    if !shop {
        item.shop = None;
    }
    item
}
fn snapshot(items: Vec<CatalogItem>) -> SyncRequest {
    SyncRequest {
        items,
        dry_run: false,
        allow_empty: false,
    }
}
fn facets(page: &WirePage) -> BTreeMap<&str, (&str, i64)> {
    page.facets
        .sources
        .iter()
        .map(|facet| (facet.source.as_str(), (facet.label.as_str(), facet.count)))
        .collect()
}

#[tokio::test]
async fn source_filters_page_the_full_catalog_and_keep_global_active_facets() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("source.sqlite3"))
        .await
        .unwrap();
    let context = Arc::new(api::Context::new(store.clone(), OWNER));
    let server = api::start("127.0.0.1:0".parse().unwrap(), context.clone()).unwrap();
    let endpoint = format!("http://{}/v1/items", server.local_addr());
    let client = Client::new();
    let mut input = vec![
        piece("example-shop-a", "A shop piece", Some("self_bought"), true),
        piece("example-shop-b", "B shop piece", None, true),
        piece("example-shop-c", "Z shop piece", Some("Example gift"), true),
        piece(
            "example-self-a",
            "C self-bought piece",
            Some("self_bought"),
            false,
        ),
        piece(
            "example-self-b",
            "D self-bought piece",
            Some("self_bought"),
            false,
        ),
        piece(
            "example-gift",
            "E gift piece",
            Some("Example gift / café"),
            false,
        ),
        piece("example-other-shop", "F other shop piece", None, true),
    ];
    let other_shop = input[6].shop.as_mut().unwrap();
    other_shop.id = "example-second-shop".into();
    other_shop.label = "Example pop-up".into();
    input[2].category = "footwear.shoes".into();
    input[2].brand = Some("Example second label".into());
    store
        .sync(snapshot(input.clone()), Some(0), "owner", Some("initial"))
        .await
        .unwrap();

    let response = client
        .get(&endpoint)
        .bearer_auth(OWNER)
        .query(&[("source", SHOP), ("limit", "1")])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let first = response.json::<WirePage>().await.unwrap();
    assert_eq!(
        (first.total, first.scope_total, first.catalog_revision),
        (3, 7, 1)
    );
    assert_eq!(first.items.len(), 1);
    assert_eq!(
        facets(&first),
        BTreeMap::from([
            (SHOP, ("Example shop", 3)),
            ("shop:example-second-shop", ("Example pop-up", 1)),
            (SELF_BOUGHT, ("Self-bought", 2)),
            ("purchase:Example gift / café", ("Example gift / café", 1)),
        ])
    );
    let all = client
        .get(&endpoint)
        .bearer_auth(OWNER)
        .query(&[("limit", "1")])
        .send()
        .await
        .unwrap()
        .json::<WirePage>()
        .await
        .unwrap();
    assert_eq!((all.total, all.scope_total, all.items.len()), (7, 7, 1));
    assert_eq!(all.facets, first.facets);
    let first_cursor = first.next_page.clone().unwrap();
    let changed = client
        .get(&endpoint)
        .bearer_auth(OWNER)
        .query(&[
            ("page_token", first_cursor.as_str()),
            ("source", SELF_BOUGHT),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(changed.status(), StatusCode::BAD_REQUEST);

    let mut seen = vec![first.items[0].catalog.id.clone()];
    let mut next = first.next_page.clone();
    while let Some(cursor) = next {
        let page = client
            .get(&endpoint)
            .bearer_auth(OWNER)
            .query(&[("page_token", cursor.as_str()), ("limit", "1")])
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<WirePage>()
            .await
            .unwrap();
        assert_eq!(page.facets, first.facets);
        assert_eq!(page.total, 3);
        seen.extend(page.items.into_iter().map(|item| item.catalog.id));
        next = page.next_page;
    }
    assert_eq!(seen, ["example-shop-a", "example-shop-b", "example-shop-c"]);
    let combined = client
        .get(&endpoint)
        .bearer_auth(OWNER)
        .query(&[
            ("source", SHOP),
            ("category", "footwear"),
            ("brand", "Example second label"),
        ])
        .send()
        .await
        .unwrap()
        .json::<WirePage>()
        .await
        .unwrap();
    assert_eq!(combined.total, 1);
    assert_eq!(combined.items[0].catalog.id, "example-shop-c");
    assert_eq!(combined.facets, first.facets);

    let gift = input.remove(5);
    input.push(piece(
        "example-exchange",
        "G exchanged piece",
        Some("Example exchange"),
        false,
    ));
    store
        .sync(snapshot(input.clone()), Some(1), "owner", Some("archive"))
        .await
        .unwrap();
    assert!(store.item(&gift.id).await.unwrap().archived);
    let response = client
        .get(&endpoint)
        .bearer_auth(OWNER)
        .query(&[("page_token", first_cursor.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let exchanged = client
        .get(&endpoint)
        .bearer_auth(OWNER)
        .query(&[("source", "purchase:Example exchange")])
        .send()
        .await
        .unwrap()
        .json::<WirePage>()
        .await
        .unwrap();
    assert_eq!((exchanged.total, exchanged.scope_total), (1, 7));
    assert_eq!(exchanged.items[0].catalog.id, "example-exchange");
    assert_eq!(
        facets(&exchanged).get("purchase:Example exchange"),
        Some(&("Example exchange", 1))
    );
    assert!(!facets(&exchanged).contains_key("purchase:Example gift / café"));

    input.push(gift);
    store
        .sync(snapshot(input), Some(2), "owner", Some("restore"))
        .await
        .unwrap();
    let restored = client
        .get(&endpoint)
        .bearer_auth(OWNER)
        .query(&[("source", "purchase:Example gift / café")])
        .send()
        .await
        .unwrap()
        .json::<WirePage>()
        .await
        .unwrap();
    assert_eq!((restored.total, restored.scope_total), (1, 8));
    assert_eq!(restored.items[0].catalog.id, "example-gift");
    assert_eq!(
        facets(&restored).get("purchase:Example gift / café"),
        Some(&("Example gift / café", 1))
    );
    server.close().await.unwrap();
    context.shutdown().await.unwrap();
    store.close().await;
}

#[tokio::test]
async fn source_queries_reject_invalid_keys_and_bind_values_literally() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("source.sqlite3"))
        .await
        .unwrap();
    store
        .sync(
            snapshot(vec![piece(
                "example-one",
                "One piece",
                Some("self_bought"),
                false,
            )]),
            Some(0),
            "owner",
            Some("initial"),
        )
        .await
        .unwrap();
    let context = Arc::new(api::Context::new(store.clone(), OWNER));
    let server = api::start("127.0.0.1:0".parse().unwrap(), context.clone()).unwrap();
    let endpoint = format!("http://{}/v1/items", server.local_addr());
    let client = Client::new();
    for invalid in [
        "shop",
        "shop:",
        "purchase:",
        "unspecified",
        "unspecified:any",
        "Shop:example",
        "all",
        "purchase:\nexample",
    ] {
        let response = client
            .get(&endpoint)
            .bearer_auth(OWNER)
            .query(&[("source", invalid)])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{invalid:?}");
    }
    for missing in ["purchase:example' OR 1=1 --", "shop:unknown-example"] {
        let page = client
            .get(&endpoint)
            .bearer_auth(OWNER)
            .query(&[("source", missing)])
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<WirePage>()
            .await
            .unwrap();
        assert_eq!((page.total, page.scope_total), (0, 1));
        assert!(page.items.is_empty());
        assert_eq!(
            facets(&page),
            BTreeMap::from([(SELF_BOUGHT, ("Self-bought", 1))])
        );
    }
    server.close().await.unwrap();
    context.shutdown().await.unwrap();
    store.close().await;
}

#[tokio::test]
async fn legacy_sourceless_rows_remain_readable_without_a_source_facet() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy.sqlite3");
    let store = Store::open(&path).await.unwrap();
    let input = vec![
        piece(
            "example-legacy",
            "Legacy piece",
            Some("Example exchange"),
            false,
        ),
        piece("example-shop", "Shop piece", None, true),
    ];
    store
        .sync(snapshot(input.clone()), Some(0), "owner", Some("initial"))
        .await
        .unwrap();
    let options = SqliteConnectOptions::new().filename(&path);
    let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
    // Seed the persisted shape accepted before source became required. New
    // imports go through the validator and cannot create this legacy state.
    let mut legacy = input[0].clone();
    legacy.purchase_source = None;
    sqlx::query("UPDATE items SET body=? WHERE id=?")
        .bind(serde_json::to_string(&legacy).unwrap())
        .bind(&legacy.id)
        .execute(&mut connection)
        .await
        .unwrap();
    let context = Arc::new(api::Context::new(store.clone(), OWNER));
    let server = api::start("127.0.0.1:0".parse().unwrap(), context.clone()).unwrap();
    let endpoint = format!("http://{}/v1/items", server.local_addr());
    let client = Client::new();
    let all = client
        .get(&endpoint)
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<WirePage>()
        .await
        .unwrap();
    assert_eq!((all.total, all.scope_total, all.items.len()), (2, 2, 2));
    assert_eq!(facets(&all), BTreeMap::from([(SHOP, ("Example shop", 1))]));
    assert_eq!(
        all.items
            .iter()
            .find(|item| item.catalog.id == legacy.id)
            .unwrap()
            .catalog,
        legacy
    );
    let detail = client
        .get(format!("{endpoint}/{}", legacy.id))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Item>()
        .await
        .unwrap();
    assert_eq!(detail.catalog, legacy);
    let filtered = client
        .get(&endpoint)
        .bearer_auth(OWNER)
        .query(&[("source", SHOP)])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<WirePage>()
        .await
        .unwrap();
    assert_eq!((filtered.total, filtered.scope_total), (1, 2));
    assert_eq!(filtered.items[0].catalog.id, "example-shop");

    let mut second_legacy = input[1].clone();
    second_legacy.shop = None;
    sqlx::query("UPDATE items SET body=? WHERE id=?")
        .bind(serde_json::to_string(&second_legacy).unwrap())
        .bind(&second_legacy.id)
        .execute(&mut connection)
        .await
        .unwrap();
    let all_legacy = client
        .get(&endpoint)
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<WirePage>()
        .await
        .unwrap();
    assert_eq!(
        (
            all_legacy.total,
            all_legacy.scope_total,
            all_legacy.items.len()
        ),
        (2, 2, 2)
    );
    assert!(all_legacy.facets.sources.is_empty());
    connection.close().await.unwrap();
    server.close().await.unwrap();
    context.shutdown().await.unwrap();
    store.close().await;
}

#[tokio::test]
async fn source_cursor_boundaries_roundtrip_within_home_assistant_request_lines() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("source.sqlite3"))
        .await
        .unwrap();
    let context = Arc::new(api::Context::new(store.clone(), OWNER));
    let server = api::start("127.0.0.1:0".parse().unwrap(), context.clone()).unwrap();
    let endpoint = format!("http://{}/v1/items", server.local_addr());
    let client = Client::new();
    let purchase = "\"".repeat(1024);
    let source = format!("purchase:{purchase}");
    let brand = "\"".repeat(1024);
    let name = "\"".repeat(512);
    let category = "a".repeat(200);
    let search = "\"".repeat(256);
    let first_id = "a".repeat(200);
    let second_id = format!("{}b", "a".repeat(199));
    let mut first = piece(&first_id, &name, Some(&purchase), false);
    first.brand = Some(brand.clone());
    first.category = category.clone();
    let mut second = first.clone();
    second.id = second_id.clone();
    store
        .sync(
            snapshot(vec![first, second]),
            Some(0),
            "owner",
            Some("boundary"),
        )
        .await
        .unwrap();

    for query in [
        vec![("source", source.as_str()), ("limit", "1")],
        vec![
            ("source", source.as_str()),
            ("brand", brand.as_str()),
            ("q", search.as_str()),
            ("category", category.as_str()),
            ("limit", "1"),
        ],
    ] {
        let first = client
            .get(&endpoint)
            .bearer_auth(OWNER)
            .query(&query)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<WirePage>()
            .await
            .unwrap();
        assert_eq!(first.items[0].catalog.id, first_id);
        assert_eq!((first.total, first.scope_total), (2, 2));
        let cursor = first.next_page.unwrap();
        let continuation = client
            .get(&endpoint)
            .bearer_auth(OWNER)
            .query(&[("page_token", cursor.as_str()), ("limit", "1")])
            .build()
            .unwrap();
        // Measure the encoded URL actually sent, with the longer HA route.
        let request_line = format!(
            "GET /api/closet/v1/items?{} HTTP/1.1\r\n",
            continuation.url().query().unwrap()
        );
        let response = client.execute(continuation).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(request_line.len() <= 8190, "{} bytes", request_line.len());
        println!(
            "{} filters: {} cursor characters, {} encoded HA request-line bytes",
            query.len() - 1,
            cursor.len(),
            request_line.len()
        );
        let next = response.json::<WirePage>().await.unwrap();
        assert_eq!(next.items.len(), 1);
        assert_eq!(next.items[0].catalog.id, second_id);
        assert_eq!((next.total, next.scope_total), (2, 2));
        assert!(next.next_page.is_none());
    }
    server.close().await.unwrap();
    context.shutdown().await.unwrap();
    store.close().await;
}
