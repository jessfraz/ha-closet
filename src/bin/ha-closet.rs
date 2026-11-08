//! A transport client for the catalog API. Validation is deliberately offline.
use std::{
    fs::File,
    io::{Read, Write},
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use closet::models::{
    FieldError, Freshness, Generation, GenerationStatus, Item, ItemsPage, Occasion, Source,
    SyncReport, SyncRequest, System, TodayRequest, TodayResponse, ValidationError, WeatherMode,
    parse_catalog, valid_id,
};
use reqwest::{
    Client, Method, RequestBuilder, Url,
    header::{AUTHORIZATION, HeaderValue},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

const MAX_INPUT_BYTES: u64 = closet::models::MAX_CATALOG_BYTES as u64;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
type Result<T> = std::result::Result<T, Failure>;

#[derive(Parser)]
#[command(
    name = "ha-closet",
    version,
    about = "Validate a wardrobe JSON file or use the Closet API",
    long_about = "Validate a wardrobe JSON file or use the Closet API. Validation is offline. Network commands make one request, never retry or follow redirects, and never fetch another page automatically."
)]
struct Cli {
    /// Required when both native and Home Assistant origins are configured.
    #[arg(long, value_enum, global = true)]
    transport: Option<Transport>,
    /// Override the selected origin. No path, query, fragment, or credentials.
    #[arg(long, global = true)]
    url: Option<String>,
    /// Read the selected credential from an owner-only file instead of its env.
    #[arg(long, global = true)]
    token_file: Option<PathBuf>,
    /// Total request deadline, including the response body, in seconds.
    #[arg(long, default_value = "30", value_parser = clap::value_parser!(u64).range(1..=300), global = true)]
    timeout: u64,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Transport {
    Native,
    HomeAssistant,
}
impl Transport {
    fn url_env(self) -> &'static str {
        match self {
            Self::Native => "CLOSET_API_URL",
            Self::HomeAssistant => "HOME_ASSISTANT_URL",
        }
    }
    fn token_env(self) -> &'static str {
        match self {
            Self::Native => "CLOSET_API_TOKEN",
            Self::HomeAssistant => "HOME_ASSISTANT_TOKEN",
        }
    }
    fn prefix(self) -> &'static str {
        match self {
            Self::Native => "/v1",
            Self::HomeAssistant => "/api/closet/v1",
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Read versions, catalog counts, and service availability.
    System,
    #[command(subcommand)]
    Inventory(Inventory),
    #[command(subcommand)]
    Items(Items),
    #[command(subcommand)]
    Today(Today),
}

#[derive(Subcommand)]
enum Inventory {
    /// Validate the complete JSON catalog locally. No credentials or network.
    Validate(Input),
    /// Preview or apply one complete authoritative catalog snapshot.
    Sync {
        #[command(flatten)]
        input: Input,
        /// Preview changes without writing. Does not require a revision or key.
        #[arg(long, conflicts_with_all = ["if_match", "idempotency_key"])]
        dry_run: bool,
        /// Permit an explicitly empty catalog to archive every existing item.
        #[arg(long)]
        allow_empty: bool,
        /// Exact quoted catalog ETag from preview, e.g. '"0"'.
        #[arg(long, required_unless_present = "dry_run", value_parser = parse_revision)]
        if_match: Option<String>,
        /// Caller-owned stable key. Reuse with the exact body after an uncertain reply.
        #[arg(long, required_unless_present = "dry_run", value_parser = parse_key)]
        idempotency_key: Option<String>,
    },
}

#[derive(Subcommand)]
enum Items {
    /// Read exactly one page. Continue with its cursor, not filters.
    List {
        #[arg(long)]
        q: Option<String>,
        #[arg(long)]
        brand: Option<String>,
        #[arg(long)]
        category: Option<String>,
        /// Exact source key: shop:<id> or purchase:<value>.
        #[arg(long)]
        source: Option<Source>,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=100))]
        limit: Option<u32>,
        /// Continue the applied query without repeating filters.
        #[arg(long, conflicts_with_all = ["q", "brand", "category", "source"])]
        page_token: Option<String>,
        #[arg(long, value_enum, default_value = "json")]
        format: Format,
    },
    Get {
        #[arg(value_parser = parse_id)]
        id: String,
    },
}

