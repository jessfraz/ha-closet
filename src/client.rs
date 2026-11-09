//! Typed Closet transport shared by the CLI's complete sync operation.
use crate::models::{FieldError, ValidationError};
use clap::ValueEnum;
use reqwest::{
    Client, Method, RequestBuilder, Url,
    header::{AUTHORIZATION, HeaderValue},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::time::Duration;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub type Result<T> = std::result::Result<T, Failure>;
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Transport {
    Native,
    HomeAssistant,
}
impl Transport {
    pub fn url_env(self) -> &'static str {
        match self {
            Self::Native => "CLOSET_API_URL",
            Self::HomeAssistant => "HOME_ASSISTANT_URL",
        }
    }
    pub fn token_env(self) -> &'static str {
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

#[derive(Debug, Serialize)]
pub struct Output<T> {
    pub status: Option<u16>,
    pub etag: Option<String>,
    pub data: T,
}

#[derive(Debug, Serialize)]
pub struct Failure {
    pub status: Option<u16>,
    pub error_code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<FieldError>>,
}
impl Failure {
    pub fn local(error_code: &'static str) -> Self {
        Self {
            status: None,
            error_code,
            errors: None,
        }
    }
    pub fn http(status: u16, error_code: &'static str) -> Self {
        Self {
            status: Some(status),
            error_code,
            errors: None,
        }
    }
    pub fn catalog(error: ValidationError) -> Self {
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

pub struct ApiClient {
    pub(crate) client: Client,
    pub(crate) origin: Url,
    pub(crate) authorization: HeaderValue,
    pub(crate) transport: Transport,
    pub(crate) connector: Option<tokio_tungstenite::Connector>,
}
impl ApiClient {
    pub fn new(origin: Url, token: &str, transport: Transport, timeout: Duration) -> Result<Self> {
        Self::with_tls_trust(origin, token, transport, timeout, None, None)
    }
    /// Explicit extra trust for installations with their own CA. Normal
    /// certificate and hostname verification remain enabled on both transports.
    pub fn with_tls_trust(
        origin: Url,
        token: &str,
        transport: Transport,
        timeout: Duration,
        certificate: Option<reqwest::Certificate>,
        connector: Option<tokio_tungstenite::Connector>,
    ) -> Result<Self> {
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
        if token.is_empty() || token.len() > 4096 || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(Failure::local("invalid_credential"));
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| Failure::local("invalid_credential"))?;
        authorization.set_sensitive(true);
        let mut builder = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(10))
            .timeout(timeout);
        if let Some(certificate) = certificate {
            builder = builder.add_root_certificate(certificate);
        }
        let client = builder
            .build()
            .map_err(|_| Failure::local("client_setup_failed"))?;
        let connector = match connector {
            Some(connector) => connector,
            None => {
                let roots = rustls::RootCertStore {
                    roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
                };
                let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
                    rustls::crypto::aws_lc_rs::default_provider(),
                ))
                .with_safe_default_protocol_versions()
                .map_err(|_| Failure::local("client_setup_failed"))?
                .with_root_certificates(roots)
                .with_no_client_auth();
                tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(config))
            }
        };
        Ok(Self {
            client,
            origin,
            authorization,
            transport,
            connector: Some(connector),
        })
    }
    pub fn request(&self, method: Method, path: &str) -> Result<RequestBuilder> {
        let url = self
            .origin
            .join(&format!("{}{path}", self.transport.prefix()))
            .map_err(|_| Failure::local("invalid_path"))?;
        Ok(self
            .client
            .request(method, url)
            .header(AUTHORIZATION, self.authorization.clone()))
    }
}
impl From<crate::images::ImageError> for Failure {
    fn from(error: crate::images::ImageError) -> Self {
        Self::local(error.0)
    }
}

impl From<crate::source::SourceError> for Failure {
    fn from(error: crate::source::SourceError) -> Self {
        Self {
            status: None,
            error_code: error.code,
            errors: Some(
                error
                    .errors
                    .into_iter()
                    .map(|error| FieldError {
                        index: None,
                        field: error.field,
                        code: error.code.to_owned(),
                    })
                    .collect(),
            ),
        }
    }
}

