//! The authenticated app, CLI and Home Assistant adapter share this API.

use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::Context as _;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use dropshot::{
    ApiDescription, Body, ClientErrorStatusCode as Status, ConfigDropshot, HttpError,
    HttpResponseAccepted, HttpResponseHeaders, HttpResponseOk, HttpServer, HttpServerStarter, Path,
    Query, RequestContext, TypedBody, endpoint,
};
use http::Response;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::{sync::Mutex, task::JoinHandle};

use crate::{
    clock::{Clock, SystemClock},
    fence::{
        FenceAcquire, FencePrepare, FenceRelease, FenceReleased, FenceResolve, FenceStatus,
        FenceToken, FenceUpload, UploadClaimStatus,
    },
    models::{
        AccessRole, CatalogItem, Generation, Item, ItemsPage, ListQuery, SafetyRequirement,
        SyncReport, SyncRequest, System, TodayRequest, TodayResponse, ValidationReport,
        VariantItems, catalog_items_schema, valid_id, validate_catalog,
    },
    store::{GenerationBasis, Store, StoreError},
    today::TodayConfig,
    tracking::{
        analytics::AnalyticsReport,
        models::{
            CommandResult, CoverageHistoryPage, HistoryPage, IngestResult, ObservationBatch,
            TrackingCommand, TrackingError, TrackingExport, TrackingSnapshot, UnitId,
        },
    },
};

#[derive(Clone)]
pub struct Context {
    pub store: Store,
    owner: [u8; 32],
    integration: Option<[u8; 32]>,
    ingress_peer: Option<IpAddr>,
    today: Option<TodayConfig>,
    tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
    stopping: Arc<AtomicBool>,
    clock: Arc<dyn Clock>,
}
impl Context {
    pub fn new(store: Store, owner: &str) -> Self {
        Self {
            store,
            owner: Sha256::digest(owner).into(),
            integration: None,
            ingress_peer: None,
            today: None,
            tasks: Arc::new(Mutex::new(Vec::new())),
            stopping: Arc::new(AtomicBool::new(false)),
            clock: Arc::new(SystemClock),
        }
    }
    pub fn with_integration(mut self, token: &str) -> Self {
        self.integration = Some(Sha256::digest(token).into());
        self
    }
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }
    pub fn with_today(mut self, config: TodayConfig) -> Self {
        self.today = Some(config);
        self
    }
    pub fn for_ingress(mut self, peer: IpAddr) -> Self {
        self.ingress_peer = Some(peer);
        self
    }
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.stopping.store(true, Ordering::Release);
        let tasks: Vec<_> = self.tasks.lock().await.drain(..).collect();
        // Each worker owns a 60-second deadline. Keep SQLite alive until every
        // accepted generation has persisted its terminal state.
        for task in tasks {
            task.await.context("generation worker interrupted")?;
        }
        Ok(())
    }
    fn timezone(&self) -> &str {
        self.today.as_ref().map_or("UTC", TodayConfig::timezone)
    }
    fn fingerprint(&self) -> &str {
        self.today
            .as_ref()
            .map_or("disabled", TodayConfig::fingerprint)
    }
    fn date(&self) -> String {
        self.today.as_ref().map_or_else(
            || Utc::now().date_naive().to_string(),
            |config| config.local_date(Utc::now()),
        )
    }
}