#[derive(Subcommand)]
enum Today {
    /// Read the current result and attempt. Does not generate or poll.
    Get {
        #[arg(long, value_enum, default_value = "json")]
        format: Format,
    },
    /// Request one attempt. A 202 result means accepted, not completed.
    Generate {
        #[arg(long, default_value = "everyday", value_parser = parse_occasion)]
        occasion: Occasion,
        #[arg(long, default_value = "required", value_parser = parse_weather_mode)]
        weather_mode: WeatherMode,
        #[arg(long, value_parser = parse_key)]
        idempotency_key: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Json,
    Table,
}

#[derive(Args)]
struct Input {
    /// JSON array file, or - for stdin. Never interpreted as a URL.
    #[arg(long, default_value = "-")]
    input: PathBuf,
}
impl Input {
    fn read(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        if self.input.as_os_str() == "-" {
            std::io::stdin()
                .take(MAX_INPUT_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| Failure::local("input_read_failed"))?;
        } else {
            File::open(&self.input)
                .map_err(|_| Failure::local("input_read_failed"))?
                .take(MAX_INPUT_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| Failure::local("input_read_failed"))?;
        }
        if bytes.len() > MAX_INPUT_BYTES as usize {
            return Err(Failure::local("input_too_large"));
        }
        Ok(bytes)
    }
}

fn parse_revision(value: &str) -> std::result::Result<String, &'static str> {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|revision| *revision >= 0)
        .map(|_| value.to_owned())
        .ok_or("expected a quoted nonnegative catalog revision")
}
fn parse_key(value: &str) -> std::result::Result<String, &'static str> {
    if value.is_empty() || value.len() > 200 || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        Err("expected 1..200 printable ASCII characters without whitespace")
    } else {
        Ok(value.to_owned())
    }
}
fn parse_id(value: &str) -> std::result::Result<String, &'static str> {
    if !valid_id(value) {
        Err("expected an opaque catalog item ID")
    } else {
        Ok(value.to_owned())
    }
}
fn parse_occasion(value: &str) -> std::result::Result<Occasion, &'static str> {
    value
        .parse()
        .map_err(|_| "expected everyday, work, or dinner")
}
fn parse_weather_mode(value: &str) -> std::result::Result<WeatherMode, &'static str> {
    value.parse().map_err(|_| "expected required or ignored")
}

#[derive(Serialize)]
struct Output<T> {
    status: Option<u16>,
    etag: Option<String>,
    data: T,
}

#[derive(Serialize)]
struct Failure {
    status: Option<u16>,
    error_code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    errors: Option<Vec<FieldError>>,
}
impl Failure {
    fn local(error_code: &'static str) -> Self {
        Self {
            status: None,
            error_code,
            errors: None,
        }
    }
    fn http(status: u16, error_code: &'static str) -> Self {
        Self {
            status: Some(status),
            error_code,
            errors: None,
        }
    }
    fn catalog(error: ValidationError) -> Self {
        Self {
            status: None,
            error_code: match error.code.as_str() {
                "catalog_too_large" => "catalog_too_large",
                "invalid_catalog_json" => "invalid_catalog_json",
                _ => "invalid_catalog",
            },
            errors: Some(error.errors),
        }
    }
}

