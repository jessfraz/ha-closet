//! Real SQLite, HTTP multipart, WebSocket and executable consumers. All data is fictional.
use base64::Engine;
use closet::{
    api,
    client::{ApiClient, Transport},
    ha_images::NativeImage,
    images::{
        DeliveryFormat, ImageManifest, ImageSha, LocalImage, LocalImages, ManifestImage,
        NativeImageId, PreparedImage, prepare_images,
    },
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
#[path = "support/toml.rs"]
mod toml_source;
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
    hold_upload: AtomicBool,
    upload_entered: Notify,
    upload_release: Notify,
    hold_readback: AtomicBool,
    readback_entered: Notify,
    readback_release: Notify,
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
    if path == "items" && protocol.hold_readback.load(Ordering::SeqCst) {
        protocol.readback_entered.notify_one();
        protocol.readback_release.notified().await;
    }
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
    let content_type = field.content_type().unwrap().to_string();
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
    assert!(name.starts_with("closet-v1-"));
    assert_eq!(
        DeliveryFormat::from_upload_name(&name).unwrap().mime(),
        content_type
    );
    let decoded = image::load_from_memory(&bytes).unwrap();
    assert!(decoded.width() <= 2048 && decoded.height() <= 2048);
    assert!(bytes.len() <= closet::images::MAX_DELIVERY_BYTES);
    if !protocol.drop_upload.load(Ordering::SeqCst) {
        state.sequence += 1;
        let id = NativeImageId::try_from(format!("{:032x}", state.sequence + 1000)).unwrap();
        let record = NativeImage {
            id: id.clone(),
            name,
            content_type,
            filesize: bytes.len() as u64,
        };
        state.images.insert(id, Stored { record, bytes });
    }
    drop(state);
    if protocol.hold_upload.load(Ordering::SeqCst) {
        protocol.upload_entered.notify_one();
        protocol.upload_release.notified().await;
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
        Some(image) => raw(200, image.bytes.clone(), &image.record.content_type),
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
            hold_upload: AtomicBool::new(false),
            upload_entered: Notify::new(),
            upload_release: Notify::new(),
            hold_readback: AtomicBool::new(false),
            readback_entered: Notify::new(),
            readback_release: Notify::new(),
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
        self.source_bytes(png())
    }
    fn source_bytes(&self, bytes: Vec<u8>) -> SourceSnapshot {
        let items = Self::source_items();
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
    fn source_items() -> Vec<CatalogItem> {
        let mut items = catalog();
        items[0].images.truncate(1);
        items.truncate(1);
        items
    }
    async fn add(&self, id: u32, name: String, bytes: Vec<u8>) -> NativeImageId {
        let id = NativeImageId::try_from(format!("{id:032x}")).unwrap();
        let content_type = DeliveryFormat::from_upload_name(&name)
            .map_or("image/png", DeliveryFormat::mime)
            .into();
        self.protocol.state.lock().await.images.insert(
            id.clone(),
            Stored {
                record: NativeImage {
                    id: id.clone(),
                    name,
                    content_type,
                    filesize: bytes.len() as u64,
                },
                bytes,
            },
        );
        id
    }
    fn delivery(&self) -> PreparedImage {
        let bytes = png();
        let path = self.directory.path().join("delivery-source.png");
        std::fs::write(&path, &bytes).unwrap();
        prepare_images(vec![LocalImage {
            sha256: ImageSha::digest(&bytes),
            path,
            size: bytes.len() as u64,
            native_hint: None,
            slots: vec![],
        }])
        .unwrap()
        .remove(0)
    }
    async fn add_delivery(&self, id: u32) -> NativeImageId {
        let image = self.delivery();
        self.add(id, image.upload_name(), image.bytes().to_vec())
            .await
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
    let old = fixture.add_delivery(1).await; // Existing delivery bytes are reused.
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
        source.report().source_hash,
        closet::models::validate_catalog(Fixture::source_items())
            .unwrap()
            .report()
            .source_hash
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
async fn optimized_jpeg_reconciles_legacy_pending_bytes_before_upload_and_reuses_wire_identity() {
    use closet::{
        client::response,
        fence::{
            FenceAcquire, FenceStatus, FenceToken, FenceUpload, UploadClaimAction,
            UploadClaimStatus,
        },
    };
    use reqwest::Method;
    let fixture = Fixture::new().await;
    let manifest: ImageManifest =
        serde_json::from_slice(include_bytes!("fixtures/image-policy/manifest.json")).unwrap();
    let bytes = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/image-policy/images")
            .join(format!("{}.png", manifest.images[1].sha256)),
    )
    .unwrap();
    let source_sha = ImageSha::digest(&bytes);
    let source = fixture.source_bytes(bytes.clone());
    let input_path = fixture
        .directory
        .path()
        .join("wardrobe-images")
        .join(format!("{source_sha}.png"));
    let manifest_path = fixture.directory.path().join("wardrobe-images.json");
    let source_manifest = std::fs::read(&manifest_path).unwrap();
    let token = FenceToken::new().unwrap();
    response::<FenceStatus>(
        fixture
            .api
            .request(Method::POST, "/inventory/fence")
            .unwrap()
            .json(&FenceAcquire {
                token: token.clone(),
                expected_revision: 0,
            }),
    )
    .await
    .unwrap();
    let claim = response::<UploadClaimStatus>(
        fixture
            .api
            .request(Method::POST, "/inventory/fence/uploads")
            .unwrap()
            .json(&FenceUpload {
                token: token.clone(),
                sha256: source_sha.clone(),
                action: UploadClaimAction::Begin,
            }),
    )
    .await
    .unwrap();
    assert!(claim.data.claimed);
    let state = fixture.options().state_directory.unwrap();
    std::fs::create_dir(&state).unwrap();
    let token_path = state.join("fence-token");
    std::fs::write(&token_path, token.as_str()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    assert_eq!(
        inventory_sync(&fixture.api, &source, &fixture.options())
            .await
            .unwrap_err()
            .error_code,
        "image_upload_pending"
    );
    assert_eq!(fixture.protocol.state.lock().await.posts, 0);
    assert_eq!(fixture.store.counts().await.unwrap().1, 0);
    let legacy = fixture
        .add(1, source_sha.upload_name(), bytes.clone())
        .await;
    let result = inventory_sync(&fixture.api, &source, &fixture.options())
        .await
        .unwrap();
    assert!(result.data.verified);
    let counts = result.data.images.unwrap();
    assert_eq!((counts.uploaded, counts.pruned), (1, 1));
    let live = fixture.store.active_items().await.unwrap();
    let native = fixture.protocol.state.lock().await;
    assert!(!native.images.contains_key(&legacy));
    assert_eq!(native.images.len(), 1);
    let stored = native.images.values().next().unwrap();
    assert_eq!(stored.record.content_type, "image/jpeg");
    assert!(stored.record.name.starts_with("closet-v1-") && stored.record.name.ends_with(".jpg"));
    assert_ne!(ImageSha::digest(&stored.bytes), source_sha);
    assert!(live[0].images[0].url.contains(stored.record.id.as_str()));
    let deployed = stored.record.id.clone();
    drop(native);
    let repeat = inventory_sync(&fixture.api, &source, &fixture.options())
        .await
        .unwrap();
    assert_eq!(repeat.data.images.unwrap().uploaded, 0);
    let native = fixture.protocol.state.lock().await;
    assert_eq!(native.posts, 1);
    assert!(native.images.contains_key(&deployed));
    drop(native);
    assert_eq!(std::fs::read(input_path).unwrap(), bytes);
    assert_eq!(std::fs::read(manifest_path).unwrap(), source_manifest);
    fixture.close().await;
}

#[tokio::test]
async fn dry_run_counts_distinct_delivery_bytes_and_apply_maps_every_source() {
    use image::ImageEncoder;
    let fixture = Fixture::new().await;
    let mut items = Fixture::source_items();
    let mut second = items[0].clone();
    second.id = "example-second-source".into();
    items.push(second);
    let raw_source = png();
    let decoded = image::load_from_memory(&raw_source).unwrap();
    let mut with_metadata = Vec::new();
    let mut encoder = image::codecs::png::PngEncoder::new(&mut with_metadata);
    encoder
        .set_exif_metadata(
            b"II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x01\0\0\0\0\0\0\0".to_vec(),
        )
        .unwrap();
    encoder
        .write_image(
            decoded.as_bytes(),
            decoded.width(),
            decoded.height(),
            decoded.color().into(),
        )
        .unwrap();
    let directory = fixture.directory.path().join("dedup-images");
    std::fs::create_dir(&directory).unwrap();
    let mut mappings = Vec::new();
    for (index, bytes) in [raw_source, with_metadata].into_iter().enumerate() {
        let sha = ImageSha::digest(&bytes);
        std::fs::write(directory.join(format!("{sha}.png")), bytes).unwrap();
        mappings.push(ManifestImage {
            item_id: items[index].id.clone(),
            sha256: sha,
            image_index: Some(0),
            native_id: None,
            upload_name: None,
        });
    }
    let path = fixture.directory.path().join("dedup-manifest.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&ImageManifest {
            version: 1,
            images: mappings,
        })
        .unwrap(),
    )
    .unwrap();
    let catalog = closet::models::validate_catalog(items).unwrap();
    let local = LocalImages::read(&path, &directory, catalog.items()).unwrap();
    assert_ne!(
        local.assets[0].source().sha256,
        local.assets[1].source().sha256
    );
    assert_eq!(local.assets[0].wire_sha(), local.assets[1].wire_sha());
    let source = SourceSnapshot::new(catalog, Some(local), None);
    let mut options = fixture.options();
    options.dry_run = true;
    let preview = inventory_sync(&fixture.api, &source, &options)
        .await
        .unwrap();
    assert_eq!(preview.data.images.unwrap().would_upload, 1);
    options.dry_run = false;
    let applied = inventory_sync(&fixture.api, &source, &options)
        .await
        .unwrap();
    assert_eq!(applied.data.images.unwrap().uploaded, 1);
    let current = fixture.store.active_items().await.unwrap();
    assert_eq!(current.len(), 2);
    assert_eq!(current[0].images[0].url, current[1].images[0].url);
    assert_eq!(fixture.protocol.state.lock().await.posts, 1);
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
    fixture.add_delivery(1).await;
    fixture.add(2, sha.upload_name(), png()).await;
    fixture.protocol.hold_delete.store(true, Ordering::SeqCst);
    {
        let options = fixture.options();
        let sync = inventory_sync(&fixture.api, &source, &options);
        tokio::pin!(sync);
        tokio::select! {result=&mut sync=>panic!("unexpected early completion {result:?}"),_=fixture.protocol.delete_entered.notified()=>{}}
        let request = SyncRequest {
            items: Fixture::source_items(),
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
    let kept = fixture.add_delivery(1).await;
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
async fn pending_cleanup_blocks_reintroduced_native_references_but_local_bytes_can_recover() {
    let fixture = Fixture::new().await;
    let source = fixture.source();
    let kept = fixture.add_delivery(1).await;
    let obsolete = fixture
        .add(2, ImageSha::digest(&png()).upload_name(), png())
        .await;
    fixture.protocol.fail_delete.store(true, Ordering::SeqCst);
    assert_eq!(
        inventory_sync(&fixture.api, &source, &fixture.options())
            .await
            .unwrap_err()
            .error_code,
        "cleanup_pending"
    );
    fixture.protocol.fail_delete.store(false, Ordering::SeqCst);
    let mut items = Fixture::source_items();
    items[0].images[0].url = format!(
        "https://{}/api/image/serve/{obsolete}/original",
        fixture.native.local_addr()
    );
    let catalog = closet::models::validate_catalog(items).unwrap();
    let references_only = SourceSnapshot::new(catalog.clone(), None, None);
    assert_eq!(
        inventory_sync(&fixture.api, &references_only, &fixture.options())
            .await
            .unwrap_err()
            .error_code,
        "cleanup_source_conflict"
    );
    let state = fixture.protocol.state.lock().await;
    assert!(state.images.contains_key(&obsolete));
    assert_eq!(state.deletes, vec![obsolete.clone()]);
    drop(state);
    assert_eq!(fixture.store.counts().await.unwrap().0, 1);
    // The same outgoing slot is recoverable when its reviewed local bytes are
    // explicitly bound. Simulate the current deployment disappearing as well.
    fixture.protocol.state.lock().await.images.remove(&kept);
    let local = LocalImages::read(
        &fixture.directory.path().join("wardrobe-images.json"),
        &fixture.directory.path().join("wardrobe-images"),
        catalog.items(),
    )
    .unwrap();
    let recoverable = SourceSnapshot::new(catalog, Some(local), None);
    let output = inventory_sync(&fixture.api, &recoverable, &fixture.options())
        .await
        .unwrap();
    assert!(output.data.verified);
    let counts = output.data.images.unwrap();
    assert_eq!((counts.uploaded, counts.pruned), (1, 1));
    let current = fixture.store.active_items().await.unwrap();
    let state = fixture.protocol.state.lock().await;
    assert!(!state.images.contains_key(&obsolete));
    let record = state.images.values().next().unwrap();
    assert!(current[0].images[0].url.contains(record.record.id.as_str()));
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

#[tokio::test]
async fn toml_resolves_new_assets_in_ordered_shared_slots_and_keeps_source_identity() {
    let fixture = Fixture::new().await;
    let input = toml_source::write(fixture.directory.path());
    let before: Vec<_> = [
        "wardrobe.toml",
        "brands.toml",
        "categories.toml",
        "fittings.toml",
        "wardrobe-images/431ced6916a2a21a156e38701afe55bbd7f88969fbbfc56d7fe099d47f265460.png",
    ]
    .into_iter()
    .map(|name| {
        let path = fixture.directory.path().join(name);
        let bytes = std::fs::read(&path).unwrap();
        (path, bytes)
    })
    .collect();
    let source = SourceSnapshot::from_toml(&input).unwrap();
    let source_hash = source.report().source_hash;
    let obsolete_bytes = base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4//8/AAX+Av4N70a4AAAAAElFTkSuQmCC").unwrap();
    let obsolete = fixture
        .add(
            1,
            ImageSha::digest(&obsolete_bytes).upload_name(),
            obsolete_bytes,
        )
        .await;
    let output = inventory_sync(&fixture.api, &source, &fixture.options())
        .await
        .unwrap();
    assert!(output.data.verified);
    assert_eq!(output.data.input_source_hash, source_hash);
    let resolved_hash = output.data.resolved_source_hash.unwrap();
    assert_ne!(resolved_hash, source_hash);
    let counts = output.data.images.unwrap();
    assert_eq!(counts.local, 1);
    assert_eq!(counts.uploaded, 1);
    assert_eq!(counts.pruned, 1);
    let items = fixture.store.active_items().await.unwrap();
    assert_eq!(items.len(), 2);
    let first = items
        .iter()
        .find(|item| item.id == "example-purchase-a")
        .unwrap();
    let second = items
        .iter()
        .find(|item| item.id == "example-purchase-b")
        .unwrap();
    assert_eq!(first.quantity, Some(2));
    assert_eq!(second.quantity, Some(1));
    assert_eq!(first.brand.as_deref(), Some("North Studio"));
    assert_eq!(first.color.as_deref(), Some("Navy"));
    assert_eq!(first.material.as_deref(), Some("Cotton (Jersey)"));
    assert_eq!(first.size.as_deref(), Some("00"));
    assert_eq!(second.size.as_deref(), Some("0"));
    assert_eq!(first.purchase_source.as_deref(), Some("self_bought"));
    assert_eq!(second.shop.as_ref().unwrap().id, "example_spring");
    assert_eq!(first.images.len(), 1);
    assert_eq!(second.images.len(), 2);
    assert_eq!(first.images[0].url, second.images[1].url);
    assert!(first.images[0].url.starts_with(&format!(
        "https://{}/api/image/serve/",
        fixture.native.local_addr()
    )));
    assert_eq!(
        second.images[0].url,
        "https://images.example.invalid/context.png"
    );
    assert_eq!(
        first.images[0].source_url.as_deref(),
        Some("https://maker.example/crew-tee")
    );
    assert_eq!(
        second.images[1].source_url.as_deref(),
        Some("https://maker.example/crew-tee/detail")
    );
    assert_eq!(second.images[1].crop.as_ref().unwrap().width, 8000);
    assert!(
        !fixture
            .protocol
            .state
            .lock()
            .await
            .images
            .contains_key(&obsolete)
    );
    let mut options = fixture.options();
    options.dry_run = true;
    let dry_run = inventory_sync(&fixture.api, &source, &options)
        .await
        .unwrap();
    assert_eq!(dry_run.etag.as_deref(), Some("\"1\""));
    assert_eq!(
        dry_run.data.resolved_source_hash.as_deref(),
        Some(resolved_hash.as_str())
    );
    assert_eq!(dry_run.data.images.unwrap().would_prune, Some(0));
    let replay = inventory_sync(&fixture.api, &source, &fixture.options())
        .await
        .unwrap();
    assert_eq!(replay.data.input_source_hash, source_hash);
    assert_eq!(
        replay.data.resolved_source_hash.as_deref(),
        Some(resolved_hash.as_str())
    );
    let diff = replay.data.inventory.unwrap();
    assert_eq!(diff.created + diff.updated + diff.archived, 0);
    assert_eq!(fixture.protocol.state.lock().await.posts, 1);
    for (path, bytes) in before {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    fixture.close().await;
}

#[tokio::test]
async fn toml_missing_asset_dry_run_has_no_fake_inventory_hash_or_prune_count() {
    let fixture = Fixture::new().await;
    let input = toml_source::write(fixture.directory.path());
    let source = SourceSnapshot::from_toml(&input).unwrap();
    let mut options = fixture.options();
    options.dry_run = true;
    let output = inventory_sync(&fixture.api, &source, &options)
        .await
        .unwrap();
    assert!(!output.data.verified);
    assert!(output.data.inventory.is_none());
    assert!(output.data.resolved_source_hash.is_none());
    let counts = output.data.images.unwrap();
    assert_eq!(counts.local, 1);
    assert_eq!(counts.would_upload, 1);
    assert_eq!(counts.would_prune, None);
    assert_eq!(fixture.store.counts().await.unwrap().0, 0);
    let native = fixture.protocol.state.lock().await;
    assert_eq!(native.posts, 0);
    assert!(native.deletes.is_empty());
    drop(native);
    assert!(!fixture.directory.path().join("state").exists());
    fixture.close().await;
}

#[tokio::test]
async fn toml_full_readset_changes_after_upload_abort_before_commit_or_prune() {
    for changed in [
        "wardrobe.toml",
        "brands.toml",
        "categories.toml",
        "fittings.toml",
        "asset",
    ] {
        let fixture = Fixture::new().await;
        let input = toml_source::write(fixture.directory.path());
        let source = SourceSnapshot::from_toml(&input).unwrap();
        fixture.protocol.hold_upload.store(true, Ordering::SeqCst);
        {
            let options = fixture.options();
            let operation = inventory_sync(&fixture.api, &source, &options);
            tokio::pin!(operation);
            tokio::select! {
                result = &mut operation => panic!("unexpected completion before upload barrier: {result:?}"),
                _ = fixture.protocol.upload_entered.notified() => {}
            }
            let path = if changed == "asset" {
                fixture
                    .directory
                    .path()
                    .join("wardrobe-images")
                    .join(format!("{}.png", ImageSha::digest(&png())))
            } else {
                fixture.directory.path().join(changed)
            };
            let mut bytes = std::fs::read(&path).unwrap();
            bytes.extend_from_slice(b"\n# changed during sync\n");
            std::fs::write(path, bytes).unwrap();
            fixture.protocol.upload_release.notify_one();
            let error = operation.await.unwrap_err();
            assert_eq!(
                error.error_code, "catalog_source_changed",
                "{changed}: {error:?}"
            );
        }
        assert_eq!(fixture.store.counts().await.unwrap().0, 0);
        let native = fixture.protocol.state.lock().await;
        assert_eq!(native.posts, 1);
        assert!(native.deletes.is_empty());
        drop(native);
        fixture.close().await;
    }
}

#[tokio::test]
async fn toml_registry_change_after_commit_prevents_native_pruning() {
    let fixture = Fixture::new().await;
    let input = toml_source::write(fixture.directory.path());
    let source = SourceSnapshot::from_toml(&input).unwrap();
    let sha = ImageSha::digest(&png());
    fixture.add(1, sha.upload_name(), png()).await;
    let duplicate = fixture.add(2, sha.upload_name(), png()).await;
    fixture.protocol.hold_readback.store(true, Ordering::SeqCst);
    {
        let options = fixture.options();
        let operation = inventory_sync(&fixture.api, &source, &options);
        tokio::pin!(operation);
        tokio::select! {
            result = &mut operation => panic!("unexpected completion before readback barrier: {result:?}"),
            _ = fixture.protocol.readback_entered.notified() => {}
        }
        assert_eq!(fixture.store.counts().await.unwrap(), (1, 2, 0));
        let path = fixture.directory.path().join("brands.toml");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(b"\n# changed after commit\n");
        std::fs::write(path, bytes).unwrap();
        fixture.protocol.readback_release.notify_one();
        let error = operation.await.unwrap_err();
        assert_eq!(error.error_code, "catalog_source_changed");
    }
    let native = fixture.protocol.state.lock().await;
    assert!(native.deletes.is_empty());
    assert!(native.images.contains_key(&duplicate));
    drop(native);
    fixture.close().await;
}

#[tokio::test]
async fn toml_uncertain_upload_keeps_existing_durable_claim_and_never_posts_twice() {
    let fixture = Fixture::new().await;
    let input = toml_source::write(fixture.directory.path());
    let source = SourceSnapshot::from_toml(&input).unwrap();
    fixture.protocol.drop_upload.store(true, Ordering::SeqCst);
    assert_eq!(
        inventory_sync(&fixture.api, &source, &fixture.options())
            .await
            .unwrap_err()
            .error_code,
        "image_upload_pending"
    );
    let mut options = fixture.options();
    options.state_directory = Some(fixture.directory.path().join("fresh-toml-worker"));
    assert_eq!(
        inventory_sync(&fixture.api, &source, &options)
            .await
            .unwrap_err()
            .error_code,
        "image_upload_pending"
    );
    let native = fixture.protocol.state.lock().await;
    assert_eq!(native.posts, 1);
    assert!(native.deletes.is_empty());
    drop(native);
    assert_eq!(fixture.store.counts().await.unwrap().0, 0);
    fixture.close().await;
}

#[tokio::test]
async fn cancelled_toml_upload_reconciles_real_object_without_another_post() {
    let fixture = Fixture::new().await;
    let input = toml_source::write(fixture.directory.path());
    let source = SourceSnapshot::from_toml(&input).unwrap();
    fixture.protocol.hold_upload.store(true, Ordering::SeqCst);
    {
        let options = fixture.options();
        let operation = inventory_sync(&fixture.api, &source, &options);
        tokio::pin!(operation);
        tokio::select! {
            result = &mut operation => panic!("unexpected completion before upload barrier: {result:?}"),
            _ = fixture.protocol.upload_entered.notified() => {}
        }
        // Dropping this real operation models the timeout/cancel select branch:
        // the server has accepted the bytes, but the client received no reply.
    }
    assert_eq!(fixture.store.counts().await.unwrap().0, 0);
    fixture.protocol.hold_upload.store(false, Ordering::SeqCst);
    fixture.protocol.upload_release.notify_one();
    let output = inventory_sync(&fixture.api, &source, &fixture.options())
        .await
        .unwrap();
    assert!(output.data.verified);
    assert_eq!(output.data.images.unwrap().uploaded, 0);
    assert_eq!(fixture.protocol.state.lock().await.posts, 1);
    fixture.close().await;
}

#[tokio::test]
async fn sync_deadline_recovers_a_durable_pending_upload_with_a_longer_same_owner_budget() {
    use closet::{
        client::response,
        fence::{FenceAcquire, FenceStatus, FenceToken},
    };
    use reqwest::Method;

    let mut fixture = Fixture::new().await;
    let input = toml_source::write(fixture.directory.path());
    let source = SourceSnapshot::from_toml(&input).unwrap();
    let mut options = fixture.options();
    options.operation_timeout = Some(Duration::from_secs(1));
    fixture.protocol.hold_upload.store(true, Ordering::SeqCst);
    let first = {
        let operation = inventory_sync(&fixture.api, &source, &options);
        tokio::pin!(operation);
        tokio::select! {
            result = &mut operation => panic!("unexpected completion before upload barrier: {result:?}"),
            _ = fixture.protocol.upload_entered.notified() => {}
        }
        tokio::time::timeout(Duration::from_secs(3), operation).await
    };
    fixture.protocol.hold_upload.store(false, Ordering::SeqCst);
    fixture.protocol.upload_release.notify_one();
    let before = fixture.store.counts().await.unwrap();
    let token_path = options
        .state_directory
        .as_ref()
        .unwrap()
        .join("fence-token");
    let token_bytes = std::fs::read_to_string(&token_path).unwrap();
    let wire_sha = {
        let native = fixture.protocol.state.lock().await;
        assert_eq!(native.posts, 1);
        ImageSha::digest(&native.images.values().next().unwrap().bytes)
    };
    fixture.restart_backend().await;
    let pending = response::<FenceStatus>(
        fixture
            .api
            .request(Method::POST, "/inventory/fence")
            .unwrap()
            .json(&FenceAcquire {
                token: FenceToken::try_from(token_bytes.clone()).unwrap(),
                expected_revision: 0,
            }),
    )
    .await
    .unwrap()
    .data;
    options.operation_timeout = Some(Duration::from_secs(5));
    let recovered = inventory_sync(&fixture.api, &source, &options)
        .await
        .unwrap();
    let final_counts = fixture.store.counts().await.unwrap();
    let posts = fixture.protocol.state.lock().await.posts;
    let token_after = std::fs::read_to_string(token_path).unwrap();
    fixture.close().await;

    assert_eq!(first.unwrap().unwrap_err().error_code, "sync_timeout");
    assert_eq!(before, (0, 0, 0));
    assert_eq!(pending.uploads, [wire_sha]);
    assert!(pending.pending.is_empty());
    assert_eq!(token_after, token_bytes);
    assert!(recovered.data.verified);
    assert_eq!(recovered.data.images.unwrap().uploaded, 0);
    assert_eq!(posts, 1);
    assert_eq!(final_counts, (1, 2, 0));
}

#[tokio::test]
async fn invalid_library_sync_budgets_fail_before_source_reads_or_connections() {
    let fixture = Fixture::new().await;
    let source = fixture.source();
    std::fs::remove_dir_all(fixture.directory.path().join("wardrobe-images")).unwrap();
    let Fixture {
        directory,
        store,
        context,
        backend,
        native,
        protocol,
        api,
    } = fixture;
    native.close().await.unwrap();
    let mut codes = Vec::new();
    for budget in [
        Duration::ZERO,
        Duration::from_secs(7201),
        Duration::from_secs(7200) + Duration::from_nanos(1),
    ] {
        let options = SyncOptions {
            operation_timeout: Some(budget),
            state_directory: Some(directory.path().join("state")),
            ..Default::default()
        };
        codes.push(
            inventory_sync(&api, &source, &options)
                .await
                .unwrap_err()
                .error_code,
        );
    }
    let counts = store.counts().await.unwrap();
    let owner_state_created = directory.path().join("state").exists();
    let native_posts = protocol.state.lock().await.posts;
    let connections = protocol.connections.load(Ordering::SeqCst);
    backend.close().await.unwrap();
    context.shutdown().await.unwrap();
    store.close().await;
    assert_eq!(codes, ["invalid_sync_timeout"; 3]);
    assert_eq!(counts, (0, 0, 0));
    assert!(!owner_state_created);
    assert_eq!((native_posts, connections), (0, 0));
}

#[tokio::test]
async fn toml_native_url_budget_rejects_before_any_https_or_websocket_request() {
    use closet::source::{
        CompiledCatalog,
        types::{InventoryEntry, SourceImage, Style, Variant},
    };
    #[derive(Serialize, Deserialize)]
    struct Document {
        schema: u8,
        styles: Vec<Style>,
        variants: Vec<Variant>,
        inventory: Vec<InventoryEntry>,
    }
    let fixture = Fixture::new().await;
    let input = toml_source::write(fixture.directory.path());
    let mut source: Document = toml::from_str(&std::fs::read_to_string(&input).unwrap()).unwrap();
    let image = source.variants[0].images[0].clone();
    source.variants[0].images = (0..20)
        .map(|index| {
            let mut image = image.clone();
            let SourceImage::Asset { reference, .. } = &mut image else {
                panic!("asset fixture")
            };
            reference.notes = Some(format!("Fictional view {index}").try_into().unwrap());
            image
        })
        .collect();
    let template = source.inventory[0].clone();
    source.inventory = (0..250)
        .map(|index| {
            let mut entry = template.clone();
            entry.id = format!("fictional-purchase-{index:04}").try_into().unwrap();
            entry.images = None;
            entry
        })
        .collect();
    std::fs::write(&input, toml::to_string_pretty(&source).unwrap()).unwrap();
    let compiled =
        CompiledCatalog::load(&input).expect("known facts fit before native URL overhead");
    assert_eq!(
        compiled
            .validate_projection_size(
                &format!("https://{}", fixture.native.local_addr())
                    .parse()
                    .unwrap()
            )
            .unwrap_err()
            .code,
        "catalog_too_large"
    );
    let snapshot = SourceSnapshot::from_toml(&input).unwrap();
    let options = fixture.options();
    // The real TLS/WSS listener is now unavailable. Any native or integration
    // request before the size preflight would fail transport, not this contract.
    let Fixture {
        directory,
        store,
        context,
        backend,
        native,
        protocol,
        api,
    } = fixture;
    native.close().await.unwrap();
    for dry_run in [false, true] {
        let options = SyncOptions {
            dry_run,
            ..SyncOptions {
                state_directory: options.state_directory.clone(),
                ..Default::default()
            }
        };
        assert_eq!(
            inventory_sync(&api, &snapshot, &options)
                .await
                .unwrap_err()
                .error_code,
            "catalog_too_large"
        );
    }
    assert_eq!(protocol.state.lock().await.posts, 0);
    assert!(protocol.state.lock().await.deletes.is_empty());
    assert_eq!(protocol.connections.load(Ordering::SeqCst), 0);
    assert!(!directory.path().join("state").exists());
    assert_eq!(store.counts().await.unwrap().0, 0);
    backend.close().await.unwrap();
    context.shutdown().await.unwrap();
    store.close().await;
}

#[tokio::test]
async fn toml_remote_only_catalog_keeps_trusted_http_home_assistant_transport() {
    use closet::source::types::{InventoryEntry, Style, Variant};
    #[derive(Serialize, Deserialize)]
    struct Document {
        schema: u8,
        styles: Vec<Style>,
        variants: Vec<Variant>,
        inventory: Vec<InventoryEntry>,
    }
    let fixture = Fixture::new().await;
    let input = toml_source::write(fixture.directory.path());
    let mut source: Document = toml::from_str(&std::fs::read_to_string(&input).unwrap()).unwrap();
    let remote = source.inventory[1].images.as_ref().unwrap()[0].clone();
    source.variants[0].images = vec![remote];
    for entry in &mut source.inventory {
        entry.images = None;
    }
    std::fs::remove_dir_all(fixture.directory.path().join("wardrobe-images")).unwrap();
    std::fs::write(&input, toml::to_string_pretty(&source).unwrap()).unwrap();
    let source = SourceSnapshot::from_toml(&input).unwrap();
    let http = dropshot::ServerBuilder::new(
        native_api(),
        fixture.protocol.clone(),
        slog::Logger::root(slog::Discard, slog::o!()),
    )
    .start()
    .unwrap();
    let api = ApiClient::new(
        format!("http://{}", http.local_addr()).parse().unwrap(),
        TOKEN,
        Transport::HomeAssistant,
        Duration::from_secs(10),
    )
    .unwrap();
    for dry_run in [true, false] {
        let options = SyncOptions {
            dry_run,
            state_directory: Some(fixture.directory.path().join("state")),
            ..Default::default()
        };
        let result = inventory_sync(&api, &source, &options).await.unwrap();
        assert_eq!(result.data.verified, !dry_run);
        assert_eq!(result.data.inventory.unwrap().created, 2);
    }
    assert_eq!(fixture.store.counts().await.unwrap(), (1, 2, 0));
    assert_eq!(fixture.protocol.state.lock().await.posts, 0);
    assert!(fixture.protocol.state.lock().await.deletes.is_empty());
    http.close().await.unwrap();
    fixture.close().await;
}
