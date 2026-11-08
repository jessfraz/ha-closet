//! Real SQLite, HTTP multipart, WebSocket and executable consumers. All data is fictional.
use base64::Engine;
use closet::{
    api,
    client::{ApiClient, Transport},
    ha_images::NativeImage,
    images::{ImageManifest, ImageSha, LocalImages, ManifestImage, NativeImageId},
    models::{CatalogItem, SyncRequest, parse_catalog},
    store::Store,
    sync::{SourceSnapshot, SyncOptions, inventory_sync},
};
use dropshot::{
    Body, MultipartBody, Path, RequestContext, UntypedBody, WebsocketConnection, channel, endpoint,
};
use futures_util::{SinkExt, StreamExt};
use http::Response;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, Notify};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, protocol::Role},
};
fn pem(label: &str, der: &[u8]) -> Vec<u8> {
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        base64::engine::general_purpose::STANDARD.encode(der)
    )
    .into_bytes()
}
const TOKEN: &str = "example-owner-credential-for-unified-sync-tests";
fn png() -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=").unwrap()
}
fn catalog() -> Vec<CatalogItem> {
    parse_catalog(include_bytes!("fixtures/catalog-crop.json"))
        .unwrap()
        .into_items()
}
#[derive(Clone)]
struct Stored {
    record: NativeImage,
    bytes: Vec<u8>,
}
#[derive(Default)]
struct NativeState {
    images: BTreeMap<NativeImageId, Stored>,
    posts: usize,
    deletes: Vec<NativeImageId>,
    sequence: usize,
}
struct Protocol {
    backend: std::sync::RwLock<String>,
    state: Mutex<NativeState>,
    reject_upload: AtomicBool,
    lose_upload_reply: AtomicBool,
    drop_upload: AtomicBool,
    fail_sync: AtomicBool,
    fail_delete: AtomicBool,
    hold_delete: AtomicBool,
    delete_entered: Notify,
    delete_release: Notify,
    connections: AtomicUsize,
}
#[derive(Deserialize, JsonSchema)]
struct Tail {
    path: Vec<String>,
}
#[derive(Deserialize, JsonSchema)]
struct ImagePath {
    id: String,
}
fn raw(status: u16, bytes: Vec<u8>, mime: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", mime)
        .body(bytes.into())
        .unwrap()
}
async fn bridge(
    ctx: RequestContext<Arc<Protocol>>,
    path: Path<Tail>,
    body: Option<UntypedBody>,
) -> Result<Response<Body>, dropshot::HttpError> {
    let path = path.into_inner().path.join("/");
    let protocol = ctx.context();
    if path == "inventory/sync"
        && ctx.request.method() == http::Method::POST
        && protocol.fail_sync.load(Ordering::SeqCst)
    {
        let request: SyncRequest =
            serde_json::from_slice(body.as_ref().unwrap().as_bytes()).unwrap();
        if !request.dry_run {
            return Ok(raw(
                412,
                b"{\"error_code\":\"revision_conflict\"}".to_vec(),
                "application/json",
            ));
        }
    }
    let query = ctx
        .request
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let url = format!(
        "{}/v1/{path}{query}",
        protocol.backend.read().unwrap().clone()
    );
    let mut request = reqwest::Client::new()
        .request(ctx.request.method().clone(), url)
        .bearer_auth(TOKEN);
    for name in ["if-match", "idempotency-key", "x-closet-sync-fence"] {
        if let Some(value) = ctx.request.headers().get(name) {
            request = request.header(name, value.clone());
        }
    }
    if let Some(body) = body {
        request = request
            .header("content-type", "application/json")
            .body(body.as_bytes().to_vec());
    }
    let upstream = request.send().await.unwrap();
    let status = upstream.status().as_u16();
    let etag = upstream.headers().get("etag").cloned();
    let bytes = upstream.bytes().await.unwrap();
    let mut response = raw(status, bytes.to_vec(), "application/json");
    if let Some(etag) = etag {
        response.headers_mut().insert("etag", etag);
    }
    Ok(response)
}
#[endpoint{method=GET,path="/api/closet/v1/{path:.*}",unpublished=true}]
async fn bridge_get(
    ctx: RequestContext<Arc<Protocol>>,
    path: Path<Tail>,
) -> Result<Response<Body>, dropshot::HttpError> {
    bridge(ctx, path, None).await
}
#[endpoint{method=POST,path="/api/closet/v1/{path:.*}",unpublished=true,request_body_max_bytes=20971520}]
async fn bridge_post(
    ctx: RequestContext<Arc<Protocol>>,
    path: Path<Tail>,
    body: UntypedBody,
) -> Result<Response<Body>, dropshot::HttpError> {
    bridge(ctx, path, Some(body)).await
}
#[endpoint{method=DELETE,path="/api/closet/v1/{path:.*}",unpublished=true}]
async fn bridge_delete(
    ctx: RequestContext<Arc<Protocol>>,
    path: Path<Tail>,
    body: UntypedBody,
) -> Result<Response<Body>, dropshot::HttpError> {
    bridge(ctx, path, Some(body)).await
}
#[endpoint{method=POST,path="/api/image/upload",request_body_max_bytes=10485760,content_type="multipart/form-data"}]
async fn upload(
    ctx: RequestContext<Arc<Protocol>>,
    mut body: MultipartBody,
) -> Result<Response<Body>, dropshot::HttpError> {
    assert_eq!(
        ctx.request.headers()["authorization"],
        format!("Bearer {TOKEN}")
    );
    let field = body.content.next_field().await.unwrap().unwrap();
    assert_eq!(field.name(), Some("file"));
    assert_eq!(
        field.content_type().map(ToString::to_string).as_deref(),
        Some("image/png")
    );
    let name = field.file_name().unwrap().to_owned();
    let bytes = field.bytes().await.unwrap().to_vec();
    assert!(body.content.next_field().await.unwrap().is_none());
    let protocol = ctx.context();
    let mut state = protocol.state.lock().await;
    state.posts += 1;
    if protocol.reject_upload.load(Ordering::SeqCst) {
        return Ok(raw(401, Vec::new(), "application/json"));
    }
    assert_eq!(
        ImageSha::from_upload_name(&name),
        Some(ImageSha::digest(&bytes))
    );
    if !protocol.drop_upload.load(Ordering::SeqCst) {
        state.sequence += 1;
        let id = NativeImageId::try_from(format!("{:032x}", state.sequence + 1000)).unwrap();
        let record = NativeImage {
            id: id.clone(),
            name,
            content_type: "image/png".into(),
            filesize: bytes.len() as u64,
        };
        state.images.insert(id, Stored { record, bytes });
    }
    if protocol.lose_upload_reply.load(Ordering::SeqCst) {
        return Ok(raw(503, Vec::new(), "application/json"));
    }
    Ok(raw(200, b"{}".to_vec(), "application/json"))
}
#[endpoint{method=GET,path="/api/image/serve/{id}/original"}]
async fn original(
    ctx: RequestContext<Arc<Protocol>>,
    path: Path<ImagePath>,
) -> Result<Response<Body>, dropshot::HttpError> {
    assert!(!ctx.request.headers().contains_key("authorization"));
    let id = NativeImageId::try_from(path.into_inner().id).unwrap();
    let state = ctx.context().state.lock().await;
    Ok(match state.images.get(&id) {
        Some(image) => raw(200, image.bytes.clone(), "image/png"),
        None => raw(404, Vec::new(), "image/png"),
    })
}
#[derive(Deserialize)]
#[serde(tag = "type")]
enum NativeCommand {
    #[serde(rename = "auth")]
    Auth { access_token: String },
    #[serde(rename = "image/list")]
    List { id: u64 },
    #[serde(rename = "image/delete")]
    Delete { id: u64, image_id: NativeImageId },
}
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum NativeReply<T> {
    AuthRequired { ha_version: &'static str },
    AuthOk {},
    Result { id: u64, success: bool, result: T },
}
async fn ws_send<T: Serialize>(
    ws: &mut WebSocketStream<dropshot::WebsocketConnectionRaw>,
    value: NativeReply<T>,
) {
    ws.send(Message::Text(serde_json::to_string(&value).unwrap().into()))
        .await
        .unwrap();
}
struct Connection(Arc<Protocol>);
impl Drop for Connection {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::SeqCst);
    }
}
#[channel{protocol=WEBSOCKETS,path="/api/websocket"}]
async fn socket(
    ctx: RequestContext<Arc<Protocol>>,
    upgraded: WebsocketConnection,
) -> dropshot::WebsocketChannelResult {
    let protocol = ctx.context().clone();
    protocol.connections.fetch_add(1, Ordering::SeqCst);
    let _guard = Connection(protocol.clone());
    let mut ws = WebSocketStream::from_raw_socket(upgraded.into_inner(), Role::Server, None).await;
    ws_send(
        &mut ws,
        NativeReply::<()>::AuthRequired {
            ha_version: "2026.11.0",
        },
    )
    .await;
    let auth = ws.next().await.unwrap().unwrap();
    let NativeCommand::Auth { access_token } =
        serde_json::from_str(auth.to_text().unwrap()).unwrap()
    else {
        panic!("auth expected")
    };
    assert_eq!(access_token, TOKEN);
    ws_send(&mut ws, NativeReply::<()>::AuthOk {}).await;
    let message = ws.next().await.unwrap().unwrap();
    let command: NativeCommand = serde_json::from_str(message.to_text().unwrap()).unwrap();
    match command {
        NativeCommand::List { id } => {
            let records: Vec<_> = protocol
                .state
                .lock()
                .await
                .images
                .values()
                .map(|image| image.record.clone())
                .collect();
            ws_send(
                &mut ws,
                NativeReply::Result {
                    id,
                    success: true,
                    result: records,
                },
            )
            .await;
        }
        NativeCommand::Delete { id, image_id } => {
            if protocol.hold_delete.load(Ordering::SeqCst) {
                protocol.delete_entered.notify_one();
                protocol.delete_release.notified().await;
            }
            let mut state = protocol.state.lock().await;
            if !protocol.fail_delete.load(Ordering::SeqCst) {
                state.images.remove(&image_id);
            }
            state.deletes.push(image_id);
            drop(state);
            ws_send(
                &mut ws,
                NativeReply::Result {
                    id,
                    success: true,
                    result: (),
                },
            )
            .await;
        }
        NativeCommand::Auth { .. } => panic!("unexpected auth"),
    }
    while let Some(Ok(message)) = ws.next().await {
        if matches!(message, Message::Close(_)) {
            break;
        }
    }
    Ok(())
}
fn native_api() -> dropshot::ApiDescription<Arc<Protocol>> {
    let mut description = dropshot::ApiDescription::new();
    description.register(bridge_get).unwrap();
    description.register(bridge_post).unwrap();
    description.register(bridge_delete).unwrap();
    description.register(upload).unwrap();
    description.register(original).unwrap();
    description.register(socket).unwrap();
    description
}
struct Fixture {
    directory: tempfile::TempDir,
    store: Store,
    context: Arc<api::Context>,
    backend: dropshot::HttpServer<Arc<api::Context>>,
    native: dropshot::HttpServer<Arc<Protocol>>,
    protocol: Arc<Protocol>,
    api: ApiClient,
}
impl Fixture {
    async fn new() -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default(); // Dropshot's test TLS listener requires its documented process default. Production clients use explicit context.
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("closet.sqlite3"))
            .await
            .unwrap();
        let context = Arc::new(api::Context::new(store.clone(), TOKEN));
        let backend = api::start(
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            context.clone(),
        )
        .unwrap();
        let protocol = Arc::new(Protocol {
            backend: std::sync::RwLock::new(format!("http://{}", backend.local_addr())),
            state: Mutex::new(NativeState::default()),
            reject_upload: AtomicBool::new(false),
            lose_upload_reply: AtomicBool::new(false),
            drop_upload: AtomicBool::new(false),
            fail_sync: AtomicBool::new(false),
            fail_delete: AtomicBool::new(false),
            hold_delete: AtomicBool::new(false),
            delete_entered: Notify::new(),
            delete_release: Notify::new(),
            connections: AtomicUsize::new(0),
        });
        let description = native_api();
        let native = dropshot::ServerBuilder::new(
            description,
            protocol.clone(),
            slog::Logger::root(slog::Discard, slog::o!()),
        )
        .tls(Some(dropshot::ConfigTls::AsBytes {
            certs: pem("CERTIFICATE", include_bytes!("fixtures/tls/cert.der")),
            key: pem("PRIVATE KEY", include_bytes!("fixtures/tls/key.der")),
        }))
        .start()
        .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(rustls::pki_types::CertificateDer::from(
                include_bytes!("fixtures/tls/ca.der").to_vec(),
            ))
            .unwrap();
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        let api = ApiClient::with_tls_trust(
            format!("https://{}", native.local_addr()).parse().unwrap(),
            TOKEN,
            Transport::HomeAssistant,
            Duration::from_secs(10),
            Some(reqwest::Certificate::from_der(include_bytes!("fixtures/tls/ca.der")).unwrap()),
            Some(tokio_tungstenite::Connector::Rustls(Arc::new(tls))),
        )
        .unwrap();
        Self {
            directory,
            store,
            context,
            backend,
            native,
            protocol,
            api,
        }
    }
    fn options(&self) -> SyncOptions {
        SyncOptions {
            state_directory: Some(self.directory.path().join("state")),
            ..Default::default()
        }
    }
    fn source(&self) -> SourceSnapshot {
        let mut items = catalog();
        items[0].images.truncate(1);
        items.truncate(1);
        let bytes = png();
        let sha = ImageSha::digest(&bytes);
        let images = self.directory.path().join("wardrobe-images");
        std::fs::create_dir_all(&images).unwrap();
        std::fs::write(images.join(format!("{sha}.png")), bytes).unwrap();
        let manifest = self.directory.path().join("wardrobe-images.json");
        std::fs::write(
            &manifest,
            serde_json::to_vec(&ImageManifest {
                version: 1,
                images: vec![ManifestImage {
                    item_id: items[0].id.clone(),
                    sha256: sha,
                    image_index: Some(0),
                    native_id: None,
                    upload_name: None,
                }],
            })
            .unwrap(),
        )
        .unwrap();
        let catalog = closet::models::validate_catalog(items).unwrap();
        let local = LocalImages::read(&manifest, &images, catalog.items()).unwrap();
        SourceSnapshot::new(catalog, Some(local), None)
    }
    async fn add(&self, id: u32, name: String, bytes: Vec<u8>) -> NativeImageId {
        let id = NativeImageId::try_from(format!("{id:032x}")).unwrap();
        self.protocol.state.lock().await.images.insert(
            id.clone(),
            Stored {
                record: NativeImage {
                    id: id.clone(),
                    name,
                    content_type: "image/png".into(),
                    filesize: bytes.len() as u64,
                },
                bytes,
            },
        );
        id
    }
    async fn restart_backend(&mut self) {
        let store = Store::open(self.directory.path().join("closet.sqlite3"))
            .await
            .unwrap();
        let context = Arc::new(api::Context::new(store.clone(), TOKEN));
        let backend = api::start(
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            context.clone(),
        )
        .unwrap();
        *self.protocol.backend.write().unwrap() = format!("http://{}", backend.local_addr());
        let previous = std::mem::replace(&mut self.backend, backend);
        previous.close().await.unwrap();
        self.context.shutdown().await.unwrap();
        self.store.close().await;
        self.context = context;
        self.store = store;
    }
    async fn close(self) {
        self.native.close().await.unwrap();
        assert_eq!(self.protocol.connections.load(Ordering::SeqCst), 0);
        self.backend.close().await.unwrap();
        self.context.shutdown().await.unwrap();
        self.store.close().await;
    }
}
#[tokio::test]
async fn one_sync_uploads_verifies_prunes_and_reuses_without_changing_sources() {
    let fixture = Fixture::new().await;
    let source = fixture.source();
    let old = fixture
        .add(1, ImageSha::digest(&png()).upload_name(), png())
        .await; // Existing identical bytes are reused.
    let unrelated = fixture
        .add(2, "another-integration.png".into(), png())
        .await;
    let output = inventory_sync(&fixture.api, &source, &fixture.options())
        .await
        .unwrap();
    assert!(output.data.verified);
    assert_eq!(output.data.images.unwrap().uploaded, 0);
    assert_eq!(fixture.protocol.state.lock().await.posts, 0);
    assert_eq!(
        source.catalog.items()[0].images[0].url,
        catalog()[0].images[0].url
    );
    let current = fixture.store.active_items().await.unwrap();
    assert!(current[0].images[0].url.contains(old.as_str()));
    // An obsolete managed copy is deleted only after the complete sync/readback.
    let duplicate = fixture
        .add(3, ImageSha::digest(&png()).upload_name(), png())
        .await;
    let output = inventory_sync(&fixture.api, &source, &fixture.options())
        .await
        .unwrap();
    assert_eq!(output.data.images.unwrap().pruned, 1);
    let state = fixture.protocol.state.lock().await;
    assert!(state.images.contains_key(&old));
    assert!(state.images.contains_key(&unrelated));
    assert!(!state.images.contains_key(&duplicate));
    drop(state);
    fixture.close().await;
}
#[tokio::test]
async fn lost_upload_reply_reconciles_once_and_definite_rejection_can_retry() {
    let fixture = Fixture::new().await;
    let source = fixture.source();
    fixture.protocol.reject_upload.store(true, Ordering::SeqCst);
    assert_eq!(
        inventory_sync(&fixture.api, &source, &fixture.options())
            .await
            .unwrap_err()
            .error_code,
        "native_image_upload_rejected"
    );
    assert_eq!(fixture.protocol.state.lock().await.posts, 1);
    fixture
        .protocol
        .reject_upload
        .store(false, Ordering::SeqCst);
    fixture
        .protocol
        .lose_upload_reply
        .store(true, Ordering::SeqCst);
    let result = inventory_sync(&fixture.api, &source, &fixture.options())
        .await
        .unwrap();
    assert_eq!(result.data.images.unwrap().uploaded, 1);
    assert_eq!(fixture.protocol.state.lock().await.posts, 2);
    inventory_sync(&fixture.api, &source, &fixture.options())
        .await
        .unwrap();
    assert_eq!(fixture.protocol.state.lock().await.posts, 2);
    fixture.close().await;
}
#[tokio::test]
async fn uncertain_post_is_durable_across_new_client_state_and_never_blindly_retried() {
    let fixture = Fixture::new().await;
    let source = fixture.source();
    fixture.protocol.drop_upload.store(true, Ordering::SeqCst);
    assert_eq!(
        inventory_sync(&fixture.api, &source, &fixture.options())
            .await
            .unwrap_err()
            .error_code,
        "image_upload_pending"
    );
    let mut options = fixture.options();
    options.state_directory = Some(fixture.directory.path().join("fresh-process-state"));
    assert_eq!(
        inventory_sync(&fixture.api, &source, &options)
            .await
            .unwrap_err()
            .error_code,
        "image_upload_pending"
    );
    assert_eq!(fixture.protocol.state.lock().await.posts, 1);
    assert!(fixture.protocol.state.lock().await.deletes.is_empty());
    fixture.close().await;
}
#[tokio::test]
async fn dry_run_is_honest_about_missing_random_ids_and_failed_sync_never_prunes() {
    let fixture = Fixture::new().await;
    let source = fixture.source();
    let mut options = fixture.options();
    options.dry_run = true;
    let preview = inventory_sync(&fixture.api, &source, &options)
        .await
        .unwrap();
    assert!(preview.data.resolved_source_hash.is_none());
    assert!(preview.data.inventory.is_none());
    assert_eq!(preview.data.images.unwrap().would_upload, 1);
    assert_eq!(fixture.protocol.state.lock().await.posts, 0);
    assert!(!fixture.directory.path().join("state").exists());
    fixture
        .add(
            99,
            "closet-ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff.png".into(),
            png(),
        )
        .await;
    options.dry_run = false;
    fixture.protocol.fail_sync.store(true, Ordering::SeqCst);
    assert_eq!(
        inventory_sync(&fixture.api, &source, &options)
            .await
            .unwrap_err()
            .error_code,
        "revision_conflict"
    );
    assert!(fixture.protocol.state.lock().await.deletes.is_empty());
    assert_eq!(fixture.store.counts().await.unwrap().0, 0);
    fixture.close().await;
}
#[tokio::test]
async fn writer_is_blocked_during_native_delete_without_holding_a_sql_transaction() {
    let fixture = Fixture::new().await;
    let source = fixture.source();
    let sha = ImageSha::digest(&png());
    fixture.add(1, sha.upload_name(), png()).await;
    fixture.add(2, sha.upload_name(), png()).await;
    fixture.protocol.hold_delete.store(true, Ordering::SeqCst);
    {
        let options = fixture.options();
        let sync = inventory_sync(&fixture.api, &source, &options);
        tokio::pin!(sync);
        tokio::select! {result=&mut sync=>panic!("unexpected early completion {result:?}"),_=fixture.protocol.delete_entered.notified()=>{}}
        let request = SyncRequest {
            items: source.catalog.items().to_vec(),
            dry_run: false,
            allow_empty: false,
        };
        assert_eq!(
            fixture
                .store
                .sync(request, Some(1), "concurrent-writer", Some("writer"))
                .await
                .unwrap_err()
                .code(),
            "cleanup_pending"
        );
        assert_eq!(fixture.store.counts().await.unwrap().1, 1); // SQL connection is free while network is suspended.
        fixture.protocol.delete_release.notify_one();
        assert!(sync.await.unwrap().data.verified);
    }
    fixture.close().await;
}

