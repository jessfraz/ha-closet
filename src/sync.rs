//! One bounded operation: resolve images, sync and verify the projection, then
//! prune only exact managed objects admitted by the server's durable fence.
use crate::{
    client::{ApiClient, Failure, Output, Result, Transport, response},
    fence::{
        FenceAcquire, FencePrepare, FenceRelease, FenceReleased, FenceResolve, FenceStatus,
        FenceToken, FenceUpload, ManagedImage, UploadClaimAction, UploadClaimStatus,
        referenced_image_id,
    },
    ha_images::{HaImages, UploadOutcome},
    images::{ImageSha, LocalImages, NativeImageId},
    models::{
        CatalogItem, Item, ItemsPage, SyncReport, SyncRequest, System, TodayResponse,
        ValidatedCatalog, validate_catalog,
    },
};
use reqwest::Method;
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

pub const SYNC_TIMEOUT: Duration = Duration::from_secs(600);
#[derive(Default)]
pub struct SyncOptions {
    pub dry_run: bool,
    pub allow_empty: bool,
    pub if_match: Option<String>,
    pub idempotency_key: Option<String>,
    pub state_directory: Option<PathBuf>,
}
#[derive(Debug, Default, Serialize)]
pub struct ImageSummary {
    pub local: usize,
    pub uploaded: usize,
    pub would_upload: usize,
    pub pruned: usize,
    pub would_prune: usize,
}
#[derive(Debug, Serialize)]
pub struct SyncOutput {
    #[serde(flatten)]
    pub inventory: Option<SyncReport>,
    pub input_source_hash: String,
    pub resolved_source_hash: Option<String>,
    pub verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<ImageSummary>,
}
/// Keep the token after a crash, but release the OS lock automatically. A file
/// created by an interrupted process never becomes a permanent lockout.
struct LocalOwner {
    _lock: File,
    token: FenceToken,
}
impl LocalOwner {
    fn open(api: &ApiClient, directory: Option<&Path>) -> Result<Self> {
        let directory = if let Some(path) = directory {
            path.to_owned()
        } else {
            let root = std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
                })
                .ok_or_else(|| Failure::local("sync_state_required"))?;
            root.join("ha-closet")
                .join(ImageSha::digest(api.origin.as_str().as_bytes()).as_str())
        };
        fs::create_dir_all(&directory).map_err(|_| Failure::local("sync_state_unavailable"))?;
        if !fs::symlink_metadata(&directory)
            .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
        {
            return Err(Failure::local("invalid_sync_state"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
                .map_err(|_| Failure::local("sync_state_unavailable"))?;
        }
        let lock_path = directory.join("sync.lock");
        if fs::symlink_metadata(&lock_path)
            .is_ok_and(|m| !m.is_file() || m.file_type().is_symlink())
        {
            return Err(Failure::local("invalid_sync_state"));
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options
            .open(lock_path)
            .map_err(|_| Failure::local("sync_state_unavailable"))?;
        lock.try_lock()
            .map_err(|_| Failure::local("sync_in_progress"))?;
        let token_path = directory.join("fence-token");
        let token = match fs::symlink_metadata(&token_path) {
            Ok(metadata) => {
                if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() != 64
                {
                    return Err(Failure::local("invalid_sync_state"));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if metadata.permissions().mode() & 0o077 != 0 {
                        return Err(Failure::local("invalid_sync_state"));
                    }
                }
                let value = fs::read_to_string(token_path)
                    .map_err(|_| Failure::local("sync_state_unavailable"))?;
                value.try_into().map_err(Failure::local)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let token =
                    FenceToken::new().map_err(|_| Failure::local("sync_state_unavailable"))?;
                let mut options = OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = options
                    .open(token_path)
                    .map_err(|_| Failure::local("sync_state_unavailable"))?;
                file.write_all(token.as_str().as_bytes())
                    .and_then(|()| file.sync_all())
                    .map_err(|_| Failure::local("sync_state_unavailable"))?;
                token
            }
            Err(_) => return Err(Failure::local("sync_state_unavailable")),
        };
        Ok(Self { _lock: lock, token })
    }
}
pub struct SourceSnapshot {
    pub catalog: ValidatedCatalog,
    pub images: Option<LocalImages>,
    input: Option<(PathBuf, ImageSha)>,
}
impl SourceSnapshot {
    pub fn new(
        catalog: ValidatedCatalog,
        images: Option<LocalImages>,
        input: Option<(&Path, &[u8])>,
    ) -> Self {
        Self {
            catalog,
            images,
            input: input.map(|(path, bytes)| (path.to_owned(), ImageSha::digest(bytes))),
        }
    }
    fn unchanged(&self) -> Result<()> {
        if let Some((path, sha)) = &self.input {
            let mut bytes = Vec::new();
            File::open(path)
                .and_then(|file| {
                    file.take(crate::models::MAX_CATALOG_BYTES as u64 + 1)
                        .read_to_end(&mut bytes)
                })
                .map_err(|_| Failure::local("input_read_failed"))?;
            if &ImageSha::digest(&bytes) != sha {
                return Err(Failure::local("catalog_source_changed"));
            }
        }
        if let Some(images) = &self.images {
            images.verify_sources(self.catalog.items())?;
        }
        Ok(())
    }
}
async fn preview(
    api: &ApiClient,
    items: &[CatalogItem],
    allow_empty: bool,
) -> Result<Output<SyncReport>> {
    response(
        api.request(Method::POST, "/inventory/sync")?
            .json(&SyncRequest {
                items: items.to_vec(),
                dry_run: true,
                allow_empty,
            }),
    )
    .await
}
async fn acquire(api: &ApiClient, token: &FenceToken, revision: i64) -> Result<FenceStatus> {
    response::<FenceStatus>(
        api.request(Method::POST, "/inventory/fence")?
            .json(&FenceAcquire {
                token: token.clone(),
                expected_revision: revision,
            }),
    )
    .await
    .map(|out| out.data)
}
async fn claim(
    api: &ApiClient,
    token: &FenceToken,
    sha: &ImageSha,
    action: UploadClaimAction,
) -> Result<bool> {
    response::<UploadClaimStatus>(api.request(Method::POST, "/inventory/fence/uploads")?.json(
        &FenceUpload {
            token: token.clone(),
            sha256: sha.clone(),
            action,
        },
    ))
    .await
    .map(|out| out.data.claimed)
}
async fn release(api: &ApiClient, token: &FenceToken) -> Result<()> {
    response::<FenceReleased>(api.request(Method::DELETE, "/inventory/fence")?.json(
        &FenceRelease {
            token: token.clone(),
        },
    ))
    .await
    .map(|_| ())
}
async fn apply_inventory(
    api: &ApiClient,
    items: &[CatalogItem],
    options: &SyncOptions,
    preview: &Output<SyncReport>,
    token: Option<&FenceToken>,
) -> Result<Output<SyncReport>> {
    let revision = options
        .if_match
        .clone()
        .or_else(|| preview.etag.clone())
        .ok_or_else(|| Failure::local("invalid_revision_response"))?;
    let key = options.idempotency_key.clone().unwrap_or_else(|| {
        format!(
            "closet-sync-{}-{}-{}",
            preview.data.catalog_revision,
            preview.data.source_hash,
            u8::from(options.allow_empty)
        )
    });
    let mut request = api
        .request(Method::POST, "/inventory/sync")?
        .header("If-Match", revision)
        .header("Idempotency-Key", key)
        .json(&SyncRequest {
            items: items.to_vec(),
            dry_run: false,
            allow_empty: options.allow_empty,
        });
    if let Some(token) = token {
        request = request.header("X-Closet-Sync-Fence", token.as_str());
    }
    response(request).await
}
async fn readback(
    api: &ApiClient,
    items: &[CatalogItem],
    report: &SyncReport,
    allow_empty: bool,
) -> Result<BTreeSet<NativeImageId>> {
    let unchanged = preview(api, items, allow_empty).await?.data;
    if unchanged.catalog_revision != report.catalog_revision
        || unchanged.source_hash != report.source_hash
        || unchanged.created + unchanged.updated + unchanged.archived != 0
    {
        return Err(Failure::local("inventory_readback_mismatch"));
    }
    let mut result = Vec::new();
    let mut cursors = BTreeSet::new();
    let mut next = None;
    loop {
        let mut request = api.request(Method::GET, "/items")?;
        if let Some(cursor) = next.take() {
            request = request.query(&[("page_token", cursor)]);
        } else {
            request = request.query(&[("limit", 100)]);
        }
        let page = response::<ItemsPage>(request).await?.data;
        if page.catalog_revision != report.catalog_revision
            || page.total != items.len() as i64
            || page.scope_total != page.total
            || page.items.iter().any(|item| item.archived)
        {
            return Err(Failure::local("inventory_readback_mismatch"));
        }
        result.extend(page.items.into_iter().map(|item| item.catalog));
        if result.len() > crate::models::MAX_CATALOG_ITEMS {
            return Err(Failure::local("inventory_readback_mismatch"));
        }
        match page.next_page {
            Some(cursor) if cursors.insert(cursor.clone()) => next = Some(cursor),
            Some(_) => return Err(Failure::local("inventory_readback_mismatch")),
            None => break,
        }
    }
    let actual = validate_catalog(result.clone()).map_err(Failure::catalog)?;
    if actual.report().source_hash != report.source_hash || result.len() != items.len() {
        return Err(Failure::local("inventory_readback_mismatch"));
    }
    let references = live_references(api, &result).await?;
    let system = response::<System>(api.request(Method::GET, "/system")?)
        .await?
        .data;
    if system.catalog_revision != report.catalog_revision || system.item_count != items.len() as i64
    {
        return Err(Failure::local("inventory_readback_mismatch"));
    }
    Ok(references)
}
async fn live_references(
    api: &ApiClient,
    items: &[CatalogItem],
) -> Result<BTreeSet<NativeImageId>> {
    let mut referenced: BTreeSet<_> = items
        .iter()
        .flat_map(|item| &item.images)
        .filter_map(|image| referenced_image_id(&image.url))
        .collect();
    let known: BTreeSet<_> = items.iter().map(|item| item.id.as_str()).collect();
    let today = response::<TodayResponse>(api.request(Method::GET, "/today")?)
        .await?
        .data;
    let missing: BTreeSet<_> = today
        .result
        .into_iter()
        .flat_map(|result| result.suggestions)
        .flat_map(|suggestion| suggestion.item_ids)
        .filter(|id| !known.contains(id.as_str()))
        .collect();
    if missing.len() > 60 {
        return Err(Failure::local("invalid_today_response"));
    }
    for id in missing {
        if !crate::models::valid_id(&id) {
            return Err(Failure::local("invalid_today_response"));
        }
        let item = response::<Item>(api.request(Method::GET, &format!("/items/{id}"))?)
            .await?
            .data;
        if item.catalog.id != id {
            return Err(Failure::local("inventory_readback_mismatch"));
        }
        referenced.extend(
            item.catalog
                .images
                .iter()
                .filter_map(|image| referenced_image_id(&image.url)),
        );
    }
    Ok(referenced)
}
async fn candidates(
    native: &HaImages<'_>,
    references: &BTreeSet<NativeImageId>,
) -> Result<Vec<ManagedImage>> {
    let mut candidates = Vec::new();
    for image in native.list().await? {
        if references.contains(&image.id) {
            continue;
        }
        if let Some(sha) = ImageSha::from_upload_name(&image.name) {
            native.verify(&image, &sha).await?;
            candidates.push(ManagedImage {
                id: image.id,
                sha256: sha,
            });
        }
    }
    Ok(candidates)
}
async fn delete_pending(
    api: &ApiClient,
    native: &HaImages<'_>,
    token: &FenceToken,
    mut state: FenceStatus,
) -> Result<usize> {
    let mut count = 0;
    for image in state.pending.clone() {
        acquire(api, token, state.catalog_revision).await?;
        let records = native
            .list()
            .await
            .map_err(|_| Failure::local("cleanup_pending"))?;
        if let Some(record) = records.iter().find(|record| record.id == image.id) {
            if record.name != image.sha256.upload_name() {
                return Err(Failure::local("cleanup_pending"));
            }
            native
                .verify(record, &image.sha256)
                .await
                .map_err(|_| Failure::local("cleanup_pending"))?;
            // A lost delete reply is not retried in this run. The persisted exact
            // ID remains fenced, and absence is the only successful resolution.
            let _ = native.delete(&image.id).await;
            if native
                .list()
                .await
                .map_err(|_| Failure::local("cleanup_pending"))?
                .iter()
                .any(|record| record.id == image.id)
            {
                return Err(Failure::local("cleanup_pending"));
            }
        }
        state = response::<FenceStatus>(
            api.request(Method::POST, "/inventory/fence/resolve")?
                .json(&FenceResolve {
                    token: token.clone(),
                    image,
                }),
        )
        .await?
        .data;
        count += 1;
    }
    Ok(count)
}
async fn resolve_assets(
    api: &ApiClient,
    native: &HaImages<'_>,
    token: &FenceToken,
    state: &FenceStatus,
    local: &LocalImages,
    summary: &mut ImageSummary,
) -> Result<BTreeMap<ImageSha, NativeImageId>> {
    let mut resolved = BTreeMap::new();
    for asset in &local.assets {
        acquire(api, token, state.catalog_revision).await?;
        if let Some(id) = native.matching(asset, &native.list().await?).await? {
            claim(api, token, &asset.sha256, UploadClaimAction::Verified).await?;
            resolved.insert(asset.sha256.clone(), id);
            continue;
        }
        if !claim(api, token, &asset.sha256, UploadClaimAction::Begin).await? {
            return Err(Failure::local("image_upload_pending"));
        }
        // Re-list after claiming: another completed/stale initial list is never
        // permission to create a duplicate native random ID.
        let second = async {
            if let Some(id) = native.matching(asset, &native.list().await?).await? {
                return Ok(Some(id));
            }
            Ok::<_, Failure>(None)
        }
        .await;
        match second {
            Ok(Some(id)) => {
                claim(api, token, &asset.sha256, UploadClaimAction::Verified).await?;
                resolved.insert(asset.sha256.clone(), id);
                continue;
            }
            Err(error) => {
                let _ = claim(api, token, &asset.sha256, UploadClaimAction::Rejected).await;
                return Err(error);
            }
            Ok(None) => {}
        }
        let outcome = match native.upload(asset).await {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = claim(api, token, &asset.sha256, UploadClaimAction::Rejected).await;
                return Err(error);
            }
        };
        if matches!(outcome, UploadOutcome::Rejected) {
            claim(api, token, &asset.sha256, UploadClaimAction::Rejected).await?;
            return Err(Failure::local("native_image_upload_rejected"));
        }
        let mut found = None;
        for delay in [0, 250, 500] {
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            if let Ok(records) = native.list().await
                && let Ok(Some(id)) = native.matching(asset, &records).await
            {
                found = Some(id);
                break;
            }
        }
        let id = found.ok_or_else(|| Failure::local("image_upload_pending"))?;
        claim(api, token, &asset.sha256, UploadClaimAction::Verified).await?;
        resolved.insert(asset.sha256.clone(), id);
        summary.uploaded += 1;
    }
    Ok(resolved)
}
async fn managed(
    api: &ApiClient,
    source: &SourceSnapshot,
    options: &SyncOptions,
) -> Result<Output<SyncOutput>> {
    let native = HaImages::new(api)?;
    let original = preview(api, source.catalog.items(), options.allow_empty).await?;
    let mut summary = ImageSummary {
        local: source
            .images
            .as_ref()
            .map_or(0, |images| images.assets.len()),
        ..Default::default()
    };
    if options.dry_run {
        let mut resolved = BTreeMap::new();
        if let Some(images) = &source.images {
            let records = native.list().await?;
            for asset in &images.assets {
                match native.matching(asset, &records).await? {
                    Some(id) => {
                        resolved.insert(asset.sha256.clone(), id);
                    }
                    None => summary.would_upload += 1,
                }
            }
        }
        let output = if summary.would_upload > 0 {
            None
        } else {
            let items = match &source.images {
                Some(images) => {
                    images.materialize(source.catalog.items(), &api.origin, &resolved)?
                }
                None => source.catalog.items().to_vec(),
            };
            let output = preview(api, &items, options.allow_empty).await?;
            summary.would_prune = candidates(&native, &live_references(api, &items).await?)
                .await?
                .len();
            Some(output)
        };
        source.unchanged()?;
        return Ok(Output {
            status: original.status,
            etag: original.etag,
            data: SyncOutput {
                resolved_source_hash: output.as_ref().map(|out| out.data.source_hash.clone()),
                inventory: output.map(|out| out.data),
                input_source_hash: source.catalog.report().source_hash.clone(),
                verified: false,
                images: Some(summary),
            },
        });
    }
    let owner = LocalOwner::open(api, options.state_directory.as_deref())?;
    let state = acquire(api, &owner.token, original.data.catalog_revision).await?;
    let result = async {
        summary.pruned += delete_pending(api, &native, &owner.token, state.clone()).await?;
        let resolved = match &source.images {
            Some(images) => {
                resolve_assets(api, &native, &owner.token, &state, images, &mut summary).await?
            }
            None => BTreeMap::new(),
        };
        let items = match &source.images {
            Some(images) => images.materialize(source.catalog.items(), &api.origin, &resolved)?,
            None => source.catalog.items().to_vec(),
        };
        validate_catalog(items.clone()).map_err(Failure::catalog)?;
        source.unchanged()?;
        let preview = preview(api, &items, options.allow_empty).await?;
        acquire(api, &owner.token, preview.data.catalog_revision).await?;
        let output = apply_inventory(api, &items, options, &preview, Some(&owner.token)).await?;
        let references = readback(api, &items, &output.data, options.allow_empty).await?;
        source.unchanged()?;
        let status = acquire(api, &owner.token, output.data.catalog_revision).await?;
        let records = native.list().await?;
        for sha in status.uploads {
            let matching: Vec<_> = records
                .iter()
                .filter(|image| image.name == sha.upload_name())
                .collect();
            if matching.is_empty() {
                return Err(Failure::local("image_upload_pending"));
            }
            for image in matching {
                native.verify(image, &sha).await?;
            }
            claim(api, &owner.token, &sha, UploadClaimAction::Verified).await?;
        }
        let images = candidates(&native, &references).await?;
        let state = response::<FenceStatus>(
            api.request(Method::POST, "/inventory/fence/prepare")?
                .json(&FencePrepare {
                    token: owner.token.clone(),
                    catalog_revision: output.data.catalog_revision,
                    images,
                }),
        )
        .await?
        .data;
        summary.pruned += delete_pending(api, &native, &owner.token, state).await?;
        Ok(Output {
            status: output.status,
            etag: output.etag,
            data: SyncOutput {
                input_source_hash: source.catalog.report().source_hash.clone(),
                resolved_source_hash: Some(output.data.source_hash.clone()),
                inventory: Some(output.data),
                verified: true,
                images: Some(summary),
            },
        })
    }
    .await;
    let released = release(api, &owner.token).await;
    match result {
        Ok(output) => {
            released?;
            Ok(output)
        }
        Err(error) => Err(error),
    }
}
pub async fn inventory_sync(
    api: &ApiClient,
    source: &SourceSnapshot,
    options: &SyncOptions,
) -> Result<Output<SyncOutput>> {
    if source.images.is_some() && api.transport != Transport::HomeAssistant {
        return Err(Failure::local("image_transport_required"));
    }
    if source.images.is_some() && api.origin.scheme() != "https" {
        return Err(Failure::local("image_https_required"));
    }
    if source
        .images
        .as_ref()
        .and_then(|images| images.native_origin.as_ref())
        .is_some_and(|origin| origin != &api.origin)
    {
        return Err(Failure::local("image_origin_mismatch"));
    }
    let operation = async {
        if api.transport == Transport::HomeAssistant {
            return managed(api, source, options).await;
        }
        let preview = preview(api, source.catalog.items(), options.allow_empty).await?;
        let output = if options.dry_run {
            preview
        } else {
            source.unchanged()?;
            apply_inventory(api, source.catalog.items(), options, &preview, None).await?
        };
        if !options.dry_run {
            readback(
                api,
                source.catalog.items(),
                &output.data,
                options.allow_empty,
            )
            .await?;
        }
        Ok(Output {
            status: output.status,
            etag: output.etag,
            data: SyncOutput {
                input_source_hash: source.catalog.report().source_hash.clone(),
                resolved_source_hash: Some(output.data.source_hash.clone()),
                inventory: Some(output.data),
                verified: !options.dry_run,
                images: None,
            },
        })
    };
    tokio::select! { result=tokio::time::timeout(SYNC_TIMEOUT,operation)=>result.map_err(|_|Failure::local("sync_timeout"))?, _=tokio::signal::ctrl_c()=>Err(Failure::local("sync_cancelled")) }
}
