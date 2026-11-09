//! Typed, versioned RFID tracking contracts. Factory EPCs are identifiers only.

use std::{collections::BTreeMap, fmt, str::FromStr};

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const TRACKING_SCHEMA_VERSION: u16 = 1;
pub const MAX_UNITS: usize = 20_000;
pub const MAX_BATCH_OBSERVATIONS: usize = 2_048;
pub const MAX_HISTORY_PAGE: u16 = 200;

macro_rules! identifier {
    ($name:ident) => {
        #[derive(
            Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
        )]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);
        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl TryFrom<String> for $name {
            type Error = &'static str;
            fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
                if value.is_empty()
                    || value.len() > 64
                    || !value
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
                {
                    return Err("invalid_tracking_id");
                }
                Ok(Self(value))
            }
        }
        impl FromStr for $name {
            type Err = &'static str;
            fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
                value.to_owned().try_into()
            }
        }
        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}
identifier!(AreaId);
identifier!(ReaderId);
identifier!(PortId);

macro_rules! durable_id {
    ($name:ident) => {
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
        )]
        #[serde(transparent)]
        pub struct $name(pub Uuid);
        impl $name {
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }
        }
        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
        impl FromStr for $name {
            type Err = uuid::Error;
            fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
                value.parse().map(Self)
            }
        }
    };
}
durable_id!(UnitId);
durable_id!(SessionId);