struct Api {
    client: Client,
    origin: Url,
    authorization: HeaderValue,
    transport: Transport,
}
impl Api {
    fn new(cli: &Cli) -> Result<Self> {
        let native_url = std::env::var(Transport::Native.url_env()).ok();
        let ha_url = std::env::var(Transport::HomeAssistant.url_env()).ok();
        let transport = match cli.transport {
            Some(transport) => transport,
            None if native_url.is_some() && ha_url.is_some() => {
                return Err(Failure::local("transport_required"));
            }
            None if ha_url.is_some() && cli.url.is_none() => Transport::HomeAssistant,
            None => Transport::Native,
        };
        let configured_url = cli
            .url
            .clone()
            .or(match transport {
                Transport::Native => native_url,
                Transport::HomeAssistant => ha_url,
            })
            .ok_or_else(|| Failure::local("url_required"))?;
        let origin = Url::parse(&configured_url).map_err(|_| Failure::local("invalid_url"))?;
        if !matches!(origin.scheme(), "http" | "https")
            || origin.host_str().is_none()
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
        {
            return Err(Failure::local("invalid_url"));
        }
        let token = if let Some(path) = &cli.token_file {
            read_token_file(path)?
        } else {
            std::env::var(transport.token_env())
                .map_err(|_| Failure::local("credential_required"))?
        };
        if token.is_empty()
            || token.len() > 4096
            || !token.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(Failure::local("invalid_credential"));
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| Failure::local("invalid_credential"))?;
        authorization.set_sensitive(true);
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(cli.timeout))
            .build()
            .map_err(|_| Failure::local("client_setup_failed"))?;
        Ok(Self {
            client,
            origin,
            authorization,
            transport,
        })
    }

    fn request(&self, method: Method, path: &str) -> Result<RequestBuilder> {
        let path = format!("{}{path}", self.transport.prefix());
        let url = self
            .origin
            .join(&path)
            .map_err(|_| Failure::local("invalid_path"))?;
        Ok(self
            .client
            .request(method, url)
            .header(AUTHORIZATION, self.authorization.clone()))
    }
}

fn read_token_file(path: &std::path::Path) -> Result<String> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|_| Failure::local("credential_read_failed"))?;
    if !metadata.is_file() || metadata.len() > 4098 {
        return Err(Failure::local("invalid_credential_file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Failure::local("invalid_credential_file"));
        }
    }
    let token =
        std::fs::read_to_string(path).map_err(|_| Failure::local("credential_read_failed"))?;
    Ok(token.trim_end_matches(['\r', '\n']).to_owned())
}

fn write_json(writer: impl Write, value: &impl Serialize) -> Result<()> {
    let mut writer = writer;
    serde_json::to_writer(&mut writer, value).map_err(|_| Failure::local("output_failed"))?;
    writer
        .write_all(b"\n")
        .map_err(|_| Failure::local("output_failed"))
}

async fn response<T: DeserializeOwned>(request: RequestBuilder) -> Result<Output<T>> {
    let mut response = request
        .send()
        .await
        .map_err(|_| Failure::local("request_failed"))?;
    let status = response.status().as_u16();
    let etag = response
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Failure::http(status, "response_read_failed"))?
    {
        if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(Failure::http(status, "response_too_large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    if !(200..300).contains(&status) {
        return Err(Failure::http(status, remote_error_code(status, &bytes)));
    }
    let data = serde_json::from_slice(&bytes)
        .map_err(|_| Failure::http(status, "invalid_json_response"))?;
    Ok(Output {
        status: Some(status),
        etag,
        data,
    })
}

fn remote_error_code(status: u16, bytes: &[u8]) -> &'static str {
    if (300..400).contains(&status) {
        return "redirect_refused";
    }
    #[derive(Deserialize)]
    struct ApiError {
        error_code: Option<String>,
        code: Option<String>,
    }
    let code = serde_json::from_slice::<ApiError>(bytes)
        .ok()
        .and_then(|error| error.error_code.or(error.code));
    // An unknown remote code or message may contain private input. Only this
    // fixed vocabulary is safe to emit as a diagnostic.
    match code.as_deref() {
        Some("invalid_input") => "invalid_input",
        Some("invalid_catalog") => "invalid_catalog",
        Some("invalid_catalog_json") => "invalid_catalog_json",
        Some("invalid_limit") => "invalid_limit",
        Some("invalid_filter") => "invalid_filter",
        Some("owner_required") => "owner_required",
        Some("admin_required") => "admin_required",
        Some("unauthorized") => "unauthorized",
        Some("forbidden") => "forbidden",
        Some("not_found") => "not_found",
        Some("revision_conflict") => "revision_conflict",
        Some("precondition_required") => "precondition_required",
        Some("idempotency_conflict") => "idempotency_conflict",
        Some("idempotency_key_required") => "idempotency_key_required",
        Some("generation_in_progress") => "generation_in_progress",
        Some("page_invalidated") => "page_invalidated",
        Some("invalid_page_token") => "invalid_page_token",
        Some("empty_catalog_requires_allow_empty") => "empty_catalog_requires_allow_empty",
        Some("weather_unavailable") => "weather_unavailable",
        Some("model_unavailable") => "model_unavailable",
        Some("catalog_too_large") => "catalog_too_large",
        Some("invalid_model_output") => "invalid_model_output",
        Some("storage_unavailable") => "storage_unavailable",
        _ => "http_error",
    }
}

