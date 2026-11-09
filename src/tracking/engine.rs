//! Pure deterministic inference. Time, identities and observations are inputs.
//! Runtime evidence is bounded and deliberately discarded on restart/reconfigure.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use crate::tracking::models::*;

const MAX_RECENT_READS: usize = 200;
const MAX_DIAGNOSTICS: usize = 100;
const MAX_RUNTIME_TAGS: usize = 2_048;
const MAX_EVENT_REFS: usize = 4_096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineMode {
    Live,
    Simulation,
}
#[derive(Clone, Debug)]
pub(crate) struct UnitChange {
    pub before: Option<StoredUnit>,
    pub after: StoredUnit,
    pub kind: HistoryKind,
    pub reason: DecisionReason,
    pub evidence: Vec<ObservationRef>,
    pub note: Option<String>,
}
impl UnitChange {
    pub(crate) fn new(
        before: Option<StoredUnit>,
        after: StoredUnit,
        kind: HistoryKind,
        reason: DecisionReason,
        note: Option<String>,
        evidence: Vec<ObservationRef>,
    ) -> Self {
        Self {
            before,
            after,
            kind,
            reason,
            note,
            evidence,
        }
    }
}
#[derive(Clone, Debug)]
pub(crate) struct DurableState {
    pub revision: i64,
    pub configuration_version: i64,
    pub config: TrackingConfig,
    pub units: UnitMap,
    pub inventory: BTreeMap<String, InventoryUnits>,
    pub sessions: BTreeMap<SessionId, EnrollmentSession>,
    pub reader_operations: BTreeMap<ReaderId, ReaderOperation>,
    pub reader_controls: BTreeMap<ReaderId, ReaderControlOperation>,
}
#[derive(Clone, Debug, Default)]
pub(crate) struct Changes {
    pub units: Vec<UnitChange>,
    pub sessions_changed: bool,
    pub accepted: bool,
    pub duplicate: bool,
}
#[derive(Clone, Debug)]
struct ReaderEvidence {
    session: Uuid,
    sequence: u64,
    latest_observed_at: DateTime<Utc>,
    received_at: DateTime<Utc>,
    epoch_at: DateTime<Utc>,
    connected: bool,
}
#[derive(Clone, Debug)]
struct Coverage {
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    state: CoverageState,
    reference: ObservationRef,
}
#[derive(Clone, Debug)]
struct Positive {
    area: AreaId,
    first: DateTime<Utc>,
    last: DateTime<Utc>,
    count: u64,
    refs: Vec<ObservationRef>,
}

