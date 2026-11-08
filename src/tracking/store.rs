//! Atomic tracking operations over the existing SQLite database and operation log.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use sqlx::{Row, SqliteConnection};

use crate::models::{CatalogItem, digest};
use crate::store::Store;
use crate::tracking::engine::{Changes, DurableState, Engine, EngineMode, UnitChange};
use crate::tracking::models::*;

const STATE: &str =
    "SELECT revision,configuration_version,config FROM tracking_state WHERE singleton=1";
const INVENTORY: &str = "SELECT catalog_id,catalog_quantity,declared_quantity,next_ordinal,needs_reconciliation,capacity_limited,archived FROM tracking_inventory WHERE catalog_id>? ORDER BY catalog_id LIMIT 500";
const UNITS: &str = "SELECT id,body FROM tracking_units WHERE id>? ORDER BY id LIMIT 500";
const SESSIONS: &str = "SELECT body FROM tracking_sessions ORDER BY id LIMIT 1001";
const OPERATIONS: &str =
    "SELECT request_hash,response FROM operations WHERE scope='tracking' AND actor=? AND key=?";
const INSERT_OPERATION: &str =
    "INSERT INTO operations(scope,actor,key,request_hash,response) VALUES('tracking',?,?,?,?)";
const BUMP: &str =
    "UPDATE tracking_state SET revision=revision+1 WHERE singleton=1 RETURNING revision";
const CONFIGURE: &str =
    "UPDATE tracking_state SET config=?,enabled=?,configuration_version=? WHERE singleton=1";
const UPSERT_INVENTORY: &str = "INSERT INTO tracking_inventory(catalog_id,catalog_quantity,declared_quantity,next_ordinal,needs_reconciliation,capacity_limited,archived) VALUES(?,?,?,?,?,?,?) ON CONFLICT(catalog_id) DO UPDATE SET catalog_quantity=excluded.catalog_quantity,declared_quantity=excluded.declared_quantity,next_ordinal=excluded.next_ordinal,needs_reconciliation=excluded.needs_reconciliation,capacity_limited=excluded.capacity_limited,archived=excluded.archived";
const UPSERT_UNIT: &str = "INSERT INTO tracking_units(id,catalog_id,ordinal,epc,status,last_confirmed_area,current_area,baseline_at,evidence,installation_state,fence_at,paused,retired,archived,exception,changed_at,last_event_id,body) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET epc=excluded.epc,status=excluded.status,last_confirmed_area=excluded.last_confirmed_area,current_area=excluded.current_area,baseline_at=excluded.baseline_at,evidence=excluded.evidence,installation_state=excluded.installation_state,fence_at=excluded.fence_at,paused=excluded.paused,retired=excluded.retired,archived=excluded.archived,exception=excluded.exception,changed_at=excluded.changed_at,last_event_id=excluded.last_event_id,body=excluded.body";
const UPSERT_SESSION: &str = "INSERT INTO tracking_sessions(id,port_id,state,capture_version,body) VALUES(?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET port_id=excluded.port_id,state=excluded.state,capture_version=excluded.capture_version,body=excluded.body";
const INSERT_HISTORY: &str = "INSERT INTO tracking_history(unit_id,at,kind,body) VALUES(?,?,?,?)";
const UPDATE_HISTORY: &str = "UPDATE tracking_history SET body=? WHERE id=?";
const HISTORY_EVENT: &str = "SELECT body FROM tracking_history WHERE id=?";
const HISTORY_UNIT: &str =
    "SELECT body FROM tracking_history WHERE unit_id=? AND id<? ORDER BY id DESC LIMIT ?";
const HISTORY_ALL: &str = "SELECT body FROM tracking_history WHERE id<? ORDER BY id DESC LIMIT ?";
const EXPORT_HISTORY: &str = "SELECT body FROM tracking_history ORDER BY id LIMIT 100001";
const EXPORT_OPERATIONS: &str = "SELECT actor,key,request_hash,response FROM operations WHERE scope='tracking' ORDER BY actor,key LIMIT 100001";
const BINDINGS: &str = "SELECT id,unit_id,epc,bound_at,retired_at,retirement FROM tracking_bindings ORDER BY id LIMIT 100001";
const RETIRE_BINDING: &str =
    "UPDATE tracking_bindings SET retired_at=?,retirement=? WHERE unit_id=? AND retired_at IS NULL";
const INSERT_BINDING: &str = "INSERT INTO tracking_bindings(unit_id,epc,bound_at,retired_at,retirement) VALUES(?,?,?,NULL,NULL)";
const CATALOG: &str = "SELECT id,body,archived FROM items WHERE id>? ORDER BY id LIMIT 500";
const READER_OPERATIONS: &str =
    "SELECT body FROM tracking_reader_operations ORDER BY reader_id LIMIT 17";
const READER_CONTROLS: &str =
    "SELECT body FROM tracking_reader_controls ORDER BY reader_id LIMIT 17";
const UPSERT_READER_OPERATION: &str = "INSERT INTO tracking_reader_operations(reader_id,body) VALUES(?,?) ON CONFLICT(reader_id) DO UPDATE SET body=excluded.body";
const UPSERT_READER_CONTROL: &str = "INSERT INTO tracking_reader_controls(reader_id,body) VALUES(?,?) ON CONFLICT(reader_id) DO UPDATE SET body=excluded.body";
const DELETE_READER_OPERATION: &str = "DELETE FROM tracking_reader_operations WHERE reader_id=?";
const DELETE_READER_CONTROL: &str = "DELETE FROM tracking_reader_controls WHERE reader_id=?";
const DECLARE_QUANTITY: &str = "UPDATE tracking_inventory SET declared_quantity=? WHERE catalog_id=? AND catalog_quantity IS NULL";

#[derive(Clone)]
struct InventoryRecord {
    view: InventoryUnits,
    next_ordinal: u32,
}
fn invalid(code: &'static str, field: &'static str) -> TrackingError {
    TrackingError::Invalid { code, field }
}
fn text(v: &str, max: usize) -> bool {
    !v.trim().is_empty() && v.len() <= max && !v.chars().any(char::is_control)
}
fn encode<T: serde::Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}
fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T> {
    Ok(serde_json::from_str(value)?)
}
fn state_name(state: EnrollmentState) -> &'static str {
    match state {
        EnrollmentState::Active => "active",
        EnrollmentState::Complete => "complete",
        EnrollmentState::Aborted => "aborted",
        EnrollmentState::Expired => "expired",
        EnrollmentState::RecoveryRequired => "recovery_required",
    }
}
fn evidence_name(e: StateEvidence) -> &'static str {
    match e {
        StateEvidence::Unknown => "unknown",
        StateEvidence::Manual => "manual",
        StateEvidence::Rfid => "rfid",
    }
}
fn installation_name(state: InstallationState) -> &'static str {
    match state {
        InstallationState::NeedsTag => "needs_tag",
        InstallationState::SeamstressQueued => "seamstress_queued",
        InstallationState::Installed => "installed",
        InstallationState::Exempt => "exempt",
    }
}
fn history_name(k: &HistoryKind) -> &'static str {
    match k {
        HistoryKind::Baseline => "baseline",
        HistoryKind::Presence => "presence",
        HistoryKind::Absence => "absence",
        HistoryKind::Correction => "correction",
        HistoryKind::Pause => "pause",
        HistoryKind::Retire => "retire",
        HistoryKind::Unbind => "unbind",
        HistoryKind::Bind => "bind",
        HistoryKind::Replace => "replace",
        HistoryKind::Undo => "undo",
        HistoryKind::Archive => "archive",
        HistoryKind::Restore => "restore",
        HistoryKind::QuantityReconciliation => "quantity_reconciliation",
    }
}
async fn inventory(conn: &mut SqliteConnection) -> Result<BTreeMap<String, InventoryRecord>> {
    let rows = paged(conn, INVENTORY, "catalog_id").await?;
    let mut result = BTreeMap::new();
    for r in rows {
        let id: String = r.try_get("catalog_id")?;
        result.insert(
            id.clone(),
            InventoryRecord {
                view: InventoryUnits {
                    catalog_id: id,
                    catalog_quantity: r.try_get("catalog_quantity")?,
                    declared_quantity: r.try_get("declared_quantity")?,
                    active_units: 0,
                    unallocated_units: 0,
                    capacity_limited: r.try_get("capacity_limited")?,
                    needs_reconciliation: r.try_get("needs_reconciliation")?,
                    archived: r.try_get("archived")?,
                },
                next_ordinal: r.try_get("next_ordinal")?,
            },
        );
    }
    Ok(result)
}
async fn load(
    conn: &mut SqliteConnection,
    runtime: &mut Engine,
    now: DateTime<Utc>,
    force: bool,
) -> Result<()> {
    let row = sqlx::query(STATE).fetch_one(&mut *conn).await?;
    let revision: i64 = row.try_get("revision")?;
    if !force && runtime.revision == revision {
        return Ok(());
    }
    let config = decode(&row.try_get::<String, _>("config")?)?;
    let configuration_version = row.try_get("configuration_version")?;
    let rows = paged(conn, UNITS, "id").await?;
    let units = rows
        .into_iter()
        .map(|r| body::<StoredUnit>(&r))
        .collect::<Result<Vec<_>>>()?;
    let mut records = inventory(conn).await?;
    for u in &units {
        if !u.unit.retired
            && let Some(r) = records.get_mut(&u.unit.catalog_id)
        {
            r.view.active_units += 1;
        }
    }
    for record in records.values_mut() {
        record.view.unallocated_units = record
            .view
            .catalog_quantity
            .or(record.view.declared_quantity)
            .unwrap_or(0)
            .saturating_sub(record.view.active_units);
    }
    let rows = sqlx::query(SESSIONS).fetch_all(&mut *conn).await?;
    if rows.len() > 1_000 {
        return Err(invalid("tracking_limit", "sessions"));
    }
    let sessions = rows
        .into_iter()
        .map(|r| body::<EnrollmentSession>(&r))
        .collect::<Result<Vec<_>>>()?;
    let mut reader_operations = BTreeMap::new();
    for r in sqlx::query(READER_OPERATIONS).fetch_all(&mut *conn).await? {
        let op: ReaderOperation = body(&r)?;
        reader_operations.insert(op.reader_id.clone(), op);
    }
    let mut reader_controls = BTreeMap::new();
    for r in sqlx::query(READER_CONTROLS).fetch_all(&mut *conn).await? {
        let op: ReaderControlOperation = body(&r)?;
        reader_controls.insert(op.reader_id.clone(), op);
    }
    runtime.load(
        DurableState {
            revision,
            configuration_version,
            config,
            units: units.into_iter().map(|u| (u.unit.id, u)).collect(),
            inventory: records.into_iter().map(|(id, r)| (id, r.view)).collect(),
            sessions: sessions.into_iter().map(|s| (s.id, s)).collect(),
            reader_operations,
            reader_controls,
        },
        now,
    );
    Ok(())
}
async fn write_inventory(conn: &mut SqliteConnection, r: &InventoryRecord) -> Result<()> {
    sqlx::query(UPSERT_INVENTORY)
        .bind(&r.view.catalog_id)
        .bind(r.view.catalog_quantity)
        .bind(r.view.declared_quantity)
        .bind(r.next_ordinal)
        .bind(r.view.needs_reconciliation)
        .bind(r.view.capacity_limited)
        .bind(r.view.archived)
        .execute(conn)
        .await?;
    Ok(())
}
async fn write_unit(conn: &mut SqliteConnection, u: &StoredUnit) -> Result<()> {
    let p = &u.unit;
    let result = sqlx::query(UPSERT_UNIT)
        .bind(p.id.to_string())
        .bind(&p.catalog_id)
        .bind(p.ordinal)
        .bind(u.epc.as_ref().map(Epc::as_str))
        .bind(p.status.as_str())
        .bind(p.last_confirmed_area.as_ref().map(AreaId::as_str))
        .bind(p.current_area.as_ref().map(AreaId::as_str))
        .bind(p.baseline_at.map(|t| t.to_rfc3339()))
        .bind(evidence_name(p.evidence))
        .bind(installation_name(p.installation.state))
        .bind(p.fence_at.to_rfc3339())
        .bind(p.paused)
        .bind(p.retired)
        .bind(p.archived)
        .bind(p.exception.map(TrackingException::as_str))
        .bind(p.changed_at.to_rfc3339())
        .bind(p.last_event_id)
        .bind(encode(u)?)
        .execute(conn)
        .await;
    match result {
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            Err(TrackingError::Conflict("epc_already_bound"))
        }
        Err(e) => Err(e.into()),
        Ok(_) => Ok(()),
    }
}
async fn write_session(conn: &mut SqliteConnection, s: &EnrollmentSession) -> Result<()> {
    let mut durable = s.clone();
    durable.candidates.clear();
    sqlx::query(UPSERT_SESSION)
        .bind(s.id.to_string())
        .bind(s.port_id.as_str())
        .bind(state_name(s.state))
        .bind(s.capture_version)
        .bind(encode(&durable)?)
        .execute(conn)
        .await?;
    Ok(())
}
#[derive(Clone, Copy)]
struct EventContext<'a> {
    actor: &'a str,
    automatic: bool,
    configuration_version: i64,
    at: DateTime<Utc>,
}
async fn event(
    conn: &mut SqliteConnection,
    change: &mut UnitChange,
    context: EventContext<'_>,
) -> Result<i64> {
    let UnitChange {
        before,
        after,
        kind,
        reason,
        note,
        evidence,
    } = change;
    let now = context.at;
    let automatic = context.automatic;
    let mut history = HistoryEvent {
        id: 0,
        unit_id: after.unit.id,
        at: now,
        actor: context.actor.to_owned(),
        kind: kind.clone(),
        reason: *reason,
        automatic,
        configuration_version: context.configuration_version,
        source_area: after.unit.last_confirmed_area.clone(),
        evidence: evidence.clone(),
        note: note.clone(),
        before: before.clone(),
        after: after.clone(),
    };
    let result = sqlx::query(INSERT_HISTORY)
        .bind(after.unit.id.to_string())
        .bind(now.to_rfc3339())
        .bind(history_name(&history.kind))
        .bind(encode(&history)?)
        .execute(&mut *conn)
        .await?;
    let id = result.last_insert_rowid();
    after.unit.last_event_id = Some(id);
    history.id = id;
    history.after = after.clone();
    if before.as_ref().and_then(|u| u.epc.as_ref()) != after.epc.as_ref() {
        let retirement = match history.kind {
            HistoryKind::Retire => BindingRetirement::UnitRetired,
            HistoryKind::Replace => BindingRetirement::Replaced,
            HistoryKind::Undo => BindingRetirement::Undo,
            _ => BindingRetirement::Unbound,
        };
        sqlx::query(RETIRE_BINDING)
            .bind(now.to_rfc3339())
            .bind(encode(&retirement)?)
            .bind(after.unit.id.to_string())
            .execute(&mut *conn)
            .await?;
        if let Some(epc) = &after.epc {
            sqlx::query(INSERT_BINDING)
                .bind(after.unit.id.to_string())
                .bind(epc.as_str())
                .bind(now.to_rfc3339())
                .execute(&mut *conn)
                .await?;
        }
    }
    write_unit(conn, after).await?;
    sqlx::query(UPDATE_HISTORY)
        .bind(encode(&history)?)
        .bind(id)
        .execute(conn)
        .await?;
    Ok(id)
}
fn new_unit(
    id: UnitId,
    catalog_id: String,
    ordinal: u32,
    archived: bool,
    now: DateTime<Utc>,
) -> StoredUnit {
    StoredUnit {
        unit: PhysicalUnit {
            id,
            catalog_id,
            ordinal,
            status: UnitStatus::Unknown,
            last_confirmed_area: None,
            current_area: None,
            has_binding: false,
            installation: Installation::default(),
            baseline_at: None,
            evidence: StateEvidence::Unknown,
            fence_at: now,
            paused: false,
            retired: false,
            archived,
            exception: Some(if archived {
                TrackingException::Archived
            } else {
                TrackingException::NeedsBaseline
            }),
            changed_at: now,
            last_event_id: None,
        },
        epc: None,
        last_confirmed_kind: None,
    }
}