async fn send<T: DeserializeOwned + Serialize>(request: RequestBuilder) -> Result<()> {
    write_json(std::io::stdout().lock(), &response::<T>(request).await?)
}

async fn run(cli: Cli) -> Result<()> {
    // This branch deliberately precedes URL/credential/transport resolution.
    // CI validation uses the server's parser without building an HTTP client.
    if let Command::Inventory(Inventory::Validate(input)) = &cli.command {
        let catalog = parse_catalog(&input.read()?).map_err(Failure::catalog)?;
        return write_json(
            std::io::stdout().lock(),
            &Output {
                status: None,
                etag: None,
                data: catalog.report(),
            },
        );
    }

    // Parse source files before touching the network, including a dry run.
    let sync_catalog = if let Command::Inventory(Inventory::Sync { input, .. }) = &cli.command {
        Some(parse_catalog(&input.read()?).map_err(Failure::catalog)?)
    } else {
        None
    };
    let api = Api::new(&cli)?;
    match cli.command {
        Command::System => send::<System>(api.request(Method::GET, "/system")?).await,
        Command::Inventory(Inventory::Validate(_)) => Err(Failure::local("invalid_arguments")),
        Command::Inventory(Inventory::Sync {
            dry_run,
            allow_empty,
            if_match,
            idempotency_key,
            ..
        }) => {
            let catalog = sync_catalog.ok_or_else(|| Failure::local("invalid_catalog"))?;
            let body = SyncRequest {
                items: catalog.into_items(),
                dry_run,
                allow_empty,
            };
            let mut request = api.request(Method::POST, "/inventory/sync")?.json(&body);
            if let Some(revision) = if_match {
                request = request.header("If-Match", revision);
            }
            if let Some(key) = idempotency_key {
                request = request.header("Idempotency-Key", key);
            }
            send::<SyncReport>(request).await
        }
        Command::Items(Items::Get { id }) => {
            send::<Item>(api.request(Method::GET, &format!("/items/{id}"))?).await
        }
        Command::Items(Items::List {
            q,
            brand,
            category,
            source,
            limit,
            page_token,
            format,
        }) => {
            let mut query = Vec::new();
            for (key, value) in [
                ("q", q),
                ("brand", brand),
                ("category", category),
                ("source", source.map(|source| source.to_string())),
                ("page_token", page_token),
            ] {
                if let Some(value) = value {
                    query.push((key, value));
                }
            }
            if let Some(limit) = limit {
                query.push(("limit", limit.to_string()));
            }
            let result =
                response::<ItemsPage>(api.request(Method::GET, "/items")?.query(&query)).await?;
            match format {
                Format::Json => write_json(std::io::stdout().lock(), &result),
                Format::Table => write_items_table(&result.data),
            }
        }
        Command::Today(Today::Get { format }) => {
            let result = response::<TodayResponse>(api.request(Method::GET, "/today")?).await?;
            match format {
                Format::Json => write_json(std::io::stdout().lock(), &result),
                Format::Table => write_today_table(&result.data),
            }
        }
        Command::Today(Today::Generate {
            occasion,
            weather_mode,
            idempotency_key,
        }) => {
            send::<Generation>(
                api.request(Method::POST, "/today/generate")?
                    .header("Idempotency-Key", idempotency_key)
                    .json(&TodayRequest {
                        occasion,
                        weather_mode,
                    }),
            )
            .await
        }
    }
}