struct Principal {
    role: AccessRole,
    actor: String,
    admin: bool,
}
fn header<'a>(request: &'a RequestContext<Arc<Context>>, name: &str) -> Option<&'a str> {
    request
        .request
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}
fn fail(status: Status, code: &str) -> HttpError {
    HttpError::for_client_error(Some(code.to_owned()), status, code.to_owned())
}
fn ingress_access(request: &RequestContext<Arc<Context>>) -> Result<(), HttpError> {
    if request
        .context()
        .ingress_peer
        .is_some_and(|peer| request.request.remote_addr().ip() != peer)
    {
        return Err(fail(Status::FORBIDDEN, "ingress_peer_required"));
    }
    Ok(())
}
fn identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && !value.chars().any(|c| c.is_control() || c.is_whitespace())
}
fn authenticate(request: &RequestContext<Arc<Context>>) -> Result<Principal, HttpError> {
    let context = request.context();
    if context.ingress_peer.is_some() {
        ingress_access(request)?;
        if request.request.method() != http::Method::GET
            && header(request, "x-closet-request") != Some("1")
        {
            return Err(fail(Status::FORBIDDEN, "browser_request_header_required"));
        }
        let id = header(request, "x-remote-user-id")
            .filter(|id| identity(id))
            .ok_or_else(|| fail(Status::UNAUTHORIZED, "ha_user_required"))?;
        // Supervisor's panel_admin gate is the authority on this peer-only
        // listener. Forged ingress headers do not work on the native listener.
        return Ok(Principal {
            role: AccessRole::Owner,
            actor: format!("ha:{id}"),
            admin: true,
        });
    }
    let token = header(request, "authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| value.len() <= 4096)
        .ok_or_else(|| fail(Status::UNAUTHORIZED, "authentication_required"))?;
    let digest: [u8; 32] = Sha256::digest(token).into();
    if bool::from(digest.ct_eq(&context.owner)) {
        return Ok(Principal {
            role: AccessRole::Owner,
            actor: "owner".into(),
            admin: true,
        });
    }
    if !context
        .integration
        .as_ref()
        .is_some_and(|expected| bool::from(digest.ct_eq(expected)))
    {
        return Err(fail(Status::UNAUTHORIZED, "invalid_credential"));
    }
    // Only the dedicated trusted-adapter credential may assert HA context.
    // The adapter derives these headers from hass_user, not caller headers.
    let id = header(request, "x-closet-actor-id")
        .filter(|id| id.starts_with("ha:") && id.len() > 3 && identity(id));
    Ok(Principal {
        role: AccessRole::Integration,
        actor: id.unwrap_or("ha:integration").into(),
        admin: id.is_some() && header(request, "x-closet-admin") == Some("true"),
    })
}
fn administrator(request: &RequestContext<Arc<Context>>) -> Result<Principal, HttpError> {
    let principal = authenticate(request)?;
    if !principal.admin {
        return Err(fail(Status::FORBIDDEN, "administrator_required"));
    }
    Ok(principal)
}
fn expected_revision(request: &RequestContext<Arc<Context>>) -> Result<Option<i64>, HttpError> {
    header(request, "if-match")
        .map(|value| {
            value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .and_then(|v| v.parse::<i64>().ok())
                .filter(|v| *v >= 0)
                .ok_or_else(|| fail(Status::BAD_REQUEST, "invalid_if_match"))
        })
        .transpose()
}
fn idempotency(request: &RequestContext<Arc<Context>>) -> Result<&str, HttpError> {
    header(request, "idempotency-key")
        .filter(|key| {
            !key.is_empty() && key.len() <= 200 && key.bytes().all(|byte| byte.is_ascii_graphic())
        })
        .ok_or_else(|| fail(Status::BAD_REQUEST, "idempotency_key_required"))
}
fn store_error(error: StoreError) -> HttpError {
    let status = match error {
        StoreError::InvalidCatalog(_) | StoreError::Invalid(_) => Status::BAD_REQUEST,
        StoreError::NotFound => Status::NOT_FOUND,
        StoreError::Conflict(_) => Status::CONFLICT,
        StoreError::Revision => Status::PRECONDITION_FAILED,
        StoreError::Precondition => Status::PRECONDITION_REQUIRED,
        StoreError::Database(_) | StoreError::Migration(_) | StoreError::Serialization(_) => {
            return HttpError::for_internal_error(error.code().into());
        }
    };
    fail(status, error.code())
}

fn tracking_error(error: TrackingError) -> HttpError {
    let status = match error {
        TrackingError::Invalid { .. } => Status::BAD_REQUEST,
        TrackingError::Conflict(_) => Status::CONFLICT,
        TrackingError::Revision => Status::PRECONDITION_FAILED,
        TrackingError::NotFound => Status::NOT_FOUND,
        TrackingError::Database(_) | TrackingError::Serialization(_) => {
            return HttpError::for_internal_error(error.code().into());
        }
    };
    fail(status, error.code())
}

#[derive(Serialize, JsonSchema)]
struct PrivateHeaders {
    #[serde(rename = "cache-control")]
    cache_control: String,
}
fn private<T: Serialize + JsonSchema + Send + Sync + 'static>(
    value: T,
) -> HttpResponseHeaders<HttpResponseOk<T>, PrivateHeaders> {
    HttpResponseHeaders::new(
        HttpResponseOk(value),
        PrivateHeaders {
            cache_control: "no-store".into(),
        },
    )
}
#[derive(Serialize, JsonSchema)]
struct RevisionHeaders {
    etag: String,
    #[serde(rename = "cache-control")]
    cache_control: String,
}
fn revision_headers(revision: i64) -> RevisionHeaders {
    RevisionHeaders {
        etag: format!("\"{revision}\""),
        cache_control: "no-store".into(),
    }
}
#[derive(Deserialize, JsonSchema)]
struct ItemPath {
    id: String,
}
#[derive(Deserialize, JsonSchema)]
struct VariantPath {
    variant_id: String,
}