#[tokio::test]
async fn uncertain_delete_recovers_exact_pending_ids_after_backend_restart() {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let mut fixture = Fixture::new().await;
    let source = fixture.source();
    let sha = ImageSha::digest(&png());
    let kept = fixture.add(1, sha.upload_name(), png()).await;
    let obsolete = fixture.add(2, sha.upload_name(), png()).await;
    fixture.protocol.fail_delete.store(true, Ordering::SeqCst);
    assert_eq!(
        inventory_sync(&fixture.api, &source, &fixture.options())
            .await
            .unwrap_err()
            .error_code,
        "cleanup_pending"
    );
    assert_eq!(
        fixture.protocol.state.lock().await.deletes,
        vec![obsolete.clone()]
    );
    fixture.restart_backend().await;
    let mut connection = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(fixture.directory.path().join("closet.sqlite3")),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE inventory_fence SET expires_at=0")
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
    fixture.protocol.fail_delete.store(false, Ordering::SeqCst);
    let mut options = fixture.options();
    options.state_directory = Some(fixture.directory.path().join("restarted-client"));
    assert!(
        inventory_sync(&fixture.api, &source, &options)
            .await
            .unwrap()
            .data
            .verified
    );
    let state = fixture.protocol.state.lock().await;
    assert_eq!(state.deletes, vec![obsolete.clone(), obsolete.clone()]);
    assert!(state.images.contains_key(&kept));
    assert!(!state.images.contains_key(&obsolete));
    drop(state);
    fixture.close().await;
}

