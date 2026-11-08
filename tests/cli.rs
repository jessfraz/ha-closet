use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener},
    process::{Command, Output, Stdio},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use chrono::Utc;
use closet::{
    api::{self, Context},
    models::{
        AccessRole, CatalogItem, FieldError, Generation, GenerationStatus, ImageConfidence,
        ImageReference, Item, ItemsPage, Price, Shop, SyncReport, System, TodayRequest,
        ValidationReport,
    },
    store::Store,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

const TOKEN: &str = "example-owner-credential-at-least-32-characters";
const READER: &str = "example-reader-credential-at-least-32-characters";

fn call(args: &[&str], input: &[u8], variables: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ha-closet"));
    command.args(args);
    for variable in [
        "CLOSET_API_URL",
        "CLOSET_API_TOKEN",
        "HOME_ASSISTANT_URL",
        "HOME_ASSISTANT_TOKEN",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env_remove(variable);
    }
    command
        .envs(variables.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    status: Option<u16>,
    etag: Option<String>,
    data: T,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Diagnostic {
    status: Option<u16>,
    error_code: String,
    #[serde(default)]
    errors: Option<Vec<FieldError>>,
}

fn success<T: DeserializeOwned>(output: Output, status: Option<u16>) -> Envelope<T> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let response: Envelope<T> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response.status, status);
    response
}

fn failure(output: Output, status: Option<u16>, code: &str) -> Diagnostic {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let response: Diagnostic = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(response.status, status);
    assert_eq!(response.error_code, code);
    response
}

fn catalog() -> Vec<CatalogItem> {
    vec![CatalogItem {
        id: "example-shirt".into(),
        brand: Some("Example Label".into()),
        name: "Cotton shirt".into(),
        category: "clothing.top".into(),
        color: Some("Blue".into()),
        size: None,
        quantity: Some(1),
        material: Some("Cotton".into()),
        sku: None,
        purchase_source: Some("direct".into()),
        current_ownership_confirmed: false,
        images: vec![ImageReference {
            url: "https://images.example.invalid/shirt.jpg".into(),
            source_url: Some("https://shop.example.invalid/shirt".into()),
            kind: "product_reference".into(),
            confidence: ImageConfidence::Medium,
            notes: Some("Reference in a different color".into()),
        }],
        condition_at_purchase: Some("Pre-owned".into()),
        notes: vec!["Fictional example".into()],
        price: Some(Price {
            amount: 50.0,
            currency: "USD".into(),
            basis: "receipt".into(),
            quantity_basis: "per_item".into(),
        }),
        shop: Some(Shop {
            id: "example-shop".into(),
            label: "Example shop".into(),
            season: None,
            years: vec![2025],
        }),
    }]
}

fn bytes(value: &impl Serialize) -> Vec<u8> {
    serde_json::to_vec(value).unwrap()
}

#[test]
fn validation_is_offline_and_uses_the_full_server_catalog_contract() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let result = success::<ValidationReport>(
        call(
            &["--token-file", "/does-not-exist", "inventory", "validate"],
            &bytes(&catalog()),
            &[
                ("CLOSET_API_URL", &origin),
                ("HOME_ASSISTANT_URL", "not an origin"),
            ],
        ),
        None,
    );
    assert!(result.data.valid);
    assert_eq!(result.data.count, 1);
    assert_eq!(result.data.source_hash.len(), 64);
    assert!(result.etag.is_none());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );

    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("catalog.json");
    std::fs::write(&source, bytes(&catalog())).unwrap();
    let result = success::<ValidationReport>(
        call(
            &["inventory", "validate", "--input", source.to_str().unwrap()],
            b"",
            &[],
        ),
        None,
    );
    assert_eq!(result.data.count, 1);
}