#[derive(Clone, Debug)]
pub(crate) struct CoverageProgress {
    pub interval: CoverageInterval,
    pub latest_verified: DateTime<Utc>,
    pub checkpoint_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct Engine {
    pub(crate) revision: i64,
    pub(crate) configuration_version: i64,
    pub(crate) config: TrackingConfig,
    pub(crate) units: UnitMap,
    pub(crate) inventory: BTreeMap<String, InventoryUnits>,
    pub(crate) sessions: BTreeMap<SessionId, EnrollmentSession>,
    pub(crate) reader_operations: BTreeMap<ReaderId, ReaderOperation>,
    pub(crate) reader_controls: BTreeMap<ReaderId, ReaderControlOperation>,
    pub(crate) mode: EngineMode,
    pub(crate) coverage_progress: Option<CoverageProgress>,
    restart_at: DateTime<Utc>,
    clock: DateTime<Utc>,
    readers: BTreeMap<ReaderId, ReaderEvidence>,
    coverage: BTreeMap<PortId, Coverage>,
    positives: BTreeMap<UnitId, Positive>,
    area_recent: BTreeMap<UnitId, BTreeMap<AreaId, DateTime<Utc>>>,
    seen_any: BTreeMap<UnitId, DateTime<Utc>>,
    absence_since: BTreeMap<UnitId, DateTime<Utc>>,
    unknown_tags: BTreeMap<Epc, DateTime<Utc>>,
    event_refs: BTreeSet<String>,
    event_order: VecDeque<String>,
    recent_reads: VecDeque<RecentRead>,
    decisions: VecDeque<DecisionDiagnostic>,
    faults: VecDeque<FaultDiagnostic>,
    decision_last: BTreeMap<UnitId, DecisionReason>,
    pub(crate) readbacks: BTreeMap<ReaderId, ReaderReadback>,
    retired_sessions: BTreeMap<ReaderId, VecDeque<Uuid>>,
}
impl Default for Engine {
    fn default() -> Self {
        Self::new(EngineMode::Live, DateTime::<Utc>::UNIX_EPOCH)
    }
}
impl Engine {
    pub fn new(mode: EngineMode, now: DateTime<Utc>) -> Self {
        Self {
            revision: -1,
            configuration_version: 0,
            config: TrackingConfig::default(),
            units: BTreeMap::new(),
            inventory: BTreeMap::new(),
            sessions: BTreeMap::new(),
            reader_operations: BTreeMap::new(),
            reader_controls: BTreeMap::new(),
            mode,
            coverage_progress: None,
            restart_at: now,
            clock: now,
            readers: BTreeMap::new(),
            coverage: BTreeMap::new(),
            positives: BTreeMap::new(),
            area_recent: BTreeMap::new(),
            seen_any: BTreeMap::new(),
            absence_since: BTreeMap::new(),
            unknown_tags: BTreeMap::new(),
            event_refs: BTreeSet::new(),
            event_order: VecDeque::new(),
            recent_reads: VecDeque::new(),
            decisions: VecDeque::new(),
            faults: VecDeque::new(),
            decision_last: BTreeMap::new(),
            readbacks: BTreeMap::new(),
            retired_sessions: BTreeMap::new(),
        }
    }
    pub(crate) fn load(&mut self, state: DurableState, now: DateTime<Utc>) {
        let mode = self.mode;
        *self = Self::new(mode, now);
        self.revision = state.revision;
        self.configuration_version = state.configuration_version;
        self.config = state.config;
        self.units = state.units;
        self.inventory = state.inventory;
        self.sessions = state.sessions;
        self.reader_operations = state.reader_operations;
        self.reader_controls = state.reader_controls;
    }
    pub(crate) fn configure(&mut self, config: TrackingConfig, now: DateTime<Utc>) -> Result<bool> {
        self.clock(now)?;
        self.validate_config(&config)?;
        if self.config == config {
            return Ok(false);
        }
        if self.sessions.values().any(|s| {
            !matches!(
                s.borrow_state,
                BorrowState::NotBorrowed | BorrowState::Restored
            )
        }) {
            return Err(TrackingError::Conflict("enrollment_restore_required"));
        }
        if self.sessions.values().any(|s| {
            matches!(
                s.state,
                EnrollmentState::Active | EnrollmentState::RecoveryRequired
            ) && !config.ports.iter().any(|p| p.id == s.port_id)
        }) {
            return Err(TrackingError::Conflict("enrollment_abort_required"));
        }
        let changed_readers = self
            .reader_operations
            .keys()
            .chain(self.reader_controls.keys())
            .filter(|id| {
                let old = self.config.readers.iter().find(|r| &r.id == *id);
                let new = config.readers.iter().find(|r| &r.id == *id);
                !matches!((old, new), (Some(old), Some(new)) if old.connection == new.connection)
            })
            .cloned()
            .collect::<BTreeSet<_>>();
        if self
            .reader_operations
            .values()
            .any(|op| changed_readers.contains(&op.reader_id) && op.requires_cleanup())
            || self
                .reader_controls
                .values()
                .any(|op| changed_readers.contains(&op.reader_id) && op.requires_cleanup())
        {
            return Err(TrackingError::Conflict("reader_cleanup_required"));
        }
        // Fully released claims describe the previous connection, not a new
        // device that happens to reuse its configured reader ID.
        self.reader_operations
            .retain(|id, _| !changed_readers.contains(id));
        self.reader_controls
            .retain(|id, _| !changed_readers.contains(id));
        self.config = config;
        self.configuration_version += 1;
        self.reset_evidence(now);
        for session in self
            .sessions
            .values_mut()
            .filter(|s| s.state == EnrollmentState::Active)
        {
            session.capture_version = session.capture_version.saturating_add(1);
            session.capture_started_at = now;
            session.candidates.clear();
            session.capture_overflow = false;
        }
        Ok(true)
    }
    pub(crate) fn requires_reader_cleanup(&self) -> bool {
        self.reader_operations
            .values()
            .any(ReaderOperation::requires_cleanup)
            || self
                .reader_controls
                .values()
                .any(ReaderControlOperation::requires_cleanup)
    }
    pub(crate) fn reset_evidence(&mut self, now: DateTime<Utc>) {
        self.restart_at = now;
        self.clock = now;
        self.readers.clear();
        self.coverage.clear();
        self.positives.clear();
        self.area_recent.clear();
        self.seen_any.clear();
        self.absence_since.clear();
        self.event_refs.clear();
        self.event_order.clear();
        self.readbacks.clear();
        for session in self.sessions.values_mut() {
            session.candidates.clear();
            session.capture_overflow = false;
        }
    }
    pub(crate) fn reset_coverage_for_enrollment(&mut self) {
        self.coverage.clear();
        self.absence_since.clear();
    }
    pub(crate) fn fence(&mut self, id: UnitId) {
        self.positives.remove(&id);
        self.area_recent.remove(&id);
        self.seen_any.remove(&id);
        self.absence_since.remove(&id);
        self.decision_last.remove(&id);
    }
    fn fault(
        &mut self,
        now: DateTime<Utc>,
        reader: Option<ReaderId>,
        port: Option<PortId>,
        code: ReaderFault,
    ) {
        if self
            .faults
            .back()
            .is_some_and(|f| f.reader_id == reader && f.port_id == port && f.code == code)
        {
            return;
        }
        self.faults.push_back(FaultDiagnostic {
            at: now,
            reader_id: reader,
            port_id: port,
            code,
        });
        while self.faults.len() > MAX_DIAGNOSTICS {
            self.faults.pop_front();
        }
    }
    pub(crate) fn rejected(&mut self, reader: ReaderId, now: DateTime<Utc>, error: &TrackingError) {
        let code = if matches!(
            error.code(),
            "stale_observation" | "out_of_order_observation" | "clock_moved_backwards"
        ) {
            ReaderFault::ClockSkew
        } else {
            ReaderFault::Protocol
        };
        self.fault(now, Some(reader), None, code);
    }
    fn decision(
        &mut self,
        id: UnitId,
        now: DateTime<Utc>,
        reason: DecisionReason,
        area: Option<AreaId>,
        evidence: Vec<ObservationRef>,
    ) {
        if self.decision_last.get(&id) == Some(&reason) {
            return;
        }
        self.decision_last.insert(id, reason);
        self.decisions.push_back(DecisionDiagnostic {
            unit_id: id,
            at: now,
            reason,
            area_id: area,
            evidence,
        });
        while self.decisions.len() > MAX_DIAGNOSTICS {
            self.decisions.pop_front();
        }
    }
    pub fn validate_config(&self, config: &TrackingConfig) -> Result<()> {
        let invalid = |field| TrackingError::Invalid {
            code: "invalid_tracking_config",
            field,
        };
        if config.schema_version != TRACKING_SCHEMA_VERSION
            || config.areas.len() > 32
            || config.readers.len() > 16
            || config.ports.len() > 128
        {
            return Err(invalid("config"));
        }
        let t = &config.thresholds;
        if t.positive_min_reads == 0
            || t.positive_min_reads > 1_000_000
            || t.positive_dwell_ms == 0
            || t.positive_dwell_ms > 600_000
            || t.movement_hysteresis_ms > 600_000
            || t.absence_ms < 1_000
            || t.absence_ms > 86_400_000
            || t.warmup_ms < 1
            || t.warmup_ms > 600_000
            || t.coverage_max_gap_ms == 0
            || t.coverage_max_gap_ms > 60_000
            || t.candidate_fresh_ms < 100
            || t.candidate_fresh_ms > 60_000
            || t.reader_event_max_age_ms == 0
            || t.reader_event_max_age_ms > 600_000
        {
            return Err(invalid("thresholds"));
        }
        let mut areas = BTreeSet::new();
        let mut readers = BTreeSet::new();
        let mut ports = BTreeSet::new();
        let mut physical = BTreeSet::new();
        for area in &config.areas {
            if !areas.insert(area.id.clone()) || !text(&area.label, 200) {
                return Err(invalid("areas"));
            }
            let a = &area.thresholds;
            if a.positive_min_reads
                .is_some_and(|v| v == 0 || v > 1_000_000)
                || a.positive_dwell_ms.is_some_and(|v| v == 0 || v > 600_000)
                || a.movement_hysteresis_ms.is_some_and(|v| v > 600_000)
                || a.absence_ms
                    .is_some_and(|v| !(1_000..=86_400_000).contains(&v))
            {
                return Err(invalid("area.thresholds"));
            }
        }
        for reader in &config.readers {
            if !readers.insert(reader.id.clone()) || !text(&reader.label, 200) {
                return Err(invalid("readers"));
            }
            match &reader.connection {
                ReaderConnection::Simulator => {}
                ReaderConnection::ImpinjLlrp {
                    host,
                    port,
                    owned_rospec_id,
                } => {
                    if host.is_empty()
                        || host.len() > 253
                        || host.bytes().any(|v| {
                            v.is_ascii_whitespace() || matches!(v, b'/' | b'@' | b'?' | b'#')
                        })
                        || *port == 0
                        || *owned_rospec_id == 0
                    {
                        return Err(invalid("reader.connection"));
                    }
                }
                ReaderConnection::ImpinjIot {
                    base_url,
                    credentials,
                } => {
                    let url =
                        url::Url::parse(base_url).map_err(|_| invalid("reader.connection"))?;
                    if url.scheme() != "https"
                        || url.host_str().is_none()
                        || !url.username().is_empty()
                        || url.password().is_some()
                        || url.query().is_some()
                        || url.fragment().is_some()
                        || url.path() != "/"
                    {
                        return Err(invalid("reader.connection"));
                    }
                    if let ReaderCredentials::BasicEnv { username, password } = credentials
                        && (!env_name(username) || !env_name(password) || username == password)
                    {
                        return Err(invalid("reader.credentials"));
                    }
                }
            }
        }
        for port in &config.ports {
            if !ports.insert(port.id.clone())
                || !readers.contains(&port.reader_id)
                || port.antenna == 0
                || port.antenna > 64
                || !physical.insert((port.reader_id.clone(), port.antenna))
                || port.area_id.as_ref().is_some_and(|a| !areas.contains(a))
                || port.role == PortRole::Monitor && port.area_id.is_none()
                || port.role == PortRole::Enrollment && port.required_for_absence
            {
                return Err(invalid("ports"));
            }
            if port.transmit_power_cdbm.is_some() || port.dwell_ms.is_some() {
                let readback = self
                    .readbacks
                    .get(&port.reader_id)
                    .ok_or(TrackingError::Conflict("reader_capabilities_unavailable"))?;
                let c = &readback.capabilities;
                if !c.antenna_ports.contains(&port.antenna) {
                    return Err(invalid("port.antenna"));
                }
                if let Some(v) = port.transmit_power_cdbm
                    && !c
                        .min_power_cdbm
                        .zip(c.max_power_cdbm)
                        .is_some_and(|(lo, hi)| (lo..=hi).contains(&v))
                {
                    return Err(invalid("port.power"));
                }
                if let Some(v) = port.dwell_ms
                    && !c
                        .min_dwell_ms
                        .zip(c.max_dwell_ms)
                        .is_some_and(|(lo, hi)| (lo..=hi).contains(&v))
                {
                    return Err(invalid("port.dwell"));
                }
            }
        }
        Ok(())
    }
    pub(crate) fn clock(&mut self, now: DateTime<Utc>) -> Result<()> {
        if now < self.clock {
            return Err(TrackingError::Invalid {
                code: "clock_moved_backwards",
                field: "received_at",
            });
        }
        self.clock = now;
        Ok(())
    }
    pub(crate) fn ingest(
        &mut self,
        batch: ObservationBatch,
        now: DateTime<Utc>,
    ) -> Result<Changes> {
        self.clock(now)?;
        if (self.mode == EngineMode::Live) != (batch.data_source == DataSource::Live) {
            return Err(TrackingError::Conflict("simulation_isolation"));
        }
        let reader = self
            .config
            .readers
            .iter()
            .find(|r| r.id == batch.reader_id && r.enabled)
            .cloned()
            .ok_or(TrackingError::Invalid {
                code: "unconfigured_reader",
                field: "reader_id",
            })?;
        if batch.data_source == DataSource::Live
            && matches!(reader.connection, ReaderConnection::Simulator)
        {
            return Err(TrackingError::Conflict("simulation_isolation"));
        }
        if batch.observations.len() > MAX_BATCH_OBSERVATIONS || batch.coverage.len() > 128 {
            return Err(TrackingError::Invalid {
                code: "observation_batch_too_large",
                field: "observations",
            });
        }
        if batch.session.is_nil()
            || batch.observed_at > now + Duration::seconds(5)
            || elapsed(batch.observed_at, now) > self.config.thresholds.reader_event_max_age_ms
            || batch.observed_at < self.restart_at
        {
            self.fault(now, Some(batch.reader_id), None, ReaderFault::ClockSkew);
            return Err(TrackingError::Invalid {
                code: "stale_observation",
                field: "observed_at",
            });
        }
        if self
            .retired_sessions
            .get(&batch.reader_id)
            .is_some_and(|sessions| sessions.contains(&batch.session))
        {
            return Err(TrackingError::Invalid {
                code: "retired_reader_session",
                field: "session",
            });
        }
        if let Some(previous) = self.readers.get(&batch.reader_id) {
            if previous.session == batch.session && batch.sequence <= previous.sequence {
                return Ok(Changes {
                    duplicate: true,
                    ..Changes::default()
                });
            }
            if batch.observed_at < previous.latest_observed_at {
                return Err(TrackingError::Invalid {
                    code: "out_of_order_observation",
                    field: "observed_at",
                });
            }
        }
        let ports: BTreeMap<_, _> = self
            .config
            .ports
            .iter()
            .filter(|p| p.reader_id == batch.reader_id)
            .map(|p| (p.id.clone(), p.clone()))
            .collect();
        for c in &batch.coverage {
            if !ports.contains_key(&c.port_id)
                || c.started_at > c.ended_at
                || c.ended_at > batch.observed_at
                || c.ended_at < self.restart_at
            {
                return Err(TrackingError::Invalid {
                    code: "invalid_coverage",
                    field: "coverage",
                });
            }
        }
        for o in &batch.observations {
            if !ports.contains_key(&o.port_id)
                || o.read_count == 0
                || o.read_count > 1_000_000
                || o.first_seen_at > o.last_seen_at
                || o.last_seen_at > batch.observed_at
                || o.tid.as_ref().is_some_and(|v| {
                    v.is_empty()
                        || v.len() > 128
                        || !v.len().is_multiple_of(2)
                        || !v.bytes().all(|c| c.is_ascii_hexdigit())
                })
                || o.event_ref.as_ref().is_some_and(|v| !text(v, 256))
                || o.peak_rssi_cdbm
                    .is_some_and(|v| !(-20_000..=0).contains(&v))
            {
                return Err(TrackingError::Invalid {
                    code: "invalid_observation",
                    field: "observations",
                });
            }
        }
        let reconnect = self.readers.get(&batch.reader_id).is_none_or(|r| {
            r.session != batch.session
                || r.connected != batch.connected
                || elapsed(r.received_at, now) > self.config.thresholds.coverage_max_gap_ms
        });
        let epoch_at = if reconnect {
            now
        } else {
            self.readers
                .get(&batch.reader_id)
                .map_or(now, |r| r.epoch_at)
        };
        if let Some(previous) = self
            .readers
            .get(&batch.reader_id)
            .filter(|r| r.session != batch.session)
        {
            let sessions = self
                .retired_sessions
                .entry(batch.reader_id.clone())
                .or_default();
            sessions.push_back(previous.session);
            while sessions.len() > 16 {
                sessions.pop_front();
            }
        }
        let mut changes = Changes {
            accepted: true,
            ..Changes::default()
        };
        if reconnect {
            for port in ports.keys() {
                self.coverage.remove(port);
            }
            self.absence_since.clear();
            self.positives.clear();
            self.area_recent.clear();
            for session in self
                .sessions
                .values_mut()
                .filter(|s| ports.contains_key(&s.port_id) && s.state == EnrollmentState::Active)
            {
                session.candidates.clear();
                session.capture_overflow = false;
                session.capture_started_at = now;
                session.capture_version = session.capture_version.saturating_add(1);
                changes.sessions_changed = true;
            }
        }
        self.readers.insert(
            batch.reader_id.clone(),
            ReaderEvidence {
                session: batch.session,
                sequence: batch.sequence,
                latest_observed_at: batch.observed_at,
                received_at: now,
                epoch_at,
                connected: batch.connected,
            },
        );
        if let Some(readback) = &batch.readback {
            if readback.reader_id != batch.reader_id
                || readback.at > now + Duration::seconds(5)
                || readback.capabilities.antenna_ports.len() > 64
                || readback.ports.len() > 128
            {
                return Err(TrackingError::Invalid {
                    code: "invalid_reader_readback",
                    field: "readback",
                });
            }
            self.readbacks
                .insert(batch.reader_id.clone(), readback.clone());
        }
        if !batch.connected {
            self.fault(
                now,
                Some(batch.reader_id.clone()),
                None,
                ReaderFault::Disconnected,
            );
        }
        if batch.purpose == ObservationPurpose::Inventory {
            for c in &batch.coverage {
                let port = ports.get(&c.port_id).ok_or(TrackingError::Invalid {
                    code: "unconfigured_port",
                    field: "coverage",
                })?;
                let verified = matches!(c.proof, CoverageProof::VerifiedInventory)
                    && (self.mode == EngineMode::Simulation
                        || self
                            .readbacks
                            .get(&batch.reader_id)
                            .is_some_and(|r| r.capabilities.continuous_inventory_proof));
                let state = if !batch.connected {
                    CoverageState::Disconnected
                } else if self.borrowed(&port.id) {
                    CoverageState::Borrowed
                } else if !port.enabled || port.role != PortRole::Monitor {
                    CoverageState::Disabled
                } else if matches!(c.proof, CoverageProof::Fault { .. }) {
                    CoverageState::Fault
                } else if verified {
                    CoverageState::Healthy
                } else {
                    CoverageState::Unverified
                };
                let since = self
                    .coverage
                    .get(&port.id)
                    .filter(|p| {
                        p.state == CoverageState::Healthy
                            && state == CoverageState::Healthy
                            && elapsed(p.until, c.started_at)
                                <= self.config.thresholds.coverage_max_gap_ms
                    })
                    .map_or(c.started_at.max(self.restart_at).max(epoch_at), |p| p.since);
                if state != CoverageState::Healthy {
                    self.absence_since.clear();
                }
                if let CoverageProof::Fault { code } = c.proof {
                    self.fault(
                        now,
                        Some(batch.reader_id.clone()),
                        Some(port.id.clone()),
                        code,
                    );
                }
                self.coverage.insert(
                    port.id.clone(),
                    Coverage {
                        since,
                        until: c.ended_at,
                        state,
                        reference: ObservationRef {
                            reader_id: batch.reader_id.clone(),
                            session: batch.session,
                            sequence: batch.sequence,
                            port_id: port.id.clone(),
                            observed_at: c.ended_at,
                            received_at: now,
                            event_ref: Some("inventory_coverage".into()),
                        },
                    },
                );
            }
        }
        let bindings: BTreeMap<_, _> = self
            .units
            .iter()
            .filter_map(|(id, u)| {
                u.epc
                    .as_ref()
                    .filter(|_| !u.unit.retired)
                    .map(|epc| (epc.clone(), *id))
            })
            .collect();
        let mut grouped: BTreeMap<UnitId, BTreeMap<AreaId, Vec<(TagObservation, ObservationRef)>>> =
            BTreeMap::new();
        let previous_seen = self.seen_any.clone();
        for mut o in batch.observations {
            let port = ports.get(&o.port_id).ok_or(TrackingError::Invalid {
                code: "unconfigured_port",
                field: "observations",
            })?;
            if o.last_seen_at <= epoch_at
                || o.last_seen_at < self.restart_at
                || elapsed(o.last_seen_at, now) > self.config.thresholds.reader_event_max_age_ms
            {
                continue;
            }
            if let Some(ref event) = o.event_ref {
                let fingerprint = format!(
                    "{}:{}:{}:{}:{}:{}",
                    batch.reader_id, batch.session, port.id, o.epc, event, o.last_seen_at
                );
                if !self.event_refs.insert(fingerprint.clone()) {
                    continue;
                }
                self.event_order.push_back(fingerprint);
                while self.event_order.len() > MAX_EVENT_REFS {
                    if let Some(old) = self.event_order.pop_front() {
                        self.event_refs.remove(&old);
                    }
                }
            }
            self.recent_reads.push_back(RecentRead {
                epc: o.epc.clone(),
                port_id: port.id.clone(),
                reader_id: batch.reader_id.clone(),
                observed_at: o.last_seen_at,
                received_at: now,
                purpose: batch.purpose,
                data_source: batch.data_source,
                read_count: o.read_count,
                tid: o.tid.clone(),
                peak_rssi_cdbm: o.peak_rssi_cdbm,
                event_ref: o.event_ref.clone(),
            });
            while self.recent_reads.len() > MAX_RECENT_READS {
                self.recent_reads.pop_front();
            }
            let bound = bindings.get(&o.epc).copied();
            if batch.purpose == ObservationPurpose::Enrollment {
                if let Some(id) = batch.capture_session
                    && let Some(session) = self.sessions.get_mut(&id)
                    && session.port_id == port.id
                    && session.state == EnrollmentState::Active
                    && session.expires_at > now
                    && matches!(
                        session.borrow_state,
                        BorrowState::NotBorrowed | BorrowState::Paused
                    )
                    && o.last_seen_at > session.capture_started_at
                {
                    if o.first_seen_at <= session.capture_started_at {
                        o.first_seen_at = o.last_seen_at;
                        o.read_count = 1;
                    }
                    if let Some(candidate) = session.candidates.iter_mut().find(|c| c.epc == o.epc)
                    {
                        if o.last_seen_at <= candidate.last_seen_at {
                            continue;
                        }
                        if elapsed(candidate.last_seen_at, o.last_seen_at)
                            > self.config.thresholds.candidate_fresh_ms
                        {
                            let fully_fresh = o.first_seen_at > candidate.last_seen_at
                                && elapsed(o.first_seen_at, o.last_seen_at)
                                    <= self.config.thresholds.candidate_fresh_ms;
                            candidate.first_seen_at = if fully_fresh {
                                o.first_seen_at
                            } else {
                                o.last_seen_at
                            };
                            candidate.read_count = if fully_fresh {
                                u64::from(o.read_count)
                            } else {
                                1
                            };
                            session.capture_version = session.capture_version.saturating_add(1);
                            changes.sessions_changed = true;
                        } else {
                            candidate.read_count =
                                candidate.read_count.saturating_add(u64::from(o.read_count));
                        }
                        candidate.last_seen_at = o.last_seen_at;
                    } else if session.candidates.len() < MAX_RUNTIME_TAGS {
                        if !session.candidates.is_empty() {
                            session.capture_version = session.capture_version.saturating_add(1);
                            changes.sessions_changed = true;
                        }
                        session.candidates.push(EnrollmentCandidate {
                            epc: o.epc.clone(),
                            first_seen_at: o.first_seen_at,
                            last_seen_at: o.last_seen_at,
                            read_count: u64::from(o.read_count),
                            bound_unit_id: bound,
                            selectable: false,
                        });
                    } else {
                        if !session.capture_overflow {
                            changes.sessions_changed = true;
                        }
                        session.capture_overflow = true;
                    }
                }
                continue;
            }
            if batch.purpose != ObservationPurpose::Inventory
                || !batch.connected
                || !self.config.enabled()
                || !port.enabled
                || port.role != PortRole::Monitor
                || self.borrowed(&port.id)
            {
                continue;
            }
            let Some(id) = bound else {
                if self.unknown_tags.len() < MAX_RUNTIME_TAGS {
                    self.unknown_tags.insert(o.epc.clone(), now);
                }
                continue;
            };
            let Some(unit) = self.units.get(&id) else {
                continue;
            };
            if unit.unit.retired
                || unit.unit.archived
                || unit.unit.paused
                || unit.unit.installation.state == InstallationState::Exempt
                || o.last_seen_at <= unit.unit.fence_at
                || o.last_seen_at <= unit.unit.changed_at
                || previous_seen
                    .get(&id)
                    .is_some_and(|at| o.last_seen_at <= *at)
            {
                continue;
            }
            let barrier = epoch_at.max(unit.unit.fence_at).max(unit.unit.changed_at);
            if o.first_seen_at <= barrier {
                o.first_seen_at = o.last_seen_at;
                o.read_count = 1;
            }
            let Some(area) = port
                .area_id
                .as_ref()
                .filter(|a| self.config.areas.iter().any(|v| &v.id == *a && v.enabled))
            else {
                continue;
            };
            self.seen_any
                .entry(id)
                .and_modify(|t| *t = (*t).max(o.last_seen_at))
                .or_insert(o.last_seen_at);
            self.absence_since.remove(&id);
            let reference = ObservationRef {
                reader_id: batch.reader_id.clone(),
                session: batch.session,
                sequence: batch.sequence,
                port_id: port.id.clone(),
                observed_at: o.last_seen_at,
                received_at: now,
                event_ref: o.event_ref.clone(),
            };
            grouped
                .entry(id)
                .or_default()
                .entry(area.clone())
                .or_default()
                .push((o, reference));
        }
        for (id, areas) in grouped {
            for (area, observations) in &areas {
                let last = observations
                    .iter()
                    .map(|(o, _)| o.last_seen_at)
                    .max()
                    .ok_or(TrackingError::Conflict("empty_observation"))?;
                self.area_recent
                    .entry(id)
                    .or_default()
                    .insert(area.clone(), last);
            }
            let window = self
                .config
                .thresholds
                .positive_dwell_ms
                .saturating_add(self.config.thresholds.movement_hysteresis_ms);
            if let Some(recent) = self.area_recent.get_mut(&id) {
                recent.retain(|_, last| elapsed(*last, now) <= window);
            }
            let ambiguous =
                areas.len() > 1 || self.area_recent.get(&id).is_some_and(|a| a.len() > 1);
            if ambiguous {
                let mut refs = self
                    .positives
                    .remove(&id)
                    .map_or_else(Vec::new, |p| p.refs.into_iter().rev().take(1).collect());
                // One reference per current area plus the previous candidate is
                // bounded by the 32 configured areas and retains both sides.
                refs.extend(
                    areas
                        .values()
                        .filter_map(|observations| observations.last().map(|(_, r)| r.clone())),
                );
                self.decision(
                    id,
                    now,
                    DecisionReason::AmbiguousEvidence,
                    None,
                    refs.clone(),
                );
                self.set_diagnostic_exception(
                    id,
                    TrackingException::AmbiguousReads,
                    now,
                    refs,
                    &mut changes.units,
                );
                continue;
            }
            let Some((area, observations)) = areas.into_iter().next() else {
                continue;
            };
            let first = observations
                .iter()
                .map(|(o, _)| o.first_seen_at)
                .min()
                .ok_or(TrackingError::Conflict("empty_observation"))?;
            let last = observations
                .iter()
                .map(|(o, _)| o.last_seen_at)
                .max()
                .ok_or(TrackingError::Conflict("empty_observation"))?;
            let count = observations
                .iter()
                .map(|(o, _)| u64::from(o.read_count))
                .sum::<u64>();
            let refs = observations
                .into_iter()
                .map(|(_, r)| r)
                .take(8)
                .collect::<Vec<_>>();
            let candidate = self.positives.entry(id).or_insert(Positive {
                area: area.clone(),
                first,
                last,
                count: 0,
                refs: Vec::new(),
            });
            if candidate.area != area
                || elapsed(candidate.last, first) > self.config.thresholds.coverage_max_gap_ms
            {
                *candidate = Positive {
                    area: area.clone(),
                    first,
                    last,
                    count: 0,
                    refs: Vec::new(),
                };
            }
            candidate.last = candidate.last.max(last);
            candidate.count = candidate.count.saturating_add(count);
            candidate.refs.extend(refs);
            if candidate.refs.len() > 8 {
                candidate.refs.drain(..candidate.refs.len() - 8);
            }
            let Some(config) = self.config.areas.iter().find(|a| a.id == area).cloned() else {
                continue;
            };
            let Some(before) = self.units.get(&id).cloned() else {
                continue;
            };
            let move_delay = if before
                .unit
                .last_confirmed_area
                .as_ref()
                .is_some_and(|a| a != &area)
            {
                config
                    .thresholds
                    .movement_hysteresis_ms
                    .unwrap_or(self.config.thresholds.movement_hysteresis_ms)
            } else {
                0
            };
            let required = config
                .thresholds
                .positive_dwell_ms
                .unwrap_or(self.config.thresholds.positive_dwell_ms)
                .saturating_add(move_delay);
            if elapsed(candidate.first, candidate.last) < required
                || candidate.count
                    < u64::from(
                        config
                            .thresholds
                            .positive_min_reads
                            .unwrap_or(self.config.thresholds.positive_min_reads),
                    )
            {
                self.decision(
                    id,
                    now,
                    DecisionReason::InsufficientEvidence,
                    Some(area),
                    Vec::new(),
                );
                continue;
            }
            let refs = candidate.refs.clone();
            let mut after = before.clone();
            after.unit.status = config.kind.present_status();
            after.unit.current_area = Some(area.clone());
            after.unit.last_confirmed_area = Some(area.clone());
            after.last_confirmed_kind = Some(config.kind);
            after.unit.baseline_at = Some(last);
            after.unit.evidence = StateEvidence::Rfid;
            if matches!(
                after.unit.exception,
                Some(
                    TrackingException::NeedsBaseline
                        | TrackingException::AmbiguousReads
                        | TrackingException::InsufficientEvidence
                )
            ) {
                after.unit.exception = None;
            }
            if before.unit.status != after.unit.status
                || before.unit.current_area != after.unit.current_area
                || before.unit.last_confirmed_area != after.unit.last_confirmed_area
                || before.unit.evidence != after.unit.evidence
                || before.unit.exception != after.unit.exception
                || before.unit.baseline_at.is_none()
            {
                after.unit.changed_at = now;
                self.units.insert(id, after.clone());
                changes.units.push(UnitChange {
                    before: Some(before),
                    after,
                    kind: HistoryKind::Presence,
                    reason: DecisionReason::StableArea,
                    evidence: refs.clone(),
                    note: None,
                });
            }
            self.decision(id, now, DecisionReason::StableArea, Some(area), refs);
        }
        self.expire_runtime(now);
        Ok(changes)
    }
    fn set_diagnostic_exception(
        &mut self,
        id: UnitId,
        exception: TrackingException,
        now: DateTime<Utc>,
        evidence: Vec<ObservationRef>,
        changes: &mut Vec<UnitChange>,
    ) {
        if let Some(before) = self.units.get(&id).cloned()
            && matches!(
                before.unit.exception,
                None | Some(
                    TrackingException::AmbiguousReads | TrackingException::InsufficientEvidence
                )
            )
            && before.unit.exception != Some(exception)
        {
            let mut after = before.clone();
            after.unit.exception = Some(exception);
            after.unit.changed_at = now;
            self.units.insert(id, after.clone());
            changes.push(UnitChange {
                before: Some(before),
                after,
                kind: HistoryKind::Correction,
                reason: DecisionReason::AmbiguousEvidence,
                evidence,
                note: None,
            });
        }
    }
    fn borrowed(&self, port: &PortId) -> bool {
        self.sessions.values().any(|s| {
            &s.port_id == port
                && !matches!(
                    s.borrow_state,
                    BorrowState::NotBorrowed | BorrowState::Restored
                )
        })
    }
    fn expire_runtime(&mut self, now: DateTime<Utc>) {
        self.unknown_tags
            .retain(|_, at| elapsed(*at, now) < 300_000);
        while self
            .recent_reads
            .front()
            .is_some_and(|r| elapsed(r.received_at, now) > 300_000)
        {
            self.recent_reads.pop_front();
        }
    }
    pub(crate) fn tick(&mut self, now: DateTime<Utc>) -> Result<Changes> {
        self.clock(now)?;
        self.expire_runtime(now);
        let mut changes = Changes::default();
        for session in self.sessions.values_mut() {
            if session.state == EnrollmentState::Active && session.expires_at <= now {
                session.state = EnrollmentState::Expired;
                session.candidates.clear();
                if session.borrow_state == BorrowState::Paused
                    || session.borrow_state == BorrowState::PauseRequested
                {
                    session.borrow_state = BorrowState::RestoreRequested;
                }
                changes.sessions_changed = true;
            }
        }
        if !self.health(now).absence_coverage_healthy {
            self.absence_since.clear();
            return Ok(changes);
        }
        let coverage_evidence = self
            .config
            .ports
            .iter()
            .filter(|p| {
                p.enabled
                    && p.role == PortRole::Monitor
                    && p.required_for_absence
                    && p.area_id.as_ref().is_some_and(|id| {
                        self.config.areas.iter().any(|a| &a.id == id && a.enabled)
                    })
            })
            .filter_map(|p| self.coverage.get(&p.id).map(|c| c.reference.clone()))
            .collect::<Vec<_>>();
        let units = self.units.values().cloned().collect::<Vec<_>>();
        for before in units {
            let id = before.unit.id;
            if before.epc.is_none()
                || before.unit.retired
                || before.unit.archived
                || before.unit.paused
                || before.unit.installation.state == InstallationState::Exempt
                || before.unit.baseline_at.is_none()
                || matches!(
                    before.unit.exception,
                    Some(
                        TrackingException::NeedsBaseline
                            | TrackingException::NeedsReconciliation
                            | TrackingException::Paused
                            | TrackingException::Archived
                            | TrackingException::EnrollmentRecovery
                            | TrackingException::QuantityUnknown
                    )
                )
            {
                continue;
            }
            let Some(area) = before
                .unit
                .last_confirmed_area
                .as_ref()
                .and_then(|id| self.config.areas.iter().find(|a| &a.id == id && a.enabled))
                .cloned()
            else {
                continue;
            };
            if before.last_confirmed_kind != Some(area.kind) {
                continue;
            }
            let last = self.seen_any.get(&id).copied().unwrap_or(self.restart_at);
            let since = self.absence_since.entry(id).or_insert(now);
            if last > *since {
                *since = last;
            }
            if elapsed(*since, now)
                < area
                    .thresholds
                    .absence_ms
                    .unwrap_or(self.config.thresholds.absence_ms)
            {
                continue;
            }
            let target = area.kind.absent_status();
            if before.unit.status == target && before.unit.current_area.is_none() {
                continue;
            }
            let mut after = before.clone();
            after.unit.status = target;
            after.unit.current_area = None;
            after.unit.changed_at = now;
            after.unit.evidence = StateEvidence::Rfid;
            if matches!(
                after.unit.exception,
                Some(TrackingException::AmbiguousReads | TrackingException::InsufficientEvidence)
            ) {
                after.unit.exception = None;
            }
            self.positives.remove(&id);
            self.area_recent.remove(&id);
            self.units.insert(id, after.clone());
            changes.units.push(UnitChange {
                before: Some(before),
                after,
                kind: HistoryKind::Absence,
                reason: DecisionReason::ConfirmedAbsence,
                evidence: coverage_evidence.clone(),
                note: None,
            });
            self.decision(
                id,
                now,
                DecisionReason::ConfirmedAbsence,
                Some(area.id),
                coverage_evidence.clone(),
            );
        }
        Ok(changes)
    }
    pub fn health(&self, now: DateTime<Utc>) -> TrackingHealth {
        let mut ports = Vec::new();
        let mut required = 0;
        let mut healthy = self.config.enabled();
        let mut warming = false;
        let mut covered_areas = BTreeSet::new();
        for port in &self.config.ports {
            let reader = self.config.readers.iter().find(|r| r.id == port.reader_id);
            let enabled_area = port
                .area_id
                .as_ref()
                .is_some_and(|id| self.config.areas.iter().any(|a| &a.id == id && a.enabled));
            let coverage = self.coverage.get(&port.id);
            let state = if !port.enabled
                || !reader.is_some_and(|r| r.enabled)
                || port.role == PortRole::Monitor && !enabled_area
            {
                CoverageState::Disabled
            } else if self.borrowed(&port.id) {
                CoverageState::Borrowed
            } else if self
                .readers
                .get(&port.reader_id)
                .is_some_and(|r| !r.connected)
            {
                CoverageState::Disconnected
            } else if !port.commissioned {
                CoverageState::Unverified
            } else if let Some(c) = coverage {
                if elapsed(c.until, now) > self.config.thresholds.coverage_max_gap_ms {
                    CoverageState::Unverified
                } else if c.state == CoverageState::Healthy
                    && elapsed(c.since, now) < self.config.thresholds.warmup_ms
                {
                    CoverageState::WarmingUp
                } else {
                    c.state
                }
            } else {
                CoverageState::Unverified
            };
            if port.enabled
                && port.role == PortRole::Monitor
                && port.required_for_absence
                && enabled_area
            {
                required += 1;
                if let Some(area) = &port.area_id {
                    covered_areas.insert(area.clone());
                }
                healthy &= state == CoverageState::Healthy;
                warming |= state == CoverageState::WarmingUp;
            }
            ports.push(PortHealth {
                port_id: port.id.clone(),
                state,
                healthy_since: coverage.map(|c| c.since),
                last_coverage_at: coverage.map(|c| c.until),
            });
        }
        healthy &= required > 0
            && self
                .config
                .areas
                .iter()
                .filter(|a| a.enabled)
                .all(|a| covered_areas.contains(&a.id));
        TrackingHealth {
            absence_coverage_healthy: healthy,
            warming_up: warming,
            required_ports: required,
            ports,
            recent_unknown_tag_count: self.unknown_tags.len(),
            restart_at: self.restart_at,
        }
    }
    pub(crate) fn session_view(
        &self,
        session: &EnrollmentSession,
        now: DateTime<Utc>,
    ) -> EnrollmentSession {
        let mut view = session.clone();
        let fresh = view
            .candidates
            .iter()
            .filter(|c| elapsed(c.last_seen_at, now) <= self.config.thresholds.candidate_fresh_ms)
            .count();
        for candidate in &mut view.candidates {
            candidate.bound_unit_id = self
                .units
                .values()
                .find(|u| !u.unit.retired && u.epc.as_ref() == Some(&candidate.epc))
                .map(|u| u.unit.id);
            candidate.selectable = !view.capture_overflow
                && view.purpose != EnrollmentPurpose::Lookup
                && view.state == EnrollmentState::Active
                && view.expires_at > now
                && matches!(
                    view.borrow_state,
                    BorrowState::NotBorrowed | BorrowState::Paused
                )
                && fresh == 1
                && elapsed(candidate.last_seen_at, now)
                    <= self.config.thresholds.candidate_fresh_ms
                && candidate.bound_unit_id.is_none()
                && candidate.read_count >= u64::from(self.config.thresholds.positive_min_reads)
                && elapsed(candidate.first_seen_at, candidate.last_seen_at)
                    >= self.config.thresholds.positive_dwell_ms;
        }
        view
    }
    pub fn snapshot(&self, now: DateTime<Utc>) -> TrackingSnapshot {
        let units = self
            .units
            .values()
            .map(|u| u.unit.clone())
            .collect::<Vec<_>>();
        let ready = units
            .iter()
            .filter(|u| u.ready_to_wear())
            .map(|u| u.id)
            .collect::<Vec<_>>();
        let mut counts = TrackingCounts {
            certified_ready: ready.len(),
            ..TrackingCounts::default()
        };
        for record in self.inventory.values().filter(|r| !r.archived) {
            counts.unallocated_units += u64::from(record.unallocated_units);
            counts.quantity_unknown_items += usize::from(
                record
                    .catalog_quantity
                    .or(record.declared_quantity)
                    .is_none(),
            );
            counts.capacity_limited_items += usize::from(record.capacity_limited);
        }
        for unit in &units {
            if unit.retired {
                counts.retired += 1;
                continue;
            }
            if unit.archived {
                counts.archived += 1;
                continue;
            }
            counts.total_units += 1;
            if unit.has_binding {
                counts.tagged_units += 1;
            }
            if unit.exception.is_some() {
                counts.exceptions += 1;
            }
            match unit.status {
                UnitStatus::Unknown => counts.unknown += 1,
                UnitStatus::CleanReady => counts.clean_ready += 1,
                UnitStatus::WornReady => counts.worn_ready += 1,
                UnitStatus::NeedsLaundry => counts.needs_laundry += 1,
                UnitStatus::QueuedDryCleaning => counts.queued_dry_cleaning += 1,
                UnitStatus::BeingWorn => counts.being_worn += 1,
                UnitStatus::InWash => counts.in_wash += 1,
                UnitStatus::AtCleaners => counts.at_cleaners += 1,
            }
        }
        TrackingSnapshot {
            schema_version: TRACKING_SCHEMA_VERSION,
            tracking_revision: self.revision,
            configuration_version: self.configuration_version,
            config: self.config.clone(),
            inventory: self.inventory.values().cloned().collect(),
            units,
            certified_ready_unit_ids: ready,
            counts,
            health: self.health(now),
            sessions: self
                .sessions
                .values()
                .map(|s| self.session_view(s, now))
                .collect(),
            diagnostics: TrackingDiagnostics {
                recent_reads: self.recent_reads.iter().cloned().collect(),
                decisions: self.decisions.iter().cloned().collect(),
                faults: self.faults.iter().cloned().collect(),
            },
            reader_readbacks: self.readbacks.values().cloned().collect(),
            reader_operations: self.reader_operations.values().cloned().collect(),
            reader_controls: self.reader_controls.values().cloned().collect(),
        }
    }
}
fn elapsed(from: DateTime<Utc>, to: DateTime<Utc>) -> u64 {
    to.signed_duration_since(from)
        .num_milliseconds()
        .try_into()
        .unwrap_or_default()
}
fn text(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
fn env_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_uppercase() || b == b'_' || i > 0 && b.is_ascii_digit())
}