#[tokio::test]
async fn active_native_thumbnail_alias_is_protected_by_cli_and_server_admission() {
    use closet::{
        client::response,
        fence::{FenceAcquire, FencePrepare, FenceStatus, FenceToken, ManagedImage},
    };
    let fixture = Fixture::new().await;
    let sha = ImageSha::digest(&png());
    let id = fixture.add(7, sha.upload_name(), png()).await;
    let mut items = catalog();
    items.truncate(1);
    items[0].images[0].url = format!(
        "https://{}/api/image/serve/{id}/512x512-photo.png?display=photo#image",
        fixture.native.local_addr()
    );
    let source = SourceSnapshot::new(closet::models::validate_catalog(items).unwrap(), None, None);
    let output = inventory_sync(&fixture.api, &source, &fixture.options())
        .await
        .unwrap();
    assert_eq!(output.data.images.unwrap().pruned, 0);
    let token = FenceToken::new().unwrap();
    response::<FenceStatus>(
        fixture
            .api
            .request(reqwest::Method::POST, "/inventory/fence")
            .unwrap()
            .json(&FenceAcquire {
                token: token.clone(),
                expected_revision: 1,
            }),
    )
    .await
    .unwrap();
    let error = response::<FenceStatus>(
        fixture
            .api
            .request(reqwest::Method::POST, "/inventory/fence/prepare")
            .unwrap()
            .json(&FencePrepare {
                token,
                catalog_revision: 1,
                images: vec![ManagedImage { id, sha256: sha }],
            }),
    )
    .await
    .unwrap_err();
    assert_eq!(error.error_code, "image_referenced");
    assert_eq!(error.status, Some(409));
    fixture.close().await;
}