#[test]
fn validation_reports_safe_locations_without_echoing_private_values() {
    let mut items = catalog();
    items[0].name = "sentinel-input-name-must-not-be-printed".into();
    items[0].quantity = Some(0);
    let output = call(&["inventory", "validate"], &bytes(&items), &[]);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("sentinel-input-name"));
    let diagnostic = failure(output, None, "invalid_catalog");
    let errors = diagnostic.errors.unwrap();
    assert!(errors.iter().any(|error| error.index == Some(0)
        && error.field == "quantity"
        && error.code == "quantity_must_be_positive"));

    let output = call(
        &["inventory", "validate"],
        br#"[{"sentinel-secret-key":"sentinel-secret-value"}]"#,
        &[],
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("sentinel-secret"));
    failure(output, None, "invalid_catalog_json");
}

#[test]
fn oversized_input_and_incomplete_write_conditions_fail_without_a_request() {
    failure(
        call(
            &["inventory", "validate"],
            &vec![b' '; 1024 * 1024 + 1],
            &[],
        ),
        None,
        "input_too_large",
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let output = call(&["inventory", "sync"], &[], &[("CLOSET_API_URL", &origin)]);
    assert_eq!(output.status.code(), Some(2));
    failure(output, None, "invalid_arguments");
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

struct Service {
    _directory: tempfile::TempDir,
    store: Store,
    server: dropshot::HttpServer<Arc<Context>>,
    origin: String,
}
impl Service {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("closet.sqlite3"))
            .await
            .unwrap();
        let server = api::start(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            Arc::new(Context::new(store.clone(), TOKEN).with_integration(READER)),
        )
        .unwrap();
        let origin = format!("http://{}", server.local_addr());
        Self {
            _directory: directory,
            store,
            server,
            origin,
        }
    }
    fn call(&self, args: &[&str], input: &[u8]) -> Output {
        call(
            args,
            input,
            &[
                ("CLOSET_API_URL", &self.origin),
                ("CLOSET_API_TOKEN", TOKEN),
            ],
        )
    }
    async fn finish(self) {
        self.server.close().await.unwrap();
        self.store.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_service_sync_reads_and_conditional_replay_preserve_source_fields() {
    let service = Service::new().await;
    let mut items = catalog();
    let mut second_purchase = items[0].clone();
    second_purchase.id = "example-shirt-second".into();
    items.push(second_purchase);
    let source = bytes(&items);
    let preview = success::<SyncReport>(
        service.call(&["inventory", "sync", "--dry-run"], &source),
        Some(200),
    );
    assert_eq!(preview.data.created, 2);
    assert_eq!(preview.etag.as_deref(), Some("\"0\""));
    let applied = success::<SyncReport>(
        service.call(
            &[
                "inventory",
                "sync",
                "--if-match",
                "\"0\"",
                "--idempotency-key",
                "example-catalog-1",
            ],
            &source,
        ),
        Some(200),
    );
    assert_eq!(applied.data.catalog_revision, 1);
    let replay = success::<SyncReport>(
        service.call(
            &[
                "inventory",
                "sync",
                "--if-match",
                "\"0\"",
                "--idempotency-key",
                "example-catalog-1",
            ],
            &source,
        ),
        Some(200),
    );
    assert_eq!(replay.data, applied.data);
    let item = success::<Item>(
        service.call(&["items", "get", "example-shirt"], b""),
        Some(200),
    );
    assert_eq!(item.data.catalog, catalog().remove(0));
    assert!(item.etag.is_some());
    let page = success::<ItemsPage>(
        service.call(
            &["items", "list", "--brand", "Example Label", "--limit", "1"],
            b"",
        ),
        Some(200),
    );
    assert_eq!(page.data.total, 2);
    assert_eq!(page.data.items.len(), 1);
    assert_eq!(page.data.items[0].catalog.id, "example-shirt");
    let next = success::<ItemsPage>(
        service.call(
            &[
                "items",
                "list",
                "--page-token",
                page.data.next_page.as_deref().unwrap(),
            ],
            b"",
        ),
        Some(200),
    );
    assert_eq!(next.data.items.len(), 1);
    assert_eq!(next.data.items[0].catalog.id, "example-shirt-second");
    let human = service.call(&["items", "list", "--format", "table"], b"");
    assert!(human.status.success());
    assert!(String::from_utf8_lossy(&human.stdout).contains("Cotton shirt"));
    assert!(String::from_utf8_lossy(&human.stdout).contains("unconfirmed"));
    failure(
        service.call(
            &[
                "inventory",
                "sync",
                "--if-match",
                "\"0\"",
                "--idempotency-key",
                "example-catalog-2",
            ],
            &source,
        ),
        Some(412),
        "revision_conflict",
    );
    let unchanged = success::<System>(service.call(&["system"], b""), Some(200));
    assert_eq!(unchanged.data.catalog_revision, 1);
    assert_eq!(unchanged.data.item_count, 2);
    service.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_service_source_filter_preserves_identity_encoding_and_pagination() {
    let service = Service::new().await;
    let mut items = catalog();
    let mut second_shop_item = items[0].clone();
    second_shop_item.id = "example-shirt-second".into();
    items.push(second_shop_item);
    let mut purchase = items[0].clone();
    purchase.id = "example-purchase".into();
    purchase.shop = None;
    purchase.purchase_source = Some("Example resale + archive & label=été/?#%".into());
    items.push(purchase);
    let mut direct = items[0].clone();
    direct.id = "example-direct".into();
    direct.shop = None;
    items.push(direct);
    let mut unspecified = items[0].clone();
    unspecified.id = "example-unspecified".into();
    unspecified.shop = None;
    unspecified.purchase_source = None;
    items.push(unspecified);
    success::<SyncReport>(
        service.call(
            &[
                "inventory",
                "sync",
                "--if-match",
                "\"0\"",
                "--idempotency-key",
                "example-sources",
            ],
            &bytes(&items),
        ),
        Some(200),
    );

    for (source, expected) in [
        (
            "purchase:Example resale + archive & label=été/?#%",
            "example-purchase",
        ),
        ("purchase:direct", "example-direct"),
        ("unspecified", "example-unspecified"),
    ] {
        let result = success::<ItemsPage>(
            service.call(&["items", "list", "--source", source], b""),
            Some(200),
        );
        assert_eq!(result.data.total, 1);
        assert_eq!(result.data.scope_total, 5);
        assert_eq!(result.data.items.len(), 1);
        assert_eq!(result.data.items[0].catalog.id, expected);
    }

    let first = success::<ItemsPage>(
        service.call(
            &[
                "items",
                "list",
                "--source",
                "shop:example-shop",
                "--limit",
                "1",
            ],
            b"",
        ),
        Some(200),
    );
    assert_eq!(first.data.total, 2);
    assert_eq!(first.data.items.len(), 1);
    assert_eq!(first.data.items[0].catalog.id, "example-shirt");
    let cursor = first.data.next_page.as_deref().unwrap();
    let next = success::<ItemsPage>(
        service.call(
            &[
                "items",
                "list",
                "--page-token",
                cursor,
                "--limit",
                "1",
                "--format",
                "json",
            ],
            b"",
        ),
        Some(200),
    );
    assert_eq!(next.data.total, 2);
    assert_eq!(next.data.items.len(), 1);
    assert_eq!(next.data.items[0].catalog.id, "example-shirt-second");
    let table = service.call(
        &[
            "items",
            "list",
            "--source",
            "shop:example-shop",
            "--format",
            "table",
        ],
        b"",
    );
    assert!(table.status.success());
    assert!(String::from_utf8_lossy(&table.stdout).contains("2 matching records"));
    service.finish().await;
}

#[test]
fn invalid_source_filters_fail_without_a_request_or_echoing_the_value() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    for source in [
        "shop:",
        "shop:sentinel-private\ninvalid",
        "purchase:",
        "unknown:sentinel-private",
        "unspecified:sentinel-private",
    ] {
        let output = call(
            &["items", "list", "--source", source],
            b"",
            &[("CLOSET_API_URL", &origin), ("CLOSET_API_TOKEN", TOKEN)],
        );
        assert_eq!(output.status.code(), Some(2));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("sentinel-private"));
        failure(output, None, "invalid_arguments");
    }
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn page_tokens_reject_filters_locally_without_echoing_private_values() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    for filter in [
        vec!["--q", "sentinel-private-search"],
        vec!["--brand", "sentinel-private-brand"],
        vec!["--category", "sentinel-private-category"],
        vec!["--source", "purchase:sentinel-private-source"],
        vec!["--confirmed-only"],
    ] {
        let mut args = vec![
            "--timeout",
            "1",
            "items",
            "list",
            "--page-token",
            "sentinel-private-cursor",
        ];
        args.extend(filter);
        let output = call(
            &args,
            b"",
            &[("CLOSET_API_URL", &origin), ("CLOSET_API_TOKEN", TOKEN)],
        );
        assert_eq!(output.status.code(), Some(2));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("sentinel-private"));
        failure(output, None, "invalid_arguments");
    }
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[derive(Debug)]
struct Captured {
    headers: String,
    body: Vec<u8>,
}

fn fixture(
    status: u16,
    extra_headers: &str,
    body: Vec<u8>,
) -> (String, mpsc::Receiver<Captured>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let extra_headers = extra_headers.to_owned();
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("fixture accept: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
            assert!(bytes.len() <= 16 * 1024);
        }
        let headers = String::from_utf8(bytes).unwrap();
        let length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|value| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        let mut request_body = vec![0; length];
        stream.read_exact(&mut request_body).unwrap();
        tx.send(Captured {
            headers,
            body: request_body,
        })
        .unwrap();
        write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n", body.len()).unwrap();
        stream.write_all(&body).unwrap();
    });
    (origin, rx, worker)
}