/// Readiness only; no private catalog or model configuration is returned.
#[endpoint {method=GET,path="/health",tags=["system"]}]
async fn health(
    request: RequestContext<Arc<Context>>,
) -> Result<HttpResponseOk<&'static str>, HttpError> {
    ingress_access(&request)?;
    request.context().store.ready().await.map_err(store_error)?;
    Ok(HttpResponseOk("ok"))
}
/// Versions and aggregate health for the integration, without item details.
#[endpoint {method=GET,path="/v1/system",tags=["system"]}]
async fn system(
    request: RequestContext<Arc<Context>>,
) -> Result<HttpResponseHeaders<HttpResponseOk<System>, PrivateHeaders>, HttpError> {
    let principal = authenticate(&request)?;
    let context = request.context();
    let (catalog_revision, item_count, archived_count) =
        context.store.counts().await.map_err(store_error)?;
    Ok(private(System {
        version: env!("CARGO_PKG_VERSION").into(),
        role: principal.role,
        catalog_revision,
        item_count,
        archived_count,
        today_configured: context
            .today
            .as_ref()
            .is_some_and(TodayConfig::is_configured),
    }))
}
/// List one revision-bound page of the active JSON projection.
#[endpoint {method=GET,path="/v1/items",tags=["inventory"]}]
async fn items_list(
    request: RequestContext<Arc<Context>>,
    query: Query<ListQuery>,
) -> Result<HttpResponseHeaders<HttpResponseOk<ItemsPage>, RevisionHeaders>, HttpError> {
    authenticate(&request)?;
    let page = request
        .context()
        .store
        .list(query.into_inner())
        .await
        .map_err(store_error)?;
    let headers = revision_headers(page.catalog_revision);
    Ok(HttpResponseHeaders::new(HttpResponseOk(page), headers))
}
/// Read a current or archived record, preserving its stable source identity.
#[endpoint {method=GET,path="/v1/items/{id}",tags=["inventory"]}]
async fn item_get(
    request: RequestContext<Arc<Context>>,
    path: Path<ItemPath>,
) -> Result<HttpResponseHeaders<HttpResponseOk<Item>, RevisionHeaders>, HttpError> {
    authenticate(&request)?;
    let id = path.into_inner().id;
    if !valid_id(&id) {
        return Err(fail(Status::BAD_REQUEST, "invalid_id"));
    }
    let item = request
        .context()
        .store
        .item(&id)
        .await
        .map_err(store_error)?;
    let headers = revision_headers(item.revision);
    Ok(HttpResponseHeaders::new(HttpResponseOk(item), headers))
}
/// Complete active acquisitions of a variant, independent of list filters and pages.
#[endpoint {method=GET,path="/v1/variants/{variant_id}/items",tags=["inventory"]}]
async fn variant_items_get(
    request: RequestContext<Arc<Context>>,
    path: Path<VariantPath>,
) -> Result<HttpResponseHeaders<HttpResponseOk<VariantItems>, RevisionHeaders>, HttpError> {
    authenticate(&request)?;
    let id = path.into_inner().variant_id;
    if !valid_id(&id) {
        return Err(fail(Status::BAD_REQUEST, "invalid_id"));
    }
    let variant = request
        .context()
        .store
        .variant_items(&id)
        .await
        .map_err(store_error)?;
    let headers = revision_headers(variant.catalog_revision);
    Ok(HttpResponseHeaders::new(HttpResponseOk(variant), headers))
}
#[derive(Deserialize, JsonSchema)]
#[serde(transparent)]
struct CatalogImport(#[schemars(schema_with = "catalog_items_schema")] Vec<CatalogItem>);

/// Use the same validator as the offline CLI without changing any state.
#[endpoint {method=POST,path="/v1/inventory/validate",tags=["inventory"],request_body_max_bytes=1048576}]
async fn inventory_validate(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<CatalogImport>,
) -> Result<HttpResponseHeaders<HttpResponseOk<ValidationReport>, PrivateHeaders>, HttpError> {
    authenticate(&request)?;
    let catalog = validate_catalog(body.into_inner().0)
        .map_err(StoreError::from)
        .map_err(store_error)?;
    Ok(private(catalog.report().clone()))
}
/// Preview or atomically reconcile a complete authoritative JSON catalog.
#[endpoint {method=POST,path="/v1/inventory/sync",tags=["inventory"],request_body_max_bytes=2097152}]
async fn inventory_sync(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<SyncRequest>,
) -> Result<HttpResponseHeaders<HttpResponseOk<SyncReport>, RevisionHeaders>, HttpError> {
    let principal = administrator(&request)?;
    let input = body.into_inner();
    let expected = expected_revision(&request)?;
    let key = if input.dry_run {
        None
    } else {
        Some(idempotency(&request)?)
    };
    let fence = header(&request, "x-closet-sync-fence")
        .map(|value| FenceToken::try_from(value.to_owned()))
        .transpose()
        .map_err(|_| fail(Status::BAD_REQUEST, "invalid_fence_token"))?;
    let report = request
        .context()
        .store
        .sync_fenced(input, expected, &principal.actor, key, fence.as_ref())
        .await
        .map_err(store_error)?;
    let headers = revision_headers(report.catalog_revision);
    Ok(HttpResponseHeaders::new(HttpResponseOk(report), headers))
}
/// Serialize authoritative writers without holding a transaction over native requests.
#[endpoint {method=POST,path="/v1/inventory/fence",tags=["inventory"],request_body_max_bytes=2048}]
async fn fence_acquire(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<FenceAcquire>,
) -> Result<HttpResponseHeaders<HttpResponseOk<FenceStatus>, PrivateHeaders>, HttpError> {
    let principal = administrator(&request)?;
    Ok(private(
        request
            .context()
            .store
            .acquire_fence(body.into_inner(), &principal.actor)
            .await
            .map_err(store_error)?,
    ))
}
/// Admit only unreferenced native IDs; persist the exact batch before deleting.
#[endpoint {method=POST,path="/v1/inventory/fence/prepare",tags=["inventory"],request_body_max_bytes=2097152}]
async fn fence_prepare(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<FencePrepare>,
) -> Result<HttpResponseHeaders<HttpResponseOk<FenceStatus>, PrivateHeaders>, HttpError> {
    let principal = administrator(&request)?;
    let context = request.context();
    Ok(private(
        context
            .store
            .prepare_prune(body.into_inner(), &principal.actor, &context.date())
            .await
            .map_err(store_error)?,
    ))
}
/// The client resolves an admitted ID only after authoritative native absence.
#[endpoint {method=POST,path="/v1/inventory/fence/resolve",tags=["inventory"],request_body_max_bytes=2048}]
async fn fence_resolve(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<FenceResolve>,
) -> Result<HttpResponseHeaders<HttpResponseOk<FenceStatus>, PrivateHeaders>, HttpError> {
    let principal = administrator(&request)?;
    Ok(private(
        request
            .context()
            .store
            .resolve_pruned(body.into_inner(), &principal.actor)
            .await
            .map_err(store_error)?,
    ))
}
/// Durable upload claims survive ephemeral CI runners and lost POST replies.
#[endpoint {method=POST,path="/v1/inventory/fence/uploads",tags=["inventory"],request_body_max_bytes=2048}]
async fn fence_upload(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<FenceUpload>,
) -> Result<HttpResponseHeaders<HttpResponseOk<UploadClaimStatus>, PrivateHeaders>, HttpError> {
    let principal = administrator(&request)?;
    Ok(private(
        request
            .context()
            .store
            .upload_claim(body.into_inner(), &principal.actor)
            .await
            .map_err(store_error)?,
    ))
}
/// A pending deletion cannot be released by a timeout or a different writer.
#[endpoint {method=DELETE,path="/v1/inventory/fence",tags=["inventory"],request_body_max_bytes=2048}]
async fn fence_release(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<FenceRelease>,
) -> Result<HttpResponseHeaders<HttpResponseOk<FenceReleased>, PrivateHeaders>, HttpError> {
    let principal = administrator(&request)?;
    request
        .context()
        .store
        .release_fence(&body.into_inner().token, &principal.actor)
        .await
        .map_err(store_error)?;
    Ok(private(FenceReleased { released: true }))
}
/// Read Today's saved result and latest attempt; never calls the model.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TodayQuery {
    footwear_requirement: Option<SafetyRequirement>,
}
#[endpoint {method=GET,path="/v1/today",tags=["today"]}]
async fn today_get(
    request: RequestContext<Arc<Context>>,
    query: Query<TodayQuery>,
) -> Result<HttpResponseHeaders<HttpResponseOk<TodayResponse>, PrivateHeaders>, HttpError> {
    authenticate(&request)?;
    let context = request.context();
    Ok(private(
        context
            .store
            .today(
                &context.date(),
                context.timezone(),
                context.fingerprint(),
                Utc::now(),
                query.into_inner().footwear_requirement,
            )
            .await
            .map_err(store_error)?,
    ))
}
#[derive(Serialize, JsonSchema)]
struct AcceptedHeaders {
    location: String,
    #[serde(rename = "cache-control")]
    cache_control: String,
}
/// Reserve one optional outfit attempt before contacting the configured AI Task.
#[endpoint {method=POST,path="/v1/today/generate",tags=["today"]}]
async fn today_generate(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<TodayRequest>,
) -> Result<HttpResponseHeaders<HttpResponseAccepted<Generation>, AcceptedHeaders>, HttpError> {
    let principal = administrator(&request)?;
    let key = idempotency(&request)?;
    let context = request.context();
    let mut tasks = context.tasks.lock().await;
    if context.stopping.load(Ordering::Acquire) {
        return Err(HttpError::for_unavail(
            Some("shutting_down".into()),
            "shutting_down".into(),
        ));
    }
    let availability = context
        .store
        .tracking_availability(context.clock.now())
        .await
        .map_err(tracking_error)?;
    let items = context
        .store
        .active_items()
        .await
        .map_err(store_error)?
        .into_iter()
        .filter(|item| {
            availability
                .eligible_catalog_ids
                .binary_search(&item.id)
                .is_ok()
        })
        .collect();
    let basis = if let Some(config) = &context.today {
        config.basis(items)
    } else {
        GenerationBasis {
            date: context.date(),
            timezone: context.timezone().into(),
            config_hash: context.fingerprint().into(),
            items,
        }
    };
    let reservation = context
        .store
        .reserve_generation(body.into_inner(), basis.clone(), &principal.actor, key)
        .await
        .map_err(store_error)?;
    if reservation.fresh {
        tasks.retain(|task| !task.is_finished());
        let store = context.store.clone();
        let config = context.today.clone();
        let generation = reservation.generation.clone();
        tasks.push(tokio::spawn(async move {
            if let Some(config) = config {
                crate::today::run(store, config, generation, basis).await;
            } else {
                let _ = store
                    .finish_generation(&generation.id, Err("today_unconfigured"))
                    .await;
            }
        }));
    }
    let location = reservation
        .generation
        .inputs
        .footwear_requirement
        .map_or_else(
            || "/v1/today".to_owned(),
            |requirement| format!("/v1/today?footwear_requirement={requirement}"),
        );
    Ok(HttpResponseHeaders::new(
        HttpResponseAccepted(reservation.generation),
        AcceptedHeaders {
            location,
            cache_control: "no-store".into(),
        },
    ))
}