/// Canonical uppercase, variable-length byte hex. Leading zeros are significant.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub struct Epc(String);
impl Epc {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for Epc {
    type Error = &'static str;
    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        if value.len() < 2
            || value.len() > 128
            || !value.len().is_multiple_of(2)
            || !value.bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Err("invalid_epc");
        }
        Ok(Self(value.to_ascii_uppercase()))
    }
}
impl FromStr for Epc {
    type Err = &'static str;
    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        value.to_owned().try_into()
    }
}
impl From<Epc> for String {
    fn from(value: Epc) -> Self {
        value.0
    }
}
impl fmt::Display for Epc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AreaKind {
    Closet,
    Rewear,
    Hamper,
    DryCleaningBag,
}
impl AreaKind {
    pub fn present_status(self) -> UnitStatus {
        match self {
            Self::Closet => UnitStatus::CleanReady,
            Self::Rewear => UnitStatus::WornReady,
            Self::Hamper => UnitStatus::NeedsLaundry,
            Self::DryCleaningBag => UnitStatus::QueuedDryCleaning,
        }
    }
    pub fn absent_status(self) -> UnitStatus {
        match self {
            Self::Closet | Self::Rewear => UnitStatus::BeingWorn,
            Self::Hamper => UnitStatus::InWash,
            Self::DryCleaningBag => UnitStatus::AtCleaners,
        }
    }
}
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum UnitStatus {
    #[default]
    Unknown,
    CleanReady,
    WornReady,
    NeedsLaundry,
    QueuedDryCleaning,
    BeingWorn,
    InWash,
    AtCleaners,
}
impl UnitStatus {
    pub fn is_ready(self) -> bool {
        matches!(self, Self::CleanReady | Self::WornReady)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::CleanReady => "clean_ready",
            Self::WornReady => "worn_ready",
            Self::NeedsLaundry => "needs_laundry",
            Self::QueuedDryCleaning => "queued_dry_cleaning",
            Self::BeingWorn => "being_worn",
            Self::InWash => "in_wash",
            Self::AtCleaners => "at_cleaners",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TrackingException {
    NeedsBaseline,
    QuantityUnknown,
    NeedsReconciliation,
    AmbiguousReads,
    InsufficientEvidence,
    Paused,
    Archived,
    EnrollmentRecovery,
}
impl TrackingException {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NeedsBaseline => "needs_baseline",
            Self::QuantityUnknown => "quantity_unknown",
            Self::NeedsReconciliation => "needs_reconciliation",
            Self::AmbiguousReads => "ambiguous_reads",
            Self::InsufficientEvidence => "insufficient_evidence",
            Self::Paused => "paused",
            Self::Archived => "archived",
            Self::EnrollmentRecovery => "enrollment_recovery",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AreaConfig {
    pub id: AreaId,
    pub label: String,
    pub kind: AreaKind,
    pub enabled: bool,
    #[serde(default)]
    pub thresholds: AreaThresholds,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReaderConnection {
    Simulator,
    ImpinjLlrp {
        host: String,
        port: u16,
        owned_rospec_id: u32,
    },
    ImpinjIot {
        base_url: String,
        credentials: ReaderCredentials,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReaderCredentials {
    None,
    BasicEnv { username: String, password: String },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReaderConfig {
    pub id: ReaderId,
    pub label: String,
    pub enabled: bool,
    pub connection: ReaderConnection,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PortRole {
    Monitor,
    Enrollment,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PortConfig {
    pub id: PortId,
    pub reader_id: ReaderId,
    pub antenna: u16,
    pub area_id: Option<AreaId>,
    pub role: PortRole,
    pub enabled: bool,
    pub required_for_absence: bool,
    /// Operator commission is necessary but never substitutes for live proof.
    pub commissioned: bool,
    pub transmit_power_cdbm: Option<i32>,
    pub dwell_ms: Option<u32>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Thresholds {
    pub positive_dwell_ms: u64,
    pub positive_min_reads: u32,
    pub movement_hysteresis_ms: u64,
    pub absence_ms: u64,
    pub warmup_ms: u64,
    pub coverage_max_gap_ms: u64,
    pub candidate_fresh_ms: u64,
    pub reader_event_max_age_ms: u64,
}
impl Default for Thresholds {
    fn default() -> Self {
        Self {
            positive_dwell_ms: 1_500,
            positive_min_reads: 3,
            movement_hysteresis_ms: 2_500,
            absence_ms: 120_000,
            warmup_ms: 30_000,
            coverage_max_gap_ms: 5_000,
            candidate_fresh_ms: 5_000,
            reader_event_max_age_ms: 60_000,
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AreaThresholds {
    pub positive_dwell_ms: Option<u64>,
    pub positive_min_reads: Option<u32>,
    pub movement_hysteresis_ms: Option<u64>,
    pub absence_ms: Option<u64>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ObservationPurpose {
    Inventory,
    Enrollment,
    Test,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DataSource {
    Live,
    Simulation,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReaderCapabilities {
    pub antenna_ports: Vec<u16>,
    pub min_power_cdbm: Option<i32>,
    pub max_power_cdbm: Option<i32>,
    pub min_dwell_ms: Option<u32>,
    pub max_dwell_ms: Option<u32>,
    pub continuous_inventory_proof: bool,
    pub inventory_pause: bool,
    pub firmware: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PortReadback {
    pub port_id: PortId,
    pub antenna: u16,
    pub enabled: bool,
    pub transmit_power_cdbm: Option<i32>,
    pub dwell_ms: Option<u32>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReaderReadback {
    pub reader_id: ReaderId,
    pub at: DateTime<Utc>,
    pub capabilities: ReaderCapabilities,
    pub ports: Vec<PortReadback>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(from = "TrackingConfigWire", into = "TrackingConfigWire")]
pub struct TrackingConfig {
    pub schema_version: u16,
    pub areas: Vec<AreaConfig>,
    pub readers: Vec<ReaderConfig>,
    pub ports: Vec<PortConfig>,
    pub thresholds: Thresholds,
    /// Legacy request spelling, used only to recognize operation journal hashes.
    #[doc(hidden)]
    pub previous_wire: PreviousTrackingConfig,
}
#[derive(Clone, Debug, Default)]
pub struct PreviousTrackingConfig(Option<bool>);

// Keep the compatibility bit at the serialization boundary. It never controls
// tracking and is deliberately excluded from domain equality.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TrackingConfigWire {
    schema_version: u16,
    /// Computed from enabled areas. Legacy input is accepted but cannot disable tracking.
    #[serde(default)]
    enabled: bool,
    areas: Vec<AreaConfig>,
    readers: Vec<ReaderConfig>,
    ports: Vec<PortConfig>,
    thresholds: Thresholds,
}
impl From<TrackingConfigWire> for TrackingConfig {
    fn from(wire: TrackingConfigWire) -> Self {
        Self {
            schema_version: wire.schema_version,
            areas: wire.areas,
            readers: wire.readers,
            ports: wire.ports,
            thresholds: wire.thresholds,
            previous_wire: PreviousTrackingConfig(Some(wire.enabled)),
        }
    }
}
impl From<TrackingConfig> for TrackingConfigWire {
    fn from(config: TrackingConfig) -> Self {
        let enabled = config.enabled();
        Self {
            schema_version: config.schema_version,
            enabled,
            areas: config.areas,
            readers: config.readers,
            ports: config.ports,
            thresholds: config.thresholds,
        }
    }
}
impl JsonSchema for TrackingConfig {
    fn schema_name() -> String {
        "TrackingConfig".into()
    }
    fn json_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        TrackingConfigWire::json_schema(generator)
    }
}
impl PartialEq for TrackingConfig {
    fn eq(&self, other: &Self) -> bool {
        self.schema_version == other.schema_version
            && self.areas == other.areas
            && self.readers == other.readers
            && self.ports == other.ports
            && self.thresholds == other.thresholds
    }
}
impl Eq for TrackingConfig {}
impl TrackingConfig {
    pub fn enabled(&self) -> bool {
        self.areas.iter().any(|area| area.enabled)
    }
    /// Enrollment positions can operate before any monitored area exists.
    pub fn reader_active(&self, reader_id: &ReaderId) -> bool {
        self.readers
            .iter()
            .any(|reader| &reader.id == reader_id && reader.enabled)
            && self
                .ports
                .iter()
                .any(|port| &port.reader_id == reader_id && port.enabled)
    }
}
impl Default for TrackingConfig {
    fn default() -> Self {
        Self {
            schema_version: TRACKING_SCHEMA_VERSION,
            areas: Vec::new(),
            readers: Vec::new(),
            ports: Vec::new(),
            thresholds: Thresholds::default(),
            previous_wire: PreviousTrackingConfig::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CoverageProof {
    /// Producer must have independently proven continuous inventory on this port.
    VerifiedInventory,
    /// TCP/keepalive/tag reads alone cannot prove healthy empty-area coverage.
    TransportOnly,
    Unknown,
    Fault {
        code: ReaderFault,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReaderFault {
    Disconnected,
    InventoryStopped,
    AntennaFault,
    ClockSkew,
    Protocol,
    RecoveryRequired,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PortCoverage {
    pub port_id: PortId,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub proof: CoverageProof,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TagObservation {
    pub epc: Epc,
    pub port_id: PortId,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub read_count: u32,
    pub tid: Option<String>,
    pub peak_rssi_cdbm: Option<i32>,
    pub event_ref: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObservationBatch {
    pub reader_id: ReaderId,
    pub purpose: ObservationPurpose,
    pub data_source: DataSource,
    pub capture_session: Option<SessionId>,
    pub session: Uuid,
    pub sequence: u64,
    pub observed_at: DateTime<Utc>,
    pub connected: bool,
    pub coverage: Vec<PortCoverage>,
    pub observations: Vec<TagObservation>,
    pub readback: Option<ReaderReadback>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CoverageState {
    Disabled,
    Unverified,
    WarmingUp,
    Healthy,
    Disconnected,
    Fault,
    Borrowed,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PortHealth {
    pub port_id: PortId,
    pub state: CoverageState,
    pub healthy_since: Option<DateTime<Utc>>,
    pub last_coverage_at: Option<DateTime<Utc>>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TrackingHealth {
    pub absence_coverage_healthy: bool,
    pub warming_up: bool,
    pub required_ports: usize,
    pub ports: Vec<PortHealth>,
    pub recent_unknown_tag_count: usize,
    pub restart_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstallationState {
    #[default]
    NeedsTag,
    #[serde(alias = "seamstress_queued")]
    Queued,
    Installed,
    Exempt,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Installation {
    pub state: InstallationState,
    pub tag_model: Option<String>,
    pub placement: Option<String>,
    pub notes: Option<String>,
    pub no_tag_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StateEvidence {
    #[default]
    Unknown,
    Manual,
    Rfid,
}
/// Snapshot deliberately exposes binding presence, not raw EPCs in inventory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PhysicalUnit {
    pub id: UnitId,
    pub catalog_id: String,
    pub ordinal: u32,
    pub status: UnitStatus,
    pub last_confirmed_area: Option<AreaId>,
    pub current_area: Option<AreaId>,
    pub has_binding: bool,
    pub installation: Installation,
    pub baseline_at: Option<DateTime<Utc>>,
    pub evidence: StateEvidence,
    pub fence_at: DateTime<Utc>,
    pub paused: bool,
    pub retired: bool,
    pub archived: bool,
    pub exception: Option<TrackingException>,
    pub changed_at: DateTime<Utc>,
    pub last_event_id: Option<i64>,
}
impl PhysicalUnit {
    pub fn ready_to_wear(&self) -> bool {
        !self.retired && !self.archived && self.baseline_at.is_some() && self.status.is_ready()
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InventoryUnits {
    pub catalog_id: String,
    pub catalog_quantity: Option<u32>,
    pub declared_quantity: Option<u32>,
    pub active_units: u32,
    pub unallocated_units: u32,
    pub capacity_limited: bool,
    pub needs_reconciliation: bool,
    pub archived: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TrackingCounts {
    pub certified_ready: usize,
    pub unallocated_units: u64,
    pub quantity_unknown_items: usize,
    pub capacity_limited_items: usize,
    pub total_units: usize,
    pub tagged_units: usize,
    pub clean_ready: usize,
    pub worn_ready: usize,
    pub needs_laundry: usize,
    pub queued_dry_cleaning: usize,
    pub being_worn: usize,
    pub in_wash: usize,
    pub at_cleaners: usize,
    pub unknown: usize,
    pub exceptions: usize,
    pub retired: usize,
    pub archived: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentPurpose {
    Bind,
    Replace,
    Lookup,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentState {
    Active,
    Complete,
    Aborted,
    Expired,
    RecoveryRequired,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BorrowState {
    NotBorrowed,
    PauseRequested,
    Paused,
    RestoreRequested,
    Restored,
    Failed,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentCandidate {
    pub epc: Epc,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub read_count: u64,
    pub bound_unit_id: Option<UnitId>,
    pub selectable: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentSession {
    pub id: SessionId,
    pub purpose: EnrollmentPurpose,
    pub port_id: PortId,
    pub queue: Vec<UnitId>,
    pub cursor: usize,
    pub state: EnrollmentState,
    pub capture_version: u32,
    pub capture_started_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub borrow_state: BorrowState,
    pub candidates: Vec<EnrollmentCandidate>,
    pub capture_overflow: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TrackingSnapshot {
    pub schema_version: u16,
    pub tracking_revision: i64,
    pub config: TrackingConfig,
    pub inventory: Vec<InventoryUnits>,
    pub units: Vec<PhysicalUnit>,
    pub diagnostics: TrackingDiagnostics,
    pub reader_readbacks: Vec<ReaderReadback>,
    pub reader_operations: Vec<ReaderOperation>,
    pub reader_controls: Vec<ReaderControlOperation>,
    pub configuration_version: i64,
    pub certified_ready_unit_ids: Vec<UnitId>,
    pub counts: TrackingCounts,
    pub health: TrackingHealth,
    pub sessions: Vec<EnrollmentSession>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TrackingCommand {
    Configure {
        config: TrackingConfig,
    },
    DeclareUnitCount {
        catalog_id: String,
        count: u32,
    },
    ReconcileUnits {
        catalog_id: String,
    },
    Installation {
        unit_id: UnitId,
        installation: Installation,
    },
    Baseline {
        unit_id: UnitId,
        area_id: AreaId,
    },
    Correct {
        unit_id: UnitId,
        status: UnitStatus,
        note: String,
    },
    Pause {
        unit_id: UnitId,
        paused: bool,
    },
    Retire {
        unit_id: UnitId,
        note: String,
    },
    Unbind {
        unit_id: UnitId,
        note: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        mistaken: bool,
    },
    Undo {
        event_id: i64,
        note: String,
    },
    StartEnrollment {
        unit_ids: Vec<UnitId>,
        port_id: PortId,
        borrow: bool,
        purpose: EnrollmentPurpose,
        ttl_seconds: u32,
    },
    ResetCapture {
        session_id: SessionId,
    },
    ResumeEnrollment {
        session_id: SessionId,
        ttl_seconds: u32,
    },
    SelectCandidate {
        session_id: SessionId,
        capture_version: u32,
        unit_id: UnitId,
        epc: Epc,
        baseline_area: Option<AreaId>,
    },
    Skip {
        session_id: SessionId,
        capture_version: u32,
        unit_id: UnitId,
    },
    AbortEnrollment {
        session_id: SessionId,
    },
    /// Reader adapter acknowledges inventory configuration, never RF tag writes.
    AcknowledgeBorrow {
        session_id: SessionId,
        state: BorrowState,
    },
    Tick,
    Import {
        data: Box<TrackingExport>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandResult {
    pub tracking_revision: i64,
    pub changed: bool,
    pub unit_ids: Vec<UnitId>,
    pub session: Option<EnrollmentSession>,
    pub event_ids: Vec<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestResult {
    pub tracking_revision: i64,
    pub accepted: bool,
    pub duplicate: bool,
    pub unit_changes: usize,
    pub candidate_changes: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DecisionReason {
    ManualBaseline,
    StableArea,
    ConfirmedAbsence,
    ManualCorrection,
    TrackingPause,
    Retirement,
    TagUnbound,
    MistakenPairing,
    TagBound,
    TagReplaced,
    Undo,
    CatalogArchive,
    CatalogRestore,
    QuantityReconciliation,
    AmbiguousEvidence,
    InsufficientEvidence,
    CoverageUnavailable,
    OldEvidence,
    InvalidClock,
    EnrollmentIsolation,
    InstallationUpdate,
    Import,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObservationRef {
    pub reader_id: ReaderId,
    pub session: Uuid,
    pub sequence: u64,
    pub port_id: PortId,
    pub observed_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub event_ref: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecentRead {
    pub epc: Epc,
    pub port_id: PortId,
    pub reader_id: ReaderId,
    pub observed_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub purpose: ObservationPurpose,
    pub data_source: DataSource,
    pub read_count: u32,
    pub tid: Option<String>,
    pub peak_rssi_cdbm: Option<i32>,
    pub event_ref: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DecisionDiagnostic {
    pub unit_id: UnitId,
    pub at: DateTime<Utc>,
    pub reason: DecisionReason,
    pub area_id: Option<AreaId>,
    pub evidence: Vec<ObservationRef>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FaultDiagnostic {
    pub at: DateTime<Utc>,
    pub reader_id: Option<ReaderId>,
    pub port_id: Option<PortId>,
    pub code: ReaderFault,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TrackingDiagnostics {
    pub recent_reads: Vec<RecentRead>,
    pub decisions: Vec<DecisionDiagnostic>,
    pub faults: Vec<FaultDiagnostic>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HistoryKind {
    Baseline,
    Presence,
    Absence,
    Correction,
    Pause,
    Retire,
    Unbind,
    Bind,
    Replace,
    Undo,
    Archive,
    Restore,
    QuantityReconciliation,
    Import,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryEvent {
    pub id: i64,
    pub unit_id: UnitId,
    pub at: DateTime<Utc>,
    pub actor: String,
    pub kind: HistoryKind,
    pub reason: DecisionReason,
    pub automatic: bool,
    pub configuration_version: i64,
    pub source_area: Option<AreaId>,
    pub evidence: Vec<ObservationRef>,
    pub note: Option<String>,
    pub before: Option<StoredUnit>,
    pub after: StoredUnit,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryPage {
    pub events: Vec<HistoryEvent>,
    pub next_before: Option<i64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StoredUnit {
    pub unit: PhysicalUnit,
    pub epc: Option<Epc>,
    pub last_confirmed_kind: Option<AreaKind>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CoverageEndReason {
    CoverageUnavailable,
    Restart,
    ConfigurationChanged,
    TrackingDisabled,
    Import,
}
impl CoverageEndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CoverageUnavailable => "coverage_unavailable",
            Self::Restart => "restart",
            Self::ConfigurationChanged => "configuration_changed",
            Self::TrackingDisabled => "tracking_disabled",
            Self::Import => "import",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CoverageInterval {
    pub id: i64,
    pub started_at: DateTime<Utc>,
    pub verified_through: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub end_reason: Option<CoverageEndReason>,
    pub configuration_version: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CoverageHistoryPage {
    pub intervals: Vec<CoverageInterval>,
    pub next_before: Option<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TrackingExport {
    pub data_source: DataSource,
    pub configurations: Vec<ConfigurationVersion>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub coverage: Vec<CoverageInterval>,
    pub schema_version: u16,
    pub exported_at: DateTime<Utc>,
    pub tracking_revision: i64,
    pub configuration_version: i64,
    pub config: TrackingConfig,
    pub inventory: Vec<InventoryUnits>,
    pub units: Vec<StoredUnit>,
    pub sessions: Vec<EnrollmentSession>,
    pub history: Vec<HistoryEvent>,
    pub bindings: Vec<TagBinding>,
    pub operations: Vec<OperationRecord>,
    pub reader_operations: Vec<ReaderOperation>,
    pub reader_controls: Vec<ReaderControlOperation>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BindingRetirement {
    MistakenPairing,
    Replaced,
    Unbound,
    UnitRetired,
    Undo,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TagBinding {
    pub id: i64,
    pub unit_id: UnitId,
    pub epc: Epc,
    pub bound_at: DateTime<Utc>,
    pub retired_at: Option<DateTime<Utc>>,
    pub retirement: Option<BindingRetirement>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigurationVersion {
    pub version: i64,
    pub at: DateTime<Utc>,
    pub config: TrackingConfig,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperationRecord {
    pub actor: String,
    pub key: String,
    pub request_hash: String,
    pub response: CommandResult,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReaderOperationPhase {
    Pending,
    Applied,
    RecoveryRequired,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReaderOperation {
    pub reader_id: ReaderId,
    pub owned_rospec_id: u32,
    pub configuration_version: i64,
    pub pending_fingerprint: Option<String>,
    pub applied_fingerprint: Option<String>,
    pub known_fingerprints: Vec<String>,
    pub phase: ReaderOperationPhase,
    pub updated_at: DateTime<Utc>,
}
impl ReaderOperation {
    /// Ownership survives an uncertain reply until exact absence is confirmed.
    pub fn requires_cleanup(&self) -> bool {
        self.pending_fingerprint.is_some()
            || self.applied_fingerprint.is_some()
            || !self.known_fingerprints.is_empty()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KeepaliveTrigger {
    None,
    Periodic,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LlrpEventSetting {
    pub event_type: u16,
    pub enabled: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LlrpKeepalive {
    pub trigger: KeepaliveTrigger,
    pub interval_ms: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LlrpControlSettings {
    pub notifications: Vec<LlrpEventSetting>,
    pub keepalive: LlrpKeepalive,
    pub hold_events_and_reports_upon_reconnect: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReaderControlPhase {
    Pending,
    Applied,
    RestorePending,
    Restored,
    RecoveryRequired,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReaderControlOperation {
    pub reader_id: ReaderId,
    pub configuration_version: i64,
    pub original: LlrpControlSettings,
    pub applied: LlrpControlSettings,
    pub phase: ReaderControlPhase,
    pub updated_at: DateTime<Utc>,
}
impl ReaderControlOperation {
    pub fn requires_cleanup(&self) -> bool {
        self.phase != ReaderControlPhase::Restored
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReaderControlPlan {
    pub operation: ReaderControlOperation,
    pub action: ReaderPlanAction,
    pub target: LlrpControlSettings,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReaderPlanAction {
    Create,
    Keep,
    Replace,
    Recover,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReaderPlan {
    pub operation: ReaderOperation,
    pub action: ReaderPlanAction,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Availability {
    pub tracking_revision: i64,
    pub eligible_catalog_ids: Vec<String>,
    pub certified_ready_catalog_ids: Vec<String>,
}
#[derive(Clone, Debug, Default)]
pub struct ReconcileResult {
    pub changed: bool,
    pub created_units: Vec<UnitId>,
}

#[derive(Debug, thiserror::Error)]
pub enum TrackingError {
    #[error("{code}")]
    Invalid {
        code: &'static str,
        field: &'static str,
    },
    #[error("{0}")]
    Conflict(&'static str),
    #[error("tracking_revision_conflict")]
    Revision,
    #[error("not_found")]
    NotFound,
    #[error("storage_unavailable")]
    Database(#[from] sqlx::Error),
    #[error("stored_data_invalid")]
    Serialization(#[from] serde_json::Error),
}
impl TrackingError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid { code, .. } | Self::Conflict(code) => code,
            Self::Revision => "tracking_revision_conflict",
            Self::NotFound => "not_found",
            Self::Database(_) => "storage_unavailable",
            Self::Serialization(_) => "stored_data_invalid",
        }
    }
}
pub type Result<T, E = TrackingError> = std::result::Result<T, E>;

/// Runtime evidence is deliberately excluded from durable export: restart must
/// earn fresh continuous coverage. These aliases keep bounded reducers explicit.
pub(crate) type UnitMap = BTreeMap<UnitId, StoredUnit>;

/// The 0.2 operation journal hashes serialized command bytes. These frozen typed
/// views recognize that spelling only during replay; live writes stay canonical.
pub(crate) fn previous_command_hash(
    command: &TrackingCommand,
) -> serde_json::Result<Option<String>> {
    legacy_hash::command_hash(command)
}
pub(crate) fn previous_activation_hash(
    command: &TrackingCommand,
) -> serde_json::Result<Option<String>> {
    legacy_hash::activation_hash(command)
}
mod legacy_hash {
    use crate::tracking::models::*;
    use serde::{Serialize, Serializer, ser::SerializeSeq};

    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    enum OldState {
        NeedsTag,
        #[serde(rename = "seamstress_queued")]
        Queued,
        Installed,
        Exempt,
    }
    fn state<S: Serializer>(value: &InstallationState, serializer: S) -> Result<S::Ok, S::Error> {
        match value {
            InstallationState::NeedsTag => OldState::NeedsTag,
            InstallationState::Queued => OldState::Queued,
            InstallationState::Installed => OldState::Installed,
            InstallationState::Exempt => OldState::Exempt,
        }
        .serialize(serializer)
    }
    #[derive(Serialize)]
    #[serde(remote = "Installation")]
    struct OldInstallation {
        #[serde(serialize_with = "state")]
        state: InstallationState,
        tag_model: Option<String>,
        placement: Option<String>,
        notes: Option<String>,
        no_tag_reason: Option<String>,
    }
    #[derive(Serialize)]
    #[serde(remote = "PhysicalUnit")]
    struct OldPhysicalUnit {
        id: UnitId,
        catalog_id: String,
        ordinal: u32,
        status: UnitStatus,
        last_confirmed_area: Option<AreaId>,
        current_area: Option<AreaId>,
        has_binding: bool,
        #[serde(with = "OldInstallation")]
        installation: Installation,
        baseline_at: Option<chrono::DateTime<chrono::Utc>>,
        evidence: StateEvidence,
        fence_at: chrono::DateTime<chrono::Utc>,
        paused: bool,
        retired: bool,
        archived: bool,
        exception: Option<TrackingException>,
        changed_at: chrono::DateTime<chrono::Utc>,
        last_event_id: Option<i64>,
    }
    #[derive(Serialize)]
    #[serde(remote = "StoredUnit")]
    struct OldUnit {
        #[serde(with = "OldPhysicalUnit")]
        unit: PhysicalUnit,
        epc: Option<crate::tracking::models::Epc>,
        last_confirmed_kind: Option<AreaKind>,
    }
    struct UnitRef<'a>(&'a StoredUnit);
    impl Serialize for UnitRef<'_> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            OldUnit::serialize(self.0, serializer)
        }
    }
    fn optional_unit<S: Serializer>(
        value: &Option<StoredUnit>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.as_ref().map(UnitRef).serialize(serializer)
    }
    fn units<S: Serializer>(value: &[StoredUnit], serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(value.len()))?;
        for unit in value {
            sequence.serialize_element(&UnitRef(unit))?;
        }
        sequence.end()
    }
    #[derive(Serialize)]
    #[serde(remote = "HistoryEvent")]
    struct OldHistory {
        id: i64,
        unit_id: UnitId,
        at: chrono::DateTime<chrono::Utc>,
        actor: String,
        kind: HistoryKind,
        reason: DecisionReason,
        automatic: bool,
        configuration_version: i64,
        source_area: Option<AreaId>,
        evidence: Vec<ObservationRef>,
        note: Option<String>,
        #[serde(serialize_with = "optional_unit")]
        before: Option<StoredUnit>,
        #[serde(with = "OldUnit")]
        after: StoredUnit,
    }
    struct HistoryRef<'a>(&'a HistoryEvent);
    impl Serialize for HistoryRef<'_> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            OldHistory::serialize(self.0, serializer)
        }
    }
    fn history<S: Serializer>(value: &[HistoryEvent], serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(value.len()))?;
        for event in value {
            sequence.serialize_element(&HistoryRef(event))?;
        }
        sequence.end()
    }
    fn previous_config<S: Serializer>(
        config: &TrackingConfig,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut wire = TrackingConfigWire::from(config.clone());
        wire.enabled = config.previous_wire.0.unwrap_or(wire.enabled);
        wire.serialize(serializer)
    }
    #[derive(Serialize)]
    struct PreviousConfiguration<'a> {
        version: i64,
        at: chrono::DateTime<chrono::Utc>,
        #[serde(serialize_with = "previous_config")]
        config: &'a TrackingConfig,
    }
    fn configurations<S: Serializer>(
        values: &[ConfigurationVersion],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(values.len()))?;
        for value in values {
            sequence.serialize_element(&PreviousConfiguration {
                version: value.version,
                at: value.at,
                config: &value.config,
            })?;
        }
        sequence.end()
    }
    #[derive(Serialize)]
    #[serde(remote = "TrackingExport")]
    struct PreviousActivationExport {
        data_source: DataSource,
        #[serde(serialize_with = "configurations")]
        configurations: Vec<ConfigurationVersion>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        coverage: Vec<CoverageInterval>,
        schema_version: u16,
        exported_at: chrono::DateTime<chrono::Utc>,
        tracking_revision: i64,
        configuration_version: i64,
        #[serde(serialize_with = "previous_config")]
        config: TrackingConfig,
        inventory: Vec<InventoryUnits>,
        units: Vec<StoredUnit>,
        sessions: Vec<EnrollmentSession>,
        history: Vec<HistoryEvent>,
        bindings: Vec<TagBinding>,
        operations: Vec<OperationRecord>,
        reader_operations: Vec<ReaderOperation>,
        reader_controls: Vec<ReaderControlOperation>,
    }
    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum PreviousActivationCommand<'a> {
        Configure {
            #[serde(serialize_with = "previous_config")]
            config: &'a TrackingConfig,
        },
        Import {
            #[serde(with = "PreviousActivationExport")]
            data: &'a TrackingExport,
        },
    }
    pub(super) fn activation_hash(command: &TrackingCommand) -> serde_json::Result<Option<String>> {
        let previous = match command {
            TrackingCommand::Configure { config } => {
                PreviousActivationCommand::Configure { config }
            }
            TrackingCommand::Import { data } => PreviousActivationCommand::Import { data },
            _ => return Ok(None),
        };
        serde_json::to_vec(&previous).map(|bytes| Some(crate::models::digest(&bytes)))
    }
    #[derive(Serialize)]
    #[serde(remote = "TrackingExport")]
    struct OldExport {
        data_source: DataSource,
        #[serde(serialize_with = "configurations")]
        configurations: Vec<ConfigurationVersion>,
        schema_version: u16,
        exported_at: chrono::DateTime<chrono::Utc>,
        tracking_revision: i64,
        configuration_version: i64,
        #[serde(serialize_with = "previous_config")]
        config: TrackingConfig,
        inventory: Vec<InventoryUnits>,
        #[serde(serialize_with = "units")]
        units: Vec<StoredUnit>,
        sessions: Vec<EnrollmentSession>,
        #[serde(serialize_with = "history")]
        history: Vec<HistoryEvent>,
        bindings: Vec<TagBinding>,
        operations: Vec<OperationRecord>,
        reader_operations: Vec<ReaderOperation>,
        reader_controls: Vec<ReaderControlOperation>,
    }
    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum OldCommand<'a> {
        Installation {
            unit_id: UnitId,
            #[serde(with = "OldInstallation")]
            installation: &'a Installation,
        },
        Import {
            #[serde(with = "OldExport")]
            data: &'a TrackingExport,
        },
    }
    pub(super) fn command_hash(command: &TrackingCommand) -> serde_json::Result<Option<String>> {
        let previous = match command {
            TrackingCommand::Installation {
                unit_id,
                installation,
            } => OldCommand::Installation {
                unit_id: *unit_id,
                installation,
            },
            TrackingCommand::Import { data } if data.coverage.is_empty() => {
                OldCommand::Import { data }
            }
            _ => return Ok(None),
        };
        serde_json::to_vec(&previous).map(|bytes| Some(crate::models::digest(&bytes)))
    }
}