fn system() -> System {
    System {
        version: "0.1.0".into(),
        role: AccessRole::Owner,
        catalog_revision: 0,
        item_count: 0,
        confirmed_count: 0,
        archived_count: 0,
        today_configured: false,
    }
}

#[test]
fn transport_selection_keeps_credentials_and_ha_paths_separate() {
    let (origin, requests, worker) = fixture(200, "", bytes(&system()));
    let result = success::<System>(
        call(
            &["--transport", "home-assistant", "system"],
            b"",
            &[
                ("CLOSET_API_URL", "http://127.0.0.1:1"),
                ("CLOSET_API_TOKEN", "native-secret-not-selected"),
                ("HOME_ASSISTANT_URL", &origin),
                ("HOME_ASSISTANT_TOKEN", "example-ha-token"),
            ],
        ),
        Some(200),
    );
    assert_eq!(result.data.item_count, 0);
    let request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(
        request
            .headers
            .starts_with("GET /api/closet/v1/system HTTP/1.1")
    );
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer example-ha-token")
    );
    assert!(!request.headers.contains("native-secret-not-selected"));
    assert!(
        !request
            .headers
            .to_ascii_lowercase()
            .contains("x-closet-actor")
    );
    worker.join().unwrap();

    failure(
        call(
            &["system"],
            b"",
            &[
                ("CLOSET_API_URL", "http://127.0.0.1:1"),
                ("HOME_ASSISTANT_URL", "http://127.0.0.1:2"),
            ],
        ),
        None,
        "transport_required",
    );
    failure(
        call(
            &["--transport", "native", "system"],
            b"",
            &[
                ("CLOSET_API_URL", "http://127.0.0.1:1"),
                ("HOME_ASSISTANT_TOKEN", "not-a-native-credential"),
            ],
        ),
        None,
        "credential_required",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn source_filter_survives_home_assistant_transport_query_encoding() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("closet.sqlite3"))
        .await
        .unwrap();
    let page = store.list(Default::default()).await.unwrap();
    let (origin, requests, worker) = fixture(200, "ETag: \"0\"\r\n", bytes(&page));
    let source = "purchase:Example resale + archive & label=été/?#%";
    let result = success::<ItemsPage>(
        call(
            &[
                "--transport",
                "home-assistant",
                "items",
                "list",
                "--source",
                source,
                "--confirmed-only",
            ],
            b"",
            &[
                ("HOME_ASSISTANT_URL", &origin),
                ("HOME_ASSISTANT_TOKEN", "example-ha-token"),
            ],
        ),
        Some(200),
    );
    assert_eq!(result.etag.as_deref(), Some("\"0\""));
    assert_eq!(result.data.total, page.total);
    assert_eq!(result.data.catalog_revision, page.catalog_revision);
    let request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    let target = request.headers.lines().next().unwrap();
    let target = target.strip_prefix("GET ").unwrap();
    let target = target.strip_suffix(" HTTP/1.1").unwrap();
    let url = url::Url::parse(&format!("{origin}{target}")).unwrap();
    assert_eq!(url.path(), "/api/closet/v1/items");
    assert!(url.fragment().is_none());
    assert_eq!(
        url.query_pairs().collect::<Vec<_>>(),
        vec![
            ("source".into(), source.into()),
            ("confirmed_only".into(), "true".into()),
        ]
    );
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer example-ha-token")
    );
    worker.join().unwrap();
    store.close().await;
}