/// Physical-unit state is separate from authoritative garment metadata.
#[endpoint {method=GET,path="/v1/tracking",tags=["tracking"]}]
async fn tracking_snapshot(
    request: RequestContext<Arc<Context>>,
) -> Result<HttpResponseHeaders<HttpResponseOk<TrackingSnapshot>, RevisionHeaders>, HttpError> {
    let principal = authenticate(&request)?;
    let context = request.context();
    let mut snapshot = context
        .store
        .tracking_snapshot_clocked(|| context.clock.now())
        .await
        .map_err(tracking_error)?;
    // HA's ordinary coordinator needs status, not raw radio identifiers or
    // pending enrollment candidates. Only trusted administrators inspect those.
    if !principal.admin {
        snapshot.sessions.clear();
        snapshot.diagnostics.recent_reads.clear();
    }
    let headers = revision_headers(snapshot.tracking_revision);
    Ok(HttpResponseHeaders::new(HttpResponseOk(snapshot), headers))
}

#[derive(Serialize, JsonSchema)]
struct ProjectedUnit {
    id: UnitId,
    ordinal: u32,
    status: crate::tracking::models::UnitStatus,
    paused: bool,
    retired: bool,
    archived: bool,
    evidence: crate::tracking::models::StateEvidence,
}

/// A small HA projection, not the enrollment workspace or a second state engine.
#[derive(Serialize, JsonSchema)]
struct TrackingProjection {
    schema_version: u16,
    tracking_revision: i64,
    config: crate::tracking::models::TrackingConfig,
    health: crate::tracking::models::TrackingHealth,
    counts: crate::tracking::models::TrackingCounts,
    certified_ready_unit_ids: Vec<UnitId>,
    units: Vec<ProjectedUnit>,
    next_page: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProjectionQuery {
    limit: Option<u16>,
    page_token: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectionCursor {
    revision: i64,
    after: UnitId,
}

#[endpoint {method=GET,path="/v1/tracking/projection",tags=["tracking"]}]
async fn tracking_projection(
    request: RequestContext<Arc<Context>>,
    query: Query<ProjectionQuery>,
) -> Result<HttpResponseHeaders<HttpResponseOk<TrackingProjection>, RevisionHeaders>, HttpError> {
    authenticate(&request)?;
    let query = query.into_inner();
    let limit = query.limit.unwrap_or(500);
    if !(1..=1000).contains(&limit) {
        return Err(fail(Status::BAD_REQUEST, "invalid_limit"));
    }
    let cursor: Option<ProjectionCursor> = query
        .page_token
        .map(|token| {
            if token.len() > 512 {
                return Err(fail(Status::BAD_REQUEST, "invalid_page_token"));
            }
            let bytes = URL_SAFE_NO_PAD
                .decode(token)
                .map_err(|_| fail(Status::BAD_REQUEST, "invalid_page_token"))?;
            serde_json::from_slice(&bytes)
                .map_err(|_| fail(Status::BAD_REQUEST, "invalid_page_token"))
        })
        .transpose()?;
    let context = request.context();
    let snapshot = context
        .store
        .tracking_snapshot_clocked(|| context.clock.now())
        .await
        .map_err(tracking_error)?;
    if cursor
        .as_ref()
        .is_some_and(|cursor| cursor.revision != snapshot.tracking_revision)
    {
        return Err(fail(Status::CONFLICT, "page_invalidated"));
    }
    if cursor.as_ref().is_some_and(|cursor| {
        !snapshot
            .units
            .iter()
            .any(|unit| unit.id == cursor.after && !unit.archived && !unit.retired)
    }) {
        return Err(fail(Status::BAD_REQUEST, "invalid_page_token"));
    }
    let headers = revision_headers(snapshot.tracking_revision);
    let mut units: Vec<_> = snapshot
        .units
        .into_iter()
        .filter(|unit| {
            !unit.archived
                && !unit.retired
                && cursor.as_ref().is_none_or(|cursor| unit.id > cursor.after)
        })
        .take(usize::from(limit) + 1)
        .collect();
    let more = units.len() > usize::from(limit);
    units.truncate(usize::from(limit));
    let next_page = if more {
        units
            .last()
            .map(|unit| {
                serde_json::to_vec(&ProjectionCursor {
                    revision: snapshot.tracking_revision,
                    after: unit.id,
                })
                .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
            })
            .transpose()
            .map_err(|_| HttpError::for_internal_error("cursor_unavailable".into()))?
    } else {
        None
    };
    let page_ids: std::collections::BTreeSet<_> = units.iter().map(|unit| unit.id).collect();
    let projection = TrackingProjection {
        schema_version: snapshot.schema_version,
        tracking_revision: snapshot.tracking_revision,
        config: snapshot.config,
        health: snapshot.health,
        counts: snapshot.counts,
        certified_ready_unit_ids: snapshot
            .certified_ready_unit_ids
            .into_iter()
            .filter(|id| page_ids.contains(id))
            .collect(),
        units: units
            .into_iter()
            .map(|unit| ProjectedUnit {
                id: unit.id,
                ordinal: unit.ordinal,
                status: unit.status,
                paused: unit.paused,
                retired: unit.retired,
                archived: unit.archived,
                evidence: unit.evidence,
            })
            .collect(),
        next_page,
    };
    Ok(HttpResponseHeaders::new(
        HttpResponseOk(projection),
        headers,
    ))
}

/// Every administrative tracking operation uses its own revision and replay key.
#[endpoint {method=POST,path="/v1/tracking/commands",tags=["tracking"],request_body_max_bytes=8388608}]
async fn tracking_command(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<TrackingCommand>,
) -> Result<HttpResponseHeaders<HttpResponseOk<CommandResult>, RevisionHeaders>, HttpError> {
    let principal = administrator(&request)?;
    let revision = expected_revision(&request)?
        .ok_or_else(|| fail(Status::PRECONDITION_REQUIRED, "tracking_revision_required"))?;
    let key = idempotency(&request)?;
    let context = request.context();
    if context.stopping.load(Ordering::Acquire) {
        return Err(HttpError::for_unavail(None, "service_stopping".into()));
    }
    let result = context
        .store
        .tracking_command_clocked(&principal.actor, key, revision, body.into_inner(), || {
            context.clock.now()
        })
        .await
        .map_err(tracking_error)?;
    let headers = revision_headers(result.tracking_revision);
    Ok(HttpResponseHeaders::new(HttpResponseOk(result), headers))
}

/// Reader sessions and sequences deduplicate bounded normalized evidence.
/// Receipt time is supplied by the server, never trusted from an HTTP caller.
#[endpoint {method=POST,path="/v1/tracking/observations",tags=["tracking"],request_body_max_bytes=1048576}]
async fn tracking_observations(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<ObservationBatch>,
) -> Result<HttpResponseHeaders<HttpResponseOk<IngestResult>, RevisionHeaders>, HttpError> {
    administrator(&request)?;
    let context = request.context();
    let result = context
        .store
        .tracking_ingest_clocked(body.into_inner(), || context.clock.now())
        .await
        .map_err(tracking_error)?;
    let headers = revision_headers(result.tracking_revision);
    Ok(HttpResponseHeaders::new(HttpResponseOk(result), headers))
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    before: Option<i64>,
    limit: Option<u16>,
}

#[derive(Deserialize, JsonSchema)]
struct UnitPath {
    id: UnitId,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AnalyticsQuery {
    days: Option<u16>,
    pick_seed: Option<u64>,
}

/// Observed wear and eligible overlooked pieces, derived from retained evidence.
#[endpoint {method=GET,path="/v1/tracking/analytics",tags=["tracking"]}]
async fn tracking_analytics(
    request: RequestContext<Arc<Context>>,
    query: Query<AnalyticsQuery>,
) -> Result<HttpResponseHeaders<HttpResponseOk<AnalyticsReport>, PrivateHeaders>, HttpError> {
    administrator(&request)?;
    let query = query.into_inner();
    let context = request.context();
    let timezone = context
        .timezone()
        .parse()
        .map_err(|_| HttpError::for_internal_error("invalid_timezone".into()))?;
    let report = context
        .store
        .tracking_analytics_clocked(
            query.days.unwrap_or(90),
            query.pick_seed.unwrap_or(0),
            timezone,
            || context.clock.now(),
        )
        .await
        .map_err(tracking_error)?;
    Ok(private(report))
}

/// Verified coverage intervals distinguish observed time from tracking gaps.
#[endpoint {method=GET,path="/v1/tracking/coverage",tags=["tracking"]}]
async fn tracking_coverage(
    request: RequestContext<Arc<Context>>,
    query: Query<HistoryQuery>,
) -> Result<HttpResponseHeaders<HttpResponseOk<CoverageHistoryPage>, PrivateHeaders>, HttpError> {
    administrator(&request)?;
    let query = query.into_inner();
    let coverage = request
        .context()
        .store
        .tracking_coverage(query.before, query.limit.unwrap_or(50))
        .await
        .map_err(tracking_error)?;
    Ok(private(coverage))
}

/// Audit history includes bindings; routine HA entities do not expose EPCs.
#[endpoint {method=GET,path="/v1/tracking/history",tags=["tracking"]}]
async fn tracking_history(
    request: RequestContext<Arc<Context>>,
    query: Query<HistoryQuery>,
) -> Result<HttpResponseHeaders<HttpResponseOk<HistoryPage>, PrivateHeaders>, HttpError> {
    administrator(&request)?;
    let query = query.into_inner();
    let history = request
        .context()
        .store
        .tracking_history(None, query.before, query.limit.unwrap_or(50))
        .await
        .map_err(tracking_error)?;
    Ok(private(history))
}

#[endpoint {method=GET,path="/v1/tracking/units/{id}/history",tags=["tracking"]}]
async fn tracking_unit_history(
    request: RequestContext<Arc<Context>>,
    path: Path<UnitPath>,
    query: Query<HistoryQuery>,
) -> Result<HttpResponseHeaders<HttpResponseOk<HistoryPage>, PrivateHeaders>, HttpError> {
    administrator(&request)?;
    let query = query.into_inner();
    let history = request
        .context()
        .store
        .tracking_history(
            Some(path.into_inner().id),
            query.before,
            query.limit.unwrap_or(50),
        )
        .await
        .map_err(tracking_error)?;
    Ok(private(history))
}

/// Private versioned operational export. Catalog metadata is not copied here.
#[endpoint {method=GET,path="/v1/tracking/export",tags=["tracking"]}]
async fn tracking_export(
    request: RequestContext<Arc<Context>>,
) -> Result<HttpResponseHeaders<HttpResponseOk<TrackingExport>, PrivateHeaders>, HttpError> {
    administrator(&request)?;
    let context = request.context();
    Ok(private(
        context
            .store
            .tracking_export_clocked(|| context.clock.now())
            .await
            .map_err(tracking_error)?,
    ))
}

fn static_response(
    content: String,
    content_type: &'static str,
) -> Result<Response<Body>, HttpError> {
    Response::builder().header("content-type",content_type).header("cache-control","no-store")
        .header("content-security-policy","default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' https: data:; connect-src 'self'; base-uri 'self'; form-action 'self'; object-src 'none'; frame-ancestors 'self'")
        .header("x-content-type-options","nosniff").header("referrer-policy","no-referrer")
        .body(Body::from(content)).map_err(|_|HttpError::for_internal_error("response_unavailable".into()))
}
fn dashboard(request: &RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    ingress_access(request)?;
    let base = if request.context().ingress_peer.is_some() {
        header(request, "x-ingress-path").unwrap_or("")
    } else {
        ""
    };
    if !base.is_empty()
        && (!base.starts_with('/')
            || base.starts_with("//")
            || base.len() > 2048
            || !base
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-')))
    {
        return Err(fail(Status::BAD_REQUEST, "invalid_ingress_prefix"));
    }
    static_response(
        include_str!("../web/index.html").replace(
            "__CLOSET_BASE__",
            &format!("{}/", base.trim_end_matches('/')),
        ),
        "text/html; charset=utf-8",
    )
}
#[endpoint {method=GET,path="/",tags=["web"]}]
async fn index(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    dashboard(&request)
}
#[endpoint {method=GET,path="/items/{id}",tags=["web"]}]
async fn detail(
    request: RequestContext<Arc<Context>>,
    _path: Path<ItemPath>,
) -> Result<Response<Body>, HttpError> {
    dashboard(&request)
}
#[endpoint {method=GET,path="/app.js",tags=["web"]}]
async fn script(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    ingress_access(&request)?;
    static_response(
        include_str!("../web/app.js").into(),
        "text/javascript; charset=utf-8",
    )
}
#[endpoint {method=GET,path="/style.css",tags=["web"]}]
async fn style(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    ingress_access(&request)?;
    static_response(
        include_str!("../web/style.css").into(),
        "text/css; charset=utf-8",
    )
}
#[endpoint {method=GET,path="/tracking.js",tags=["web"]}]
async fn tracking_script(
    request: RequestContext<Arc<Context>>,
) -> Result<Response<Body>, HttpError> {
    ingress_access(&request)?;
    static_response(
        include_str!("../web/tracking.js").into(),
        "text/javascript; charset=utf-8",
    )
}
#[endpoint {method=GET,path="/analytics.js",tags=["web"]}]
async fn analytics_script(
    request: RequestContext<Arc<Context>>,
) -> Result<Response<Body>, HttpError> {
    ingress_access(&request)?;
    static_response(
        include_str!("../web/analytics.js").into(),
        "text/javascript; charset=utf-8",
    )
}
#[endpoint {method=GET,path="/brand.svg",tags=["web"]}]
async fn brand(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    ingress_access(&request)?;
    static_response(include_str!("../web/brand.svg").into(), "image/svg+xml")
}
#[endpoint {method=GET,path="/brand-dark.svg",tags=["web"]}]
async fn brand_dark(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    ingress_access(&request)?;
    static_response(
        include_str!("../web/brand-dark.svg").into(),
        "image/svg+xml",
    )
}
#[endpoint {method=GET,path="/openapi.json",tags=["system"]}]
async fn schema(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    ingress_access(&request)?;
    static_response(
        serde_json::to_string(
            &openapi().map_err(|_| HttpError::for_internal_error("schema_unavailable".into()))?,
        )
        .map_err(|_| HttpError::for_internal_error("schema_unavailable".into()))?,
        "application/json",
    )
}

pub fn description() -> anyhow::Result<ApiDescription<Arc<Context>>> {
    let mut api = ApiDescription::new();
    let endpoints: [dropshot::ApiEndpoint<Arc<Context>>; 32] = [
        health.into(),
        system.into(),
        items_list.into(),
        item_get.into(),
        variant_items_get.into(),
        inventory_validate.into(),
        inventory_sync.into(),
        fence_acquire.into(),
        fence_prepare.into(),
        fence_resolve.into(),
        fence_release.into(),
        fence_upload.into(),
        today_get.into(),
        today_generate.into(),
        tracking_snapshot.into(),
        tracking_projection.into(),
        tracking_command.into(),
        tracking_observations.into(),
        tracking_coverage.into(),
        tracking_analytics.into(),
        tracking_history.into(),
        tracking_unit_history.into(),
        tracking_export.into(),
        index.into(),
        detail.into(),
        script.into(),
        tracking_script.into(),
        analytics_script.into(),
        style.into(),
        brand.into(),
        brand_dark.into(),
        schema.into(),
    ];
    for endpoint in endpoints {
        api.register(endpoint).context("register API endpoint")?;
    }
    Ok(api)
}
pub fn openapi() -> anyhow::Result<serde_json::Value> {
    Ok(description()?.openapi("Closet API",semver::Version::parse(env!("CARGO_PKG_VERSION"))?).description("Inventory validation and atomic sync, read-only catalog browsing, and optional on-demand outfit suggestions. Native owner or trusted HA administrator mutations require explicit idempotency/precondition headers. GET never generates model work.").json()?)
}
pub fn start(bind: SocketAddr, context: Arc<Context>) -> anyhow::Result<HttpServer<Arc<Context>>> {
    start_with_logger(bind, context, slog::Logger::root(slog::Discard, slog::o!()))
}
pub fn start_with_logger(
    bind: SocketAddr,
    context: Arc<Context>,
    logger: slog::Logger,
) -> anyhow::Result<HttpServer<Arc<Context>>> {
    let config = ConfigDropshot {
        bind_address: bind,
        ..Default::default()
    };
    Ok(
        HttpServerStarter::new(&config, description()?, context, &logger)
            .map_err(|_| anyhow::anyhow!("start API listener"))?
            .start(),
    )
}