/// Offline validates structure, not undocumented hardware capabilities.
pub fn validate_config(config: &TrackingConfig) -> Result<()> {
    let mut structural = config.clone();
    for port in &mut structural.ports {
        if port.dwell_ms == Some(0)
            || port
                .transmit_power_cdbm
                .is_some_and(|p| !(-10_000..=10_000).contains(&p))
        {
            return Err(TrackingError::Invalid {
                code: "invalid_tracking_config",
                field: "ports",
            });
        }
        port.transmit_power_cdbm = None;
        port.dwell_ms = None;
    }
    Engine::default().validate_config(&structural)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::{DateTime, Duration, Utc};
    use uuid::Uuid;

    use crate::tracking::engine::{DurableState, Engine, EngineMode};
    use crate::tracking::models::*;

    fn time(ms: i64) -> DateTime<Utc> {
        DateTime::<Utc>::UNIX_EPOCH + Duration::milliseconds(ms)
    }
    fn id<T: std::str::FromStr>(s: &str) -> T
    where
        T::Err: std::fmt::Debug,
    {
        s.parse().unwrap()
    }
    fn configuration() -> TrackingConfig {
        let areas = [
            ("closet", AreaKind::Closet),
            ("rewear", AreaKind::Rewear),
            ("hamper", AreaKind::Hamper),
            ("drybag", AreaKind::DryCleaningBag),
        ]
        .into_iter()
        .map(|(name, kind)| AreaConfig {
            id: id(name),
            label: name.into(),
            kind,
            enabled: true,
            thresholds: AreaThresholds::default(),
        })
        .collect::<Vec<_>>();
        let ports = areas
            .iter()
            .enumerate()
            .map(|(n, area)| PortConfig {
                id: id(&format!("port-{n}")),
                reader_id: id("reader"),
                antenna: n as u16 + 1,
                area_id: Some(area.id.clone()),
                role: PortRole::Monitor,
                enabled: true,
                required_for_absence: true,
                commissioned: true,
                transmit_power_cdbm: None,
                dwell_ms: None,
            })
            .collect();
        TrackingConfig {
            schema_version: 1,
            areas,
            ports,
            readers: vec![ReaderConfig {
                id: id("reader"),
                label: "Fixture".into(),
                enabled: true,
                connection: ReaderConnection::Simulator,
            }],
            thresholds: Thresholds {
                positive_dwell_ms: 100,
                positive_min_reads: 2,
                movement_hysteresis_ms: 100,
                absence_ms: 1_000,
                warmup_ms: 100,
                coverage_max_gap_ms: 1_000,
                candidate_fresh_ms: 500,
                reader_event_max_age_ms: 10_000,
            },
            ..Default::default()
        }
    }
    fn batch(at: i64, sequence: u64, epoch: u128, port: Option<usize>) -> ObservationBatch {
        ObservationBatch {
            reader_id: id("reader"),
            purpose: ObservationPurpose::Inventory,
            data_source: DataSource::Simulation,
            capture_session: None,
            session: Uuid::from_u128(epoch),
            sequence,
            observed_at: time(at),
            connected: true,
            coverage: (0..4)
                .map(|n| PortCoverage {
                    port_id: id(&format!("port-{n}")),
                    started_at: time((at - 1_000).max(0)),
                    ended_at: time(at),
                    proof: CoverageProof::VerifiedInventory,
                })
                .collect(),
            observations: port
                .map(|p| TagObservation {
                    epc: id("0001"),
                    port_id: id(&format!("port-{p}")),
                    first_seen_at: time(at - 399),
                    last_seen_at: time(at),
                    read_count: 3,
                    tid: None,
                    peak_rssi_cdbm: Some(-5_000),
                    event_ref: None,
                })
                .into_iter()
                .collect(),
            readback: None,
        }
    }
    #[test]
    fn the_same_normalized_inputs_config_changes_and_ticks_replay_identically_without_an_allocator()
    {
        fn run() -> Vec<(UnitStatus, Option<AreaId>, i64, Vec<DecisionReason>)> {
            let unit_id = UnitId(Uuid::from_u128(99));
            let unit = StoredUnit {
                unit: PhysicalUnit {
                    id: unit_id,
                    catalog_id: "fictional-garment".into(),
                    ordinal: 1,
                    status: UnitStatus::CleanReady,
                    last_confirmed_area: Some(id("closet")),
                    current_area: Some(id("closet")),
                    has_binding: true,
                    installation: Installation {
                        state: InstallationState::Installed,
                        ..Installation::default()
                    },
                    baseline_at: Some(time(0)),
                    evidence: StateEvidence::Manual,
                    fence_at: time(0),
                    paused: false,
                    retired: false,
                    archived: false,
                    exception: None,
                    changed_at: time(0),
                    last_event_id: None,
                },
                epc: Some(id("0001")),
                last_confirmed_kind: Some(AreaKind::Closet),
            };
            let record = InventoryUnits {
                catalog_id: "fictional-garment".into(),
                catalog_quantity: Some(1),
                declared_quantity: None,
                active_units: 1,
                unallocated_units: 0,
                capacity_limited: false,
                needs_reconciliation: false,
                archived: false,
            };
            let mut engine = Engine::new(EngineMode::Simulation, time(0));
            engine.load(
                DurableState {
                    revision: 0,
                    configuration_version: 1,
                    config: configuration(),
                    units: BTreeMap::from([(unit_id, unit)]),
                    inventory: BTreeMap::from([("fictional-garment".into(), record)]),
                    sessions: BTreeMap::new(),
                    reader_operations: BTreeMap::new(),
                    reader_controls: BTreeMap::new(),
                },
                time(0),
            );
            let mut outcomes = Vec::new();
            for (at, port) in [
                (0, None),
                (400, Some(0)),
                (1_400, None),
                (1_800, Some(2)),
                (2_800, None),
                (3_800, None),
            ] {
                let changes = engine
                    .ingest(batch(at, at as u64 + 1, 1, port), time(at))
                    .unwrap();
                let mut reasons = changes
                    .units
                    .into_iter()
                    .map(|c| c.reason)
                    .collect::<Vec<_>>();
                let changes = engine.tick(time(at)).unwrap();
                reasons.extend(changes.units.into_iter().map(|c| c.reason));
                let unit = &engine.units[&unit_id].unit;
                outcomes.push((
                    unit.status,
                    unit.last_confirmed_area.clone(),
                    engine.configuration_version,
                    reasons,
                ));
            }
            let mut updated = configuration();
            updated.thresholds.absence_ms = 2_000;
            updated.thresholds.positive_dwell_ms = 150;
            engine.configure(updated, time(3_900)).unwrap();
            for (at, port) in [
                (4_000, None),
                (4_400, Some(0)),
                (5_400, None),
                (6_400, None),
            ] {
                let changes = engine
                    .ingest(batch(at, at as u64 + 1, 2, port), time(at))
                    .unwrap();
                let mut reasons = changes
                    .units
                    .into_iter()
                    .map(|c| c.reason)
                    .collect::<Vec<_>>();
                let changes = engine.tick(time(at)).unwrap();
                reasons.extend(changes.units.into_iter().map(|c| c.reason));
                let unit = &engine.units[&unit_id].unit;
                outcomes.push((
                    unit.status,
                    unit.last_confirmed_area.clone(),
                    engine.configuration_version,
                    reasons,
                ));
            }
            outcomes
        }
        let first = run();
        let second = run();
        assert_eq!(first, second);
        assert!(first.iter().any(|r| r.0 == UnitStatus::InWash));
        assert_eq!(first.last().unwrap().0, UnitStatus::BeingWorn);
        assert_eq!(first.last().unwrap().1, Some(id("closet")));
        assert_eq!(first.last().unwrap().2, 2);
    }
}