fn write_items_table(page: &ItemsPage) -> Result<()> {
    let mut writer = std::io::stdout().lock();
    writeln!(writer, "ID\tBRAND\tITEM\tSIZE\tCOLOR")
        .map_err(|_| Failure::local("output_failed"))?;
    for item in &page.items {
        let catalog = &item.catalog;
        writeln!(
            writer,
            "{}\t{}\t{}\t{}\t{}",
            display_text(&catalog.id),
            catalog
                .brand
                .as_deref()
                .map(display_text)
                .unwrap_or_default(),
            display_text(&catalog.name),
            catalog
                .size
                .as_deref()
                .map(display_text)
                .unwrap_or_default(),
            catalog
                .color
                .as_deref()
                .map(display_text)
                .unwrap_or_default()
        )
        .map_err(|_| Failure::local("output_failed"))?;
    }
    writeln!(writer, "{} matching records", page.total)
        .map_err(|_| Failure::local("output_failed"))?;
    if page.next_page.is_some() {
        writeln!(
            writer,
            "More records available; use JSON output for the next_page cursor."
        )
        .map_err(|_| Failure::local("output_failed"))?;
    }
    Ok(())
}

fn write_today_table(today: &TodayResponse) -> Result<()> {
    let mut writer = std::io::stdout().lock();
    writeln!(writer, "{} ({})", today.date, display_text(&today.timezone))
        .map_err(|_| Failure::local("output_failed"))?;
    // JSON remains the exact typed result. This small display mode intentionally
    // does not fetch item names, resolve another page, or generate suggestions.
    if let Some(generation) = &today.generation {
        let status = match generation.status {
            GenerationStatus::Running => "running",
            GenerationStatus::Succeeded => "succeeded",
            GenerationStatus::Failed => "failed",
            GenerationStatus::Interrupted => "interrupted",
        };
        writeln!(writer, "Generation: {status}").map_err(|_| Failure::local("output_failed"))?;
        if let Some(code) = &generation.error_code {
            writeln!(writer, "Attempt error: {}", display_text(code))
                .map_err(|_| Failure::local("output_failed"))?;
        }
    }
    if let Some(result) = &today.result {
        let freshness = match result.freshness {
            Freshness::Ready => "ready",
            Freshness::Stale => "stale",
        };
        writeln!(
            writer,
            "Result: {freshness}; {} / weather {}",
            result.inputs.occasion, result.inputs.weather_mode
        )
        .map_err(|_| Failure::local("output_failed"))?;
        if !result.stale_reasons.is_empty() {
            writeln!(
                writer,
                "Stale reasons: {}",
                result
                    .stale_reasons
                    .iter()
                    .map(|reason| display_text(reason))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            .map_err(|_| Failure::local("output_failed"))?;
        }
        for suggestion in &result.suggestions {
            writeln!(
                writer,
                "{}\t{}",
                display_text(&suggestion.label),
                suggestion
                    .item_ids
                    .iter()
                    .map(|id| display_text(id))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            .map_err(|_| Failure::local("output_failed"))?;
        }
    } else {
        writeln!(writer, "No suggestions for today.")
            .map_err(|_| Failure::local("output_failed"))?;
    }
    Ok(())
}

fn display_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            return if error.print().is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        Err(_) => {
            let _ = write_json(
                std::io::stderr().lock(),
                &Failure::local("invalid_arguments"),
            );
            return ExitCode::from(2);
        }
    };
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = write_json(std::io::stderr().lock(), &error);
            ExitCode::FAILURE
        }
    }
}