#[test]
fn generation_is_one_accepted_request_with_typed_inputs_not_a_poll_loop() {
    let accepted = Generation {
        id: "example-generation".into(),
        status: GenerationStatus::Running,
        inputs: TodayRequest::default(),
        started_at: Utc::now(),
        finished_at: None,
        error_code: None,
    };
    let (origin, requests, worker) = fixture(202, "", bytes(&accepted));
    let result = success::<Generation>(
        call(
            &[
                "today",
                "generate",
                "--idempotency-key",
                "example-generation-1",
            ],
            b"",
            &[("CLOSET_API_URL", &origin), ("CLOSET_API_TOKEN", TOKEN)],
        ),
        Some(202),
    );
    assert!(matches!(result.data.status, GenerationStatus::Running));
    let request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(
        request
            .headers
            .starts_with("POST /v1/today/generate HTTP/1.1")
    );
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("idempotency-key: example-generation-1")
    );
    let body: TodayRequest = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body, TodayRequest::default());
    worker.join().unwrap();
}

#[test]
fn redirects_and_untrusted_error_bodies_never_reach_credentials_or_diagnostics() {
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let (origin, requests, worker) = fixture(
        302,
        &format!(
            "Location: http://{}/sentinel-private-location\r\n",
            target.local_addr().unwrap()
        ),
        b"sentinel-private-body".to_vec(),
    );
    let output = call(
        &["system"],
        b"",
        &[("CLOSET_API_URL", &origin), ("CLOSET_API_TOKEN", TOKEN)],
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("sentinel-private"));
    failure(output, Some(302), "redirect_refused");
    requests.recv_timeout(Duration::from_secs(2)).unwrap();
    worker.join().unwrap();
    assert_eq!(
        target.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );

    let (origin, requests, worker) = fixture(
        503,
        "",
        br#"{"error_code":"sentinel-private-code","message":"sentinel-private-message"}"#.to_vec(),
    );
    let output = call(
        &["system"],
        b"",
        &[("CLOSET_API_URL", &origin), ("CLOSET_API_TOKEN", TOKEN)],
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("sentinel-private"));
    failure(output, Some(503), "http_error");
    requests.recv_timeout(Duration::from_secs(2)).unwrap();
    worker.join().unwrap();
}

#[cfg(unix)]
#[test]
fn token_files_require_private_permissions_and_do_not_echo_paths() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let token_path = directory.path().join("sentinel-private-token-file");
    std::fs::write(&token_path, "example-file-token\n").unwrap();
    std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let output = call(
        &["--token-file", token_path.to_str().unwrap(), "system"],
        b"",
        &[("CLOSET_API_URL", "http://127.0.0.1:1")],
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("sentinel-private"));
    failure(output, None, "invalid_credential_file");
    std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let (origin, requests, worker) = fixture(200, "", bytes(&system()));
    success::<System>(
        call(
            &["--token-file", token_path.to_str().unwrap(), "system"],
            b"",
            &[("CLOSET_API_URL", &origin)],
        ),
        Some(200),
    );
    let request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer example-file-token")
    );
    worker.join().unwrap();
}
