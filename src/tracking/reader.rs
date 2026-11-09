//! Local R700 IoT HTTP input. Radio reports never constitute empty-area proof.
//!
//! Wire fields follow Impinj's published OpenAPI 1.8/1.11, not the obsolete v0
//! tutorial. In particular, antenna activation precedes inventory; it is not a
//! completed scan. No code in this module writes tag memory or reader presets.

#[path = "reader_llrp.rs"]
pub mod llrp;

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::{Duration, Instant},
};

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD},
};
use chrono::{DateTime, Utc};
use reqwest::{
    Client, Response, Url,
    header::{AUTHORIZATION, HeaderValue},
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::watch;
use uuid::Uuid;

use crate::store::Store;
use crate::tracking::models::{
    CoverageProof, DataSource, EnrollmentState, Epc, ObservationBatch, ObservationPurpose,
    PortConfig, PortCoverage, PortId, PortReadback, PortRole, ReaderCapabilities, ReaderConfig,
    ReaderConnection, ReaderCredentials, ReaderFault, ReaderId, ReaderReadback, TagObservation,
    TrackingError,
};

pub const MAX_EVENT_BYTES: usize = 65_536;
const MAX_CHUNK_BYTES: usize = 1_048_576;
const DEDUPLICATION_WINDOW: usize = 8_192;

/// Both clocks are explicit. Wall time validates reader timestamps; monotonic
/// time controls deadlines/backoff and cannot advance absence during downtime.
pub trait ReaderClock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
    fn elapsed(&self) -> Duration;
}

pub struct SystemReaderClock {
    started: Instant,
}
impl Default for SystemReaderClock {
    fn default() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}
impl ReaderClock for SystemReaderClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

