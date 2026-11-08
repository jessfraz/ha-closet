//! The authenticated app, CLI and Home Assistant adapter share this API.

use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::Context as _;
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
    models::{
        AccessRole, CatalogItem, Generation, Item, ItemsPage, ListQuery, SyncReport, SyncRequest,
        System, TodayRequest, TodayResponse, ValidationReport, valid_id, validate_catalog,
    },
    store::{GenerationBasis, Store, StoreError},
    today::TodayConfig,
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
        }
    }
    pub fn with_integration(mut self, token: &str) -> Self {
        self.integration = Some(Sha256::digest(token).into());
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
/// Use the same validator as the offline CLI without changing any state.
#[endpoint {method=POST,path="/v1/inventory/validate",tags=["inventory"],request_body_max_bytes=1048576}]
async fn inventory_validate(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<Vec<CatalogItem>>,
) -> Result<HttpResponseHeaders<HttpResponseOk<ValidationReport>, PrivateHeaders>, HttpError> {
    authenticate(&request)?;
    let catalog = validate_catalog(body.into_inner())
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
    let report = request
        .context()
        .store
        .sync(input, expected, &principal.actor, key)
        .await
        .map_err(store_error)?;
    let headers = revision_headers(report.catalog_revision);
    Ok(HttpResponseHeaders::new(HttpResponseOk(report), headers))
}
/// Read Today's saved result and latest attempt; never calls the model.
#[endpoint {method=GET,path="/v1/today",tags=["today"]}]
async fn today_get(
    request: RequestContext<Arc<Context>>,
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
    let items = context.store.active_items().await.map_err(store_error)?;
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
    Ok(HttpResponseHeaders::new(
        HttpResponseAccepted(reservation.generation),
        AcceptedHeaders {
            location: "/v1/today".into(),
            cache_control: "no-store".into(),
        },
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
    let endpoints: [dropshot::ApiEndpoint<Arc<Context>>; 15] = [
        health.into(),
        system.into(),
        items_list.into(),
        item_get.into(),
        inventory_validate.into(),
        inventory_sync.into(),
        today_get.into(),
        today_generate.into(),
        index.into(),
        detail.into(),
        script.into(),
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
    Ok(description()?.openapi("Closet API",semver::Version::parse(env!("CARGO_PKG_VERSION"))?).description("JSON-authoritative inventory validation and atomic sync, read-only catalog browsing, and optional on-demand outfit suggestions. Native owner or trusted HA administrator mutations require explicit idempotency/precondition headers. GET never generates model work.").json()?)
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
