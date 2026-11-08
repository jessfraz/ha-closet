use chrono::{Duration, Utc};
use closet::{
    fence::{
        FenceAcquire, FencePrepare, FenceResolve, FenceToken, FenceUpload, ManagedImage,
        UploadClaimAction,
    },
    images::{ImageSha, NativeImageId},
    models::{
        CatalogItem, Freshness, Suggestion, SyncRequest, TodayRequest, TodayResult, parse_catalog,
    },
    store::{GenerationBasis, Store},
};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
fn items() -> Vec<CatalogItem> {
    parse_catalog(include_bytes!("fixtures/catalog-crop.json"))
        .unwrap()
        .into_items()
}
fn request(items: Vec<CatalogItem>) -> SyncRequest {
    SyncRequest {
        items,
        dry_run: false,
        allow_empty: true,
    }
}
fn image(n: u32) -> ManagedImage {
    ManagedImage {
        id: NativeImageId::try_from(format!("{n:032x}")).unwrap(),
        sha256: ImageSha::digest(format!("fictional-{n}").as_bytes()),
    }
}
async fn expire(path: &std::path::Path) {
    let mut connection =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path))
            .await
            .unwrap();
    sqlx::query("UPDATE inventory_fence SET expires_at=0")
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
}
#[tokio::test]
async fn all_writers_obey_durable_pending_prune_across_expiration_restart_and_takeover() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("closet.sqlite3");
    let store = Store::open(&path).await.unwrap();
    let token = FenceToken::new().unwrap();
    store
        .acquire_fence(
            FenceAcquire {
                token: token.clone(),
                expected_revision: 0,
            },
            "owner",
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .sync(request(items()), Some(0), "legacy-writer", Some("blocked"))
            .await
            .unwrap_err()
            .code(),
        "sync_in_progress"
    );
    let applied = store
        .sync_fenced(
            request(items()),
            Some(0),
            "owner",
            Some("initial"),
            Some(&token),
        )
        .await
        .unwrap();
    assert_eq!(applied.catalog_revision, 1);
    let obsolete = image(9);
    store
        .prepare_prune(
            FencePrepare {
                token: token.clone(),
                catalog_revision: 1,
                images: vec![obsolete.clone()],
            },
            "owner",
            "2026-01-01",
        )
        .await
        .unwrap();
    expire(&path).await;
    assert_eq!(
        store
            .sync(request(items()), Some(1), "owner", Some("blocked-again"))
            .await
            .unwrap_err()
            .code(),
        "cleanup_pending"
    );
    assert_eq!(
        store
            .sync_fenced(
                request(items()),
                Some(1),
                "owner",
                Some("expired"),
                Some(&token)
            )
            .await
            .unwrap_err()
            .code(),
        "sync_fence_expired"
    );
    store.close().await;
    let store = Store::open(&path).await.unwrap();
    let new = FenceToken::new().unwrap();
    let status = store
        .acquire_fence(
            FenceAcquire {
                token: new.clone(),
                expected_revision: 1,
            },
            "new-owner",
        )
        .await
        .unwrap();
    assert_eq!(status.pending, vec![obsolete.clone()]);
    assert_eq!(
        store
            .release_fence(&new, "new-owner")
            .await
            .unwrap_err()
            .code(),
        "cleanup_pending"
    );
    let wrong = ManagedImage {
        sha256: ImageSha::digest(b"wrong"),
        ..obsolete.clone()
    };
    assert_eq!(
        store
            .resolve_pruned(
                FenceResolve {
                    token: new.clone(),
                    image: wrong
                },
                "new-owner"
            )
            .await
            .unwrap_err()
            .code(),
        "invalid_prune_image"
    );
    let status = store
        .resolve_pruned(
            FenceResolve {
                token: new.clone(),
                image: obsolete.clone(),
            },
            "new-owner",
        )
        .await
        .unwrap();
    assert!(status.pending.is_empty());
    store
        .resolve_pruned(
            FenceResolve {
                token: new.clone(),
                image: obsolete,
            },
            "new-owner",
        )
        .await
        .unwrap();
    store.release_fence(&new, "new-owner").await.unwrap();
    store
        .sync(
            request(items()),
            Some(1),
            "legacy-writer",
            Some("unblocked"),
        )
        .await
        .unwrap();
    store.close().await;
}
#[tokio::test]
async fn upload_claims_are_durable_but_definite_rejection_and_verified_resolution_release_them() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("closet.sqlite3");
    let store = Store::open(&path).await.unwrap();
    let token = FenceToken::new().unwrap();
    let sha = ImageSha::digest(b"fictional-source");
    store
        .acquire_fence(
            FenceAcquire {
                token: token.clone(),
                expected_revision: 0,
            },
            "owner",
        )
        .await
        .unwrap();
    for expected in [true, false] {
        assert_eq!(
            store
                .upload_claim(
                    FenceUpload {
                        token: token.clone(),
                        sha256: sha.clone(),
                        action: UploadClaimAction::Begin
                    },
                    "owner"
                )
                .await
                .unwrap()
                .claimed,
            expected
        );
    }
    store.release_fence(&token, "owner").await.unwrap();
    store.close().await;
    let store = Store::open(&path).await.unwrap();
    let other = FenceToken::new().unwrap();
    let state = store
        .acquire_fence(
            FenceAcquire {
                token: other.clone(),
                expected_revision: 0,
            },
            "other",
        )
        .await
        .unwrap();
    assert_eq!(state.uploads, vec![sha.clone()]);
    for action in [UploadClaimAction::Rejected, UploadClaimAction::Verified] {
        store
            .upload_claim(
                FenceUpload {
                    token: other.clone(),
                    sha256: sha.clone(),
                    action,
                },
                "other",
            )
            .await
            .unwrap();
        assert!(
            store
                .upload_claim(
                    FenceUpload {
                        token: other.clone(),
                        sha256: sha.clone(),
                        action: UploadClaimAction::Begin
                    },
                    "other"
                )
                .await
                .unwrap()
                .claimed
        );
    }
    store.close().await;
}
#[tokio::test]
async fn active_aliases_and_current_saved_today_archived_records_are_live_references() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("closet.sqlite3"))
        .await
        .unwrap();
    let mut catalog = items();
    catalog.truncate(1);
    let old = image(7);
    catalog[0].images[0].url = format!(
        "https://ha.example.invalid/api/image/serve/{}/512x512-photo.png",
        old.id
    );
    store
        .sync(request(catalog.clone()), Some(0), "owner", Some("initial"))
        .await
        .unwrap();
    let now = Utc::now();
    let date = now.date_naive().to_string();
    let basis = GenerationBasis {
        date: date.clone(),
        timezone: "UTC".into(),
        config_hash: "example".into(),
        items: catalog.clone(),
    };
    let generation = store
        .reserve_generation(TodayRequest::default(), basis, "owner", "today")
        .await
        .unwrap()
        .generation;
    store
        .finish_generation(
            &generation.id,
            Ok(TodayResult {
                generation_id: generation.id.clone(),
                inputs: generation.inputs,
                generated_at: now,
                expires_at: now + Duration::hours(1),
                freshness: Freshness::Ready,
                stale_reasons: Vec::new(),
                weather: None,
                suggestions: vec![Suggestion {
                    occasion: None,
                    label: "Fictional".into(),
                    item_ids: vec![catalog[0].id.clone()],
                    advisory_reason: "Example".into(),
                    warnings: Vec::new(),
                }],
            }),
        )
        .await
        .unwrap();
    store
        .sync(request(Vec::new()), Some(1), "owner", Some("archive"))
        .await
        .unwrap();
    let token = FenceToken::new().unwrap();
    store
        .acquire_fence(
            FenceAcquire {
                token: token.clone(),
                expected_revision: 2,
            },
            "owner",
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .prepare_prune(
                FencePrepare {
                    token: token.clone(),
                    catalog_revision: 2,
                    images: vec![old.clone()]
                },
                "owner",
                &date
            )
            .await
            .unwrap_err()
            .code(),
        "image_referenced"
    );
    // Yesterday's saved result is no longer rendered and is not a forever-retain policy.
    store
        .prepare_prune(
            FencePrepare {
                token: token.clone(),
                catalog_revision: 2,
                images: vec![old.clone()],
            },
            "owner",
            "2099-01-01",
        )
        .await
        .unwrap();
    store
        .resolve_pruned(
            FenceResolve {
                token: token.clone(),
                image: old.clone(),
            },
            "owner",
        )
        .await
        .unwrap();
    catalog[0].images[0].url = format!(
        "https://other.example.invalid/api/image/serve/%30{}/256x256-photo.png?size=original#photo",
        &old.id.as_str()[1..]
    );
    store
        .sync_fenced(
            request(catalog),
            Some(2),
            "owner",
            Some("alias"),
            Some(&token),
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .prepare_prune(
                FencePrepare {
                    token: token.clone(),
                    catalog_revision: 3,
                    images: vec![old]
                },
                "owner",
                &date
            )
            .await
            .unwrap_err()
            .code(),
        "image_referenced"
    );
    store.release_fence(&token, "owner").await.unwrap();
    store.close().await;
}