/// Called inside the catalog's existing transaction, after its item writes.
/// Metadata changes never allocate/renumber units; decreases require retirement.
pub async fn reconcile_catalog(
    conn: &mut SqliteConnection,
    now: DateTime<Utc>,
) -> Result<ReconcileResult> {
    let rows = paged(conn, CATALOG, "id").await?;
    let mut records = inventory(conn).await?;
    let unit_rows = paged(conn, UNITS, "id").await?;
    let mut units = unit_rows
        .into_iter()
        .map(|r| body::<StoredUnit>(&r))
        .collect::<Result<Vec<_>>>()?;
    let version: i64 = sqlx::query(STATE)
        .fetch_one(&mut *conn)
        .await?
        .try_get("configuration_version")?;
    let mut result = ReconcileResult::default();
    let archived_by_id = rows
        .iter()
        .map(|r| {
            Ok((
                r.try_get::<String, _>("id")?,
                r.try_get::<bool, _>("archived")?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let mut final_active = units
        .iter()
        .filter(|u| {
            !u.unit.retired
                && !archived_by_id
                    .get(&u.unit.catalog_id)
                    .copied()
                    .unwrap_or(true)
        })
        .count();
    for row in rows {
        let id: String = row.try_get("id")?;
        let item: CatalogItem = decode(&row.try_get::<String, _>("body")?)?;
        let archived: bool = row.try_get("archived")?;
        let previous = records.get(&id).cloned();
        let mut record = previous.clone().unwrap_or(InventoryRecord {
            view: InventoryUnits {
                catalog_id: id.clone(),
                catalog_quantity: item.quantity,
                declared_quantity: None,
                active_units: 0,
                unallocated_units: 0,
                capacity_limited: false,
                needs_reconciliation: false,
                archived,
            },
            next_ordinal: 1,
        });
        record.view.catalog_quantity = item.quantity.filter(|q| *q > 0);
        record.view.archived = archived;
        let mut active = units
            .iter()
            .filter(|u| u.unit.catalog_id == id && !u.unit.retired)
            .count() as u32;
        let target = record
            .view
            .catalog_quantity
            .or(record.view.declared_quantity);
        let prior_target = previous
            .as_ref()
            .and_then(|r| r.view.catalog_quantity.or(r.view.declared_quantity));
        let requested_additions = if archived {
            0
        } else if target.is_some_and(|t| {
            prior_target.is_none_or(|p| t > p)
                || previous
                    .as_ref()
                    .is_some_and(|p| p.next_ordinal == 1 || p.view.capacity_limited)
        }) {
            target.unwrap_or(active).saturating_sub(active)
        } else {
            0
        };
        let additions = requested_additions.min(MAX_UNITS.saturating_sub(final_active) as u32);
        record.view.capacity_limited = !archived
            && (requested_additions > additions
                || previous.as_ref().is_some_and(|p| p.view.capacity_limited)
                    && target.is_some_and(|t| active.saturating_add(additions) < t));
        record.view.needs_reconciliation = target.is_none() || target.is_some_and(|t| active != t);
        final_active += additions as usize;
        if previous.is_none() {
            write_inventory(conn, &record).await?;
        }
        for _ in 0..additions {
            let mut unit = new_unit(
                UnitId::new(),
                id.clone(),
                record.next_ordinal,
                archived,
                now,
            );
            record.next_ordinal = record
                .next_ordinal
                .checked_add(1)
                .ok_or(invalid("tracking_limit", "ordinal"))?;
            // Persist the parent row before its new child; the surrounding sync
            // transaction owns rollback if any later item is invalid.
            write_unit(conn, &unit).await?;
            let mut change = UnitChange::new(
                None,
                unit,
                HistoryKind::QuantityReconciliation,
                DecisionReason::QuantityReconciliation,
                None,
                Vec::new(),
            );
            event(
                conn,
                &mut change,
                EventContext {
                    actor: "catalog",
                    automatic: true,
                    configuration_version: version,
                    at: now,
                },
            )
            .await?;
            unit = change.after;
            result.created_units.push(unit.unit.id);
            units.push(unit);
            active += 1;
            result.changed = true;
        }
        record.view.needs_reconciliation = target.is_none() || target.is_some_and(|t| active != t);
        for unit in units.iter_mut().filter(|u| u.unit.catalog_id == id) {
            let before = unit.clone();
            if unit.unit.archived != archived {
                unit.unit.archived = archived;
                unit.unit.fence_at = now;
                unit.unit.current_area = None;
                if !archived {
                    unit.unit.baseline_at = None;
                }
                unit.unit.exception = Some(if archived {
                    TrackingException::Archived
                } else {
                    TrackingException::NeedsBaseline
                });
            }
            if !unit.unit.retired && !archived {
                if record.view.needs_reconciliation {
                    unit.unit.exception = Some(TrackingException::NeedsReconciliation);
                } else if unit.unit.exception == Some(TrackingException::NeedsReconciliation) {
                    unit.unit.exception = if unit.unit.baseline_at.is_none() {
                        Some(TrackingException::NeedsBaseline)
                    } else {
                        None
                    };
                }
            }
            if unit != &before {
                unit.unit.changed_at = now;
                let restored = !archived && before.unit.archived;
                let (kind, reason) = if restored {
                    (HistoryKind::Restore, DecisionReason::CatalogRestore)
                } else if archived && !before.unit.archived {
                    (HistoryKind::Archive, DecisionReason::CatalogArchive)
                } else {
                    (
                        HistoryKind::QuantityReconciliation,
                        DecisionReason::QuantityReconciliation,
                    )
                };
                let mut change =
                    UnitChange::new(Some(before), unit.clone(), kind, reason, None, Vec::new());
                event(
                    conn,
                    &mut change,
                    EventContext {
                        actor: "catalog",
                        automatic: true,
                        configuration_version: version,
                        at: now,
                    },
                )
                .await?;
                *unit = change.after;
                result.changed = true;
            }
        }
        record.view.active_units = active;
        if previous.as_ref().is_none_or(|r| {
            r.view.catalog_quantity != record.view.catalog_quantity
                || r.view.declared_quantity != record.view.declared_quantity
                || r.view.needs_reconciliation != record.view.needs_reconciliation
                || r.view.capacity_limited != record.view.capacity_limited
                || r.view.archived != record.view.archived
                || r.next_ordinal != record.next_ordinal
        }) {
            result.changed = true;
            write_inventory(conn, &record).await?;
        }
        records.insert(id, record);
    }
    if result.changed {
        sqlx::query(BUMP).fetch_one(conn).await?;
    }
    Ok(result)
}

impl Store {
    pub async fn open_simulator(now: DateTime<Utc>) -> crate::store::Result<Self> {
        let store = Self::open(":memory:").await?;
        {
            let mut runtime = store.tracking.lock().await;
            *runtime = Engine::new(EngineMode::Simulation, now);
        }
        store
            .tracking_startup(now)
            .await
            .map_err(crate::store::StoreError::from)?;
        Ok(store)
    }
    pub async fn tracking_startup(&self, now: DateTime<Utc>) -> Result<()> {
        self.tracking_startup_clocked(move || now).await
    }
    pub async fn tracking_startup_clocked(
        &self,
        clock: impl FnOnce() -> DateTime<Utc> + Send,
    ) -> Result<()> {
        let mut runtime = self.tracking.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = clock();
        reconcile_catalog(&mut tx, now).await?;
        load(&mut tx, &mut runtime, now, true).await?;
        let mut changed = false;
        for session in runtime.sessions.values_mut() {
            if session.state == EnrollmentState::Active
                || !matches!(
                    session.borrow_state,
                    BorrowState::NotBorrowed | BorrowState::Restored
                )
            {
                reset_capture(session, now);
                if !matches!(
                    session.borrow_state,
                    BorrowState::NotBorrowed | BorrowState::Restored
                ) {
                    session.state = EnrollmentState::RecoveryRequired;
                    session.borrow_state = BorrowState::RestoreRequested;
                }
                write_session(&mut tx, session).await?;
                changed = true;
            }
        }
        if changed {
            runtime.revision = sqlx::query_scalar(BUMP).fetch_one(&mut *tx).await?;
        }
        tx.commit().await?;
        runtime.reset_evidence(now);
        Ok(())
    }
    pub async fn tracking_snapshot(&self, now: DateTime<Utc>) -> Result<TrackingSnapshot> {
        self.tracking_snapshot_clocked(move || now).await
    }
    pub async fn tracking_snapshot_clocked(
        &self,
        clock: impl FnOnce() -> DateTime<Utc> + Send,
    ) -> Result<TrackingSnapshot> {
        let mut runtime = self.tracking.lock().await;
        let mut tx = self.pool.begin().await?;
        let now = clock();
        load(&mut tx, &mut runtime, now, false).await?;
        tx.commit().await?;
        Ok(runtime.snapshot(now))
    }
    pub async fn tracking_ingest(
        &self,
        batch: ObservationBatch,
        now: DateTime<Utc>,
    ) -> Result<IngestResult> {
        self.tracking_ingest_clocked(batch, move || now).await
    }
    pub async fn tracking_ingest_clocked(
        &self,
        batch: ObservationBatch,
        clock: impl FnOnce() -> DateTime<Utc> + Send,
    ) -> Result<IngestResult> {
        let mut runtime = self.tracking.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = clock();
        load(&mut tx, &mut runtime, now, false).await?;
        let mut next = runtime.clone();
        let reader = batch.reader_id.clone();
        let changes = match next.ingest(batch, now) {
            Ok(changes) => changes,
            Err(error) => {
                runtime.rejected(reader, now, &error);
                return Err(error);
            }
        };
        let count = changes.units.len();
        let accepted = changes.accepted;
        let duplicate = changes.duplicate;
        let candidates = next.sessions != runtime.sessions;
        Self::persist_runtime(&mut tx, &mut next, changes, now).await?;
        tx.commit().await?;
        *runtime = next;
        Ok(IngestResult {
            tracking_revision: runtime.revision,
            accepted,
            duplicate,
            unit_changes: count,
            candidate_changes: candidates,
        })
    }
    pub async fn tracking_tick(&self, now: DateTime<Utc>) -> Result<IngestResult> {
        self.tracking_tick_clocked(move || now).await
    }
    pub async fn tracking_tick_clocked(
        &self,
        clock: impl FnOnce() -> DateTime<Utc> + Send,
    ) -> Result<IngestResult> {
        let mut runtime = self.tracking.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = clock();
        load(&mut tx, &mut runtime, now, false).await?;
        let mut next = runtime.clone();
        let changes = next.tick(now)?;
        let count = changes.units.len();
        Self::persist_runtime(&mut tx, &mut next, changes, now).await?;
        tx.commit().await?;
        *runtime = next;
        Ok(IngestResult {
            tracking_revision: runtime.revision,
            accepted: true,
            duplicate: false,
            unit_changes: count,
            candidate_changes: false,
        })
    }
    async fn persist_runtime(
        conn: &mut SqliteConnection,
        next: &mut Engine,
        changes: Changes,
        now: DateTime<Utc>,
    ) -> Result<()> {
        if changes.units.is_empty() && !changes.sessions_changed {
            return Ok(());
        }
        let revision: i64 = sqlx::query(STATE)
            .fetch_one(&mut *conn)
            .await?
            .try_get("revision")?;
        if revision != next.revision {
            return Err(TrackingError::Revision);
        }
        for mut change in changes.units {
            event(
                &mut *conn,
                &mut change,
                EventContext {
                    actor: "reader",
                    automatic: true,
                    configuration_version: next.configuration_version,
                    at: now,
                },
            )
            .await?;
            next.units.insert(change.after.unit.id, change.after);
        }
        if changes.sessions_changed {
            for session in next.sessions.values() {
                write_session(&mut *conn, session).await?;
            }
        }
        next.revision = sqlx::query_scalar(BUMP).fetch_one(&mut *conn).await?;
        Ok(())
    }
    pub async fn tracking_history(
        &self,
        unit_id: Option<UnitId>,
        before: Option<i64>,
        limit: u16,
    ) -> Result<HistoryPage> {
        if limit == 0 || limit > MAX_HISTORY_PAGE || before.is_some_and(|v| v <= 0) {
            return Err(invalid("invalid_history_page", "limit"));
        }
        let end = before.unwrap_or(i64::MAX);
        let count = i64::from(limit) + 1;
        let rows = if let Some(id) = unit_id {
            sqlx::query(HISTORY_UNIT)
                .bind(id.to_string())
                .bind(end)
                .bind(count)
                .fetch_all(&self.pool)
                .await?
        } else {
            sqlx::query(HISTORY_ALL)
                .bind(end)
                .bind(count)
                .fetch_all(&self.pool)
                .await?
        };
        let mut events = rows
            .into_iter()
            .map(|r| body::<HistoryEvent>(&r))
            .collect::<Result<Vec<_>>>()?;
        let more = events.len() > usize::from(limit);
        events.truncate(usize::from(limit));
        let next_before = if more {
            events.last().map(|e| e.id)
        } else {
            None
        };
        Ok(HistoryPage {
            events,
            next_before,
        })
    }
    pub async fn tracking_availability(&self, _now: DateTime<Utc>) -> Result<Availability> {
        let mut tx = self.pool.begin().await?;
        let result = availability(&mut tx).await?;
        tx.commit().await?;
        Ok(result)
    }
    pub async fn tracking_runtime_config(
        &self,
        now: DateTime<Utc>,
    ) -> Result<(i64, TrackingConfig)> {
        self.tracking_runtime_config_clocked(move || now).await
    }
    pub async fn tracking_runtime_config_clocked(
        &self,
        clock: impl FnOnce() -> DateTime<Utc> + Send,
    ) -> Result<(i64, TrackingConfig)> {
        let mut runtime = self.tracking.lock().await;
        let mut conn = self.pool.acquire().await?;
        let now = clock();
        let row = sqlx::query(STATE).fetch_one(&mut *conn).await?;
        let version: i64 = row.try_get("configuration_version")?;
        if version != runtime.configuration_version {
            load(&mut conn, &mut runtime, now, false).await?;
        }
        let config = decode(&row.try_get::<String, _>("config")?)?;
        Ok((version, config))
    }
    pub async fn tracking_command(
        &self,
        actor: &str,
        key: &str,
        expected_revision: i64,
        command: TrackingCommand,
        now: DateTime<Utc>,
    ) -> Result<CommandResult> {
        self.tracking_command_clocked(actor, key, expected_revision, command, move || now)
            .await
    }
    pub async fn tracking_command_clocked(
        &self,
        actor: &str,
        key: &str,
        expected_revision: i64,
        command: TrackingCommand,
        clock: impl FnOnce() -> DateTime<Utc> + Send,
    ) -> Result<CommandResult> {
        if !text(actor, 200) || !text(key, 200) {
            return Err(invalid("invalid_operation", "idempotency_key"));
        }
        let request_hash = digest(&serde_json::to_vec(&command)?);
        let mut runtime = self.tracking.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = clock();
        if let Some(row) = sqlx::query(OPERATIONS)
            .bind(actor)
            .bind(key)
            .fetch_optional(&mut *tx)
            .await?
        {
            if row.try_get::<String, _>("request_hash")? != request_hash {
                return Err(TrackingError::Conflict("idempotency_conflict"));
            }
            let response: CommandResult = decode(&row.try_get::<String, _>("response")?)?;
            tx.commit().await?;
            return Ok(response);
        }
        load(&mut tx, &mut runtime, now, false).await?;
        if expected_revision != runtime.revision {
            return Err(TrackingError::Revision);
        }
        let mut next = runtime.clone();
        next.clock(now)?;
        let mut changes: Vec<UnitChange> = Vec::new();
        let mut session_id = None;
        let mut config_changed = false;
        let mut sessions_changed = false;
        let mut changed = false;
        let mut created = Vec::new();
        match command {
            TrackingCommand::Configure { config } => {
                if next.configure(config, now)? {
                    config_changed = true;
                    sessions_changed = true;
                    changed = true;
                }
            }
            TrackingCommand::DeclareUnitCount { catalog_id, count } => {
                if count == 0 {
                    return Err(invalid("invalid_unit_count", "count"));
                }
                let record = next
                    .inventory
                    .get(&catalog_id)
                    .ok_or(TrackingError::NotFound)?;
                if record.catalog_quantity.is_some() {
                    return Err(TrackingError::Conflict("catalog_quantity_known"));
                }
                if record.archived {
                    return Err(TrackingError::Conflict("catalog_archived"));
                }
                if record.declared_quantity != Some(count) {
                    sqlx::query(DECLARE_QUANTITY)
                        .bind(count)
                        .bind(&catalog_id)
                        .execute(&mut *tx)
                        .await?;
                    created = create_missing_units(&mut tx, &catalog_id, count, now, actor)
                        .await?
                        .created_units;
                    let result = reconcile_catalog(&mut tx, now).await?;
                    created.extend(result.created_units);
                    load(&mut tx, &mut next, now, true).await?;
                    changed = true;
                }
            }
            TrackingCommand::ReconcileUnits { catalog_id } => {
                let record = next
                    .inventory
                    .get(&catalog_id)
                    .ok_or(TrackingError::NotFound)?;
                if record.archived {
                    return Err(TrackingError::Conflict("catalog_archived"));
                }
                let target = record
                    .catalog_quantity
                    .or(record.declared_quantity)
                    .ok_or(TrackingError::Conflict("quantity_unknown"))?;
                let allocation =
                    create_missing_units(&mut tx, &catalog_id, target, now, actor).await?;
                created = allocation.created_units;
                changed = allocation.changed;
                load(&mut tx, &mut next, now, true).await?;
            }
            TrackingCommand::Baseline { unit_id, area_id } => {
                let area = next
                    .config
                    .areas
                    .iter()
                    .find(|a| a.id == area_id && a.enabled)
                    .cloned()
                    .ok_or(invalid("unknown_area", "area_id"))?;
                let before = active_unit(&next, unit_id)?;
                let mut after = before.clone();
                after.unit.status = area.kind.present_status();
                after.unit.last_confirmed_area = Some(area_id.clone());
                after.unit.current_area = Some(area_id);
                after.last_confirmed_kind = Some(area.kind);
                after.unit.baseline_at = Some(now);
                after.unit.evidence = StateEvidence::Manual;
                after.unit.fence_at = now;
                after.unit.changed_at = now;
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
                next.fence(unit_id);
                changes.push(UnitChange::new(
                    Some(before),
                    after,
                    HistoryKind::Baseline,
                    DecisionReason::ManualBaseline,
                    None,
                    Vec::new(),
                ));
            }
            TrackingCommand::Correct {
                unit_id,
                status,
                note,
            } => {
                if !text(&note, 500) {
                    return Err(invalid("invalid_note", "note"));
                }
                let before = active_unit(&next, unit_id)?;
                let mut after = before.clone();
                after.unit.status = status;
                after.unit.baseline_at = if status == UnitStatus::Unknown {
                    None
                } else {
                    Some(now)
                };
                after.unit.evidence = if status == UnitStatus::Unknown {
                    StateEvidence::Unknown
                } else {
                    StateEvidence::Manual
                };
                after.unit.current_area = None;
                after.unit.last_confirmed_area = None;
                after.last_confirmed_kind = None;
                after.unit.fence_at = now;
                after.unit.changed_at = now;
                if !matches!(
                    after.unit.exception,
                    Some(TrackingException::NeedsReconciliation | TrackingException::Paused)
                ) {
                    after.unit.exception = if status == UnitStatus::Unknown {
                        Some(TrackingException::NeedsBaseline)
                    } else {
                        None
                    };
                }
                next.fence(unit_id);
                changes.push(UnitChange::new(
                    Some(before),
                    after,
                    HistoryKind::Correction,
                    DecisionReason::ManualCorrection,
                    Some(note),
                    Vec::new(),
                ));
            }
            TrackingCommand::Installation {
                unit_id,
                installation,
            } => {
                if installation
                    .tag_model
                    .as_ref()
                    .is_some_and(|s| !text(s, 200))
                    || installation
                        .placement
                        .as_ref()
                        .is_some_and(|s| !text(s, 200))
                    || installation.notes.as_ref().is_some_and(|s| !text(s, 500))
                    || installation
                        .no_tag_reason
                        .as_ref()
                        .is_some_and(|s| !text(s, 500))
                {
                    return Err(invalid("invalid_installation", "installation"));
                }
                let before = active_unit(&next, unit_id)?;
                if before.unit.installation != installation {
                    let mut after = before.clone();
                    after.unit.installation = installation;
                    after.unit.fence_at = now;
                    after.unit.changed_at = now;
                    next.fence(unit_id);
                    changes.push(UnitChange::new(
                        Some(before),
                        after,
                        HistoryKind::Correction,
                        DecisionReason::InstallationUpdate,
                        None,
                        Vec::new(),
                    ));
                }
            }
            TrackingCommand::Pause { unit_id, paused } => {
                let before = active_unit(&next, unit_id)?;
                if before.unit.paused != paused {
                    let mut after = before.clone();
                    after.unit.paused = paused;
                    after.unit.fence_at = now;
                    after.unit.changed_at = now;
                    if paused && after.unit.exception.is_none() {
                        after.unit.exception = Some(TrackingException::Paused);
                    } else if !paused && after.unit.exception == Some(TrackingException::Paused) {
                        after.unit.exception = if after.unit.baseline_at.is_none() {
                            Some(TrackingException::NeedsBaseline)
                        } else {
                            None
                        };
                    }
                    next.fence(unit_id);
                    changes.push(UnitChange::new(
                        Some(before),
                        after,
                        HistoryKind::Pause,
                        DecisionReason::TrackingPause,
                        None,
                        Vec::new(),
                    ));
                }
            }
            TrackingCommand::Retire { unit_id, note } => {
                if !text(&note, 500) {
                    return Err(invalid("invalid_note", "note"));
                }
                let before = active_unit(&next, unit_id)?;
                let mut after = before.clone();
                after.unit.retired = true;
                after.epc = None;
                after.unit.has_binding = false;
                after.unit.current_area = None;
                after.unit.exception = None;
                after.unit.fence_at = now;
                after.unit.changed_at = now;
                next.fence(unit_id);
                changes.push(UnitChange::new(
                    Some(before),
                    after,
                    HistoryKind::Retire,
                    DecisionReason::Retirement,
                    Some(note),
                    Vec::new(),
                ));
            }
            TrackingCommand::Unbind { unit_id, note } => {
                if !text(&note, 500) {
                    return Err(invalid("invalid_note", "note"));
                }
                let before = active_unit(&next, unit_id)?;
                if before.epc.is_some() {
                    let mut after = before.clone();
                    after.epc = None;
                    after.unit.has_binding = false;
                    after.unit.status = UnitStatus::Unknown;
                    after.unit.current_area = None;
                    after.unit.baseline_at = None;
                    after.unit.evidence = StateEvidence::Unknown;
                    after.unit.exception = Some(TrackingException::NeedsBaseline);
                    after.unit.fence_at = now;
                    after.unit.changed_at = now;
                    next.fence(unit_id);
                    changes.push(UnitChange::new(
                        Some(before),
                        after,
                        HistoryKind::Unbind,
                        DecisionReason::TagUnbound,
                        Some(note),
                        Vec::new(),
                    ));
                }
            }
            TrackingCommand::Undo { event_id, note } => {
                if !text(&note, 500) {
                    return Err(invalid("invalid_note", "note"));
                }
                let row = sqlx::query(HISTORY_EVENT)
                    .bind(event_id)
                    .fetch_optional(&mut *tx)
                    .await?
                    .ok_or(TrackingError::NotFound)?;
                let history: HistoryEvent = decode(&row.try_get::<String, _>("body")?)?;
                let before = next
                    .units
                    .get(&history.unit_id)
                    .cloned()
                    .ok_or(TrackingError::NotFound)?;
                if before.unit.last_event_id != Some(event_id) {
                    return Err(TrackingError::Conflict("undo_not_latest"));
                }
                let mut after = history
                    .before
                    .ok_or(TrackingError::Conflict("undo_not_supported"))?;
                if let Some(epc) = &after.epc
                    && next.units.values().any(|u| {
                        u.unit.id != after.unit.id && !u.unit.retired && u.epc.as_ref() == Some(epc)
                    })
                {
                    return Err(TrackingError::Conflict("epc_already_bound"));
                }
                if next
                    .inventory
                    .get(&after.unit.catalog_id)
                    .is_none_or(|i| i.archived != after.unit.archived)
                {
                    return Err(TrackingError::Conflict("catalog_changed"));
                }
                after.unit.fence_at = now;
                after.unit.changed_at = now;
                if after.unit.baseline_at.is_some() {
                    after.unit.baseline_at = Some(now);
                    after.unit.evidence = StateEvidence::Manual;
                }
                next.fence(after.unit.id);
                changes.push(UnitChange::new(
                    Some(before),
                    after,
                    HistoryKind::Undo,
                    DecisionReason::Undo,
                    Some(note),
                    Vec::new(),
                ));
            }
            TrackingCommand::StartEnrollment {
                unit_ids,
                port_id,
                borrow,
                purpose,
                ttl_seconds,
            } => {
                validate_queue(&next, &unit_ids, purpose)?;
                if !(1..=3_600).contains(&ttl_seconds) {
                    return Err(invalid("invalid_session_ttl", "ttl_seconds"));
                }
                validate_station(&next, &port_id, borrow)?;
                if next.sessions.len() >= 1_000 {
                    return Err(TrackingError::Conflict("enrollment_session_limit"));
                }
                let id = SessionId::new();
                let session = EnrollmentSession {
                    id,
                    purpose,
                    port_id,
                    queue: unit_ids,
                    cursor: 0,
                    state: EnrollmentState::Active,
                    capture_version: 1,
                    capture_started_at: now,
                    expires_at: now + Duration::seconds(i64::from(ttl_seconds)),
                    borrow_state: if borrow {
                        BorrowState::PauseRequested
                    } else {
                        BorrowState::NotBorrowed
                    },
                    candidates: Vec::new(),
                    capture_overflow: false,
                };
                next.sessions.insert(id, session);
                session_id = Some(id);
                sessions_changed = true;
                changed = true;
                next.reset_coverage_for_enrollment();
            }
            TrackingCommand::ResetCapture { session_id: id } => {
                let session = session_mut(&mut next, id, now)?;
                reset_capture(session, now);
                session_id = Some(id);
                sessions_changed = true;
                changed = true;
            }
            TrackingCommand::ResumeEnrollment {
                session_id: id,
                ttl_seconds,
            } => {
                if !(1..=3_600).contains(&ttl_seconds) {
                    return Err(invalid("invalid_session_ttl", "ttl_seconds"));
                }
                let previous = next
                    .sessions
                    .get(&id)
                    .cloned()
                    .ok_or(TrackingError::NotFound)?;
                if previous.state == EnrollmentState::Complete
                    || !matches!(
                        previous.borrow_state,
                        BorrowState::NotBorrowed | BorrowState::Restored
                    )
                {
                    return Err(TrackingError::Conflict("enrollment_restore_required"));
                }
                validate_station_except(
                    &next,
                    &previous.port_id,
                    previous.borrow_state == BorrowState::Restored,
                    Some(id),
                )?;
                let session = next.sessions.get_mut(&id).ok_or(TrackingError::NotFound)?;
                session.state = EnrollmentState::Active;
                session.expires_at = now + Duration::seconds(i64::from(ttl_seconds));
                if session.borrow_state == BorrowState::Restored {
                    session.borrow_state = BorrowState::PauseRequested;
                }
                reset_capture(session, now);
                session_id = Some(id);
                sessions_changed = true;
                changed = true;
                next.reset_coverage_for_enrollment();
            }
            TrackingCommand::SelectCandidate {
                session_id: id,
                capture_version,
                unit_id,
                epc,
                baseline_area,
            } => {
                let session = next
                    .sessions
                    .get(&id)
                    .cloned()
                    .ok_or(TrackingError::NotFound)?;
                if session.purpose == EnrollmentPurpose::Lookup {
                    return Err(TrackingError::Conflict("lookup_read_only"));
                }
                validate_selection(&session, capture_version, unit_id, now)?;
                let view = next.session_view(&session, now);
                if !view.candidates.iter().any(|c| c.epc == epc && c.selectable) {
                    return Err(TrackingError::Conflict("enrollment_not_isolated_fresh"));
                }
                let before = active_unit(&next, unit_id)?;
                validate_queue(&next, &[unit_id], session.purpose)?;
                if next
                    .units
                    .values()
                    .any(|u| !u.unit.retired && u.epc.as_ref() == Some(&epc))
                {
                    return Err(TrackingError::Conflict("epc_already_bound"));
                }
                let replace = session.purpose == EnrollmentPurpose::Replace;
                let mut after = before.clone();
                after.epc = Some(epc);
                after.unit.has_binding = true;
                after.unit.installation.state = InstallationState::Installed;
                after.unit.fence_at = now;
                after.unit.changed_at = now;
                if !replace && before.unit.baseline_at.is_none() {
                    after.unit.status = UnitStatus::Unknown;
                    after.unit.current_area = None;
                    after.unit.baseline_at = None;
                    after.unit.evidence = StateEvidence::Unknown;
                    after.unit.exception = Some(TrackingException::NeedsBaseline);
                }
                if let Some(area_id) = baseline_area {
                    let area = next
                        .config
                        .areas
                        .iter()
                        .find(|a| a.id == area_id && a.enabled)
                        .ok_or(invalid("unknown_area", "baseline_area"))?;
                    after.unit.status = area.kind.present_status();
                    after.unit.current_area = Some(area_id.clone());
                    after.unit.last_confirmed_area = Some(area_id);
                    after.last_confirmed_kind = Some(area.kind);
                    after.unit.baseline_at = Some(now);
                    after.unit.evidence = StateEvidence::Manual;
                    after.unit.exception = None;
                }
                next.fence(unit_id);
                changes.push(UnitChange::new(
                    Some(before),
                    after,
                    if replace {
                        HistoryKind::Replace
                    } else {
                        HistoryKind::Bind
                    },
                    if replace {
                        DecisionReason::TagReplaced
                    } else {
                        DecisionReason::TagBound
                    },
                    None,
                    Vec::new(),
                ));
                advance_session(
                    next.sessions.get_mut(&id).ok_or(TrackingError::NotFound)?,
                    now,
                );
                session_id = Some(id);
                sessions_changed = true;
                changed = true;
            }
            TrackingCommand::Skip {
                session_id: id,
                capture_version,
                unit_id,
            } => {
                let session = session_mut(&mut next, id, now)?;
                validate_selection(session, capture_version, unit_id, now)?;
                advance_session(session, now);
                session_id = Some(id);
                sessions_changed = true;
                changed = true;
            }
            TrackingCommand::AbortEnrollment { session_id: id } => {
                let session = next.sessions.get_mut(&id).ok_or(TrackingError::NotFound)?;
                if session.state == EnrollmentState::Active
                    || session.state == EnrollmentState::RecoveryRequired
                {
                    session.state = EnrollmentState::Aborted;
                    session.candidates.clear();
                    if !matches!(
                        session.borrow_state,
                        BorrowState::NotBorrowed | BorrowState::Restored
                    ) {
                        session.borrow_state = BorrowState::RestoreRequested;
                    }
                    session_id = Some(id);
                    sessions_changed = true;
                    changed = true;
                }
            }
            TrackingCommand::AcknowledgeBorrow {
                session_id: id,
                state,
            } => {
                let session = next.sessions.get_mut(&id).ok_or(TrackingError::NotFound)?;
                let allowed = matches!(
                    (session.borrow_state, state),
                    (BorrowState::PauseRequested, BorrowState::Paused)
                        | (BorrowState::RestoreRequested, BorrowState::Restored)
                        | (BorrowState::Failed, BorrowState::Restored)
                        | (
                            BorrowState::PauseRequested
                                | BorrowState::Paused
                                | BorrowState::RestoreRequested,
                            BorrowState::Failed
                        )
                );
                if !allowed {
                    return Err(TrackingError::Conflict("invalid_borrow_transition"));
                }
                session.borrow_state = state;
                reset_capture(session, now);
                if state == BorrowState::Failed {
                    session.state = EnrollmentState::RecoveryRequired;
                } else if state == BorrowState::Restored
                    && session.state == EnrollmentState::RecoveryRequired
                {
                    session.state = EnrollmentState::Aborted;
                }
                session_id = Some(id);
                sessions_changed = true;
                changed = true;
                next.reset_coverage_for_enrollment();
            }
            TrackingCommand::Tick => {
                let result = next.tick(now)?;
                sessions_changed = result.sessions_changed;
                changed = sessions_changed;
                for change in result.units {
                    changes.push(UnitChange::new(
                        change.before,
                        change.after,
                        change.kind,
                        change.reason,
                        None,
                        change.evidence,
                    ));
                }
            }
            TrackingCommand::Import { data } => {
                self.import_tracking(&mut tx, &mut next, *data, now).await?;
                changed = true;
            }
        }
        let mut event_ids = Vec::new();
        let mut unit_ids = created;
        for mut change in changes {
            let automatic = matches!(
                change.reason,
                DecisionReason::StableArea
                    | DecisionReason::ConfirmedAbsence
                    | DecisionReason::AmbiguousEvidence
            );
            let id = event(
                &mut tx,
                &mut change,
                EventContext {
                    actor,
                    automatic,
                    configuration_version: next.configuration_version,
                    at: now,
                },
            )
            .await?;
            event_ids.push(id);
            unit_ids.push(change.after.unit.id);
            next.units.insert(change.after.unit.id, change.after);
            changed = true;
        }
        if config_changed {
            for id in runtime
                .reader_operations
                .keys()
                .filter(|id| !next.reader_operations.contains_key(*id))
            {
                sqlx::query(DELETE_READER_OPERATION)
                    .bind(id.as_str())
                    .execute(&mut *tx)
                    .await?;
            }
            for id in runtime
                .reader_controls
                .keys()
                .filter(|id| !next.reader_controls.contains_key(*id))
            {
                sqlx::query(DELETE_READER_CONTROL)
                    .bind(id.as_str())
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query(CONFIGURE)
                .bind(encode(&next.config)?)
                .bind(next.config.enabled)
                .bind(next.configuration_version)
                .execute(&mut *tx)
                .await?;
            sqlx::query(INSERT_CONFIGURATION)
                .bind(next.configuration_version)
                .bind(now.to_rfc3339())
                .bind(encode(&next.config)?)
                .execute(&mut *tx)
                .await?;
        }
        if sessions_changed {
            for session in next.sessions.values() {
                write_session(&mut tx, session).await?;
            }
        }
        if changed {
            let result = reconcile_catalog(&mut tx, now).await?;
            unit_ids.extend(result.created_units);
            let database_revision: i64 = sqlx::query(STATE)
                .fetch_one(&mut *tx)
                .await?
                .try_get("revision")?;
            next.revision = if database_revision == runtime.revision {
                sqlx::query_scalar(BUMP).fetch_one(&mut *tx).await?
            } else {
                database_revision
            };
            // Refresh only the inventory summary. Do not discard the newly
            // captured session/evidence while committing its atomic binding.
            let rows = paged(&mut tx, UNITS, "id").await?;
            next.units = rows
                .into_iter()
                .map(|r| body::<StoredUnit>(&r).map(|u| (u.unit.id, u)))
                .collect::<Result<BTreeMap<_, _>>>()?;
            let records = inventory(&mut tx).await?;
            next.inventory = records
                .into_iter()
                .map(|(id, mut r)| {
                    r.view.active_units = next
                        .units
                        .values()
                        .filter(|u| u.unit.catalog_id == id && !u.unit.retired)
                        .count() as u32;
                    r.view.unallocated_units = r
                        .view
                        .catalog_quantity
                        .or(r.view.declared_quantity)
                        .unwrap_or(0)
                        .saturating_sub(r.view.active_units);
                    (id, r.view)
                })
                .collect();
        }
        unit_ids.sort();
        unit_ids.dedup();
        let response = CommandResult {
            tracking_revision: next.revision,
            changed,
            unit_ids,
            session: session_id
                .and_then(|id| next.sessions.get(&id).map(|s| next.session_view(s, now))),
            event_ids,
        };
        sqlx::query(INSERT_OPERATION)
            .bind(actor)
            .bind(key)
            .bind(request_hash)
            .bind(encode(&response)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        *runtime = next;
        Ok(response)
    }
    pub async fn tracking_export(&self, now: DateTime<Utc>) -> Result<TrackingExport> {
        self.tracking_export_clocked(move || now).await
    }
    pub async fn tracking_export_clocked(
        &self,
        clock: impl FnOnce() -> DateTime<Utc> + Send,
    ) -> Result<TrackingExport> {
        let mut runtime = self.tracking.lock().await;
        let mut tx = self.pool.begin().await?;
        let now = clock();
        load(&mut tx, &mut runtime, now, false).await?;
        let rows = sqlx::query(EXPORT_HISTORY).fetch_all(&mut *tx).await?;
        if rows.len() > 100_000 {
            return Err(invalid("tracking_export_too_large", "history"));
        }
        let history = rows
            .into_iter()
            .map(|r| body::<HistoryEvent>(&r))
            .collect::<Result<Vec<_>>>()?;
        let rows = sqlx::query(EXPORT_OPERATIONS).fetch_all(&mut *tx).await?;
        if rows.len() > 100_000 {
            return Err(invalid("tracking_export_too_large", "operations"));
        }
        let mut operations = Vec::new();
        for row in rows {
            operations.push(OperationRecord {
                actor: row.try_get("actor")?,
                key: row.try_get("key")?,
                request_hash: row.try_get("request_hash")?,
                response: decode(&row.try_get::<String, _>("response")?)?,
            });
        }
        let rows = sqlx::query(BINDINGS).fetch_all(&mut *tx).await?;
        if rows.len() > 100_000 {
            return Err(invalid("tracking_export_too_large", "bindings"));
        }
        let mut bindings = Vec::new();
        for row in rows {
            bindings.push(TagBinding {
                id: row.try_get("id")?,
                unit_id: row
                    .try_get::<String, _>("unit_id")?
                    .parse()
                    .map_err(|_| invalid("stored_data_invalid", "unit_id"))?,
                epc: row
                    .try_get::<String, _>("epc")?
                    .parse()
                    .map_err(|_| invalid("stored_data_invalid", "epc"))?,
                bound_at: parse_time(&row.try_get::<String, _>("bound_at")?)?,
                retired_at: row
                    .try_get::<Option<String>, _>("retired_at")?
                    .as_deref()
                    .map(parse_time)
                    .transpose()?,
                retirement: row
                    .try_get::<Option<String>, _>("retirement")?
                    .as_deref()
                    .map(decode)
                    .transpose()?,
            });
        }
        let mut configurations = Vec::new();
        for row in sqlx::query(CONFIGURATIONS).fetch_all(&mut *tx).await? {
            configurations.push(ConfigurationVersion {
                version: row.try_get("version")?,
                at: parse_time(&row.try_get::<String, _>("at")?)?,
                config: decode(&row.try_get::<String, _>("body")?)?,
            });
        }
        if configurations.len() > 10_000 {
            return Err(invalid("tracking_export_too_large", "configurations"));
        }
        let data = TrackingExport {
            data_source: if runtime.mode == EngineMode::Live {
                DataSource::Live
            } else {
                DataSource::Simulation
            },
            configurations,
            schema_version: TRACKING_SCHEMA_VERSION,
            exported_at: now,
            tracking_revision: runtime.revision,
            configuration_version: runtime.configuration_version,
            config: runtime.config.clone(),
            inventory: runtime.inventory.values().cloned().collect(),
            units: runtime.units.values().cloned().collect(),
            sessions: runtime
                .sessions
                .values()
                .cloned()
                .map(|mut s| {
                    s.candidates.clear();
                    s
                })
                .collect(),
            history,
            bindings,
            operations,
            reader_operations: runtime.reader_operations.values().cloned().collect(),
            reader_controls: runtime.reader_controls.values().cloned().collect(),
        };
        tx.commit().await?;
        Ok(data)
    }
    async fn import_tracking(
        &self,
        conn: &mut SqliteConnection,
        next: &mut Engine,
        data: TrackingExport,
        now: DateTime<Utc>,
    ) -> Result<()> {
        if next.requires_reader_cleanup() {
            return Err(TrackingError::Conflict("reader_cleanup_required"));
        }
        if data.schema_version != TRACKING_SCHEMA_VERSION
            || data.units.len() > 100_000
            || data.inventory.len() > 100_000
            || data.sessions.len() > 1_000
            || data.history.len() > 100_000
            || data.bindings.len() > 100_000
            || data.operations.len() > 100_000
            || data.configurations.len() > 10_000
        {
            return Err(invalid("invalid_tracking_export", "schema_version"));
        }
        if next.mode == EngineMode::Live && data.data_source != DataSource::Live {
            return Err(TrackingError::Conflict("simulation_isolation"));
        }
        crate::tracking::engine::validate_config(&data.config)?;
        let catalog = paged(conn, CATALOG, "id")
            .await?
            .into_iter()
            .map(|r| r.try_get::<String, _>("id"))
            .collect::<std::result::Result<BTreeSet<_>, _>>()?;
        let mut ids = BTreeSet::new();
        let mut positions = BTreeSet::new();
        let mut epcs = BTreeSet::new();
        let mut inventory_ids = BTreeSet::new();
        for r in &data.inventory {
            if !catalog.contains(&r.catalog_id)
                || !inventory_ids.insert(r.catalog_id.clone())
                || r.catalog_quantity == Some(0)
                || r.declared_quantity == Some(0)
            {
                return Err(invalid("invalid_tracking_export", "inventory"));
            }
        }
        for u in &data.units {
            if u.unit.id.0.is_nil()
                || !ids.insert(u.unit.id)
                || !inventory_ids.contains(&u.unit.catalog_id)
                || u.unit.ordinal == 0
                || !positions.insert((u.unit.catalog_id.clone(), u.unit.ordinal))
                || u.unit.has_binding != u.epc.is_some()
                || u.unit.retired && u.epc.is_some()
            {
                return Err(invalid("invalid_tracking_export", "units"));
            }
            if let Some(current) = next.units.get(&u.unit.id)
                && (current.unit.catalog_id != u.unit.catalog_id
                    || current.unit.ordinal != u.unit.ordinal)
            {
                return Err(TrackingError::Conflict("import_unit_identity_conflict"));
            }
            if let Some(epc) = &u.epc
                && !epcs.insert(epc.clone())
            {
                return Err(TrackingError::Conflict("epc_already_bound"));
            }
        }
        let mut binding_ids = BTreeSet::new();
        let mut binding_epcs = BTreeSet::new();
        let mut binding_units = BTreeSet::new();
        for b in &data.bindings {
            if b.id <= 0
                || !binding_ids.insert(b.id)
                || !ids.contains(&b.unit_id)
                || b.retired_at.is_some() != b.retirement.is_some()
            {
                return Err(invalid("invalid_tracking_export", "bindings"));
            }
            if b.retired_at.is_none()
                && (!binding_epcs.insert(b.epc.clone())
                    || !binding_units.insert(b.unit_id)
                    || !data
                        .units
                        .iter()
                        .any(|u| u.unit.id == b.unit_id && u.epc.as_ref() == Some(&b.epc)))
            {
                return Err(invalid("invalid_tracking_export", "bindings"));
            }
        }
        if binding_epcs != epcs {
            return Err(invalid("invalid_tracking_export", "bindings"));
        }
        let mut events = BTreeSet::new();
        for h in &data.history {
            if h.id <= 0
                || !events.insert(h.id)
                || !ids.contains(&h.unit_id)
                || h.after.unit.id != h.unit_id
                || h.before.as_ref().is_some_and(|u| u.unit.id != h.unit_id)
            {
                return Err(invalid("invalid_tracking_export", "history"));
            }
        }
        for u in &data.units {
            if u.unit.last_event_id.is_some_and(|id| !events.contains(&id)) {
                return Err(invalid("invalid_tracking_export", "last_event_id"));
            }
        }
        let mut session_ids = BTreeSet::new();
        let mut stations = BTreeSet::new();
        for s in &data.sessions {
            let requires_current_port = matches!(
                s.state,
                EnrollmentState::Active | EnrollmentState::RecoveryRequired
            ) || !matches!(
                s.borrow_state,
                BorrowState::NotBorrowed | BorrowState::Restored
            );
            let known_port = data.config.ports.iter().any(|p| p.id == s.port_id)
                || !requires_current_port
                    && data
                        .configurations
                        .iter()
                        .any(|c| c.config.ports.iter().any(|p| p.id == s.port_id));
            if s.id.0.is_nil()
                || !session_ids.insert(s.id)
                || s.cursor > s.queue.len()
                || s.queue.iter().any(|id| !ids.contains(id))
                || !known_port
                || matches!(
                    s.state,
                    EnrollmentState::Active | EnrollmentState::RecoveryRequired
                ) && !stations.insert(s.port_id.clone())
            {
                return Err(invalid("invalid_tracking_export", "sessions"));
            }
        }
        let mut versions = BTreeSet::new();
        for c in &data.configurations {
            if c.version < 0 || !versions.insert(c.version) {
                return Err(invalid("invalid_tracking_export", "configurations"));
            }
            crate::tracking::engine::validate_config(&c.config)?;
            if let Some(row) = sqlx::query(CONFIGURATION)
                .bind(c.version)
                .fetch_optional(&mut *conn)
                .await?
            {
                let existing: TrackingConfig = decode(&row.try_get::<String, _>("body")?)?;
                if existing != c.config {
                    return Err(TrackingError::Conflict("configuration_history_conflict"));
                }
            }
        }
        if data
            .history
            .iter()
            .any(|h| !versions.contains(&h.configuration_version))
            || !data
                .configurations
                .iter()
                .any(|c| c.version == data.configuration_version && c.config == data.config)
        {
            return Err(invalid("invalid_tracking_export", "configuration_version"));
        }
        for op in &data.operations {
            if !text(&op.actor, 200) || !text(&op.key, 200) || !fingerprint(&op.request_hash) {
                return Err(invalid("invalid_tracking_export", "operations"));
            }
            if let Some(row) = sqlx::query(OPERATIONS)
                .bind(&op.actor)
                .bind(&op.key)
                .fetch_optional(&mut *conn)
                .await?
                && (row.try_get::<String, _>("request_hash")? != op.request_hash
                    || decode::<CommandResult>(&row.try_get::<String, _>("response")?)?
                        .tracking_revision
                        != op.response.tracking_revision)
            {
                return Err(TrackingError::Conflict("operation_history_conflict"));
            }
        }
        for op in &data.reader_operations {
            validate_reader_operation(op, &data.config)?;
        }
        for op in &data.reader_controls {
            validate_control(&op.original)?;
            validate_control(&op.applied)?;
            if !data.config.readers.iter().any(|r| r.id == op.reader_id) {
                return Err(invalid("invalid_tracking_export", "reader_controls"));
            }
        }
        // Validate completely before destructive replacement. The operation log
        // and catalog are never erased by a tracking restore.
        for query in [
            DELETE_BINDINGS,
            DELETE_HISTORY,
            DELETE_SESSIONS,
            DELETE_UNITS,
            DELETE_INVENTORY,
            DELETE_READER_OPERATIONS,
            DELETE_READER_CONTROLS,
        ] {
            sqlx::query(query).execute(&mut *conn).await?;
        }
        for record in &data.inventory {
            let next_ordinal = data
                .units
                .iter()
                .filter(|u| u.unit.catalog_id == record.catalog_id)
                .map(|u| u.unit.ordinal)
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or(invalid("tracking_limit", "ordinal"))?;
            write_inventory(
                conn,
                &InventoryRecord {
                    view: record.clone(),
                    next_ordinal,
                },
            )
            .await?;
        }
        next.units.clear();
        for mut u in data.units {
            u.unit.fence_at = now;
            write_unit(conn, &u).await?;
            next.units.insert(u.unit.id, u);
        }
        for h in data.history {
            sqlx::query(IMPORT_HISTORY)
                .bind(h.id)
                .bind(h.unit_id.to_string())
                .bind(h.at.to_rfc3339())
                .bind(history_name(&h.kind))
                .bind(encode(&h)?)
                .execute(&mut *conn)
                .await?;
        }
        for b in data.bindings {
            sqlx::query(IMPORT_BINDING)
                .bind(b.id)
                .bind(b.unit_id.to_string())
                .bind(b.epc.as_str())
                .bind(b.bound_at.to_rfc3339())
                .bind(b.retired_at.map(|t| t.to_rfc3339()))
                .bind(b.retirement.map(|r| encode(&r)).transpose()?)
                .execute(&mut *conn)
                .await?;
        }
        for c in data.configurations {
            sqlx::query(INSERT_CONFIGURATION)
                .bind(c.version)
                .bind(c.at.to_rfc3339())
                .bind(encode(&c.config)?)
                .execute(&mut *conn)
                .await?;
        }
        next.configuration_version = next.configuration_version.max(data.configuration_version) + 1;
        next.config = data.config;
        sqlx::query(CONFIGURE)
            .bind(encode(&next.config)?)
            .bind(next.config.enabled)
            .bind(next.configuration_version)
            .execute(&mut *conn)
            .await?;
        sqlx::query(INSERT_CONFIGURATION)
            .bind(next.configuration_version)
            .bind(now.to_rfc3339())
            .bind(encode(&next.config)?)
            .execute(&mut *conn)
            .await?;
        next.sessions.clear();
        for mut s in data.sessions {
            reset_capture(&mut s, now);
            if !matches!(
                s.borrow_state,
                BorrowState::NotBorrowed | BorrowState::Restored
            ) {
                s.borrow_state = BorrowState::RestoreRequested;
                s.state = EnrollmentState::RecoveryRequired;
            }
            write_session(conn, &s).await?;
            next.sessions.insert(s.id, s);
        }
        for op in data.operations {
            if sqlx::query(OPERATIONS)
                .bind(&op.actor)
                .bind(&op.key)
                .fetch_optional(&mut *conn)
                .await?
                .is_none()
            {
                sqlx::query(INSERT_OPERATION)
                    .bind(op.actor)
                    .bind(op.key)
                    .bind(op.request_hash)
                    .bind(encode(&op.response)?)
                    .execute(&mut *conn)
                    .await?;
            }
        }
        next.reader_operations.clear();
        for mut op in data.reader_operations {
            op.configuration_version = next.configuration_version;
            sqlx::query(UPSERT_READER_OPERATION)
                .bind(op.reader_id.as_str())
                .bind(encode(&op)?)
                .execute(&mut *conn)
                .await?;
            next.reader_operations.insert(op.reader_id.clone(), op);
        }
        next.reader_controls.clear();
        for mut op in data.reader_controls {
            op.configuration_version = next.configuration_version;
            sqlx::query(UPSERT_READER_CONTROL)
                .bind(op.reader_id.as_str())
                .bind(encode(&op)?)
                .execute(&mut *conn)
                .await?;
            next.reader_controls.insert(op.reader_id.clone(), op);
        }
        next.reset_evidence(now);
        Ok(())
    }
    pub async fn tracking_reader_plan(
        &self,
        reader_id: &ReaderId,
        configuration_version: i64,
        desired_fingerprint: &str,
        observed_fingerprint: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<ReaderPlan> {
        self.tracking_reader_plan_clocked(
            reader_id,
            configuration_version,
            desired_fingerprint,
            observed_fingerprint,
            move || now,
        )
        .await
    }
    pub async fn tracking_reader_plan_clocked(
        &self,
        reader_id: &ReaderId,
        configuration_version: i64,
        desired_fingerprint: &str,
        observed_fingerprint: Option<&str>,
        clock: impl FnOnce() -> DateTime<Utc> + Send,
    ) -> Result<ReaderPlan> {
        if !fingerprint(desired_fingerprint)
            || observed_fingerprint.is_some_and(|f| !fingerprint(f))
        {
            return Err(invalid("invalid_reader_fingerprint", "fingerprint"));
        }
        let mut runtime = self.tracking.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = clock();
        load(&mut tx, &mut runtime, now, false).await?;
        if configuration_version != runtime.configuration_version {
            return Err(TrackingError::Conflict("configuration_changed"));
        }
        let owned_rospec_id = match runtime
            .config
            .readers
            .iter()
            .find(|r| &r.id == reader_id)
            .map(|r| &r.connection)
        {
            Some(ReaderConnection::ImpinjLlrp {
                owned_rospec_id, ..
            }) => *owned_rospec_id,
            _ => return Err(invalid("reader_not_llrp", "reader_id")),
        };
        let previous = runtime.reader_operations.get(reader_id).cloned();
        let mut op = previous.clone().unwrap_or(ReaderOperation {
            reader_id: reader_id.clone(),
            owned_rospec_id,
            configuration_version,
            pending_fingerprint: None,
            applied_fingerprint: None,
            known_fingerprints: Vec::new(),
            phase: ReaderOperationPhase::Pending,
            updated_at: now,
        });
        if op.owned_rospec_id != owned_rospec_id {
            if op.pending_fingerprint.is_some() || op.applied_fingerprint.is_some() {
                return Err(TrackingError::Conflict(
                    "previous_reader_program_not_released",
                ));
            }
            op.owned_rospec_id = owned_rospec_id;
            op.known_fingerprints.clear();
        }
        let action = match observed_fingerprint {
            Some(f)
                if f == desired_fingerprint
                    && op.known_fingerprints.iter().any(|known| known == f) =>
            {
                ReaderPlanAction::Keep
            }
            None => ReaderPlanAction::Create,
            Some(f) if op.known_fingerprints.iter().any(|known| known == f) => {
                ReaderPlanAction::Replace
            }
            Some(_) => ReaderPlanAction::Recover,
        };
        op.configuration_version = configuration_version;
        if action == ReaderPlanAction::Keep {
            op.phase = ReaderOperationPhase::Applied;
            op.applied_fingerprint = Some(desired_fingerprint.to_owned());
            op.pending_fingerprint = None;
        } else if action == ReaderPlanAction::Recover {
            op.phase = ReaderOperationPhase::RecoveryRequired;
        } else {
            op.phase = ReaderOperationPhase::Pending;
            op.pending_fingerprint = Some(desired_fingerprint.to_owned());
        }
        if action != ReaderPlanAction::Recover
            && !op
                .known_fingerprints
                .iter()
                .any(|f| f == desired_fingerprint)
        {
            op.known_fingerprints.push(desired_fingerprint.to_owned());
            while op.known_fingerprints.len() > 16 {
                let removable = op
                    .known_fingerprints
                    .iter()
                    .position(|f| {
                        Some(f) != op.applied_fingerprint.as_ref()
                            && Some(f) != op.pending_fingerprint.as_ref()
                    })
                    .ok_or(TrackingError::Conflict("reader_operation_history_full"))?;
                op.known_fingerprints.remove(removable);
            }
        }
        let semantic_change = previous.as_ref().is_none_or(|p| {
            p.owned_rospec_id != op.owned_rospec_id
                || p.configuration_version != op.configuration_version
                || p.phase != op.phase
                || p.pending_fingerprint != op.pending_fingerprint
                || p.applied_fingerprint != op.applied_fingerprint
                || p.known_fingerprints != op.known_fingerprints
        });
        if semantic_change {
            op.updated_at = now;
            sqlx::query(UPSERT_READER_OPERATION)
                .bind(reader_id.as_str())
                .bind(encode(&op)?)
                .execute(&mut *tx)
                .await?;
            runtime.revision = sqlx::query_scalar(BUMP).fetch_one(&mut *tx).await?;
            runtime
                .reader_operations
                .insert(reader_id.clone(), op.clone());
        } else if let Some(previous) = previous {
            op.updated_at = previous.updated_at;
        }
        tx.commit().await?;
        Ok(ReaderPlan {
            operation: op,
            action,
        })
    }
    pub async fn tracking_reader_applied(
        &self,
        reader_id: &ReaderId,
        configuration_version: i64,
        fingerprint: &str,
        now: DateTime<Utc>,
    ) -> Result<ReaderOperation> {
        self.tracking_reader_applied_clocked(
            reader_id,
            configuration_version,
            fingerprint,
            move || now,
        )
        .await
    }
    pub async fn tracking_reader_applied_clocked(
        &self,
        reader_id: &ReaderId,
        configuration_version: i64,
        fingerprint: &str,
        clock: impl FnOnce() -> DateTime<Utc> + Send,
    ) -> Result<ReaderOperation> {
        let mut runtime = self.tracking.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = clock();
        load(&mut tx, &mut runtime, now, false).await?;
        let mut op = runtime
            .reader_operations
            .get(reader_id)
            .cloned()
            .ok_or(TrackingError::NotFound)?;
        if op.configuration_version != configuration_version
            || op.pending_fingerprint.as_deref() != Some(fingerprint)
        {
            if op.configuration_version == configuration_version
                && op.applied_fingerprint.as_deref() == Some(fingerprint)
                && op.phase == ReaderOperationPhase::Applied
            {
                tx.commit().await?;
                return Ok(op);
            }
            return Err(TrackingError::Conflict("reader_operation_changed"));
        }
        op.applied_fingerprint = Some(fingerprint.to_owned());
        op.pending_fingerprint = None;
        op.phase = ReaderOperationPhase::Applied;
        op.updated_at = now;
        sqlx::query(UPSERT_READER_OPERATION)
            .bind(reader_id.as_str())
            .bind(encode(&op)?)
            .execute(&mut *tx)
            .await?;
        runtime.revision = sqlx::query_scalar(BUMP).fetch_one(&mut *tx).await?;
        runtime
            .reader_operations
            .insert(reader_id.clone(), op.clone());
        tx.commit().await?;
        Ok(op)
    }
    pub async fn tracking_reader_released(
        &self,
        reader_id: &ReaderId,
        owned_rospec_id: u32,
        fingerprint: &str,
        now: DateTime<Utc>,
    ) -> Result<ReaderOperation> {
        self.tracking_reader_released_clocked(reader_id, owned_rospec_id, fingerprint, move || now)
            .await
    }
    pub async fn tracking_reader_released_clocked(
        &self,
        reader_id: &ReaderId,
        owned_rospec_id: u32,
        fingerprint: &str,
        clock: impl FnOnce() -> DateTime<Utc> + Send,
    ) -> Result<ReaderOperation> {
        let mut runtime = self.tracking.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = clock();
        load(&mut tx, &mut runtime, now, false).await?;
        let mut op = runtime
            .reader_operations
            .get(reader_id)
            .cloned()
            .ok_or(TrackingError::NotFound)?;
        if op.owned_rospec_id == owned_rospec_id
            && op.applied_fingerprint.is_none()
            && op.pending_fingerprint.is_none()
            && op.known_fingerprints.is_empty()
        {
            tx.commit().await?;
            return Ok(op);
        }
        if op.owned_rospec_id != owned_rospec_id
            || !op.known_fingerprints.iter().any(|f| f == fingerprint)
        {
            return Err(TrackingError::Conflict("reader_operation_changed"));
        }
        if op.requires_cleanup() {
            op.applied_fingerprint = None;
            op.pending_fingerprint = None;
            op.known_fingerprints.clear();
            op.phase = ReaderOperationPhase::Pending;
            op.updated_at = now;
            sqlx::query(UPSERT_READER_OPERATION)
                .bind(reader_id.as_str())
                .bind(encode(&op)?)
                .execute(&mut *tx)
                .await?;
            runtime.revision = sqlx::query_scalar(BUMP).fetch_one(&mut *tx).await?;
            runtime
                .reader_operations
                .insert(reader_id.clone(), op.clone());
        }
        tx.commit().await?;
        Ok(op)
    }
    pub async fn tracking_reader_plan_control(
        &self,
        reader_id: &ReaderId,
        configuration_version: i64,
        current: &LlrpControlSettings,
        desired: &LlrpControlSettings,
        now: DateTime<Utc>,
    ) -> Result<ReaderControlPlan> {
        self.tracking_reader_plan_control_clocked(
            reader_id,
            configuration_version,
            current,
            desired,
            move || now,
        )
        .await
    }
    pub async fn tracking_reader_plan_control_clocked(
        &self,
        reader_id: &ReaderId,
        configuration_version: i64,
        current: &LlrpControlSettings,
        desired: &LlrpControlSettings,
        clock: impl FnOnce() -> DateTime<Utc> + Send,
    ) -> Result<ReaderControlPlan> {
        validate_control(current)?;
        validate_control(desired)?;
        let mut runtime = self.tracking.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = clock();
        load(&mut tx, &mut runtime, now, false).await?;
        if runtime.configuration_version != configuration_version {
            return Err(TrackingError::Conflict("configuration_changed"));
        }
        if !runtime.config.readers.iter().any(|r| {
            &r.id == reader_id && matches!(r.connection, ReaderConnection::ImpinjLlrp { .. })
        }) {
            return Err(invalid("reader_not_llrp", "reader_id"));
        }
        let previous = runtime.reader_controls.get(reader_id).cloned();
        let original = previous
            .as_ref()
            .filter(|p| p.phase != ReaderControlPhase::Restored)
            .map_or_else(|| current.clone(), |p| p.original.clone());
        let known = previous.as_ref().is_none_or(|p| {
            p.phase == ReaderControlPhase::Restored
                || *current == p.applied
                || *current == p.original
        });
        let action = if !known {
            ReaderPlanAction::Recover
        } else if current == desired {
            ReaderPlanAction::Keep
        } else if previous.is_some() {
            ReaderPlanAction::Replace
        } else {
            ReaderPlanAction::Create
        };
        let mut op = ReaderControlOperation {
            reader_id: reader_id.clone(),
            configuration_version,
            original,
            applied: if action == ReaderPlanAction::Recover {
                previous
                    .as_ref()
                    .map_or_else(|| desired.clone(), |p| p.applied.clone())
            } else {
                desired.clone()
            },
            phase: if action == ReaderPlanAction::Recover {
                ReaderControlPhase::RecoveryRequired
            } else if action == ReaderPlanAction::Keep {
                ReaderControlPhase::Applied
            } else {
                ReaderControlPhase::Pending
            },
            updated_at: now,
        };
        if let Some(p) = &previous
            && p.phase == ReaderControlPhase::RestorePending
            && current != &p.original
        {
            return Err(TrackingError::Conflict("reader_restore_pending"));
        }
        let changed = previous.as_ref().is_none_or(|p| {
            p.configuration_version != op.configuration_version
                || p.original != op.original
                || p.applied != op.applied
                || p.phase != op.phase
        });
        if changed {
            sqlx::query(UPSERT_READER_CONTROL)
                .bind(reader_id.as_str())
                .bind(encode(&op)?)
                .execute(&mut *tx)
                .await?;
            runtime.revision = sqlx::query_scalar(BUMP).fetch_one(&mut *tx).await?;
            runtime
                .reader_controls
                .insert(reader_id.clone(), op.clone());
        } else if let Some(p) = previous {
            op.updated_at = p.updated_at;
        }
        tx.commit().await?;
        Ok(ReaderControlPlan {
            target: desired.clone(),
            operation: op,
            action,
        })
    }
    pub async fn tracking_reader_control_applied(
        &self,
        reader_id: &ReaderId,
        configuration_version: i64,
        observed: &LlrpControlSettings,
        restoring: bool,
        now: DateTime<Utc>,
    ) -> Result<ReaderControlOperation> {
        self.tracking_reader_control_applied_clocked(
            reader_id,
            configuration_version,
            observed,
            restoring,
            move || now,
        )
        .await
    }
    pub async fn tracking_reader_control_applied_clocked(
        &self,
        reader_id: &ReaderId,
        configuration_version: i64,
        observed: &LlrpControlSettings,
        restoring: bool,
        clock: impl FnOnce() -> DateTime<Utc> + Send,
    ) -> Result<ReaderControlOperation> {
        validate_control(observed)?;
        let mut runtime = self.tracking.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = clock();
        load(&mut tx, &mut runtime, now, false).await?;
        let mut op = runtime
            .reader_controls
            .get(reader_id)
            .cloned()
            .ok_or(TrackingError::NotFound)?;
        if op.configuration_version != configuration_version
            || if restoring {
                observed != &op.original
            } else {
                observed != &op.applied
            }
        {
            return Err(TrackingError::Conflict("reader_operation_changed"));
        }
        let phase = if restoring {
            ReaderControlPhase::Restored
        } else {
            ReaderControlPhase::Applied
        };
        if op.phase != phase {
            if restoring && op.phase != ReaderControlPhase::RestorePending
                || !restoring && op.phase != ReaderControlPhase::Pending
            {
                return Err(TrackingError::Conflict("reader_operation_changed"));
            }
            op.phase = phase;
            op.updated_at = now;
            sqlx::query(UPSERT_READER_CONTROL)
                .bind(reader_id.as_str())
                .bind(encode(&op)?)
                .execute(&mut *tx)
                .await?;
            runtime.revision = sqlx::query_scalar(BUMP).fetch_one(&mut *tx).await?;
            runtime
                .reader_controls
                .insert(reader_id.clone(), op.clone());
        }
        tx.commit().await?;
        Ok(op)
    }
    pub async fn tracking_reader_restore_control(
        &self,
        reader_id: &ReaderId,
        current: &LlrpControlSettings,
        now: DateTime<Utc>,
    ) -> Result<ReaderControlPlan> {
        self.tracking_reader_restore_control_clocked(reader_id, current, move || now)
            .await
    }
    pub async fn tracking_reader_restore_control_clocked(
        &self,
        reader_id: &ReaderId,
        current: &LlrpControlSettings,
        clock: impl FnOnce() -> DateTime<Utc> + Send,
    ) -> Result<ReaderControlPlan> {
        validate_control(current)?;
        let mut runtime = self.tracking.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = clock();
        load(&mut tx, &mut runtime, now, false).await?;
        let mut op = runtime
            .reader_controls
            .get(reader_id)
            .cloned()
            .ok_or(TrackingError::NotFound)?;
        let action = if current == &op.original {
            ReaderPlanAction::Keep
        } else if current == &op.applied {
            ReaderPlanAction::Replace
        } else {
            ReaderPlanAction::Recover
        };
        let phase = if action == ReaderPlanAction::Keep {
            ReaderControlPhase::Restored
        } else if action == ReaderPlanAction::Replace {
            ReaderControlPhase::RestorePending
        } else {
            ReaderControlPhase::RecoveryRequired
        };
        if op.phase != phase {
            op.phase = phase;
            op.updated_at = now;
            sqlx::query(UPSERT_READER_CONTROL)
                .bind(reader_id.as_str())
                .bind(encode(&op)?)
                .execute(&mut *tx)
                .await?;
            runtime.revision = sqlx::query_scalar(BUMP).fetch_one(&mut *tx).await?;
            runtime
                .reader_controls
                .insert(reader_id.clone(), op.clone());
        }
        let target = op.original.clone();
        tx.commit().await?;
        Ok(ReaderControlPlan {
            operation: op,
            action,
            target,
        })
    }
}

async fn paged(
    conn: &mut SqliteConnection,
    query: &'static str,
    key: &'static str,
) -> Result<Vec<sqlx::sqlite::SqliteRow>> {
    let mut cursor = String::new();
    let mut result = Vec::new();
    loop {
        let rows = sqlx::query(query)
            .bind(&cursor)
            .fetch_all(&mut *conn)
            .await?;
        let count = rows.len();
        if let Some(last) = rows.last() {
            let next: String = last.try_get(key)?;
            if next <= cursor {
                return Err(invalid("stored_data_invalid", "cursor"));
            }
            cursor = next;
        }
        result.extend(rows);
        if count < 500 {
            break;
        }
    }
    Ok(result)
}
fn body<T: serde::de::DeserializeOwned>(row: &sqlx::sqlite::SqliteRow) -> Result<T> {
    decode(&row.try_get::<String, _>("body")?)
}
fn parse_time(value: &str) -> Result<DateTime<Utc>> {
    value
        .parse()
        .map_err(|_| invalid("stored_data_invalid", "timestamp"))
}
fn active_unit(engine: &Engine, id: UnitId) -> Result<StoredUnit> {
    let unit = engine
        .units
        .get(&id)
        .cloned()
        .ok_or(TrackingError::NotFound)?;
    if unit.unit.retired || unit.unit.archived {
        return Err(TrackingError::Conflict("unit_inactive"));
    }
    Ok(unit)
}
fn validate_queue(engine: &Engine, ids: &[UnitId], purpose: EnrollmentPurpose) -> Result<()> {
    if purpose == EnrollmentPurpose::Lookup {
        return if ids.is_empty() {
            Ok(())
        } else {
            Err(invalid("lookup_queue_must_be_empty", "unit_ids"))
        };
    }
    if ids.is_empty() || ids.len() > 5_000 {
        return Err(invalid("invalid_enrollment_queue", "unit_ids"));
    }
    let mut seen = BTreeSet::new();
    for id in ids {
        if !seen.insert(*id) {
            return Err(invalid("duplicate_unit", "unit_ids"));
        }
        let unit = active_unit(engine, *id)?;
        let record = engine
            .inventory
            .get(&unit.unit.catalog_id)
            .ok_or(TrackingError::NotFound)?;
        if record.needs_reconciliation {
            return Err(TrackingError::Conflict("quantity_reconciliation_required"));
        }
        if record
            .catalog_quantity
            .or(record.declared_quantity)
            .is_none()
        {
            return Err(TrackingError::Conflict("quantity_unknown"));
        }
        if unit.unit.installation.state == InstallationState::Exempt {
            return Err(TrackingError::Conflict("unit_tracking_exempt"));
        }
        if purpose == EnrollmentPurpose::Bind && unit.epc.is_some()
            || purpose == EnrollmentPurpose::Replace && unit.epc.is_none()
        {
            return Err(TrackingError::Conflict("binding_changed"));
        }
    }
    Ok(())
}
fn validate_station(engine: &Engine, port: &PortId, borrow: bool) -> Result<()> {
    validate_station_except(engine, port, borrow, None)
}
fn validate_station_except(
    engine: &Engine,
    port: &PortId,
    borrow: bool,
    except: Option<SessionId>,
) -> Result<()> {
    let config = engine
        .config
        .ports
        .iter()
        .find(|p| &p.id == port && p.enabled)
        .ok_or(invalid("unknown_enrollment_station", "port_id"))?;
    if !engine
        .config
        .readers
        .iter()
        .any(|r| r.id == config.reader_id && r.enabled)
    {
        return Err(TrackingError::Conflict("reader_disabled"));
    }
    if engine.sessions.values().any(|s| {
        Some(s.id) != except
            && &s.port_id == port
            && (matches!(
                s.state,
                EnrollmentState::Active | EnrollmentState::RecoveryRequired
            ) || !matches!(
                s.borrow_state,
                BorrowState::NotBorrowed | BorrowState::Restored
            ))
    }) {
        return Err(TrackingError::Conflict("enrollment_station_busy"));
    }
    if borrow {
        if config.role != PortRole::Monitor
            || !engine
                .readbacks
                .get(&config.reader_id)
                .is_some_and(|r| r.capabilities.inventory_pause)
        {
            return Err(TrackingError::Conflict("borrow_unsupported"));
        }
    } else if config.role != PortRole::Enrollment {
        return Err(TrackingError::Conflict(
            "enrollment_requires_explicit_borrow",
        ));
    }
    Ok(())
}
fn session_mut(
    engine: &mut Engine,
    id: SessionId,
    now: DateTime<Utc>,
) -> Result<&mut EnrollmentSession> {
    let session = engine
        .sessions
        .get_mut(&id)
        .ok_or(TrackingError::NotFound)?;
    if session.state != EnrollmentState::Active || session.expires_at <= now {
        return Err(TrackingError::Conflict("enrollment_expired"));
    }
    Ok(session)
}
fn reset_capture(session: &mut EnrollmentSession, now: DateTime<Utc>) {
    session.candidates.clear();
    session.capture_overflow = false;
    session.capture_started_at = now;
    session.capture_version = session.capture_version.saturating_add(1);
}
fn advance_session(session: &mut EnrollmentSession, now: DateTime<Utc>) {
    session.cursor += 1;
    reset_capture(session, now);
    if session.cursor == session.queue.len() {
        session.state = EnrollmentState::Complete;
        if !matches!(
            session.borrow_state,
            BorrowState::NotBorrowed | BorrowState::Restored
        ) {
            session.borrow_state = BorrowState::RestoreRequested;
        }
    }
}
fn validate_selection(
    session: &EnrollmentSession,
    version: u32,
    id: UnitId,
    now: DateTime<Utc>,
) -> Result<()> {
    if session.state != EnrollmentState::Active || session.expires_at <= now {
        return Err(TrackingError::Conflict("enrollment_expired"));
    }
    if session.capture_version != version || session.queue.get(session.cursor) != Some(&id) {
        return Err(TrackingError::Conflict("enrollment_selection_changed"));
    }
    Ok(())
}
fn fingerprint(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn validate_reader_operation(op: &ReaderOperation, config: &TrackingConfig) -> Result<()> {
    if op.owned_rospec_id==0||op.known_fingerprints.len()>16||op.known_fingerprints.iter().any(|f|!fingerprint(f))||op.pending_fingerprint.as_ref().is_some_and(|f|!fingerprint(f))||op.applied_fingerprint.as_ref().is_some_and(|f|!fingerprint(f))||!config.readers.iter().any(|r|r.id==op.reader_id&&matches!(&r.connection,ReaderConnection::ImpinjLlrp{owned_rospec_id,..}if *owned_rospec_id==op.owned_rospec_id)){return Err(invalid("invalid_tracking_export","reader_operations"));}
    Ok(())
}
fn validate_control(settings: &LlrpControlSettings) -> Result<()> {
    let mut types = BTreeSet::new();
    if settings.notifications.len() > 64
        || settings
            .notifications
            .iter()
            .any(|n| !types.insert(n.event_type))
        || settings.keepalive.trigger == KeepaliveTrigger::Periodic
            && !(100..=600_000).contains(&settings.keepalive.interval_ms)
    {
        return Err(invalid("invalid_reader_control", "control"));
    }
    Ok(())
}
async fn create_missing_units(
    conn: &mut SqliteConnection,
    id: &str,
    target: u32,
    now: DateTime<Utc>,
    actor: &str,
) -> Result<ReconcileResult> {
    let mut records = inventory(conn).await?;
    let record = records.get_mut(id).ok_or(TrackingError::NotFound)?;
    let rows = paged(conn, UNITS, "id").await?;
    let units = rows
        .iter()
        .map(body::<StoredUnit>)
        .collect::<Result<Vec<_>>>()?;
    let active = units
        .iter()
        .filter(|u| u.unit.catalog_id == id && !u.unit.retired)
        .count() as u32;
    let requested = target.saturating_sub(active);
    let active_total = units
        .iter()
        .filter(|u| !u.unit.retired && !u.unit.archived)
        .count();
    let count = requested.min(MAX_UNITS.saturating_sub(active_total) as u32);
    let before_capacity = record.view.capacity_limited;
    let before_reconciliation = record.view.needs_reconciliation;
    record.view.capacity_limited = requested > count;
    record.view.needs_reconciliation = target != active.saturating_add(count);
    let version: i64 = sqlx::query(STATE)
        .fetch_one(&mut *conn)
        .await?
        .try_get("configuration_version")?;
    let mut ids = Vec::new();
    for _ in 0..count {
        let mut unit = new_unit(
            UnitId::new(),
            id.to_owned(),
            record.next_ordinal,
            record.view.archived,
            now,
        );
        record.next_ordinal = record
            .next_ordinal
            .checked_add(1)
            .ok_or(invalid("tracking_limit", "ordinal"))?;
        write_unit(conn, &unit).await?;
        let mut change = UnitChange::new(
            None,
            unit,
            HistoryKind::QuantityReconciliation,
            DecisionReason::QuantityReconciliation,
            None,
            Vec::new(),
        );
        event(
            conn,
            &mut change,
            EventContext {
                actor,
                automatic: false,
                configuration_version: version,
                at: now,
            },
        )
        .await?;
        unit = change.after;
        ids.push(unit.unit.id);
    }
    let changed = count > 0
        || before_capacity != record.view.capacity_limited
        || before_reconciliation != record.view.needs_reconciliation;
    if changed {
        write_inventory(conn, record).await?;
    }
    Ok(ReconcileResult {
        changed,
        created_units: ids,
    })
}
const CONFIGURATIONS: &str =
    "SELECT version,at,body FROM tracking_configurations ORDER BY version LIMIT 10001";
const CONFIGURATION: &str = "SELECT body FROM tracking_configurations WHERE version=?";
const INSERT_CONFIGURATION: &str =
    "INSERT OR IGNORE INTO tracking_configurations(version,at,body) VALUES(?,?,?)";
const DELETE_BINDINGS: &str = "DELETE FROM tracking_bindings";
const DELETE_HISTORY: &str = "DELETE FROM tracking_history";
const DELETE_SESSIONS: &str = "DELETE FROM tracking_sessions";
const DELETE_UNITS: &str = "DELETE FROM tracking_units";
const DELETE_INVENTORY: &str = "DELETE FROM tracking_inventory";
const DELETE_READER_OPERATIONS: &str = "DELETE FROM tracking_reader_operations";
const DELETE_READER_CONTROLS: &str = "DELETE FROM tracking_reader_controls";
const IMPORT_HISTORY: &str =
    "INSERT INTO tracking_history(id,unit_id,at,kind,body) VALUES(?,?,?,?,?)";
const IMPORT_BINDING: &str = "INSERT INTO tracking_bindings(id,unit_id,epc,bound_at,retired_at,retirement) VALUES(?,?,?,?,?,?)";

const AVAILABILITY: &str = r#"
SELECT i.id,
  EXISTS(SELECT 1 FROM tracking_units u
    WHERE u.catalog_id=i.id AND u.retired=0 AND u.archived=0
      AND u.baseline_at IS NOT NULL
      AND u.status IN ('clean_ready','worn_ready')) AS ready,
  (NOT EXISTS(SELECT 1 FROM tracking_units u
      WHERE u.catalog_id=i.id AND u.retired=0 AND u.archived=0)
   OR EXISTS(SELECT 1 FROM tracking_units u
      WHERE u.catalog_id=i.id AND u.retired=0 AND u.archived=0
        AND (u.baseline_at IS NULL
          OR (u.installation_state='exempt' AND u.evidence!='manual')
          OR (u.epc IS NULL AND u.evidence!='manual')))
   OR EXISTS(SELECT 1 FROM tracking_inventory t
      WHERE t.catalog_id=i.id AND t.capacity_limited=1)) AS fallback
FROM items i WHERE i.archived=0 ORDER BY i.id
"#;

/// Existing-transaction variant for Today publication on the one SQLite pool.
pub async fn availability(conn: &mut SqliteConnection) -> Result<Availability> {
    let revision: i64 = sqlx::query(STATE)
        .fetch_one(&mut *conn)
        .await?
        .try_get("revision")?;
    let rows = sqlx::query(AVAILABILITY).fetch_all(conn).await?;
    let mut eligible = Vec::new();
    let mut certified = Vec::new();
    for row in rows {
        let id: String = row.try_get("id")?;
        let ready: bool = row.try_get("ready")?;
        let fallback: bool = row.try_get("fallback")?;
        if ready || fallback {
            eligible.push(id.clone());
        }
        if ready {
            certified.push(id);
        }
    }
    Ok(Availability {
        tracking_revision: revision,
        eligible_catalog_ids: eligible,
        certified_ready_catalog_ids: certified,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use chrono::{DateTime, Utc};
    use serde::Serialize;
    use sqlx::{Execute, QueryBuilder, Row, Sqlite};

    use crate::models::{CatalogItem, SyncRequest};
    use crate::store::Store;

    use super::*;

    fn fixture(number: usize) -> CatalogItem {
        CatalogItem {
            id: format!("fixture-{number:04}"),
            brand: Some("Example studio".into()),
            name: format!("Fictional garment {number}"),
            category: "clothing.top.shirt".into(),
            color: Some("Blue".into()),
            size: Some("M".into()),
            quantity: Some(1),
            material: Some("Cotton".into()),
            sku: None,
            purchase_source: Some("fixture".into()),
            images: Vec::new(),
            notes: Vec::new(),
            price: None,
            shop: None,
        }
    }
    fn identifier<T: std::str::FromStr>(s: &str) -> T
    where
        T::Err: std::fmt::Debug,
    {
        s.parse().unwrap()
    }

    #[derive(Serialize)]
    struct Plan {
        name: &'static str,
        details: Vec<String>,
        opcodes: usize,
    }

    #[tokio::test]
    async fn actual_tracking_queries_use_native_sqlite_plans_on_active_and_archived_history() {
        let now = DateTime::<Utc>::UNIX_EPOCH;
        let store = Store::open_simulator(now).await.unwrap();
        for phase in 0..2 {
            store
                .sync_at(
                    SyncRequest {
                        items: (phase * 1_000..(phase + 1) * 1_000).map(fixture).collect(),
                        dry_run: false,
                        allow_empty: false,
                    },
                    Some(phase as i64),
                    "fixture",
                    Some(&format!("seed-{phase}")),
                    now,
                )
                .await
                .unwrap();
        }
        let mut cfg = TrackingConfig {
            enabled: true,
            ..TrackingConfig::default()
        };
        cfg.areas.push(AreaConfig {
            id: identifier("closet"),
            label: "Fictional closet".into(),
            kind: AreaKind::Closet,
            enabled: true,
            thresholds: AreaThresholds::default(),
        });
        cfg.readers.push(ReaderConfig {
            id: identifier("reader"),
            label: "Fictional reader".into(),
            enabled: true,
            connection: ReaderConnection::ImpinjLlrp {
                host: "reader.example.invalid".into(),
                port: 5084,
                owned_rospec_id: 47,
            },
        });
        cfg.ports.push(PortConfig {
            id: identifier("station"),
            reader_id: identifier("reader"),
            antenna: 1,
            area_id: None,
            role: PortRole::Enrollment,
            enabled: true,
            required_for_absence: false,
            commissioned: false,
            transmit_power_cdbm: None,
            dwell_ms: None,
        });
        let revision = store
            .tracking_snapshot(now)
            .await
            .unwrap()
            .tracking_revision;
        store
            .tracking_command(
                "fixture-operator",
                "configure",
                revision,
                TrackingCommand::Configure {
                    config: cfg.clone(),
                },
                now,
            )
            .await
            .unwrap();
        let reader = identifier("reader");
        let operation = store
            .tracking_reader_plan(&reader, 1, &"a".repeat(64), None, now)
            .await
            .unwrap()
            .operation;
        let control = LlrpControlSettings {
            notifications: vec![LlrpEventSetting {
                event_type: 2,
                enabled: false,
            }],
            keepalive: LlrpKeepalive {
                trigger: KeepaliveTrigger::None,
                interval_ms: 0,
            },
            hold_events_and_reports_upon_reconnect: false,
        };
        let control_operation = store
            .tracking_reader_plan_control(&reader, 1, &control, &control, now)
            .await
            .unwrap()
            .operation;
        // The setup uses the actual write/event helpers. Every active unit is
        // bound, half ready and half nonready, plus1,000 archived identities.
        let mut tx = store.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        let rows = paged(&mut tx, UNITS, "id").await.unwrap();
        let mut first = None;
        for (n, row) in rows.iter().enumerate() {
            let before: StoredUnit = body(row).unwrap();
            if before.unit.archived {
                continue;
            }
            let mut after = before.clone();
            after.epc = Some(identifier(&format!("{n:024X}")));
            after.unit.has_binding = true;
            after.unit.installation.state = InstallationState::Installed;
            after.unit.baseline_at = Some(now);
            after.unit.evidence = StateEvidence::Rfid;
            after.unit.exception = None;
            after.unit.status = if n % 2 == 0 {
                UnitStatus::CleanReady
            } else {
                UnitStatus::BeingWorn
            };
            after.unit.last_confirmed_area = Some(identifier("closet"));
            after.last_confirmed_kind = Some(AreaKind::Closet);
            let mut change = UnitChange::new(
                Some(before),
                after,
                HistoryKind::Bind,
                DecisionReason::TagBound,
                None,
                Vec::new(),
            );
            event(
                &mut tx,
                &mut change,
                EventContext {
                    actor: "fixture-operator",
                    automatic: false,
                    configuration_version: 1,
                    at: now,
                },
            )
            .await
            .unwrap();
            if first.is_none() {
                first = Some(change.after);
            }
        }
        sqlx::query(BUMP).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        sqlx::query("ANALYZE").execute(&store.pool).await.unwrap();
        let unit = first.unwrap();
        let id = unit.unit.id.to_string();
        let timestamp = now.to_rfc3339();
        let encoded = encode(&unit).unwrap();
        let session = EnrollmentSession {
            id: SessionId::new(),
            purpose: EnrollmentPurpose::Lookup,
            port_id: identifier("station"),
            queue: Vec::new(),
            cursor: 0,
            state: EnrollmentState::Active,
            capture_version: 1,
            capture_started_at: now,
            expires_at: now + chrono::Duration::seconds(600),
            borrow_state: BorrowState::NotBorrowed,
            candidates: Vec::new(),
            capture_overflow: false,
        };
        let response = CommandResult {
            tracking_revision: 1,
            changed: true,
            unit_ids: vec![unit.unit.id],
            session: None,
            event_ids: vec![1],
        };
        let history: HistoryEvent = body(
            &sqlx::query(HISTORY_EVENT)
                .bind(unit.unit.last_event_id)
                .fetch_one(&store.pool)
                .await
                .unwrap(),
        )
        .unwrap();
        let queries = [
            ("state", sqlx::query(STATE)),
            (
                "inventory page",
                sqlx::query(INVENTORY).bind("fixture-0800"),
            ),
            ("units page", sqlx::query(UNITS).bind(&id)),
            ("sessions", sqlx::query(SESSIONS)),
            (
                "operation replay",
                sqlx::query(OPERATIONS)
                    .bind("fixture-operator")
                    .bind("configure"),
            ),
            (
                "insert operation",
                sqlx::query(INSERT_OPERATION)
                    .bind("fixture-operator")
                    .bind("new-command")
                    .bind("a".repeat(64))
                    .bind(encode(&response).unwrap()),
            ),
            ("revision bump", sqlx::query(BUMP)),
            (
                "configure",
                sqlx::query(CONFIGURE)
                    .bind(encode(&cfg).unwrap())
                    .bind(true)
                    .bind(2_i64),
            ),
            (
                "inventory upsert",
                sqlx::query(UPSERT_INVENTORY)
                    .bind(&unit.unit.catalog_id)
                    .bind(1_i64)
                    .bind(Option::<i64>::None)
                    .bind(2_i64)
                    .bind(false)
                    .bind(false)
                    .bind(false),
            ),
            (
                "unit upsert",
                sqlx::query(UPSERT_UNIT)
                    .bind(&id)
                    .bind(&unit.unit.catalog_id)
                    .bind(unit.unit.ordinal)
                    .bind(unit.epc.as_ref().map(Epc::as_str))
                    .bind(unit.unit.status.as_str())
                    .bind("closet")
                    .bind(Option::<String>::None)
                    .bind(&timestamp)
                    .bind("rfid")
                    .bind("installed")
                    .bind(&timestamp)
                    .bind(false)
                    .bind(false)
                    .bind(false)
                    .bind(Option::<String>::None)
                    .bind(&timestamp)
                    .bind(unit.unit.last_event_id)
                    .bind(&encoded),
            ),
            (
                "session upsert",
                sqlx::query(UPSERT_SESSION)
                    .bind(session.id.to_string())
                    .bind(session.port_id.as_str())
                    .bind("active")
                    .bind(1_i64)
                    .bind(encode(&session).unwrap()),
            ),
            (
                "insert history",
                sqlx::query(INSERT_HISTORY)
                    .bind(&id)
                    .bind(&timestamp)
                    .bind("bind")
                    .bind(encode(&history).unwrap()),
            ),
            (
                "update history",
                sqlx::query(UPDATE_HISTORY)
                    .bind(encode(&history).unwrap())
                    .bind(history.id),
            ),
            ("history event", sqlx::query(HISTORY_EVENT).bind(history.id)),
            (
                "unit history",
                sqlx::query(HISTORY_UNIT)
                    .bind(&id)
                    .bind(i64::MAX)
                    .bind(50_i64),
            ),
            (
                "global history",
                sqlx::query(HISTORY_ALL).bind(i64::MAX).bind(50_i64),
            ),
            ("export history", sqlx::query(EXPORT_HISTORY)),
            ("export operations", sqlx::query(EXPORT_OPERATIONS)),
            ("export bindings", sqlx::query(BINDINGS)),
            (
                "retire binding",
                sqlx::query(RETIRE_BINDING)
                    .bind(&timestamp)
                    .bind(encode(&BindingRetirement::Replaced).unwrap())
                    .bind(&id),
            ),
            (
                "insert binding",
                sqlx::query(INSERT_BINDING)
                    .bind(&id)
                    .bind("00000000000000000000FFFF")
                    .bind(&timestamp),
            ),
            ("catalog page", sqlx::query(CATALOG).bind("fixture-0800")),
            ("reader operations", sqlx::query(READER_OPERATIONS)),
            ("reader controls", sqlx::query(READER_CONTROLS)),
            (
                "reader operation upsert",
                sqlx::query(UPSERT_READER_OPERATION)
                    .bind(reader.as_str())
                    .bind(encode(&operation).unwrap()),
            ),
            (
                "reader control upsert",
                sqlx::query(UPSERT_READER_CONTROL)
                    .bind(reader.as_str())
                    .bind(encode(&control_operation).unwrap()),
            ),
            (
                "declare runtime quantity",
                sqlx::query(DECLARE_QUANTITY)
                    .bind(2_i64)
                    .bind(&unit.unit.catalog_id),
            ),
            ("configuration archive", sqlx::query(CONFIGURATIONS)),
            (
                "configuration lookup",
                sqlx::query(CONFIGURATION).bind(1_i64),
            ),
            (
                "configuration insert",
                sqlx::query(INSERT_CONFIGURATION)
                    .bind(2_i64)
                    .bind(&timestamp)
                    .bind(encode(&cfg).unwrap()),
            ),
            ("restore delete bindings", sqlx::query(DELETE_BINDINGS)),
            ("restore delete history", sqlx::query(DELETE_HISTORY)),
            ("restore delete sessions", sqlx::query(DELETE_SESSIONS)),
            ("restore delete units", sqlx::query(DELETE_UNITS)),
            ("restore delete inventory", sqlx::query(DELETE_INVENTORY)),
            (
                "restore delete reader operations",
                sqlx::query(DELETE_READER_OPERATIONS),
            ),
            (
                "restore delete reader controls",
                sqlx::query(DELETE_READER_CONTROLS),
            ),
            (
                "restore history",
                sqlx::query(IMPORT_HISTORY)
                    .bind(history.id)
                    .bind(&id)
                    .bind(&timestamp)
                    .bind("bind")
                    .bind(encode(&history).unwrap()),
            ),
            (
                "restore binding",
                sqlx::query(IMPORT_BINDING)
                    .bind(1_i64)
                    .bind(&id)
                    .bind("00000000000000000000FFFF")
                    .bind(&timestamp)
                    .bind(Option::<String>::None)
                    .bind(Option::<String>::None),
            ),
            (
                "closed reader operation delete",
                sqlx::query(DELETE_READER_OPERATION).bind("reader"),
            ),
            (
                "closed reader control delete",
                sqlx::query(DELETE_READER_CONTROL).bind("reader"),
            ),
            ("availability", sqlx::query(AVAILABILITY)),
        ];
        let mut plans = Vec::new();
        for (name, mut query) in queries {
            let args = query.take_arguments().unwrap().unwrap();
            let sql = query.sql();
            let mut explain = QueryBuilder::<Sqlite>::new("EXPLAIN QUERY PLAN ");
            explain.push(sql.as_str());
            let details = sqlx::query_with::<Sqlite, _>(explain.into_sql(), args.clone())
                .fetch_all(&store.pool)
                .await
                .unwrap()
                .iter()
                .map(|r| r.get::<String, _>("detail"))
                .collect::<Vec<_>>();
            let mut bytecode = QueryBuilder::<Sqlite>::new("EXPLAIN ");
            bytecode.push(sql.as_str());
            let opcodes = sqlx::query_with::<Sqlite, _>(bytecode.into_sql(), args)
                .fetch_all(&store.pool)
                .await
                .unwrap()
                .len();
            assert!(opcodes > 0);
            plans.push(Plan {
                name,
                details,
                opcodes,
            });
        }
        assert!(
            plans
                .iter()
                .find(|p| p.name == "unit history")
                .unwrap()
                .details
                .iter()
                .any(|p| p.contains("tracking_history_unit"))
        );
        assert!(
            plans
                .iter()
                .find(|p| p.name == "availability")
                .unwrap()
                .details
                .iter()
                .any(|p| p.contains("tracking_units_catalog_status"))
        );
        let start = Instant::now();
        let available = store.tracking_availability(now).await.unwrap();
        let micros = start.elapsed().as_micros();
        assert_eq!(available.eligible_catalog_ids.len(), 500);
        assert_eq!(available.certified_ready_catalog_ids.len(), 500);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tracking_units")
                .fetch_one(&store.pool)
                .await
                .unwrap(),
            2_000,
            "EXPLAIN must not execute destructive statements"
        );
        println!(
            "tracking plans: {}; availability_us={micros}; active=1000 archived=1000",
            serde_json::to_string(&plans).unwrap()
        );
        if let Ok(path) = std::env::var("CLOSET_TRACKING_PLAN_OUTPUT") {
            std::fs::write(path, serde_json::to_vec_pretty(&plans).unwrap()).unwrap();
        }
        store.close().await;
    }
    struct LegacyDatabase {
        _directory: tempfile::TempDir,
        path: std::path::PathBuf,
        item: CatalogItem,
        item_body: String,
        today_body: String,
        report: crate::models::SyncReport,
    }
    #[derive(serde::Serialize)]
    struct LegacyBasis {
        date: String,
        timezone: String,
        config_hash: String,
        items: Vec<CatalogItem>,
    }
    async fn legacy_database(quantity: Option<u32>) -> LegacyDatabase {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        let directory = tempfile::tempdir().unwrap();
        let migration_dir = directory.path().join("released-migrations");
        std::fs::create_dir(&migration_dir).unwrap();
        for name in [
            "0001_catalog.sql",
            "0002_remove_ownership_confirmation.sql",
            "0003_remove_purchase_condition.sql",
            "0004_inventory_fence.sql",
        ] {
            std::fs::copy(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("migrations")
                    .join(name),
                migration_dir.join(name),
            )
            .unwrap();
        }
        let path = directory.path().join("released.sqlite3");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true)
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        sqlx::migrate::Migrator::new(migration_dir.as_path())
            .await
            .unwrap()
            .run(&pool)
            .await
            .unwrap();
        // The released app had already completed these empty initial rewrites.
        sqlx::query("DROP TABLE ownership_removal_pending")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DROP TABLE condition_removal_pending")
            .execute(&pool)
            .await
            .unwrap();
        let mut item = fixture(1);
        item.quantity = quantity;
        let item_body = encode(&item).unwrap();
        let source_hash = digest(&serde_json::to_vec(&vec![item.clone()]).unwrap());
        let styling_hash = crate::store::styling_hash(&item).unwrap();
        sqlx::query("INSERT INTO items(id,body,brand,sort_brand,name,sort_name,category,sku,archived,revision,styling_hash) VALUES(?,?,?,?,?,?,?,?,0,1,?)").bind(&item.id).bind(&item_body).bind(&item.brand).bind("example studio").bind(&item.name).bind("fictional garment").bind(&item.category).bind(&item.sku).bind(&styling_hash).execute(&pool).await.unwrap();
        sqlx::query("UPDATE catalog_state SET revision=1,source_hash=? WHERE singleton=1")
            .bind(&source_hash)
            .execute(&pool)
            .await
            .unwrap();
        let now = DateTime::<Utc>::UNIX_EPOCH;
        let inputs = crate::models::TodayRequest {
            occasion: crate::models::Occasion::Everyday,
            weather_mode: crate::models::WeatherMode::Ignored,
        };
        let generation = crate::models::Generation {
            id: "fictional-saved-generation".into(),
            status: crate::models::GenerationStatus::Succeeded,
            inputs: inputs.clone(),
            started_at: now,
            finished_at: Some(now),
            error_code: None,
        };
        let basis = LegacyBasis {
            date: "1970-01-01".into(),
            timezone: "UTC".into(),
            config_hash: "historical-config".into(),
            items: vec![item.clone()],
        };
        sqlx::query("INSERT INTO generations(id,local_date,timezone,config_hash,status,body,basis,started_at) VALUES(?,'1970-01-01','UTC','historical-config','succeeded',?,?,?)").bind(&generation.id).bind(encode(&generation).unwrap()).bind(encode(&basis).unwrap()).bind(now.to_rfc3339()).execute(&pool).await.unwrap();
        let today = crate::models::TodayResult {
            generation_id: generation.id,
            inputs,
            generated_at: now,
            expires_at: now + chrono::Duration::days(1),
            freshness: crate::models::Freshness::Ready,
            stale_reasons: Vec::new(),
            weather: None,
            suggestions: vec![crate::models::Suggestion {
                occasion: Some(crate::models::Occasion::Everyday),
                label: "Saved fictional suggestion".into(),
                item_ids: vec![item.id.clone()],
                advisory_reason: "Historical fixture".into(),
                warnings: Vec::new(),
            }],
        };
        let today_body = encode(&today).unwrap();
        let signatures = BTreeMap::from([(item.id.clone(), styling_hash)]);
        sqlx::query("INSERT INTO today_results(local_date,timezone,config_hash,body,signatures) VALUES('1970-01-01','UTC','historical-config',?,?)").bind(&today_body).bind(encode(&signatures).unwrap()).execute(&pool).await.unwrap();
        let report = crate::models::SyncReport {
            catalog_revision: 1,
            source_hash,
            created: 1,
            updated: 0,
            archived: 0,
            unchanged: 0,
        };
        let input_hash = digest(&serde_json::to_vec(&(vec![item.clone()], false)).unwrap());
        sqlx::query("INSERT INTO operations(scope,actor,key,request_hash,response) VALUES('sync','fixture','released-sync',?,?)").bind(input_hash).bind(encode(&report).unwrap()).execute(&pool).await.unwrap();
        pool.close().await;
        LegacyDatabase {
            _directory: directory,
            path,
            item,
            item_body,
            today_body,
            report,
        }
    }
    fn setup_config() -> TrackingConfig {
        TrackingConfig {
            enabled: true,
            areas: vec![AreaConfig {
                id: identifier("closet"),
                label: "Fictional closet".into(),
                kind: AreaKind::Closet,
                enabled: true,
                thresholds: AreaThresholds::default(),
            }],
            readers: vec![ReaderConfig {
                id: identifier("reader"),
                label: "Fictional reader".into(),
                enabled: true,
                connection: ReaderConnection::ImpinjLlrp {
                    host: "reader.example.invalid".into(),
                    port: 5084,
                    owned_rospec_id: 47,
                },
            }],
            ports: vec![
                PortConfig {
                    id: identifier("monitor"),
                    reader_id: identifier("reader"),
                    antenna: 1,
                    area_id: Some(identifier("closet")),
                    role: PortRole::Monitor,
                    enabled: true,
                    required_for_absence: true,
                    commissioned: true,
                    transmit_power_cdbm: None,
                    dwell_ms: None,
                },
                PortConfig {
                    id: identifier("station"),
                    reader_id: identifier("reader"),
                    antenna: 2,
                    area_id: None,
                    role: PortRole::Enrollment,
                    enabled: true,
                    required_for_absence: false,
                    commissioned: false,
                    transmit_power_cdbm: None,
                    dwell_ms: None,
                },
            ],
            thresholds: Thresholds {
                positive_dwell_ms: 100,
                positive_min_reads: 2,
                ..Thresholds::default()
            },
            ..TrackingConfig::default()
        }
    }
    async fn command(
        store: &Store,
        key: &str,
        command: TrackingCommand,
        now: DateTime<Utc>,
    ) -> CommandResult {
        let revision = store
            .tracking_snapshot(now)
            .await
            .unwrap()
            .tracking_revision;
        store
            .tracking_command("fixture-operator", key, revision, command, now)
            .await
            .unwrap()
    }
    fn batch(
        now: DateTime<Utc>,
        sequence: u64,
        purpose: ObservationPurpose,
        capture: Option<SessionId>,
        observations: Vec<TagObservation>,
    ) -> ObservationBatch {
        ObservationBatch {
            reader_id: identifier("reader"),
            purpose,
            data_source: DataSource::Live,
            capture_session: capture,
            session: uuid::Uuid::from_u128(1),
            sequence,
            observed_at: now,
            connected: true,
            coverage: Vec::new(),
            observations,
            readback: None,
        }
    }

    #[tokio::test]
    async fn released_schema_upgrade_and_reopen_preserve_catalog_retry_today_bindings_and_unit_ids()
    {
        let legacy = legacy_database(Some(1)).await;
        let store = Store::open(&legacy.path).await.unwrap();
        let now = Utc::now() + chrono::Duration::seconds(1);
        assert_eq!(store.counts().await.unwrap(), (1, 1, 0));
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT body FROM items")
                .fetch_one(&store.pool)
                .await
                .unwrap(),
            legacy.item_body
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT body FROM today_results")
                .fetch_one(&store.pool)
                .await
                .unwrap(),
            legacy.today_body
        );
        let replay = store
            .sync(
                SyncRequest {
                    items: vec![legacy.item.clone()],
                    dry_run: false,
                    allow_empty: false,
                },
                Some(0),
                "fixture",
                Some("released-sync"),
            )
            .await
            .unwrap();
        assert_eq!(encode(&replay).unwrap(), encode(&legacy.report).unwrap());
        command(
            &store,
            "config",
            TrackingCommand::Configure {
                config: setup_config(),
            },
            now,
        )
        .await;
        let unit = store.tracking_snapshot(now).await.unwrap().units[0].id;
        store
            .tracking_ingest(
                batch(now, 1, ObservationPurpose::Inventory, None, Vec::new()),
                now,
            )
            .await
            .unwrap();
        let capture_at = now + chrono::Duration::milliseconds(1);
        let session = command(
            &store,
            "capture",
            TrackingCommand::StartEnrollment {
                unit_ids: vec![unit],
                port_id: identifier("station"),
                borrow: false,
                purpose: EnrollmentPurpose::Bind,
                ttl_seconds: 600,
            },
            capture_at,
        )
        .await
        .session
        .unwrap();
        let observed = now + chrono::Duration::milliseconds(202);
        store
            .tracking_ingest(
                batch(
                    observed,
                    2,
                    ObservationPurpose::Enrollment,
                    Some(session.id),
                    vec![TagObservation {
                        epc: identifier("0001"),
                        port_id: identifier("station"),
                        first_seen_at: now + chrono::Duration::milliseconds(2),
                        last_seen_at: observed,
                        read_count: 3,
                        tid: None,
                        peak_rssi_cdbm: None,
                        event_ref: Some("fictional-event".into()),
                    }],
                ),
                observed,
            )
            .await
            .unwrap();
        let version = store.tracking_snapshot(observed).await.unwrap().sessions[0].capture_version;
        command(
            &store,
            "pair",
            TrackingCommand::SelectCandidate {
                session_id: session.id,
                capture_version: version,
                unit_id: unit,
                epc: identifier("0001"),
                baseline_area: Some(identifier("closet")),
            },
            observed,
        )
        .await;
        store.close().await;
        let reopened = Store::open(&legacy.path).await.unwrap();
        let restored = reopened
            .tracking_snapshot(observed + chrono::Duration::seconds(1))
            .await
            .unwrap();
        assert_eq!(restored.units[0].id, unit);
        assert_eq!(restored.units[0].status, UnitStatus::CleanReady);
        assert!(!restored.health.absence_coverage_healthy);
        let exported = reopened
            .tracking_export(observed + chrono::Duration::seconds(1))
            .await
            .unwrap();
        assert_eq!(exported.units[0].epc, Some(identifier("0001")));
        assert_eq!(exported.bindings.len(), 1);
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT body FROM items")
                .fetch_one(&reopened.pool)
                .await
                .unwrap(),
            legacy.item_body
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT body FROM today_results")
                .fetch_one(&reopened.pool)
                .await
                .unwrap(),
            legacy.today_body
        );
        assert_eq!(reopened.counts().await.unwrap(), (1, 1, 0));
        reopened.close().await;
    }

    #[tokio::test]
    async fn historical_unknown_quantity_requires_an_explicit_runtime_count_without_rewriting_catalog()
     {
        let legacy = legacy_database(None).await;
        let store = Store::open(&legacy.path).await.unwrap();
        let now = Utc::now();
        let before = store.tracking_snapshot(now).await.unwrap();
        assert!(before.units.is_empty());
        assert_eq!(before.inventory[0].catalog_quantity, None);
        assert_eq!(before.inventory[0].declared_quantity, None);
        assert_eq!(before.counts.quantity_unknown_items, 1);
        command(
            &store,
            "declare",
            TrackingCommand::DeclareUnitCount {
                catalog_id: legacy.item.id.clone(),
                count: 2,
            },
            now,
        )
        .await;
        let after = store.tracking_snapshot(now).await.unwrap();
        assert_eq!(after.units.len(), 2);
        assert_eq!(after.inventory[0].declared_quantity, Some(2));
        let history = store.tracking_history(None, None, 20).await.unwrap();
        assert!(
            history
                .events
                .iter()
                .filter(|e| matches!(e.kind, HistoryKind::QuantityReconciliation))
                .all(|e| e.actor == "fixture-operator" && !e.automatic)
        );
        assert!(
            after
                .units
                .iter()
                .all(|u| u.baseline_at.is_none() && !u.has_binding)
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT body FROM items")
                .fetch_one(&store.pool)
                .await
                .unwrap(),
            legacy.item_body
        );
        assert_eq!(store.counts().await.unwrap(), (1, 1, 0));
        store.close().await;
    }

    #[tokio::test]
    async fn borrowed_capture_and_pending_reader_claims_recover_after_a_real_database_restart() {
        let legacy = legacy_database(Some(1)).await;
        let store = Store::open(&legacy.path).await.unwrap();
        let now = Utc::now() + chrono::Duration::seconds(1);
        command(
            &store,
            "configure",
            TrackingCommand::Configure {
                config: setup_config(),
            },
            now,
        )
        .await;
        let unit = store.tracking_snapshot(now).await.unwrap().units[0].id;
        let mut hello = batch(now, 1, ObservationPurpose::Inventory, None, Vec::new());
        hello.readback = Some(ReaderReadback {
            reader_id: identifier("reader"),
            at: now,
            capabilities: ReaderCapabilities {
                antenna_ports: vec![1, 2],
                min_power_cdbm: None,
                max_power_cdbm: None,
                min_dwell_ms: None,
                max_dwell_ms: None,
                continuous_inventory_proof: true,
                inventory_pause: true,
                firmware: Some("fictional fixture".into()),
            },
            ports: Vec::new(),
        });
        store.tracking_ingest(hello, now).await.unwrap();
        let session = command(
            &store,
            "borrow",
            TrackingCommand::StartEnrollment {
                unit_ids: vec![unit],
                port_id: identifier("monitor"),
                borrow: true,
                purpose: EnrollmentPurpose::Bind,
                ttl_seconds: 600,
            },
            now,
        )
        .await
        .session
        .unwrap();
        assert_eq!(session.borrow_state, BorrowState::PauseRequested);
        command(
            &store,
            "paused",
            TrackingCommand::AcknowledgeBorrow {
                session_id: session.id,
                state: BorrowState::Paused,
            },
            now,
        )
        .await;
        let reader = identifier("reader");
        store
            .tracking_reader_plan(&reader, 1, &"a".repeat(64), None, now)
            .await
            .unwrap();
        let original = LlrpControlSettings {
            notifications: Vec::new(),
            keepalive: LlrpKeepalive {
                trigger: KeepaliveTrigger::None,
                interval_ms: 0,
            },
            hold_events_and_reports_upon_reconnect: true,
        };
        let mut desired = original.clone();
        desired.keepalive = LlrpKeepalive {
            trigger: KeepaliveTrigger::Periodic,
            interval_ms: 2_000,
        };
        store
            .tracking_reader_plan_control(&reader, 1, &original, &desired, now)
            .await
            .unwrap();
        store.close().await;
        let reopened = Store::open(&legacy.path).await.unwrap();
        let read_at = now + chrono::Duration::seconds(1);
        let after = reopened.tracking_snapshot(read_at).await.unwrap();
        assert_eq!(after.units[0].id, unit);
        let session = &after.sessions[0];
        assert_eq!(session.state, EnrollmentState::RecoveryRequired);
        assert_eq!(session.borrow_state, BorrowState::RestoreRequested);
        assert!(session.candidates.is_empty());
        assert!(!after.health.absence_coverage_healthy);
        assert_eq!(
            after.reader_operations[0].pending_fingerprint,
            Some("a".repeat(64))
        );
        assert_eq!(after.reader_controls[0].original, original);
        assert_eq!(after.reader_controls[0].phase, ReaderControlPhase::Pending);
        assert_eq!(
            reopened
                .tracking_reader_plan(&reader, 1, &"a".repeat(64), Some(&"a".repeat(64)), read_at)
                .await
                .unwrap()
                .action,
            ReaderPlanAction::Keep
        );
        assert_eq!(
            reopened
                .tracking_reader_restore_control(&reader, &desired, read_at)
                .await
                .unwrap()
                .target,
            original
        );
        reopened
            .tracking_reader_control_applied(&reader, 1, &original, true, read_at)
            .await
            .unwrap();
        command(
            &reopened,
            "restored",
            TrackingCommand::AcknowledgeBorrow {
                session_id: session.id,
                state: BorrowState::Restored,
            },
            read_at,
        )
        .await;
        assert_eq!(
            reopened.tracking_snapshot(read_at).await.unwrap().sessions[0].borrow_state,
            BorrowState::Restored
        );
        reopened.close().await;
    }

    #[tokio::test]
    async fn live_clocks_are_sampled_after_mutex_and_database_admission_but_replay_remains_strict()
    {
        use std::future::{Future, poll_fn};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::Poll;

        let now = DateTime::<Utc>::UNIX_EPOCH;
        let store = Store::open_simulator(now).await.unwrap();
        let sampled = AtomicBool::new(false);
        let mut admitted_tick = std::pin::pin!(store.tracking_tick_clocked(|| {
            sampled.store(true, Ordering::SeqCst);
            now + chrono::Duration::seconds(2)
        }));
        let mut runtime = store.tracking.lock().await;
        poll_fn(|cx| {
            assert!(admitted_tick.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert!(!sampled.load(Ordering::SeqCst));
        runtime.clock(now + chrono::Duration::seconds(1)).unwrap();
        drop(runtime);
        admitted_tick.await.unwrap();
        assert!(sampled.load(Ordering::SeqCst));

        // Explicit scenario timestamps must still reject a backwards clock.
        assert!(matches!(
            store.tracking_tick(now).await,
            Err(TrackingError::Invalid {
                code: "clock_moved_backwards",
                ..
            })
        ));

        sampled.store(false, Ordering::SeqCst);
        let connection = store.pool.acquire().await.unwrap();
        let mut admitted_command = std::pin::pin!(store.tracking_command_clocked(
            "fixture-operator",
            "admitted-clock",
            0,
            TrackingCommand::Tick,
            || {
                sampled.store(true, Ordering::SeqCst);
                now + chrono::Duration::seconds(3)
            },
        ));
        poll_fn(|cx| {
            assert!(admitted_command.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert!(!sampled.load(Ordering::SeqCst));
        drop(connection);
        admitted_command.await.unwrap();
        assert!(sampled.load(Ordering::SeqCst));
        store.close().await;
    }

    #[tokio::test]
    async fn queued_catalog_writer_cannot_interleave_between_tick_load_and_persistence() {
        use std::future::{Future, poll_fn};
        use std::task::Poll;

        let now = DateTime::<Utc>::UNIX_EPOCH;
        let store = Store::open_simulator(now).await.unwrap();
        store
            .sync_at(
                SyncRequest {
                    items: vec![fixture(1)],
                    dry_run: false,
                    allow_empty: false,
                },
                Some(0),
                "fixture",
                Some("initial"),
                now,
            )
            .await
            .unwrap();
        command(
            &store,
            "config",
            TrackingCommand::Configure {
                config: setup_config(),
            },
            now,
        )
        .await;
        let session = command(
            &store,
            "lookup",
            TrackingCommand::StartEnrollment {
                unit_ids: Vec::new(),
                port_id: identifier("station"),
                borrow: false,
                purpose: EnrollmentPurpose::Lookup,
                ttl_seconds: 60,
            },
            now,
        )
        .await
        .session
        .unwrap();
        let after_expiry = now + chrono::Duration::seconds(61);
        let mut changed_item = fixture(1);
        changed_item.quantity = Some(2);
        let connection = store.pool.acquire().await.unwrap();
        let mut tick = std::pin::pin!(store.tracking_tick(after_expiry));
        poll_fn(|cx| {
            assert!(tick.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        // Queue the writer behind tracking's admitted connection request. Releasing
        // the connection between load and persist would hand it to this sync first.
        let mut sync = std::pin::pin!(store.sync_at(
            SyncRequest {
                items: vec![changed_item],
                dry_run: false,
                allow_empty: false
            },
            Some(1),
            "fixture",
            Some("quantity-change"),
            after_expiry,
        ));
        poll_fn(|cx| {
            assert!(sync.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(connection);
        let (tick_result, sync_result) = tokio::join!(tick, sync);
        assert!(tick_result.unwrap().accepted);
        sync_result.unwrap();
        let snapshot = store.tracking_snapshot(after_expiry).await.unwrap();
        assert_eq!(snapshot.units.len(), 2);
        assert_eq!(
            snapshot
                .sessions
                .iter()
                .find(|s| s.id == session.id)
                .unwrap()
                .state,
            EnrollmentState::Expired
        );
        store.close().await;
    }
}
