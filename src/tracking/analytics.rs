//! Analytics are derived from the durable journal, never recent reader diagnostics.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Days, Duration, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqliteConnection};

use crate::models::CatalogItem;
use crate::store::Store;
use crate::tracking::models::{
    AreaKind, DecisionReason, HistoryKind, InstallationState, Result, StateEvidence, StoredUnit,
    TrackingError, UnitId, UnitStatus,
};

const PAGE: usize = 500;
const MINIMUM_OBSERVATION_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
const LIST_LIMIT: usize = 12;
const STATE: &str = "SELECT enabled FROM tracking_state WHERE singleton=1";
const CATALOG: &str = "SELECT id,body FROM items WHERE id>? ORDER BY id LIMIT 500";
const UNITS: &str = "SELECT id,body FROM tracking_units WHERE id>? ORDER BY id LIMIT 500";
const HISTORY_FORWARD: &str = "SELECT id,unit_id,at,json_extract(body,'$.automatic') AS automatic,body->'$.kind' AS kind,body->'$.reason' AS reason,body->'$.before' AS before,body->'$.after' AS after FROM tracking_history WHERE id>? ORDER BY id LIMIT 500";
const HISTORY_REVERSE: &str = "SELECT id,unit_id,at,body->'$.kind' AS kind,body->'$.reason' AS reason,json_extract(body,'$.before.unit.last_event_id') AS target,json_extract(body,'$.before.epc') AS before_epc,json_extract(body,'$.after.epc') AS after_epc FROM tracking_history WHERE id<? ORDER BY id DESC LIMIT 500";
const COVERAGE: &str = "SELECT id,started_at,verified_through FROM tracking_coverage WHERE id>? AND julianday(verified_through)>=julianday(?) AND julianday(started_at)<julianday(?) ORDER BY id LIMIT 500";

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnalyticsReport {
    pub as_of: DateTime<Utc>,
    pub from: DateTime<Utc>,
    pub days: u16,
    pub timezone: String,
    pub tracking_enabled: bool,
    pub healthy_observation_ms: u64,
    pub minimum_observation_ms: u64,
    pub total_units: usize,
    pub tracked_units: usize,
    pub inferred_wears: u64,
    pub distinct_items_worn: usize,
    pub daily: Vec<WearDay>,
    pub most_worn: Vec<AnalyticsItem>,
    pub overlooked: Vec<OverlookedUnit>,
    pub pick: Option<OverlookedUnit>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WearDay {
    pub date: NaiveDate,
    pub inferred_wears: u64,
    pub observed_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnalyticsItem {
    pub item: CatalogItem,
    pub unit_count: usize,
    pub tracked_units: usize,
    pub inferred_wears: u64,
    /// Latest valid inference across retained history, not just the selected window.
    pub last_worn_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OverlookedUnit {
    pub unit_id: UnitId,
    pub ordinal: u32,
    pub item: CatalogItem,
    pub inferred_wears: u64,
    /// Latest valid inference across retained history, not just the selected window.
    pub last_worn_at: Option<DateTime<Utc>>,
    pub observed_ms: u64,
    pub ready_to_wear: bool,
}

#[derive(Clone, Copy)]
struct Span {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

struct DaySpan {
    date: NaiveDate,
    span: Span,
}

struct Coverage {
    spans: Vec<Span>,
    cumulative_ms: Vec<u64>,
}

impl Coverage {
    fn through(&self, at: DateTime<Utc>) -> u64 {
        let completed = self.spans.partition_point(|span| span.end <= at);
        let before = self.cumulative_ms.get(completed).copied().unwrap_or(0);
        self.spans.get(completed).map_or(before, |span| {
            before.saturating_add(millis(Span {
                start: span.start,
                end: at.min(span.end),
            }))
        })
    }

    fn between(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> u64 {
        if start >= end {
            0
        } else {
            self.through(end).saturating_sub(self.through(start))
        }
    }

    fn total(&self) -> u64 {
        self.cumulative_ms.last().copied().unwrap_or(0)
    }
}

struct JournalEvent {
    id: i64,
    unit_id: UnitId,
    at: DateTime<Utc>,
    kind: HistoryKind,
    reason: DecisionReason,
    automatic: bool,
    before: Option<StoredUnit>,
    after: StoredUnit,
}

struct Journal {
    units: BTreeMap<UnitId, UnitAnalytics>,
    daily: BTreeMap<NaiveDate, u64>,
}

#[derive(Default)]
struct UnitAnalytics {
    state: Option<(DateTime<Utc>, StoredUnit)>,
    state_is_effective: bool,
    episode: Option<DateTime<Utc>>,
    wears: u64,
    last_worn_at: Option<DateTime<Utc>>,
    observed_ms: u64,
}

impl UnitAnalytics {
    fn finish_episode(
        &mut self,
        from: DateTime<Utc>,
        now: DateTime<Utc>,
        timezone: Tz,
        daily: &mut BTreeMap<NaiveDate, u64>,
    ) {
        if let Some(at) = self.episode.take() {
            self.last_worn_at = Some(self.last_worn_at.map_or(at, |last| last.max(at)));
            if at >= from && at <= now {
                self.wears = self.wears.saturating_add(1);
                let count = daily
                    .entry(at.with_timezone(&timezone).date_naive())
                    .or_default();
                *count = count.saturating_add(1);
            }
        }
    }

    fn observe_until(&mut self, until: DateTime<Utc>, coverage: &Coverage) {
        if let Some((start, state)) = &self.state
            && self.state_is_effective
            && observable(state)
        {
            self.observed_ms = self
                .observed_ms
                .saturating_add(coverage.between(*start, until));
        }
    }
}

fn invalid(code: &'static str, field: &'static str) -> TrackingError {
    TrackingError::Invalid { code, field }
}

/// A tag without a usable baseline/area cannot observe absence, even if all
/// antennas are healthy. Paused, no-tag and reconciliation periods earn no credit.
fn observable(state: &StoredUnit) -> bool {
    let unit = &state.unit;
    state.epc.is_some()
        && unit.has_binding
        && unit.baseline_at.is_some()
        && unit.last_confirmed_area.is_some()
        && state.last_confirmed_kind.is_some()
        && unit.evidence != StateEvidence::Unknown
        && !unit.paused
        && !unit.retired
        && !unit.archived
        && unit.installation.state != InstallationState::Exempt
        && unit.exception.is_none()
}

fn automatic_wear(event: &JournalEvent) -> bool {
    event.automatic
        && event.kind == HistoryKind::Absence
        && event.reason == DecisionReason::ConfirmedAbsence
        && event.after.unit.status == UnitStatus::BeingWorn
        && event.after.unit.evidence == StateEvidence::Rfid
        && event.before.as_ref().is_some_and(|before| {
            before.unit.status.is_ready()
                && before.epc.is_some()
                && before.unit.baseline_at.is_some()
                && !before.unit.retired
                && !before.unit.archived
                && !before.unit.paused
                && before.unit.installation.state != InstallationState::Exempt
                && matches!(
                    before.last_confirmed_kind,
                    Some(AreaKind::Closet | AreaKind::Rewear)
                )
        })
}

fn millis(span: Span) -> u64 {
    u64::try_from((span.end - span.start).num_milliseconds()).unwrap_or(0)
}

fn intersection(a: Span, b: Span) -> Option<Span> {
    let span = Span {
        start: a.start.max(b.start),
        end: a.end.min(b.end),
    };
    (span.start < span.end).then_some(span)
}

fn day_start(date: NaiveDate, timezone: Tz) -> Result<DateTime<Utc>> {
    // Midnight can be ambiguous, skipped, or an entire local date can be skipped.
    // Choosing the first valid local minute also gives skipped dates zero exposure.
    let midnight = date.and_time(NaiveTime::MIN);
    for minute in 0..=2_880 {
        let local = midnight
            .checked_add_signed(Duration::minutes(minute))
            .ok_or_else(|| invalid("invalid_analytics_window", "days"))?;
        if let Some(at) = timezone.from_local_datetime(&local).earliest() {
            return Ok(at.with_timezone(&Utc));
        }
    }
    Err(invalid("invalid_analytics_window", "timezone"))
}

fn window(now: DateTime<Utc>, days: u16, timezone: Tz) -> Result<Vec<DaySpan>> {
    let first = now
        .with_timezone(&timezone)
        .date_naive()
        .checked_sub_days(Days::new(u64::from(days - 1)))
        .ok_or_else(|| invalid("invalid_analytics_window", "days"))?;
    let mut result = Vec::with_capacity(usize::from(days));
    for offset in 0..days {
        let date = first
            .checked_add_days(Days::new(u64::from(offset)))
            .ok_or_else(|| invalid("invalid_analytics_window", "days"))?;
        let next = date
            .succ_opt()
            .ok_or_else(|| invalid("invalid_analytics_window", "days"))?;
        result.push(DaySpan {
            date,
            span: Span {
                start: day_start(date, timezone)?,
                end: day_start(next, timezone)?.min(now),
            },
        });
    }
    Ok(result)
}

async fn coverage(
    conn: &mut SqliteConnection,
    from: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<Coverage> {
    let mut result: Vec<Span> = Vec::new();
    let mut after = 0_i64;
    // SQL uses conservative whole-UTC-date bounds; chrono applies exact bounds,
    // avoiding reliance on lexical ordering of optional RFC3339 fractions.
    let sql_from = from.date_naive().to_string();
    let sql_until = now
        .date_naive()
        .succ_opt()
        .ok_or_else(|| invalid("invalid_analytics_window", "days"))?
        .to_string();
    loop {
        let rows = sqlx::query(COVERAGE)
            .bind(after)
            .bind(&sql_from)
            .bind(&sql_until)
            .fetch_all(&mut *conn)
            .await?;
        let more = rows.len() == PAGE;
        for row in rows {
            after = row.try_get("id")?;
            let start: DateTime<Utc> = row
                .try_get::<String, _>("started_at")?
                .parse()
                .map_err(|_| invalid("invalid_tracking_coverage", "started_at"))?;
            let end: DateTime<Utc> = row
                .try_get::<String, _>("verified_through")?
                .parse()
                .map_err(|_| invalid("invalid_tracking_coverage", "verified_through"))?;
            if let Some(span) = intersection(
                Span { start, end },
                Span {
                    start: from,
                    end: now,
                },
            ) {
                result.push(span);
            }
        }
        if !more {
            break;
        }
    }
    result.sort_by_key(|span| (span.start, span.end));
    let mut merged: Vec<Span> = Vec::new();
    for span in result {
        if let Some(last) = merged.last_mut()
            && span.start <= last.end
        {
            last.end = last.end.max(span.end);
        } else {
            merged.push(span);
        }
    }
    let mut cumulative_ms = Vec::with_capacity(merged.len() + 1);
    cumulative_ms.push(0_u64);
    for span in &merged {
        cumulative_ms.push(
            cumulative_ms
                .last()
                .copied()
                .unwrap_or(0)
                .saturating_add(millis(*span)),
        );
    }
    Ok(Coverage {
        spans: merged,
        cumulative_ms,
    })
}

#[derive(Default)]
struct JournalEffects {
    undone: BTreeSet<i64>,
    mistaken: BTreeMap<UnitId, Vec<(i64, i64)>>,
}

impl JournalEffects {
    fn mistaken(&self, unit: UnitId, event: i64) -> bool {
        self.mistaken.get(&unit).is_some_and(|ranges| {
            let index = ranges.partition_point(|(_, end)| *end < event);
            ranges.get(index).is_some_and(|(start, _)| *start <= event)
        })
    }
}

async fn journal_effects(
    conn: &mut SqliteConnection,
    now: DateTime<Utc>,
) -> Result<JournalEffects> {
    let mut effects = JournalEffects::default();
    let mut pending: BTreeMap<UnitId, (String, i64)> = BTreeMap::new();
    let mut before = i64::MAX;
    loop {
        let rows = sqlx::query(HISTORY_REVERSE)
            .bind(before)
            .fetch_all(&mut *conn)
            .await?;
        let more = rows.len() == PAGE;
        for row in rows {
            before = row.try_get("id")?;
            let at: DateTime<Utc> = row
                .try_get::<String, _>("at")?
                .parse()
                .map_err(|_| invalid("stored_data_invalid", "at"))?;
            if at > now || effects.undone.contains(&before) {
                continue;
            }
            let kind: HistoryKind = serde_json::from_str(row.try_get("kind")?)?;
            if kind == HistoryKind::Undo
                && let Some(target) = row.try_get::<Option<i64>, _>("target")?
                && target > 0
                && target < before
            {
                // Reverse folding handles Undo of Undo without double counting.
                effects.undone.insert(target);
            }
            let unit: UnitId = row
                .try_get::<String, _>("unit_id")?
                .parse()
                .map_err(|_| invalid("stored_data_invalid", "unit_id"))?;
            let before_epc: Option<String> = row.try_get("before_epc")?;
            let after_epc: Option<String> = row.try_get("after_epc")?;
            if let Some((epc, end)) = pending.get(&unit)
                && after_epc.as_ref() == Some(epc)
                && before_epc != after_epc
            {
                // This association, rather than the EPC forever, was wrong.
                // Ordinary replacement/unbinding preserves its valid history.
                effects
                    .mistaken
                    .entry(unit)
                    .or_default()
                    .push((before, *end));
                pending.remove(&unit);
            }
            let reason: DecisionReason = serde_json::from_str(row.try_get("reason")?)?;
            if kind == HistoryKind::Unbind
                && reason == DecisionReason::MistakenPairing
                && let Some(epc) = before_epc
            {
                pending.insert(unit, (epc, before));
            }
        }
        if !more {
            break;
        }
    }
    // Old imports can predate a retained binding event. They still must not
    // attribute the corrected association to this piece.
    for (unit, (_, end)) in pending {
        effects.mistaken.entry(unit).or_default().push((0, end));
    }
    for ranges in effects.mistaken.values_mut() {
        ranges.reverse();
    }
    Ok(effects)
}

async fn journal(
    conn: &mut SqliteConnection,
    from: DateTime<Utc>,
    now: DateTime<Utc>,
    coverage: &Coverage,
    timezone: Tz,
) -> Result<Journal> {
    let effects = journal_effects(conn, now).await?;
    let mut units: BTreeMap<UnitId, UnitAnalytics> = BTreeMap::new();
    let mut daily = BTreeMap::new();
    let mut after = 0_i64;
    loop {
        let rows = sqlx::query(HISTORY_FORWARD)
            .bind(after)
            .fetch_all(&mut *conn)
            .await?;
        let more = rows.len() == PAGE;
        for row in rows {
            after = row.try_get("id")?;
            let event = JournalEvent {
                id: after,
                unit_id: row
                    .try_get::<String, _>("unit_id")?
                    .parse()
                    .map_err(|_| invalid("stored_data_invalid", "unit_id"))?,
                at: row
                    .try_get::<String, _>("at")?
                    .parse()
                    .map_err(|_| invalid("stored_data_invalid", "at"))?,
                kind: serde_json::from_str(row.try_get("kind")?)?,
                reason: serde_json::from_str(row.try_get("reason")?)?,
                automatic: row.try_get("automatic")?,
                before: serde_json::from_str(row.try_get("before")?)?,
                after: serde_json::from_str(row.try_get("after")?)?,
            };
            if event.at > now {
                continue;
            }
            let stats = units.entry(event.unit_id).or_default();
            stats.observe_until(event.at, coverage);
            stats.state = Some((event.at, event.after.clone()));
            // An undone binding/baseline cannot prove observation before its
            // correction. Preserve real paused/unbound gaps instead of turning
            // an administrative Undo into retroactive antenna coverage.
            stats.state_is_effective =
                !effects.undone.contains(&event.id) && !effects.mistaken(event.unit_id, event.id);
            if !stats.state_is_effective || event.kind == HistoryKind::Undo {
                continue;
            }
            if automatic_wear(&event) {
                stats.finish_episode(from, now, timezone, &mut daily);
                stats.episode = Some(event.at);
            } else if event.after.unit.status != UnitStatus::BeingWorn {
                if event.kind == HistoryKind::Import
                    || event.kind == HistoryKind::Correction
                        && event.reason == DecisionReason::ManualCorrection
                {
                    // A correction/import undoing the current inferred episode
                    // cancels that inference.
                    // A completed episode was already committed by its real return.
                    stats.episode = None;
                } else {
                    stats.finish_episode(from, now, timezone, &mut daily);
                }
            }
        }
        if !more {
            break;
        }
    }
    for stats in units.values_mut() {
        stats.observe_until(now, coverage);
        stats.finish_episode(from, now, timezone, &mut daily);
    }
    Ok(Journal { units, daily })
}

impl Store {
    pub async fn tracking_analytics_clocked(
        &self,
        days: u16,
        pick_seed: u64,
        timezone: Tz,
        clock: impl FnOnce() -> DateTime<Utc> + Send,
    ) -> Result<AnalyticsReport> {
        if !matches!(days, 30 | 90 | 365) {
            return Err(invalid("invalid_analytics_window", "days"));
        }
        let mut tx = self.pool.begin().await?;
        // Establish one SQLite read snapshot before sampling the request clock.
        let tracking_enabled: bool = sqlx::query_scalar(STATE).fetch_one(&mut *tx).await?;
        let now = clock();
        let day_spans = window(now, days, timezone)?;
        let from = day_spans
            .first()
            .map(|day| day.span.start)
            .ok_or_else(|| invalid("invalid_analytics_window", "days"))?;
        let coverage = coverage(&mut tx, from, now).await?;
        let journal = journal(&mut tx, from, now, &coverage, timezone).await?;
        let mut catalog: BTreeMap<String, AnalyticsItem> = BTreeMap::new();
        let mut cursor = String::new();
        loop {
            let rows = sqlx::query(CATALOG)
                .bind(&cursor)
                .fetch_all(&mut *tx)
                .await?;
            let more = rows.len() == PAGE;
            for row in rows {
                cursor = row.try_get("id")?;
                let item = serde_json::from_str(row.try_get("body")?)?;
                catalog.insert(
                    cursor.clone(),
                    AnalyticsItem {
                        item,
                        unit_count: 0,
                        tracked_units: 0,
                        inferred_wears: 0,
                        last_worn_at: None,
                    },
                );
            }
            if !more {
                break;
            }
        }
        let daily: Vec<WearDay> = day_spans
            .iter()
            .map(|day| WearDay {
                date: day.date,
                inferred_wears: journal.daily.get(&day.date).copied().unwrap_or(0),
                observed_ms: coverage.between(day.span.start, day.span.end),
            })
            .collect();
        let mut overlooked = Vec::new();
        let mut total_units = 0;
        let mut tracked_units = 0;
        cursor.clear();
        loop {
            let rows = sqlx::query(UNITS).bind(&cursor).fetch_all(&mut *tx).await?;
            let more = rows.len() == PAGE;
            for row in rows {
                cursor = row.try_get("id")?;
                let unit: StoredUnit = serde_json::from_str(row.try_get("body")?)?;
                let Some(item) = catalog.get_mut(&unit.unit.catalog_id) else {
                    continue;
                };
                let active = !unit.unit.retired && !unit.unit.archived;
                let tracked = active && observable(&unit);
                if active {
                    total_units += 1;
                    item.unit_count += 1;
                }
                if tracked {
                    tracked_units += 1;
                    item.tracked_units += 1;
                }
                let Some(stats) = journal.units.get(&unit.unit.id) else {
                    continue;
                };
                let wears = stats.wears;
                item.inferred_wears = item.inferred_wears.saturating_add(wears);
                item.last_worn_at = item.last_worn_at.max(stats.last_worn_at);
                if tracking_enabled
                    && tracked
                    && unit.unit.ready_to_wear()
                    && stats.observed_ms >= MINIMUM_OBSERVATION_MS
                {
                    overlooked.push(OverlookedUnit {
                        unit_id: unit.unit.id,
                        ordinal: unit.unit.ordinal,
                        item: item.item.clone(),
                        inferred_wears: wears,
                        last_worn_at: stats.last_worn_at,
                        observed_ms: stats.observed_ms,
                        ready_to_wear: true,
                    });
                }
            }
            if !more {
                break;
            }
        }
        tx.commit().await?;
        let inferred_wears = catalog
            .values()
            .map(|item| item.inferred_wears)
            .fold(0, u64::saturating_add);
        let mut most_worn: Vec<_> = catalog
            .into_values()
            .filter(|item| item.inferred_wears > 0)
            .collect();
        let distinct_items_worn = most_worn.len();
        most_worn.sort_by(|a, b| {
            b.inferred_wears
                .cmp(&a.inferred_wears)
                .then_with(|| b.last_worn_at.cmp(&a.last_worn_at))
                .then_with(|| a.item.id.cmp(&b.item.id))
        });
        most_worn.truncate(LIST_LIMIT);
        overlooked.sort_by(|a, b| {
            a.inferred_wears
                .cmp(&b.inferred_wears)
                .then_with(|| a.last_worn_at.cmp(&b.last_worn_at))
                .then_with(|| b.observed_ms.cmp(&a.observed_ms))
                .then_with(|| a.unit_id.cmp(&b.unit_id))
        });
        overlooked.truncate(LIST_LIMIT);
        let pick = overlooked
            .iter()
            .min_by_key(|unit| {
                let mut hash = Sha256::new();
                hash.update(pick_seed.to_le_bytes());
                hash.update(unit.unit_id.0.as_bytes());
                hash.finalize().to_vec()
            })
            .cloned();
        Ok(AnalyticsReport {
            as_of: now,
            from,
            days,
            timezone: timezone.to_string(),
            tracking_enabled,
            healthy_observation_ms: coverage.total(),
            minimum_observation_ms: MINIMUM_OBSERVATION_MS,
            total_units,
            tracked_units,
            inferred_wears,
            distinct_items_worn,
            daily,
            most_worn,
            overlooked,
            pick,
        })
    }
}