#[tokio::test]
async fn executable_owns_inventory_readback_and_managed_cleanup_in_one_command() {
    #[derive(Deserialize)]
    struct Envelope {
        data: Applied,
    }
    #[derive(Deserialize)]
    struct Applied {
        verified: bool,
        images: Counts,
    }
    #[derive(Deserialize)]
    struct Counts {
        pruned: usize,
        uploaded: usize,
    }
    let fixture = Fixture::new().await;
    let sha = ImageSha::digest(&png());
    let live = fixture.add(1, sha.upload_name(), png()).await;
    let old = fixture.add(2, sha.upload_name(), png()).await;
    let server = dropshot::ServerBuilder::new(
        native_api(),
        fixture.protocol.clone(),
        slog::Logger::root(slog::Discard, slog::o!()),
    )
    .start()
    .unwrap();
    let mut catalog = catalog();
    catalog.truncate(1);
    catalog[0].images[0].url = format!(
        "https://{}/api/image/serve/{live}/original",
        fixture.native.local_addr()
    );
    let input = fixture.directory.path().join("catalog.json");
    std::fs::write(&input, serde_json::to_vec(&catalog).unwrap()).unwrap();
    let before = std::fs::read(&input).unwrap();
    let state = fixture.directory.path().join("cli-state");
    let origin = format!("http://{}", server.local_addr());
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_ha-closet"))
            .args([
                "--transport",
                "home-assistant",
                "--url",
                &origin,
                "inventory",
                "sync",
                "--input",
                input.to_str().unwrap(),
                "--state-dir",
                state.to_str().unwrap(),
            ])
            .env("HOME_ASSISTANT_TOKEN", TOKEN)
            .env_remove("HTTP_PROXY")
            .env_remove("HTTPS_PROXY")
            .env_remove("ALL_PROXY")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Envelope = serde_json::from_slice(&output.stdout).unwrap();
    assert!(result.data.verified);
    assert_eq!(result.data.images.pruned, 1);
    assert_eq!(result.data.images.uploaded, 0);
    assert_eq!(
        std::fs::read(fixture.directory.path().join("catalog.json")).unwrap(),
        before
    );
    let state = fixture.protocol.state.lock().await;
    assert!(state.images.contains_key(&live));
    assert!(!state.images.contains_key(&old));
    assert_eq!(state.posts, 0);
    drop(state);
    server.close().await.unwrap();
    fixture.close().await;
}