pub async fn response<T: DeserializeOwned>(request: RequestBuilder) -> Result<Output<T>> {
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
        Some("administrator_required") => "administrator_required",
        Some("unauthorized") => "unauthorized",
        Some("forbidden") => "forbidden",
        Some("not_found") => "not_found",
        Some("revision_conflict") => "revision_conflict",
        Some("tracking_revision_conflict") => "tracking_revision_conflict",
        Some("reader_cleanup_required") => "reader_cleanup_required",
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
        Some("sync_in_progress") => "sync_in_progress",
        Some("sync_fence_expired") => "sync_fence_expired",
        Some("cleanup_pending") => "cleanup_pending",
        Some("image_upload_pending") => "image_upload_pending",
        Some("image_referenced") => "image_referenced",
        Some("invalid_prune_image") => "invalid_prune_image",
        Some("invalid_prune_batch") => "invalid_prune_batch",
        Some("areas") => "areas",
        Some("binding_changed") => "binding_changed",
        Some("borrow_unsupported") => "borrow_unsupported",
        Some("catalog_archived") => "catalog_archived",
        Some("catalog_changed") => "catalog_changed",
        Some("catalog_quantity_known") => "catalog_quantity_known",
        Some("clock_moved_backwards") => "clock_moved_backwards",
        Some("config") => "config",
        Some("configuration_changed") => "configuration_changed",
        Some("configuration_history_conflict") => "configuration_history_conflict",
        Some("coverage_history_conflict") => "coverage_history_conflict",
        Some("coverage_changed") => "coverage_changed",
        Some("duplicate_unit") => "duplicate_unit",
        Some("empty_observation") => "empty_observation",
        Some("enrollment_expired") => "enrollment_expired",
        Some("enrollment_not_isolated_fresh") => "enrollment_not_isolated_fresh",
        Some("enrollment_requires_explicit_borrow") => "enrollment_requires_explicit_borrow",
        Some("enrollment_restore_required") => "enrollment_restore_required",
        Some("enrollment_selection_changed") => "enrollment_selection_changed",
        Some("enrollment_session_limit") => "enrollment_session_limit",
        Some("enrollment_station_busy") => "enrollment_station_busy",
        Some("epc_already_bound") => "epc_already_bound",
        Some("history_conflict") => "history_conflict",
        Some("history_would_be_lost") => "history_would_be_lost",
        Some("import_unit_identity_conflict") => "import_unit_identity_conflict",
        Some("invalid_borrow_transition") => "invalid_borrow_transition",
        Some("invalid_analytics_window") => "invalid_analytics_window",
        Some("invalid_coverage") => "invalid_coverage",
        Some("invalid_enrollment_queue") => "invalid_enrollment_queue",
        Some("invalid_history_page") => "invalid_history_page",
        Some("invalid_installation") => "invalid_installation",
        Some("invalid_note") => "invalid_note",
        Some("invalid_observation") => "invalid_observation",
        Some("invalid_operation") => "invalid_operation",
        Some("invalid_reader_control") => "invalid_reader_control",
        Some("invalid_reader_fingerprint") => "invalid_reader_fingerprint",
        Some("invalid_reader_readback") => "invalid_reader_readback",
        Some("invalid_session_ttl") => "invalid_session_ttl",
        Some("invalid_tracking_config") => "invalid_tracking_config",
        Some("invalid_tracking_export") => "invalid_tracking_export",
        Some("invalid_tracking_coverage") => "invalid_tracking_coverage",
        Some("invalid_tracking_input") => "invalid_tracking_input",
        Some("invalid_unit_count") => "invalid_unit_count",
        Some("lookup_queue_must_be_empty") => "lookup_queue_must_be_empty",
        Some("lookup_read_only") => "lookup_read_only",
        Some("observation_batch_too_large") => "observation_batch_too_large",
        Some("operation_history_conflict") => "operation_history_conflict",
        Some("out_of_order_observation") => "out_of_order_observation",
        Some("ports") => "ports",
        Some("previous_reader_program_not_released") => "previous_reader_program_not_released",
        Some("quantity_reconciliation_required") => "quantity_reconciliation_required",
        Some("quantity_unknown") => "quantity_unknown",
        Some("reader_capabilities_unavailable") => "reader_capabilities_unavailable",
        Some("reader_disabled") => "reader_disabled",
        Some("reader_not_llrp") => "reader_not_llrp",
        Some("reader_operation_changed") => "reader_operation_changed",
        Some("reader_operation_history_full") => "reader_operation_history_full",
        Some("reader_restore_pending") => "reader_restore_pending",
        Some("readers") => "readers",
        Some("referenced_items_unavailable") => "referenced_items_unavailable",
        Some("retired_reader_session") => "retired_reader_session",
        Some("simulation_isolation") => "simulation_isolation",
        Some("stale_observation") => "stale_observation",
        Some("stored_data_invalid") => "stored_data_invalid",
        Some("thresholds") => "thresholds",
        Some("tracking_export_too_large") => "tracking_export_too_large",
        Some("tracking_input_too_large") => "tracking_input_too_large",
        Some("tracking_limit") => "tracking_limit",
        Some("tracking_revision_required") => "tracking_revision_required",
        Some("unconfigured_port") => "unconfigured_port",
        Some("unconfigured_reader") => "unconfigured_reader",
        Some("undo_not_latest") => "undo_not_latest",
        Some("undo_not_supported") => "undo_not_supported",
        Some("unit_inactive") => "unit_inactive",
        Some("unit_tracking_exempt") => "unit_tracking_exempt",
        Some("unknown_area") => "unknown_area",
        Some("unknown_enrollment_station") => "unknown_enrollment_station",
        _ => "http_error",
    }
}