/// A reader UTC sample was generated after this local request/connect began.
/// Subtracting that upper bound on reader lead yields a conservative server
/// timestamp, not a future reader timestamp mistaken for post-manual evidence.
#[derive(Clone, Copy, Debug, Default)]
struct ReaderTimeBasis {
    lead: chrono::Duration,
    calibrated: bool,
}
impl ReaderTimeBasis {
    fn from_sample(reader_at: DateTime<Utc>, request_started_at: DateTime<Utc>) -> Self {
        Self {
            lead: reader_at.signed_duration_since(request_started_at),
            calibrated: true,
        }
    }
    fn server_time(self, reader_at: DateTime<Utc>) -> Result<DateTime<Utc>, ReaderError> {
        reader_at
            .checked_sub_signed(self.lead)
            .ok_or(ReaderError::Protocol)
    }
    fn observation_time(
        self,
        reader_at: DateTime<Utc>,
        received_at: DateTime<Utc>,
    ) -> Result<DateTime<Utc>, ReaderError> {
        let normalized = self.server_time(reader_at)?;
        if self.calibrated && normalized > received_at {
            return Err(ReaderError::ClockSkew);
        }
        Ok(normalized)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReaderError {
    #[error("invalid_reader_configuration")]
    Configuration,
    #[error("unsupported_reader_capability")]
    Unsupported,
    #[error("reader_credentials_unavailable")]
    Credentials,
    #[error("reader_transport_failed")]
    Transport,
    #[error("reader_protocol_invalid")]
    Protocol,
    #[error("reader_event_too_large")]
    Oversized,
    #[error("reader_clock_sample_changed")]
    ClockSkew,
    #[error("reader_evidence_refresh_required")]
    EvidenceChanged,
    #[error("reader_state_failed:{0}")]
    State(&'static str),
    #[error("reader_recovery_required")]
    RecoveryRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscardReason {
    Duplicate,
    Stale,
    Future,
    OutOfOrder,
    UnknownEvent,
}

#[derive(Debug)]
pub enum DecodedEvent {
    Batch(Box<ObservationBatch>),
    Discarded(DiscardReason),
    KeepAlive,
}

/// Bounded newline framing for the documented JSON event stream and CRLF
/// keepalives. EOF with an unfinished JSON record is an interrupted stream.
#[derive(Default)]
pub struct EventFramer {
    pending: Vec<u8>,
}
impl EventFramer {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>, ReaderError> {
        if bytes.len() > MAX_CHUNK_BYTES {
            return Err(ReaderError::Oversized);
        }
        let mut records = Vec::new();
        for part in bytes.split_inclusive(|byte| *byte == b'\n') {
            if self.pending.len().saturating_add(part.len()) > MAX_EVENT_BYTES {
                return Err(ReaderError::Oversized);
            }
            self.pending.extend_from_slice(part);
            if part.last() == Some(&b'\n') {
                if records.len() == crate::tracking::models::MAX_BATCH_OBSERVATIONS {
                    return Err(ReaderError::Oversized);
                }
                records.push(std::mem::take(&mut self.pending));
            }
        }
        Ok(records)
    }
    pub fn finish(&self) -> Result<(), ReaderError> {
        if self.pending.iter().all(u8::is_ascii_whitespace) {
            Ok(())
        } else {
            Err(ReaderError::Protocol)
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireEvent {
    timestamp: DateTime<Utc>,
    event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tag_inventory_event: Option<WireTag>,
    #[serde(skip_serializing_if = "Option::is_none")]
    antenna_activation_event: Option<WirePort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    antenna_connected_event: Option<WirePort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    antenna_disconnected_event: Option<WirePort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inventory_status_event: Option<WireInventoryStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    overflow_event: Option<WireOverflow>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireTag {
    epc: Option<String>,
    epc_hex: Option<String>,
    antenna_port: u16,
    last_seen_time: Option<DateTime<Utc>>,
    tid: Option<String>,
    tid_hex: Option<String>,
    peak_rssi_cdbm: Option<i32>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WirePort {
    antenna_port: u16,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct WireInventoryStatus {
    status: InventoryStatus,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum InventoryStatus {
    Idle,
    Running,
    Armed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireOverflow {
    events_purged: Option<u64>,
    events_remaining: Option<u64>,
    first_event_timestamp: Option<DateTime<Utc>>,
    last_event_timestamp: Option<DateTime<Utc>>,
}

/// Converts the same bytes in live and offline/replay modes. A connection epoch
/// is locally generated, NOT a reader boot identifier. R700 v1 offers no stream
/// sequence/resume token. Reconnect starts a fresh epoch and rejects history.
pub struct R700Decoder {
    reader: ReaderId,
    ports: BTreeMap<u16, PortId>,
    connected_at: DateTime<Utc>,
    session: Uuid,
    sequence: u64,
    last_event_at: Option<DateTime<Utc>>,
    source_high_water: Option<DateTime<Utc>>,
    envelope_at: Option<DateTime<Utc>>,
    time_basis: ReaderTimeBasis,
    fingerprints: BTreeSet<[u8; 32]>,
    fingerprint_order: VecDeque<[u8; 32]>,
    max_age: chrono::Duration,
    max_skew: chrono::Duration,
    data_source: DataSource,
}
impl R700Decoder {
    pub fn new(
        reader: ReaderId,
        ports: &[PortConfig],
        connected_at: DateTime<Utc>,
        session: Uuid,
        max_age: Duration,
        max_skew: Duration,
        data_source: DataSource,
    ) -> Result<Self, ReaderError> {
        if session.is_nil()
            || max_age.is_zero()
            || max_age > Duration::from_secs(600)
            || max_skew > Duration::from_secs(5)
        {
            return Err(ReaderError::Configuration);
        }
        let mut mapping = BTreeMap::new();
        for port in ports
            .iter()
            .filter(|port| port.enabled && port.reader_id == reader)
        {
            if port.antenna == 0 || mapping.insert(port.antenna, port.id.clone()).is_some() {
                return Err(ReaderError::Configuration);
            }
        }
        if mapping.is_empty() {
            return Err(ReaderError::Configuration);
        }
        Ok(Self {
            reader,
            ports: mapping,
            connected_at,
            session,
            sequence: 0,
            last_event_at: None,
            source_high_water: None,
            envelope_at: None,
            time_basis: ReaderTimeBasis::default(),
            fingerprints: BTreeSet::new(),
            fingerprint_order: VecDeque::new(),
            max_age: chrono::Duration::from_std(max_age).map_err(|_| ReaderError::Configuration)?,
            max_skew: chrono::Duration::from_std(max_skew)
                .map_err(|_| ReaderError::Configuration)?,
            data_source,
        })
    }

    /// Reconnect retains duplicate fingerprints but replaces the local epoch.
    /// `reader_fence` is the freshly queried reader time, not an estimate made
    /// from buffered tag reports. This also fences readers with a clock offset.
    pub fn reconnect(&mut self, reader_fence: DateTime<Utc>, session: Uuid) {
        self.connected_at = reader_fence.max(self.source_high_water.unwrap_or(reader_fence));
        self.session = session;
        self.sequence = 0;
        self.last_event_at = None;
    }

    pub fn decode(
        &mut self,
        line: &[u8],
        received_at: DateTime<Utc>,
    ) -> Result<DecodedEvent, ReaderError> {
        if line.len() > MAX_EVENT_BYTES {
            return Err(ReaderError::Oversized);
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            return Ok(DecodedEvent::KeepAlive);
        }
        let event: WireEvent = serde_json::from_slice(line).map_err(|_| ReaderError::Protocol)?;
        if event.timestamp <= self.connected_at
            || received_at.signed_duration_since(event.timestamp) > self.max_age
        {
            return Ok(DecodedEvent::Discarded(DiscardReason::Stale));
        }
        if event.timestamp.signed_duration_since(received_at) > self.max_skew {
            return Ok(DecodedEvent::Discarded(DiscardReason::Future));
        }
        let normalized_at = self
            .time_basis
            .observation_time(event.timestamp, received_at)?;
        let fingerprint: [u8; 32] =
            Sha256::digest(serde_json::to_vec(&event).map_err(|_| ReaderError::Protocol)?).into();
        if self.fingerprints.contains(&fingerprint) {
            return Ok(DecodedEvent::Discarded(DiscardReason::Duplicate));
        }
        if self
            .last_event_at
            .is_some_and(|last| event.timestamp < last)
        {
            return Ok(DecodedEvent::Discarded(DiscardReason::OutOfOrder));
        }
        let detail_count = usize::from(event.tag_inventory_event.is_some())
            + usize::from(event.antenna_activation_event.is_some())
            + usize::from(event.antenna_connected_event.is_some())
            + usize::from(event.antenna_disconnected_event.is_some())
            + usize::from(event.inventory_status_event.is_some())
            + usize::from(event.overflow_event.is_some());
        if detail_count > 1 {
            return Err(ReaderError::Protocol);
        }
        let mut observations = Vec::new();
        let mut coverage = Vec::new();
        match event.event_type.as_str() {
            "tagInventory" => {
                let tag = event
                    .tag_inventory_event
                    .as_ref()
                    .ok_or(ReaderError::Protocol)?;
                let port = self
                    .ports
                    .get(&tag.antenna_port)
                    .ok_or(ReaderError::Configuration)?
                    .clone();
                let epc = decode_identifier(tag.epc_hex.as_deref(), tag.epc.as_deref())?;
                let seen = tag.last_seen_time.unwrap_or(event.timestamp);
                if seen <= self.connected_at
                    || received_at.signed_duration_since(seen) > self.max_age
                {
                    return Ok(DecodedEvent::Discarded(DiscardReason::Stale));
                }
                if seen > event.timestamp || seen.signed_duration_since(received_at) > self.max_skew
                {
                    return Ok(DecodedEvent::Discarded(DiscardReason::Future));
                }
                let tid = if tag.tid.is_some() || tag.tid_hex.is_some() {
                    Some(
                        decode_identifier(tag.tid_hex.as_deref(), tag.tid.as_deref())?
                            .as_str()
                            .to_owned(),
                    )
                } else {
                    None
                };
                observations.push(TagObservation {
                    epc,
                    port_id: port,
                    first_seen_at: self.time_basis.observation_time(seen, received_at)?,
                    last_seen_at: self.time_basis.observation_time(seen, received_at)?,
                    read_count: 1,
                    tid,
                    peak_rssi_cdbm: tag.peak_rssi_cdbm,
                    event_ref: Some(format!("iot:{}", hex(&fingerprint))),
                });
            }
            "antennaActivation" | "antennaConnected" | "antennaDisconnected" => {
                let wire = match event.event_type.as_str() {
                    "antennaActivation" => event.antenna_activation_event.as_ref(),
                    "antennaConnected" => event.antenna_connected_event.as_ref(),
                    _ => event.antenna_disconnected_event.as_ref(),
                }
                .ok_or(ReaderError::Protocol)?;
                let port = self
                    .ports
                    .get(&wire.antenna_port)
                    .ok_or(ReaderError::Configuration)?
                    .clone();
                let proof = if event.event_type == "antennaDisconnected" {
                    CoverageProof::Fault {
                        code: ReaderFault::AntennaFault,
                    }
                } else {
                    CoverageProof::TransportOnly
                };
                coverage.push(PortCoverage {
                    port_id: port,
                    started_at: self.time_basis.server_time(event.timestamp)?,
                    ended_at: self.time_basis.server_time(event.timestamp)?,
                    proof,
                });
            }
            "inventoryStatus" => {
                let status = event
                    .inventory_status_event
                    .as_ref()
                    .ok_or(ReaderError::Protocol)?;
                let proof = match status.status {
                    InventoryStatus::Running => CoverageProof::TransportOnly,
                    InventoryStatus::Idle | InventoryStatus::Armed => CoverageProof::Fault {
                        code: ReaderFault::InventoryStopped,
                    },
                };
                coverage = self.coverage(self.time_basis.server_time(event.timestamp)?, proof);
            }
            "overflow" => {
                event.overflow_event.as_ref().ok_or(ReaderError::Protocol)?;
                coverage = self.coverage(
                    self.time_basis.server_time(event.timestamp)?,
                    CoverageProof::Fault {
                        code: ReaderFault::RecoveryRequired,
                    },
                );
            }
            _ => return Ok(DecodedEvent::Discarded(DiscardReason::UnknownEvent)),
        }
        self.fingerprints.insert(fingerprint);
        self.fingerprint_order.push_back(fingerprint);
        if self.fingerprint_order.len() > DEDUPLICATION_WINDOW
            && let Some(old) = self.fingerprint_order.pop_front()
        {
            self.fingerprints.remove(&old);
        }
        self.last_event_at = Some(event.timestamp);
        self.source_high_water = Some(
            event
                .timestamp
                .max(self.source_high_water.unwrap_or(event.timestamp)),
        );
        Ok(DecodedEvent::Batch(Box::new(self.batch(
            normalized_at,
            true,
            coverage,
            observations,
        )?)))
    }

    pub fn connection_event(
        &mut self,
        at: DateTime<Utc>,
        connected: bool,
        fault: Option<ReaderFault>,
    ) -> Result<ObservationBatch, ReaderError> {
        let last = self
            .last_event_at
            .map(|at| self.time_basis.server_time(at))
            .transpose()?;
        let at = at.max(last.unwrap_or(at));
        let proof = fault.map_or(CoverageProof::TransportOnly, |code| CoverageProof::Fault {
            code,
        });
        self.batch(at, connected, self.coverage(at, proof), Vec::new())
    }
    fn coverage(&self, at: DateTime<Utc>, proof: CoverageProof) -> Vec<PortCoverage> {
        self.ports
            .values()
            .map(|port| PortCoverage {
                port_id: port.clone(),
                started_at: at,
                ended_at: at,
                proof: proof.clone(),
            })
            .collect()
    }
    fn batch(
        &mut self,
        at: DateTime<Utc>,
        connected: bool,
        coverage: Vec<PortCoverage>,
        observations: Vec<TagObservation>,
    ) -> Result<ObservationBatch, ReaderError> {
        self.sequence = self.sequence.checked_add(1).ok_or(ReaderError::Protocol)?;
        let at = at.max(self.envelope_at.unwrap_or(at));
        self.envelope_at = Some(at);
        Ok(ObservationBatch {
            reader_id: self.reader.clone(),
            session: self.session,
            sequence: self.sequence,
            observed_at: at,
            connected,
            coverage,
            observations,
            purpose: ObservationPurpose::Inventory,
            data_source: self.data_source,
            capture_session: None,
            readback: None,
        })
    }
}

fn decode_identifier(
    hex_value: Option<&str>,
    base64_value: Option<&str>,
) -> Result<Epc, ReaderError> {
    let from_hex = hex_value
        .map(str::parse::<Epc>)
        .transpose()
        .map_err(|_| ReaderError::Protocol)?;
    let from_base64 = base64_value
        .map(|value| {
            if value.len() > 88 {
                return Err(ReaderError::Protocol);
            }
            let bytes = URL_SAFE_NO_PAD
                .decode(value)
                .or_else(|_| URL_SAFE.decode(value))
                .map_err(|_| ReaderError::Protocol)?;
            hex(&bytes)
                .parse::<Epc>()
                .map_err(|_| ReaderError::Protocol)
        })
        .transpose()?;
    match (from_hex, from_base64) {
        (Some(left), Some(right)) if left != right => Err(ReaderError::Protocol),
        (Some(value), _) | (_, Some(value)) => Ok(value),
        _ => Err(ReaderError::Protocol),
    }
}
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    bytes
        .iter()
        .flat_map(|byte| {
            [
                char::from(DIGITS[usize::from(byte >> 4)]),
                char::from(DIGITS[usize::from(byte & 15)]),
            ]
        })
        .collect()
}

/// Saturating exponential reconnect delay. A successful HTTP handshake alone
/// does not reset this delay: a disconnecting reader must not become a busy loop.
#[derive(Clone, Debug)]
pub struct ReconnectBackoff {
    initial: Duration,
    maximum: Duration,
    next: Duration,
}
impl ReconnectBackoff {
    pub fn new(initial: Duration, maximum: Duration) -> Result<Self, ReaderError> {
        if initial.is_zero() || maximum < initial || maximum > Duration::from_secs(300) {
            return Err(ReaderError::Configuration);
        }
        Ok(Self {
            initial,
            maximum,
            next: initial,
        })
    }
    pub fn delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = self.next.saturating_mul(2).min(self.maximum);
        delay
    }
    pub fn recovered(&mut self) {
        self.next = self.initial;
    }
}

#[derive(Clone)]
pub struct ReaderOptions {
    pub secrets: std::sync::Arc<dyn ReaderSecrets>,
    pub ca_certificate: Option<reqwest::Certificate>,
    pub setup_timeout: Duration,
    pub idle_timeout: Duration,
    pub max_event_age: Duration,
    pub max_clock_skew: Duration,
    pub reconnect_initial: Duration,
    pub reconnect_maximum: Duration,
}
impl Default for ReaderOptions {
    fn default() -> Self {
        Self {
            secrets: std::sync::Arc::new(EnvironmentSecrets),
            ca_certificate: None,
            setup_timeout: Duration::from_secs(15),
            idle_timeout: Duration::from_secs(90),
            max_event_age: Duration::from_secs(60),
            max_clock_skew: Duration::from_secs(2),
            reconnect_initial: Duration::from_secs(1),
            reconnect_maximum: Duration::from_secs(30),
        }
    }
}

/// An existing owner-only secret file can supply this resolver without mutating
/// process environment. Configuration/export contain names, never these values.
pub trait ReaderSecrets: Send + Sync {
    fn resolve(&self, name: &str) -> Result<String, ReaderError>;
}
pub struct EnvironmentSecrets;
impl ReaderSecrets for EnvironmentSecrets {
    fn resolve(&self, name: &str) -> Result<String, ReaderError> {
        // Reader configuration must not turn an arbitrary HTTPS endpoint into
        // a way to export the process's HA, app, or model credentials.
        if !name.starts_with("CLOSET_READER_") {
            return Err(ReaderError::Credentials);
        }
        std::env::var(name).map_err(|_| ReaderError::Credentials)
    }
}

/// No derived Debug: this object holds a sensitive authorization header.
pub struct R700Client {
    client: Client,
    origin: Url,
    authorization: Option<HeaderValue>,
    timeout: Duration,
}
impl R700Client {
    pub fn new(config: &ReaderConfig, options: &ReaderOptions) -> Result<Self, ReaderError> {
        if !config.enabled {
            return Err(ReaderError::Configuration);
        }
        let ReaderConnection::ImpinjIot {
            base_url,
            credentials,
        } = &config.connection
        else {
            return Err(ReaderError::Configuration);
        };
        let origin = Url::parse(base_url).map_err(|_| ReaderError::Configuration)?;
        if origin.scheme() != "https"
            || origin.host_str().is_none()
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
            || options.setup_timeout.is_zero()
            || options.setup_timeout > Duration::from_secs(60)
            || options.idle_timeout < Duration::from_secs(3)
            || options.idle_timeout > Duration::from_secs(900)
        {
            return Err(ReaderError::Configuration);
        }
        let authorization = match credentials {
            ReaderCredentials::None => None,
            ReaderCredentials::BasicEnv { username, password } => {
                let username = credential(username, options.secrets.as_ref())?;
                let password = credential(password, options.secrets.as_ref())?;
                if username.contains(':') {
                    return Err(ReaderError::Credentials);
                }
                let mut value = HeaderValue::from_str(&format!(
                    "Basic {}",
                    STANDARD.encode(format!("{username}:{password}"))
                ))
                .map_err(|_| ReaderError::Credentials)?;
                value.set_sensitive(true);
                Some(value)
            }
        };
        let mut builder = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(options.setup_timeout);
        if let Some(certificate) = options.ca_certificate.clone() {
            builder = builder.add_root_certificate(certificate);
        }
        let client = builder.build().map_err(|_| ReaderError::Transport)?;
        Ok(Self {
            client,
            origin,
            authorization,
            timeout: options.setup_timeout,
        })
    }
    fn get(&self, segments: &[&str]) -> Result<reqwest::RequestBuilder, ReaderError> {
        let mut url = self.origin.clone();
        url.path_segments_mut()
            .map_err(|_| ReaderError::Configuration)?
            .extend(["api", "v1"])
            .extend(segments);
        let mut request = self.client.get(url);
        if let Some(authorization) = &self.authorization {
            request = request.header(AUTHORIZATION, authorization.clone());
        }
        Ok(request)
    }
    async fn json<T: DeserializeOwned>(&self, segments: &[&str]) -> Result<T, ReaderError> {
        let response = self
            .get(segments)?
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|_| ReaderError::Transport)?;
        if !response.status().is_success() {
            return Err(ReaderError::Transport);
        }
        let bytes = limited_body(response).await?;
        serde_json::from_slice(&bytes).map_err(|_| ReaderError::Protocol)
    }
    async fn inspect(
        &self,
        reader: &ReaderId,
        ports: &[PortConfig],
        clock: &dyn ReaderClock,
    ) -> Result<IotInspection, ReaderError> {
        let now = clock.now();
        let schema: ApiDescription = self.json(&["openapi.json"]).await?;
        let api_version =
            semver::Version::parse(&schema.info.version).map_err(|_| ReaderError::Protocol)?;
        if api_version < semver::Version::new(1, 8, 0)
            || schema.base_path != "/api/v1"
            || !schema.paths.contains_key("/data/stream")
        {
            return Err(ReaderError::Unsupported);
        }
        let request_started_at = clock.now();
        let status: WireStatus = self.json(&["status"]).await?;
        let time_basis = ReaderTimeBasis::from_sample(status.time, request_started_at);
        if status.interface != "IoT" {
            return Err(ReaderError::Unsupported);
        }
        let stream: StreamSettings = self.json(&["http-stream"]).await?;
        // IoT has no replay cursor or end-of-history marker. Even timestamps
        // cannot prove unseen history fresh after a reader clock correction.
        if stream.event_buffer_size != Some(0) {
            return Err(ReaderError::RecoveryRequired);
        }
        // Suppressions/history are not coverage. Three actual keepalive intervals
        // are needed for stall detection, including non-default reader settings.
        if stream.keep_alive_interval_seconds == 0 || stream.keep_alive_interval_seconds > 300 {
            return Err(ReaderError::Protocol);
        }
        let image: SystemImage = self.json(&["system", "image"]).await?;
        let preset = if let Some(active) = &status.active_preset {
            if active.profile != "inventory" {
                return Err(ReaderError::Unsupported);
            }
            if let Some(id) = &active.id {
                Some(
                    self.json::<Preset>(&["profiles", "inventory", "presets", id])
                        .await?,
                )
            } else {
                None
            }
        } else {
            None
        };
        let antenna = schema
            .definitions
            .get("AntennaPort")
            .ok_or(ReaderError::Protocol)?;
        let maximum = antenna
            .maximum
            .and_then(|n| u16::try_from(n).ok())
            .filter(|n| (1..=32).contains(n))
            .ok_or(ReaderError::Protocol)?;
        let power = schema
            .definitions
            .get("TransmitPowerCdbm")
            .ok_or(ReaderError::Protocol)?;
        let readback = ReaderReadback {
            reader_id: reader.clone(),
            at: now,
            capabilities: ReaderCapabilities {
                antenna_ports: (1..=maximum).collect(),
                min_power_cdbm: power.minimum.and_then(|n| i32::try_from(n).ok()),
                max_power_cdbm: power.maximum.and_then(|n| i32::try_from(n).ok()),
                min_dwell_ms: None,
                max_dwell_ms: None,
                continuous_inventory_proof: false,
                inventory_pause: false,
                firmware: Some(image.primary_firmware),
            },
            ports: ports
                .iter()
                .filter(|port| port.reader_id == *reader)
                .map(|port| {
                    let actual = preset.as_ref().and_then(|preset| {
                        preset
                            .antenna_configs
                            .iter()
                            .find(|antenna| antenna.antenna_port == port.antenna)
                    });
                    PortReadback {
                        port_id: port.id.clone(),
                        antenna: port.antenna,
                        enabled: status.status.as_deref() == Some("running") && actual.is_some(),
                        transmit_power_cdbm: actual.map(|antenna| antenna.transmit_power_cdbm),
                        dwell_ms: None,
                    }
                })
                .collect(),
        };
        Ok(IotInspection {
            readback,
            reader_time: status.time,
            time_basis,
            running: status.status.as_deref() == Some("running"),
            keepalive_timeout: Duration::from_secs(stream.keep_alive_interval_seconds * 3),
        })
    }
    async fn stream(&self) -> Result<Response, ReaderError> {
        let response = tokio::time::timeout(self.timeout, self.get(&["data", "stream"])?.send())
            .await
            .map_err(|_| ReaderError::Transport)?
            .map_err(|_| ReaderError::Transport)?;
        if !response.status().is_success() {
            return Err(ReaderError::Transport);
        }
        Ok(response)
    }
}
fn credential(name: &str, secrets: &dyn ReaderSecrets) -> Result<String, ReaderError> {
    if name.is_empty()
        || name.len() > 128
        || !name.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_uppercase() || (index > 0 && byte.is_ascii_digit())
        })
    {
        return Err(ReaderError::Configuration);
    }
    let value = secrets.resolve(name)?;
    if value.is_empty() || value.len() > 4_096 || value.chars().any(char::is_control) {
        return Err(ReaderError::Credentials);
    }
    Ok(value)
}
async fn limited_body(mut response: Response) -> Result<Vec<u8>, ReaderError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ReaderError::Transport)? {
        if bytes.len().saturating_add(chunk.len()) > MAX_CHUNK_BYTES {
            return Err(ReaderError::Oversized);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiDescription {
    info: ApiInfo,
    base_path: String,
    definitions: BTreeMap<String, SchemaLimits>,
    paths: BTreeMap<String, IgnoredObject>,
}
#[derive(Deserialize)]
struct ApiInfo {
    version: String,
}
#[derive(Deserialize)]
struct SchemaLimits {
    minimum: Option<i64>,
    maximum: Option<i64>,
}
#[derive(Deserialize)]
struct IgnoredObject {}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireStatus {
    interface: String,
    time: DateTime<Utc>,
    status: Option<String>,
    active_preset: Option<ActivePreset>,
}
#[derive(Deserialize)]
struct ActivePreset {
    id: Option<String>,
    profile: String,
}
#[derive(Debug)]
struct IotInspection {
    readback: ReaderReadback,
    reader_time: DateTime<Utc>,
    time_basis: ReaderTimeBasis,
    running: bool,
    keepalive_timeout: Duration,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StreamSettings {
    event_buffer_size: Option<u32>,
    #[serde(default = "default_keepalive")]
    keep_alive_interval_seconds: u64,
}
fn default_keepalive() -> u64 {
    30
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SystemImage {
    primary_firmware: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Preset {
    antenna_configs: Vec<PresetAntenna>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PresetAntenna {
    antenna_port: u16,
    transmit_power_cdbm: i32,
}

/// Runs until shutdown. Setup failures and lost streams invalidate coverage
/// before bounded reconnect. Dropping the HTTP response closes only this
/// subscriber; the independently provisioned reader inventory is untouched.
pub async fn run_iot(
    store: &Store,
    reader: &ReaderConfig,
    ports: &[PortConfig],
    options: ReaderOptions,
    clock: &dyn ReaderClock,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ReaderError> {
    let client = R700Client::new(reader, &options)?;
    let mut decoder = R700Decoder::new(
        reader.id.clone(),
        ports,
        clock.now(),
        Uuid::now_v7(),
        options.max_event_age,
        options.max_clock_skew,
        DataSource::Live,
    )?;
    let mut backoff = ReconnectBackoff::new(options.reconnect_initial, options.reconnect_maximum)?;
    let context = IotRun {
        store,
        client: &client,
        reader,
        ports,
        options: &options,
        clock,
    };
    loop {
        if *shutdown.borrow() {
            break;
        }
        let started = clock.elapsed();
        let mut stream_shutdown = shutdown.clone();
        let outcome = tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            result = consume_iot(&context, &mut decoder, &mut stream_shutdown) => result,
        };
        if matches!(outcome, Err(ReaderError::State(_))) {
            return outcome;
        }
        let fault = if matches!(
            outcome,
            Err(ReaderError::Unsupported
                | ReaderError::Configuration
                | ReaderError::RecoveryRequired)
        ) {
            ReaderFault::RecoveryRequired
        } else if matches!(outcome, Err(ReaderError::ClockSkew)) {
            ReaderFault::ClockSkew
        } else if matches!(outcome, Err(ReaderError::Protocol | ReaderError::Oversized)) {
            ReaderFault::Protocol
        } else {
            ReaderFault::Disconnected
        };
        let at = clock.now();
        let batch = decoder.connection_event(at, false, Some(fault))?;
        admit_disconnect(store, batch, clock).await?;
        if *shutdown.borrow() {
            break;
        }
        if clock.elapsed().saturating_sub(started) >= Duration::from_secs(90) {
            backoff.recovered();
        }
        let delay = backoff.delay();
        tokio::select! { _ = shutdown.changed() => break, _ = tokio::time::sleep(delay) => {} }
    }
    let at = clock.now();
    let batch = decoder.connection_event(at, false, Some(ReaderFault::Disconnected))?;
    admit_disconnect(store, batch, clock).await?;
    Ok(())
}

struct IotRun<'a> {
    store: &'a Store,
    client: &'a R700Client,
    reader: &'a ReaderConfig,
    ports: &'a [PortConfig],
    options: &'a ReaderOptions,
    clock: &'a dyn ReaderClock,
}
async fn consume_iot(
    context: &IotRun<'_>,
    decoder: &mut R700Decoder,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<(), ReaderError> {
    let IotRun {
        store,
        client,
        reader,
        ports,
        options,
        clock,
    } = *context;
    let IotInspection {
        readback,
        reader_time,
        time_basis,
        running,
        keepalive_timeout,
    } = client.inspect(&reader.id, ports, clock).await?;
    if reader_time.signed_duration_since(clock.now()).abs()
        > chrono::Duration::from_std(options.max_clock_skew)
            .map_err(|_| ReaderError::Configuration)?
    {
        let at = clock.now();
        admit_live(
            store,
            decoder.connection_event(at, true, Some(ReaderFault::ClockSkew))?,
            clock,
        )
        .await?;
        return Err(ReaderError::Protocol);
    }
    decoder.time_basis = time_basis;
    decoder.reconnect(reader_time, Uuid::now_v7());
    let mut response = client.stream().await?;
    let mut initial = decoder.connection_event(
        clock.now(),
        true,
        if running {
            None
        } else {
            Some(ReaderFault::InventoryStopped)
        },
    )?;
    initial.readback = Some(readback);
    admit_live(store, initial, clock).await?;
    let mut framer = EventFramer::default();
    let mut pending: Option<ObservationBatch> = None;
    let mut flush = tokio::time::interval(Duration::from_millis(250));
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_data = clock.elapsed();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => return Ok(()),
            _ = flush.tick() => {
                if clock.elapsed().saturating_sub(last_data) >= options.idle_timeout.max(keepalive_timeout) { return Err(ReaderError::Transport); }
                if let Some(batch) = pending.take() { ingest_routed(store, decoder, ports, batch, clock).await?; }
            }
            chunk = response.chunk() => {
                let Some(chunk) = chunk.map_err(|_| ReaderError::Transport)? else { framer.finish()?; return Err(ReaderError::Transport); };
                last_data = clock.elapsed();
                for record in framer.push(&chunk)? {
                    if let DecodedEvent::Batch(batch) = decoder.decode(&record, clock.now())? {
                        let batch=*batch;
                        if !batch.coverage.is_empty() {
                            if let Some(earlier) = pending.take() { ingest_routed(store, decoder, ports, earlier, clock).await?; }
                            ingest_routed(store, decoder, ports, batch, clock).await?;
                        } else if let Some(earlier) = &mut pending {
                            earlier.sequence = batch.sequence;
                            earlier.observed_at = batch.observed_at;
                            earlier.observations.extend(batch.observations);
                            if earlier.observations.len() >= crate::tracking::models::MAX_BATCH_OBSERVATIONS
                                && let Some(batch) = pending.take() { ingest_routed(store, decoder, ports, batch, clock).await?; }
                        } else { pending = Some(batch); }
                    }
                }
            }
        }
    }
}

async fn ingest_routed(
    store: &Store,
    decoder: &mut R700Decoder,
    ports: &[PortConfig],
    mut batch: ObservationBatch,
    clock: &dyn ReaderClock,
) -> Result<(), ReaderError> {
    if batch.observations.is_empty() {
        decoder.sequence = decoder
            .sequence
            .checked_add(1)
            .ok_or(ReaderError::Protocol)?;
        batch.sequence = decoder.sequence;
        admit_live(store, batch, clock).await?;
        return Ok(());
    }
    let snapshot = store
        .tracking_snapshot_clocked(|| clock.now())
        .await
        .map_err(|error| ReaderError::State(error.code()))?;
    if !snapshot
        .reader_readbacks
        .iter()
        .any(|readback| readback.reader_id == batch.reader_id)
    {
        return Err(ReaderError::EvidenceChanged);
    }
    let mut inventory = batch.clone();
    inventory.observations.clear();
    let mut captures: BTreeMap<crate::tracking::models::SessionId, Vec<TagObservation>> =
        BTreeMap::new();
    for observation in &batch.observations {
        let port = ports
            .iter()
            .find(|port| port.id == observation.port_id)
            .ok_or(ReaderError::Configuration)?;
        if let Some(session) = snapshot
            .sessions
            .iter()
            .find(|session| session.port_id == port.id && session.state == EnrollmentState::Active)
        {
            // Capture starts fence old observations. The engine rechecks it too.
            if observation.first_seen_at > session.capture_started_at {
                captures
                    .entry(session.id)
                    .or_default()
                    .push(observation.clone());
            }
        } else if port.role == PortRole::Monitor {
            inventory.observations.push(observation.clone());
        }
    }
    if !inventory.observations.is_empty() {
        decoder.sequence = decoder
            .sequence
            .checked_add(1)
            .ok_or(ReaderError::Protocol)?;
        inventory.sequence = decoder.sequence;
        admit_live(store, inventory, clock).await?;
    }
    for (session, observations) in captures {
        let mut capture = batch.clone();
        capture.purpose = ObservationPurpose::Enrollment;
        capture.capture_session = Some(session);
        capture.observations = observations;
        capture.coverage.clear();
        decoder.sequence = decoder
            .sequence
            .checked_add(1)
            .ok_or(ReaderError::Protocol)?;
        capture.sequence = decoder.sequence;
        admit_live(store, capture, clock).await?;
    }
    Ok(())
}

// Admission time belongs to Store's serialized live boundary, not to an
// earlier socket read or routing snapshot. A superseded batch is discarded;
// reconnect earns fresh readback/epoch evidence instead of replaying it.
async fn admit_live(
    store: &Store,
    batch: ObservationBatch,
    clock: &dyn ReaderClock,
) -> Result<(), ReaderError> {
    match store.tracking_ingest_clocked(batch, || clock.now()).await {
        Ok(_) => Ok(()),
        Err(
            TrackingError::Revision
            | TrackingError::Invalid {
                code: "stale_observation" | "out_of_order_observation",
                ..
            },
        ) => Err(ReaderError::EvidenceChanged),
        Err(error) => Err(ReaderError::State(error.code())),
    }
}
async fn admit_disconnect(
    store: &Store,
    batch: ObservationBatch,
    clock: &dyn ReaderClock,
) -> Result<(), ReaderError> {
    match admit_live(store, batch, clock).await {
        Err(ReaderError::EvidenceChanged) => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracking::models::{AreaId, PortRole};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::Arc,
    };

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).unwrap()
    }
    fn ports() -> Vec<PortConfig> {
        vec![PortConfig {
            id: "port-a".parse().unwrap(),
            reader_id: "reader-a".parse().unwrap(),
            antenna: 1,
            area_id: Some("area-a".parse::<AreaId>().unwrap()),
            role: PortRole::Monitor,
            enabled: true,
            commissioned: true,
            required_for_absence: true,
            transmit_power_cdbm: None,
            dwell_ms: None,
        }]
    }
    fn decoder() -> R700Decoder {
        R700Decoder::new(
            "reader-a".parse().unwrap(),
            &ports(),
            at(0),
            Uuid::from_u128(1),
            Duration::from_secs(60),
            Duration::from_secs(2),
            DataSource::Simulation,
        )
        .unwrap()
    }
    fn tag(time: i64, epc: &str) -> WireEvent {
        WireEvent {
            timestamp: at(time),
            event_type: "tagInventory".into(),
            tag_inventory_event: Some(WireTag {
                epc: None,
                epc_hex: Some(epc.into()),
                antenna_port: 1,
                last_seen_time: Some(at(time)),
                tid: None,
                tid_hex: None,
                peak_rssi_cdbm: Some(-5700),
            }),
            antenna_activation_event: None,
            antenna_connected_event: None,
            antenna_disconnected_event: None,
            inventory_status_event: None,
            overflow_event: None,
        }
    }
    fn decode(
        decoder: &mut R700Decoder,
        event: &WireEvent,
        now: i64,
    ) -> Result<DecodedEvent, ReaderError> {
        decoder.decode(&serde_json::to_vec(event).unwrap(), at(now))
    }
    #[test]
    fn actual_wire_shape_preserves_leading_zeros_variable_lengths_and_metadata() {
        let mut decoder = decoder();
        let mut event = tag(1, "000001aB");
        let wire = event.tag_inventory_event.as_mut().unwrap();
        wire.epc_hex = None;
        wire.epc = Some(URL_SAFE_NO_PAD.encode([0, 0, 1, 171]));
        wire.tid_hex = Some("001122".into());
        let DecodedEvent::Batch(batch) = decode(&mut decoder, &event, 1).unwrap() else {
            panic!("missing tag");
        };
        let observation = &batch.observations[0];
        assert_eq!(observation.epc.as_str(), "000001AB");
        assert_eq!(observation.tid.as_deref(), Some("001122"));
        assert_eq!(observation.peak_rssi_cdbm, Some(-5700));
        assert_eq!(observation.read_count, 1); // No invented vendor count.
        assert!(batch.coverage.is_empty());
        assert_eq!(batch.data_source, DataSource::Simulation);
        assert!(observation.event_ref.as_ref().unwrap().starts_with("iot:"));
    }
    #[test]
    fn duplicate_out_of_order_stale_future_and_reconnect_history_are_not_fresh_reads() {
        let mut decoder = decoder();
        let event = tag(2, "0011");
        assert!(matches!(
            decode(&mut decoder, &event, 2).unwrap(),
            DecodedEvent::Batch(_)
        ));
        assert!(matches!(
            decode(&mut decoder, &event, 2).unwrap(),
            DecodedEvent::Discarded(DiscardReason::Duplicate)
        ));
        assert!(matches!(
            decode(&mut decoder, &tag(1, "0022"), 2).unwrap(),
            DecodedEvent::Discarded(DiscardReason::OutOfOrder)
        ));
        assert!(matches!(
            decode(&mut decoder, &tag(-1, "0022"), 2).unwrap(),
            DecodedEvent::Discarded(DiscardReason::Stale)
        ));
        assert!(matches!(
            decode(&mut decoder, &tag(5, "0022"), 2).unwrap(),
            DecodedEvent::Discarded(DiscardReason::Future)
        ));
        decoder.reconnect(at(3), Uuid::now_v7());
        assert!(matches!(
            decode(&mut decoder, &event, 3).unwrap(),
            DecodedEvent::Discarded(DiscardReason::Stale)
        ));
        assert!(matches!(
            decode(&mut decoder, &tag(4, "0011"), 4).unwrap(),
            DecodedEvent::Batch(_)
        ));
    }
    #[test]
    fn allowed_clock_skew_keeps_reconnect_envelopes_monotonic_without_replaying_history() {
        let mut decoder = decoder();
        let DecodedEvent::Batch(first) = decode(&mut decoder, &tag(2, "0011"), 1).unwrap() else {
            panic!("missing allowed-skew report");
        };
        assert_eq!(first.observed_at, at(2));
        assert_eq!(
            decoder
                .connection_event(at(1), false, Some(ReaderFault::Disconnected))
                .unwrap()
                .observed_at,
            at(2)
        );
        decoder.reconnect(at(1), Uuid::now_v7());
        assert_eq!(
            decoder
                .connection_event(at(1), true, None)
                .unwrap()
                .observed_at,
            at(2)
        );
        assert!(matches!(
            decode(&mut decoder, &tag(2, "0022"), 2).unwrap(),
            DecodedEvent::Discarded(DiscardReason::Stale)
        ));
        let DecodedEvent::Batch(fresh) = decode(&mut decoder, &tag(3, "0022"), 3).unwrap() else {
            panic!("missing fresh post-fence report");
        };
        assert_eq!(fresh.observations[0].last_seen_at, at(3));
        assert_eq!(fresh.observed_at, at(3));
    }
    #[test]
    fn last_seen_time_not_queue_timestamp_controls_freshness() {
        let mut decoder = decoder();
        let mut event = tag(2, "0011");
        event.tag_inventory_event.as_mut().unwrap().last_seen_time = Some(at(-10));
        assert!(matches!(
            decode(&mut decoder, &event, 2).unwrap(),
            DecodedEvent::Discarded(DiscardReason::Stale)
        ));
        event.tag_inventory_event.as_mut().unwrap().last_seen_time = Some(at(3));
        assert!(matches!(
            decode(&mut decoder, &event, 3).unwrap(),
            DecodedEvent::Discarded(DiscardReason::Future)
        ));
    }
    #[test]
    fn invalid_ids_mismatched_encodings_and_unconfigured_ports_reject() {
        for value in ["", "0", "GG", &"00".repeat(65)] {
            assert_eq!(
                decode(&mut decoder(), &tag(1, value), 1).unwrap_err(),
                ReaderError::Protocol
            );
        }
        let mut event = tag(1, "0011");
        event.tag_inventory_event.as_mut().unwrap().epc = Some(URL_SAFE_NO_PAD.encode([0, 34]));
        assert_eq!(
            decode(&mut decoder(), &event, 1).unwrap_err(),
            ReaderError::Protocol
        );
        event.tag_inventory_event.as_mut().unwrap().antenna_port = 2;
        assert_eq!(
            decode(&mut decoder(), &event, 1).unwrap_err(),
            ReaderError::Configuration
        );
    }
    #[test]
    fn activation_running_connected_and_keepalive_never_prove_empty_inventory() {
        let mut decoder = decoder();
        for (index, kind) in ["antennaActivation", "antennaConnected", "inventoryStatus"]
            .iter()
            .enumerate()
        {
            let mut event = tag(i64::try_from(index + 1).unwrap(), "0011");
            event.event_type = (*kind).into();
            event.tag_inventory_event = None;
            match *kind {
                "antennaActivation" => {
                    event.antenna_activation_event = Some(WirePort { antenna_port: 1 })
                }
                "antennaConnected" => {
                    event.antenna_connected_event = Some(WirePort { antenna_port: 1 })
                }
                _ => {
                    event.inventory_status_event = Some(WireInventoryStatus {
                        status: InventoryStatus::Running,
                    })
                }
            }
            let DecodedEvent::Batch(batch) = decode(&mut decoder, &event, 5).unwrap() else {
                panic!("missing health");
            };
            assert!(batch.observations.is_empty());
            assert!(
                batch
                    .coverage
                    .iter()
                    .all(|coverage| coverage.proof == CoverageProof::TransportOnly)
            );
        }
        assert!(matches!(
            decoder.decode(b"\r\n", at(5)).unwrap(),
            DecodedEvent::KeepAlive
        ));
        let batch = decoder
            .connection_event(at(5), false, Some(ReaderFault::Disconnected))
            .unwrap();
        assert!(!batch.connected);
        assert!(batch.coverage.iter().all(|coverage| coverage.proof
            == CoverageProof::Fault {
                code: ReaderFault::Disconnected
            }));
    }
    #[test]
    fn antenna_fault_overflow_and_stopped_inventory_remain_faults() {
        let mut decoder = decoder();
        for (index, kind) in ["antennaDisconnected", "overflow", "inventoryStatus"]
            .iter()
            .enumerate()
        {
            let mut event = tag(i64::try_from(index + 1).unwrap(), "0011");
            event.tag_inventory_event = None;
            event.event_type = (*kind).into();
            match *kind {
                "antennaDisconnected" => {
                    event.antenna_disconnected_event = Some(WirePort { antenna_port: 1 })
                }
                "overflow" => {
                    event.overflow_event = Some(WireOverflow {
                        events_purged: Some(10),
                        events_remaining: Some(1),
                        first_event_timestamp: None,
                        last_event_timestamp: None,
                    })
                }
                _ => {
                    event.inventory_status_event = Some(WireInventoryStatus {
                        status: InventoryStatus::Armed,
                    })
                }
            }
            let DecodedEvent::Batch(batch) = decode(&mut decoder, &event, 5).unwrap() else {
                panic!("missing fault");
            };
            assert!(
                batch
                    .coverage
                    .iter()
                    .all(|coverage| matches!(coverage.proof, CoverageProof::Fault { .. }))
            );
        }
    }
    #[test]
    fn bounded_framing_handles_split_records_keepalive_and_interrupted_stream() {
        let wire = serde_json::to_vec(&tag(1, "0011")).unwrap();
        let mut framer = EventFramer::default();
        assert!(framer.push(&wire[..10]).unwrap().is_empty());
        assert_eq!(framer.finish(), Err(ReaderError::Protocol));
        assert!(framer.push(&wire[10..]).unwrap().is_empty());
        let records = framer.push(b"\n\r\n").unwrap();
        assert_eq!(records.len(), 2);
        assert!(matches!(
            decoder().decode(&records[0], at(1)).unwrap(),
            DecodedEvent::Batch(_)
        ));
        assert_eq!(framer.finish(), Ok(()));
        assert_eq!(
            framer.push(&vec![b'x'; MAX_EVENT_BYTES + 1]),
            Err(ReaderError::Oversized)
        );
        assert_eq!(
            decoder()
                .decode(&vec![b'x'; MAX_EVENT_BYTES + 1], at(1))
                .unwrap_err(),
            ReaderError::Oversized
        );
    }
    #[test]
    fn backoff_is_bounded_and_does_not_reset_for_a_handshake() {
        let mut backoff =
            ReconnectBackoff::new(Duration::from_secs(1), Duration::from_secs(5)).unwrap();
        assert_eq!(
            (0..6)
                .map(|_| backoff.delay().as_secs())
                .collect::<Vec<_>>(),
            [1, 2, 4, 5, 5, 5]
        );
        backoff.recovered();
        assert_eq!(backoff.delay(), Duration::from_secs(1));
        assert!(ReconnectBackoff::new(Duration::ZERO, Duration::from_secs(1)).is_err());
    }
    #[test]
    fn future_unknown_events_and_legacy_v0_do_not_gain_authority() {
        let mut event = tag(1, "0011");
        event.event_type = "futureEvent".into();
        event.tag_inventory_event = None;
        assert!(matches!(
            decode(&mut decoder(), &event, 1).unwrap(),
            DecodedEvent::Discarded(DiscardReason::UnknownEvent)
        ));
        assert_eq!(decoder().decode(br#"{"TagInventoryEvent":{"epc":"ABE","antennaPort":1},"timestamp":"2026-11-01T00:00:00Z"}"#, at(1)).unwrap_err(), ReaderError::Protocol);
    }
    async fn capture_fences_use_server_time(reader_lead_ms: i64) {
        use crate::tracking::models::{
            AreaConfig, AreaKind, AreaThresholds, EnrollmentPurpose, TRACKING_SCHEMA_VERSION,
            Thresholds, TrackingCommand, TrackingConfig,
        };
        struct CaptureClock(std::sync::Mutex<DateTime<Utc>>);
        impl ReaderClock for CaptureClock {
            fn now(&self) -> DateTime<Utc> {
                *self.0.lock().unwrap()
            }
            fn elapsed(&self) -> Duration {
                Duration::ZERO
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("capture.sqlite3"))
            .await
            .unwrap();
        let start = Utc::now() + chrono::Duration::seconds(1);
        let clock = CaptureClock(std::sync::Mutex::new(start));
        let mut catalog: Vec<crate::models::CatalogItem> =
            serde_json::from_str(include_str!("../../config/catalog.example.json")).unwrap();
        catalog.truncate(1);
        catalog[0].quantity = Some(2);
        store
            .sync_at(
                crate::models::SyncRequest {
                    items: catalog,
                    dry_run: false,
                    allow_empty: false,
                },
                Some(0),
                "fixture",
                Some("seed"),
                start,
            )
            .await
            .unwrap();
        let mut configured_ports = ports();
        configured_ports[0].role = PortRole::Enrollment;
        configured_ports[0].required_for_absence = false;
        let config = TrackingConfig {
            previous_wire: Default::default(),
            schema_version: TRACKING_SCHEMA_VERSION,
            areas: vec![AreaConfig {
                id: "area-a".parse().unwrap(),
                label: "Fictional area".into(),
                kind: AreaKind::Closet,
                enabled: true,
                thresholds: AreaThresholds::default(),
            }],
            readers: vec![ReaderConfig {
                id: "reader-a".parse().unwrap(),
                label: "Fictional reader".into(),
                enabled: true,
                connection: ReaderConnection::ImpinjIot {
                    base_url: "https://reader.example.invalid".into(),
                    credentials: ReaderCredentials::None,
                },
            }],
            ports: configured_ports.clone(),
            thresholds: Thresholds {
                positive_dwell_ms: 1,
                positive_min_reads: 2,
                ..Default::default()
            },
        };
        let revision = store
            .tracking_snapshot(start)
            .await
            .unwrap()
            .tracking_revision;
        store
            .tracking_command(
                "fixture",
                "configure",
                revision,
                TrackingCommand::Configure { config },
                start,
            )
            .await
            .unwrap();
        let mut decoder = R700Decoder::new(
            "reader-a".parse().unwrap(),
            &configured_ports,
            start,
            Uuid::now_v7(),
            Duration::from_secs(60),
            Duration::from_secs(2),
            DataSource::Live,
        )
        .unwrap();
        decoder.time_basis = ReaderTimeBasis::from_sample(
            start + chrono::Duration::milliseconds(reader_lead_ms),
            start,
        );
        decoder.reconnect(
            start + chrono::Duration::milliseconds(reader_lead_ms),
            Uuid::now_v7(),
        );
        let mut initial = decoder.connection_event(start, true, None).unwrap();
        initial.readback = Some(ReaderReadback {
            reader_id: "reader-a".parse().unwrap(),
            at: start,
            capabilities: ReaderCapabilities {
                antenna_ports: vec![1],
                min_power_cdbm: None,
                max_power_cdbm: None,
                min_dwell_ms: None,
                max_dwell_ms: None,
                continuous_inventory_proof: false,
                inventory_pause: false,
                firmware: Some("10.4.0".into()),
            },
            ports: Vec::new(),
        });
        admit_live(&store, initial, &clock).await.unwrap();
        let snapshot = store.tracking_snapshot(start).await.unwrap();
        let units: Vec<_> = snapshot.units.iter().map(|u| u.id).collect();
        let capture_at = start + chrono::Duration::milliseconds(10);
        let session = store
            .tracking_command(
                "fixture",
                "capture",
                snapshot.tracking_revision,
                TrackingCommand::StartEnrollment {
                    unit_ids: units.clone(),
                    port_id: configured_ports[0].id.clone(),
                    borrow: false,
                    purpose: EnrollmentPurpose::Bind,
                    ttl_seconds: 60,
                },
                capture_at,
            )
            .await
            .unwrap()
            .session
            .unwrap();
        let mut pending: Option<ObservationBatch> = None;
        for ms in [20, 22] {
            let received = start + chrono::Duration::milliseconds(ms);
            *clock.0.lock().unwrap() = received;
            let raw = received + chrono::Duration::milliseconds(reader_lead_ms);
            let mut event = tag(1, "00AB");
            event.timestamp = raw;
            event.tag_inventory_event.as_mut().unwrap().last_seen_time = Some(raw);
            let DecodedEvent::Batch(batch) = decoder
                .decode(&serde_json::to_vec(&event).unwrap(), clock.now())
                .unwrap()
            else {
                panic!("expected actual modern wire report")
            };
            if let Some(pending) = &mut pending {
                pending.sequence = batch.sequence;
                pending.observed_at = batch.observed_at;
                pending.observations.extend(batch.observations);
            } else {
                pending = Some(*batch);
            }
        }
        let reset_at = start + chrono::Duration::milliseconds(30);
        let revision = store
            .tracking_snapshot(reset_at)
            .await
            .unwrap()
            .tracking_revision;
        store
            .tracking_command(
                "fixture",
                "skip",
                revision,
                TrackingCommand::Skip {
                    session_id: session.id,
                    capture_version: session.capture_version,
                    unit_id: units[0],
                },
                reset_at,
            )
            .await
            .unwrap();
        *clock.0.lock().unwrap() = start + chrono::Duration::milliseconds(40);
        ingest_routed(
            &store,
            &mut decoder,
            &configured_ports,
            pending.unwrap(),
            &clock,
        )
        .await
        .unwrap();
        let snapshot = store
            .tracking_snapshot_clocked(|| clock.now())
            .await
            .unwrap();
        let session = &snapshot.sessions[0];
        assert_eq!(session.cursor, 1);
        assert!(
            session.candidates.is_empty(),
            "pre-skip unbound wire reports became candidates for the next unit: {:?}",
            session.candidates
        );
        // Genuine post-skip reports remain usable immediately at both0 and+1.9s.
        for ms in [50, 52] {
            let received = start + chrono::Duration::milliseconds(ms);
            *clock.0.lock().unwrap() = received;
            let raw = received + chrono::Duration::milliseconds(reader_lead_ms);
            let mut event = tag(1, "00AB");
            event.timestamp = raw;
            event.tag_inventory_event.as_mut().unwrap().last_seen_time = Some(raw);
            let DecodedEvent::Batch(batch) = decoder
                .decode(&serde_json::to_vec(&event).unwrap(), clock.now())
                .unwrap()
            else {
                panic!("fresh wire report discarded")
            };
            ingest_routed(&store, &mut decoder, &configured_ports, *batch, &clock)
                .await
                .unwrap();
        }
        let snapshot = store
            .tracking_snapshot_clocked(|| clock.now())
            .await
            .unwrap();
        let capture = &snapshot.sessions[0];
        assert_eq!(capture.candidates.len(), 1);
        assert!(
            capture.candidates[0].selectable,
            "aligned/fast clocks must not add the2s skew allowance to enrollment dwell"
        );
        let selected_at = start + chrono::Duration::milliseconds(55);
        store
            .tracking_command(
                "fixture",
                "pair",
                snapshot.tracking_revision,
                TrackingCommand::SelectCandidate {
                    session_id: capture.id,
                    capture_version: capture.capture_version,
                    unit_id: units[1],
                    epc: "00AB".parse().unwrap(),
                    baseline_area: Some("area-a".parse().unwrap()),
                },
                selected_at,
            )
            .await
            .unwrap();
        let configure_at = start + chrono::Duration::milliseconds(60);
        let mut snapshot = store.tracking_snapshot(configure_at).await.unwrap();
        configured_ports[0].role = PortRole::Monitor;
        configured_ports[0].required_for_absence = true;
        snapshot.config.ports = configured_ports.clone();
        store
            .tracking_command(
                "fixture",
                "monitor",
                snapshot.tracking_revision,
                TrackingCommand::Configure {
                    config: snapshot.config,
                },
                configure_at,
            )
            .await
            .unwrap();
        decoder.ports = configured_ports
            .iter()
            .map(|port| (port.antenna, port.id.clone()))
            .collect();
        decoder.time_basis = ReaderTimeBasis::from_sample(
            configure_at + chrono::Duration::milliseconds(reader_lead_ms),
            configure_at,
        );
        decoder.reconnect(
            configure_at + chrono::Duration::milliseconds(reader_lead_ms),
            Uuid::now_v7(),
        );
        *clock.0.lock().unwrap() = configure_at;
        let mut initial = decoder.connection_event(configure_at, true, None).unwrap();
        initial.readback = Some(ReaderReadback {
            reader_id: "reader-a".parse().unwrap(),
            at: configure_at,
            capabilities: ReaderCapabilities {
                antenna_ports: vec![1],
                min_power_cdbm: None,
                max_power_cdbm: None,
                min_dwell_ms: None,
                max_dwell_ms: None,
                continuous_inventory_proof: false,
                inventory_pause: false,
                firmware: Some("10.4.0".into()),
            },
            ports: Vec::new(),
        });
        admit_live(&store, initial, &clock).await.unwrap();
        let mut pending: Option<ObservationBatch> = None;
        for ms in [70, 72] {
            *clock.0.lock().unwrap() = start + chrono::Duration::milliseconds(ms);
            let raw = clock.now() + chrono::Duration::milliseconds(reader_lead_ms);
            let mut event = tag(1, "00AB");
            event.timestamp = raw;
            event.tag_inventory_event.as_mut().unwrap().last_seen_time = Some(raw);
            let DecodedEvent::Batch(batch) = decoder
                .decode(&serde_json::to_vec(&event).unwrap(), clock.now())
                .unwrap()
            else {
                panic!("old bound wire report discarded before manual correction")
            };
            if let Some(pending) = &mut pending {
                pending.sequence = batch.sequence;
                pending.observed_at = batch.observed_at;
                pending.observations.extend(batch.observations);
            } else {
                pending = Some(*batch);
            }
        }
        let correction_at = start + chrono::Duration::milliseconds(80);
        let revision = store
            .tracking_snapshot(correction_at)
            .await
            .unwrap()
            .tracking_revision;
        store
            .tracking_command(
                "fixture",
                "correct",
                revision,
                TrackingCommand::Correct {
                    unit_id: units[1],
                    status: crate::tracking::models::UnitStatus::NeedsLaundry,
                    note: "Fictional manual correction".into(),
                },
                correction_at,
            )
            .await
            .unwrap();
        *clock.0.lock().unwrap() = start + chrono::Duration::milliseconds(90);
        ingest_routed(
            &store,
            &mut decoder,
            &configured_ports,
            pending.unwrap(),
            &clock,
        )
        .await
        .unwrap();
        let snapshot = store
            .tracking_snapshot_clocked(|| clock.now())
            .await
            .unwrap();
        assert_eq!(
            snapshot
                .units
                .iter()
                .find(|unit| unit.id == units[1])
                .unwrap()
                .status,
            crate::tracking::models::UnitStatus::NeedsLaundry,
            "pre-correction wire evidence crossed the manual server fence"
        );
        store.close().await;
    }
    #[tokio::test]
    async fn fast_reader_queued_unbound_tag_cannot_cross_skip_capture_fence() {
        capture_fences_use_server_time(1900).await;
    }
    #[tokio::test]
    async fn aligned_reader_capture_stays_fast_and_manual_fences_remain_strict() {
        capture_fences_use_server_time(0).await;
    }
    #[tokio::test]
    async fn superseded_live_batches_refresh_without_retry_but_storage_errors_stay_fatal() {
        use crate::tracking::models::{
            AreaConfig, AreaKind, AreaThresholds, TRACKING_SCHEMA_VERSION, Thresholds,
            TrackingCommand, TrackingConfig,
        };
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("reader.sqlite3"))
            .await
            .unwrap();
        let clock = SystemReaderClock::default();
        let config = TrackingConfig {
            schema_version: TRACKING_SCHEMA_VERSION,
            areas: vec![AreaConfig {
                id: "area-a".parse().unwrap(),
                label: "Fictional area".into(),
                kind: AreaKind::Closet,
                enabled: true,
                thresholds: AreaThresholds::default(),
            }],
            readers: vec![ReaderConfig {
                id: "reader-a".parse().unwrap(),
                label: "Fictional reader".into(),
                enabled: true,
                connection: ReaderConnection::ImpinjIot {
                    base_url: "https://reader.example.invalid".into(),
                    credentials: ReaderCredentials::None,
                },
            }],
            ports: ports(),
            thresholds: Thresholds::default(),
            ..Default::default()
        };
        let revision = store
            .tracking_snapshot_clocked(|| clock.now())
            .await
            .unwrap()
            .tracking_revision;
        store
            .tracking_command_clocked(
                "fixture",
                "configure",
                revision,
                TrackingCommand::Configure { config },
                || clock.now(),
            )
            .await
            .unwrap();
        let mut decoder = R700Decoder::new(
            "reader-a".parse().unwrap(),
            &ports(),
            clock.now(),
            Uuid::now_v7(),
            Duration::from_secs(60),
            Duration::from_secs(2),
            DataSource::Live,
        )
        .unwrap();
        let stale = decoder
            .connection_event(clock.now() - chrono::Duration::seconds(120), true, None)
            .unwrap();
        assert_eq!(
            admit_live(&store, stale, &clock).await,
            Err(ReaderError::EvidenceChanged)
        );
        let fresh = decoder.connection_event(clock.now(), true, None).unwrap();
        admit_live(&store, fresh.clone(), &clock).await.unwrap();
        let mut out_of_order = fresh;
        out_of_order.sequence += 1;
        out_of_order.observed_at -= chrono::Duration::milliseconds(1);
        assert_eq!(
            admit_live(&store, out_of_order, &clock).await,
            Err(ReaderError::EvidenceChanged)
        );
        admit_live(
            &store,
            decoder.connection_event(clock.now(), true, None).unwrap(),
            &clock,
        )
        .await
        .unwrap();
        store.close().await;
        assert_eq!(
            admit_live(
                &store,
                decoder.connection_event(clock.now(), true, None).unwrap(),
                &clock
            )
            .await,
            Err(ReaderError::State("storage_unavailable"))
        );
    }
    #[test]
    fn environment_secrets_cannot_export_unrelated_process_credentials() {
        const PROBE: &str = "CLOSET_READER_SCOPE_PROBE";
        if std::env::var_os(PROBE).is_some() {
            for name in ["SUPERVISOR_TOKEN", "CLOSET_HA_TOKEN", "OPENAI_API_KEY"] {
                assert_eq!(
                    EnvironmentSecrets.resolve(name),
                    Err(ReaderError::Credentials)
                );
            }
            assert_eq!(
                EnvironmentSecrets
                    .resolve("CLOSET_READER_FIXTURE_PASSWORD")
                    .unwrap(),
                "fictional-reader-password"
            );
            return;
        }
        let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","tracking::reader::tests::environment_secrets_cannot_export_unrelated_process_credentials"])
            .env(PROBE,"1").env("SUPERVISOR_TOKEN","fictional-unrelated-token").env("CLOSET_HA_TOKEN","fictional-unrelated-token").env("OPENAI_API_KEY","fictional-unrelated-token").env("CLOSET_READER_FIXTURE_PASSWORD","fictional-reader-password").output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn https_peer(
        stream_settings: &'static str,
        request_count: usize,
        reader_lead_ms: i64,
    ) -> (
        std::net::SocketAddr,
        DateTime<Utc>,
        std::thread::JoinHandle<Vec<String>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let cert = rustls::pki_types::CertificateDer::from(
            include_bytes!("../../tests/fixtures/tls/cert.der").to_vec(),
        );
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(
            include_bytes!("../../tests/fixtures/tls/key.der").to_vec(),
        );
        let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key.into())
        .unwrap();
        let now = Utc::now();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..request_count {
                let (socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut socket = rustls::StreamOwned::new(
                    rustls::ServerConnection::new(Arc::new(tls.clone())).unwrap(),
                    socket,
                );
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).unwrap();
                    request.extend(byte);
                }
                let request = String::from_utf8(request).unwrap();
                let first = request.lines().next().unwrap();
                let body = match first {
                    "GET /api/v1/openapi.json HTTP/1.1" => r#"{"basePath":"/api/v1","info":{"version":"1.11.0"},"definitions":{"AntennaPort":{"minimum":1,"maximum":4},"TransmitPowerCdbm":{"minimum":1000,"maximum":3000}},"paths":{"/data/stream":{}}}"#.to_owned(),
                    "GET /api/v1/status HTTP/1.1" => format!("{{\"interface\":\"IoT\",\"time\":\"{}\",\"status\":\"running\"}}",(Utc::now()+chrono::Duration::milliseconds(reader_lead_ms)).to_rfc3339()),
                    "GET /api/v1/http-stream HTTP/1.1" => stream_settings.to_owned(),
                    "GET /api/v1/system/image HTTP/1.1" => r#"{"primaryFirmware":"10.4.0"}"#.to_owned(),
                    "GET /api/v1/data/stream HTTP/1.1" => "\r\n".to_owned(),
                    _ => panic!("unexpected request {first}"),
                };
                requests.push(first.to_owned());
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                socket.flush().unwrap();
            }
            requests
        });
        (address, now, server)
    }
    #[tokio::test]
    async fn real_https_reader_gets_documented_paths_with_verified_tls_and_no_writes() {
        let (address, now, server) = https_peer(
            r#"{"keepAliveIntervalSeconds":40,"eventBufferSize":0}"#,
            5,
            0,
        );
        let config = ReaderConfig {
            id: "reader-a".parse().unwrap(),
            label: "Fictional reader".into(),
            enabled: true,
            connection: ReaderConnection::ImpinjIot {
                base_url: format!("https://{address}"),
                credentials: ReaderCredentials::None,
            },
        };
        let options = ReaderOptions {
            ca_certificate: Some(
                reqwest::Certificate::from_der(include_bytes!("../../tests/fixtures/tls/ca.der"))
                    .unwrap(),
            ),
            ..Default::default()
        };
        let client = R700Client::new(&config, &options).unwrap();
        let IotInspection {
            readback,
            reader_time,
            running,
            keepalive_timeout: idle_timeout,
            ..
        } = client
            .inspect(&config.id, &ports(), &SystemReaderClock::default())
            .await
            .unwrap();
        assert!(reader_time >= now && reader_time <= Utc::now());
        assert!(running);
        assert_eq!(idle_timeout, Duration::from_secs(120));
        assert_eq!(readback.capabilities.max_power_cdbm, Some(3000));
        assert!(!readback.capabilities.continuous_inventory_proof);
        assert!(!readback.capabilities.inventory_pause);
        assert_eq!(readback.capabilities.min_dwell_ms, None);
        assert_eq!(readback.capabilities.firmware.as_deref(), Some("10.4.0"));
        let mut response = client.stream().await.unwrap();
        assert_eq!(response.chunk().await.unwrap().unwrap().as_ref(), b"\r\n");
        assert!(response.chunk().await.unwrap().is_none());
        assert_eq!(server.join().unwrap().len(), 5);
        let insecure = ReaderConfig {
            connection: ReaderConnection::ImpinjIot {
                base_url: format!("http://{address}"),
                credentials: ReaderCredentials::None,
            },
            ..config
        };
        assert!(matches!(
            R700Client::new(&insecure, &options),
            Err(ReaderError::Configuration)
        ));
    }
    #[tokio::test]
    async fn real_https_reader_clock_sample_normalizes_modern_fast_wire_reports() {
        let (address, _, server) = https_peer(
            r#"{"keepAliveIntervalSeconds":1,"eventBufferSize":0}"#,
            4,
            1900,
        );
        let config = ReaderConfig {
            id: "reader-a".parse().unwrap(),
            label: "Fictional fast reader".into(),
            enabled: true,
            connection: ReaderConnection::ImpinjIot {
                base_url: format!("https://{address}"),
                credentials: ReaderCredentials::None,
            },
        };
        let options = ReaderOptions {
            ca_certificate: Some(
                reqwest::Certificate::from_der(include_bytes!("../../tests/fixtures/tls/ca.der"))
                    .unwrap(),
            ),
            ..Default::default()
        };
        let clock = SystemReaderClock::default();
        let before = clock.now();
        let inspection = R700Client::new(&config, &options)
            .unwrap()
            .inspect(&config.id, &ports(), &clock)
            .await
            .unwrap();
        let after = clock.now();
        assert!(inspection.reader_time >= before + chrono::Duration::milliseconds(1900));
        let sampled_server = inspection
            .time_basis
            .server_time(inspection.reader_time)
            .unwrap();
        assert!(sampled_server >= before && sampled_server <= after);
        let mut decoder = R700Decoder::new(
            config.id,
            &ports(),
            inspection.reader_time,
            Uuid::now_v7(),
            Duration::from_secs(60),
            Duration::from_secs(2),
            DataSource::Live,
        )
        .unwrap();
        decoder.time_basis = inspection.time_basis;
        let received = clock.now();
        let source = received + chrono::Duration::milliseconds(1900);
        let mut event = tag(1, "00AB");
        event.timestamp = source;
        event.tag_inventory_event.as_mut().unwrap().last_seen_time = Some(source);
        let DecodedEvent::Batch(batch) = decoder
            .decode(&serde_json::to_vec(&event).unwrap(), received)
            .unwrap()
        else {
            panic!("actual fast-clock wire report discarded")
        };
        assert!(batch.observations[0].last_seen_at <= received);
        assert!(received - batch.observations[0].last_seen_at <= after - before);
        assert_eq!(server.join().unwrap().len(), 4);
    }
    #[test]
    fn a_forward_clock_change_cannot_publish_future_server_evidence() {
        let mut decoder = decoder();
        decoder.time_basis = ReaderTimeBasis::from_sample(at(0), at(0));
        assert_eq!(
            decode(&mut decoder, &tag(2, "00AB"), 1).unwrap_err(),
            ReaderError::ClockSkew
        );
    }
    #[tokio::test]
    async fn real_https_reader_rejects_unknown_or_enabled_history_before_streaming() {
        for settings in [
            r#"{"keepAliveIntervalSeconds":1}"#,
            r#"{"keepAliveIntervalSeconds":1,"eventBufferSize":300000}"#,
        ] {
            let (address, _now, server) = https_peer(settings, 3, 0);
            let config = ReaderConfig {
                id: "reader-a".parse().unwrap(),
                label: "Fictional reader".into(),
                enabled: true,
                connection: ReaderConnection::ImpinjIot {
                    base_url: format!("https://{address}"),
                    credentials: ReaderCredentials::None,
                },
            };
            let options = ReaderOptions {
                ca_certificate: Some(
                    reqwest::Certificate::from_der(include_bytes!(
                        "../../tests/fixtures/tls/ca.der"
                    ))
                    .unwrap(),
                ),
                ..Default::default()
            };
            let client = R700Client::new(&config, &options).unwrap();
            assert_eq!(
                client
                    .inspect(&config.id, &ports(), &SystemReaderClock::default())
                    .await
                    .unwrap_err(),
                ReaderError::RecoveryRequired
            );
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 3);
            assert!(requests.iter().all(|request| request.starts_with("GET ") && !request.contains("/data/stream")));
        }
    }
}
