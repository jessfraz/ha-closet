//! SQLite owns the projection and durable operations, never a second catalog.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{
    QueryBuilder, Row, Sqlite, SqliteConnection, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow},
};

use crate::fence::{
    FenceAcquire, FencePrepare, FenceResolve, FenceStatus, FenceToken, FenceUpload, ManagedImage,
    UploadClaimAction, UploadClaimStatus, referenced_image_id,
};
use crate::models::{
    CatalogIdentity, CatalogItem, Facets, Freshness, Generation, GenerationStatus, InventoryCopy,
    Item, ItemFilters, ItemsPage, ListQuery, MAX_CATALOG_ITEMS, SafetyRequirement, Source,
    SourceFacet, SyncReport, SyncRequest, TodayRequest, TodayResponse, TodayResult, Usage,
    ValidationError, VariantGroup, digest, validate_catalog,
};

const COUNTS_SQL: &str = "SELECT (SELECT COUNT(*) FROM items WHERE archived=0) AS active, (SELECT COUNT(*) FROM items WHERE archived=1) AS archived";
const ITEM_SQL: &str = "SELECT body, revision, archived FROM items WHERE id = ?";
const CURSOR_POSITION_SQL: &str =
    "SELECT sort_brand AS brand, sort_name AS name, id FROM items WHERE id=? AND archived=0";
const ACTIVE_ITEMS_SQL: &str = "SELECT body FROM items WHERE archived = 0 ORDER BY id";
const SCOPE_COUNT_SQL: &str = "SELECT COUNT(*) FROM items WHERE archived = 0";
const BRANDS_SQL: &str =
    "SELECT DISTINCT brand FROM items WHERE archived = 0 AND brand IS NOT NULL ORDER BY brand";
const CATEGORIES_SQL: &str =
    "SELECT DISTINCT category FROM items WHERE archived = 0 ORDER BY category";
const SOURCE_SQL: &str = "CASE WHEN json_extract(body, '$.shop.id') IS NOT NULL THEN 'shop:' || json_extract(body, '$.shop.id') WHEN json_extract(body, '$.purchase_source') IS NOT NULL THEN 'purchase:' || json_extract(body, '$.purchase_source') ELSE NULL END";
const RUNNING_EXISTS_SQL: &str = "SELECT EXISTS(SELECT 1 FROM generations WHERE status='running')";
const CURRENT_GENERATION_SQL: &str = "SELECT body FROM generations WHERE local_date=? AND timezone=? ORDER BY started_at DESC, id DESC LIMIT 1";
const GENERATION_BASIS_SQL: &str = "SELECT basis FROM generations WHERE id=?";
const TODAY_RESULT_SQL: &str =
    "SELECT body,signatures,config_hash,timezone FROM today_results WHERE local_date=?";
const RUNNING_GENERATIONS_SQL: &str = "SELECT body FROM generations WHERE status='running'";
const STATE_SQL: &str = "SELECT revision, source_hash FROM catalog_state WHERE singleton=1";
const OPERATION_SQL: &str = "SELECT request_hash, response, resource_id FROM operations WHERE scope=? AND actor=? AND key=?";
const GENERATION_SQL: &str = "SELECT body FROM generations WHERE id=?";
// Validated filters and an item ID fit below this URL-safe base64 bound, even
// with maximal JSON escaping. Leave room for HA's 8190-byte request line.
const MAX_PAGE_TOKEN_BYTES: usize = 7168;

const FENCE_READ_SQL: &str = "SELECT token_hash, actor, expires_at > unixepoch() AS active, pending, uploads FROM inventory_fence WHERE singleton=1";
const FENCE_ACQUIRE_SQL: &str = "INSERT INTO inventory_fence(singleton,token_hash,actor,expires_at,pending,uploads) VALUES(1,?,?,unixepoch()+300,?,?) ON CONFLICT(singleton) DO UPDATE SET token_hash=excluded.token_hash, actor=excluded.actor, expires_at=excluded.expires_at, pending=excluded.pending, uploads=excluded.uploads";
const FENCE_PENDING_SQL: &str =
    "UPDATE inventory_fence SET pending=?,expires_at=unixepoch()+300 WHERE singleton=1";
const FENCE_RELEASE_SQL: &str = "UPDATE inventory_fence SET expires_at=0 WHERE singleton=1";
const FENCE_UPLOADS_SQL: &str =
    "UPDATE inventory_fence SET uploads=?,expires_at=unixepoch()+300 WHERE singleton=1";
const LIVE_IMAGE_ITEMS_SQL: &str = "SELECT body FROM items WHERE archived=0 UNION ALL SELECT body FROM items WHERE archived=1 AND id IN (SELECT ids.value FROM today_results AS result, json_each(result.body, '$.suggestions') AS suggestion, json_each(suggestion.value, '$.item_ids') AS ids WHERE result.local_date=?)";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("invalid_catalog")]
    InvalidCatalog(#[from] ValidationError),
    #[error("{0}")]
    Invalid(&'static str),
    #[error("not_found")]
    NotFound,
    #[error("{0}")]
    Conflict(&'static str),
    #[error("revision_conflict")]
    Revision,
    #[error("precondition_required")]
    Precondition,
    #[error("storage_unavailable")]
    Database(#[from] sqlx::Error),
    #[error("stored_data_invalid")]
    Serialization(#[from] serde_json::Error),
    #[error("migration_failed")]
    Migration(#[from] sqlx::migrate::MigrateError),
}
impl StoreError {
    pub fn code(&self) -> &str {
        match self {
            Self::InvalidCatalog(error) => &error.code,
            Self::Invalid(code) | Self::Conflict(code) => code,
            Self::NotFound => "not_found",
            Self::Revision => "revision_conflict",
            Self::Precondition => "precondition_required",
            Self::Database(_) | Self::Migration(_) => "storage_unavailable",
            Self::Serialization(_) => "stored_data_invalid",
        }
    }
}
pub type Result<T> = std::result::Result<T, StoreError>;

impl From<crate::tracking::models::TrackingError> for StoreError {
    fn from(error: crate::tracking::models::TrackingError) -> Self {
        use crate::tracking::models::TrackingError;
        match error {
            TrackingError::Invalid { code, .. } => Self::Invalid(code),
            TrackingError::Conflict(code) => Self::Conflict(code),
            TrackingError::Revision => Self::Conflict("tracking_revision_conflict"),
            TrackingError::NotFound => Self::NotFound,
            TrackingError::Database(error) => Self::Database(error),
            TrackingError::Serialization(error) => Self::Serialization(error),
        }
    }
}

#[derive(Clone)]
pub struct Store {
    pub(crate) pool: SqlitePool,
    pub(crate) tracking: std::sync::Arc<tokio::sync::Mutex<crate::tracking::engine::Engine>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenerationBasis {
    pub date: String,
    pub timezone: String,
    pub config_hash: String,
    pub items: Vec<CatalogItem>,
}
pub struct Reservation {
    pub generation: Generation,
    pub fresh: bool,
}
#[derive(Serialize, Deserialize)]
struct Cursor {
    revision: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tracking_revision: Option<i64>,
    filters: ItemFilters,
    id: String,
}
#[derive(sqlx::FromRow)]
struct CursorPosition {
    brand: String,
    name: String,
    id: String,
}
#[derive(Serialize)]
struct StylingFacts<'a> {
    id: &'a str,
    name: &'a str,
    brand: &'a Option<String>,
    category: &'a str,
    color: &'a Option<String>,
    material: &'a Option<String>,
    quantity_known: bool,
    qualified_image: bool,
    #[serde(skip_serializing_if = "Usage::is_everyday")]
    usage: Usage,
}

pub fn styling_hash(item: &CatalogItem) -> Result<String> {
    Ok(hash(&serde_json::to_vec(&StylingFacts {
        id: &item.id,
        name: &item.name,
        brand: &item.brand,
        category: &item.category,
        color: &item.color,
        material: &item.material,
        quantity_known: item.quantity.is_some(),
        qualified_image: item.images.iter().any(|image| {
            image.confidence != crate::models::ImageConfidence::High || image.notes.is_some()
        }),
        usage: item.usage,
    })?))
}
fn hash(bytes: &[u8]) -> String {
    digest(bytes)
}

// One-time rewrite statements are shared with the native SQLite plan checks.
const MIGRATION_PENDING_SQL: &str = "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='ownership_removal_pending')";
const MIGRATION_ITEMS_SQL: &str = "SELECT id,body,archived,styling_hash FROM items ORDER BY id";
const MIGRATION_REWRITE_ITEM_SQL: &str = "UPDATE items SET body=?,styling_hash=? WHERE id=?";
const MIGRATION_REWRITE_STATE_SQL: &str =
    "UPDATE catalog_state SET source_hash=? WHERE singleton=1";
const MIGRATION_GENERATIONS_SQL: &str = "SELECT id,json_extract(body,'$.inputs') AS inputs,json_remove(body,'$.inputs.ownership_scope') AS body,basis FROM generations ORDER BY id";
const MIGRATION_REWRITE_GENERATION_SQL: &str = "UPDATE generations SET body=?,basis=? WHERE id=?";
const MIGRATION_OPERATIONS_SQL: &str =
    "SELECT scope,actor,key,request_hash,response,resource_id FROM operations";
const MIGRATION_REWRITE_TODAY_OPERATION_SQL: &str =
    "UPDATE operations SET request_hash=? WHERE scope='today' AND actor=? AND key=?";
const MIGRATION_REWRITE_SYNC_OPERATION_SQL: &str =
    "UPDATE operations SET request_hash=?,response=? WHERE scope='sync' AND actor=? AND key=?";
const MIGRATION_RESULTS_SQL: &str = "SELECT local_date,json_remove(body,'$.inputs.ownership_scope') AS body,signatures FROM today_results";
const MIGRATION_REWRITE_RESULT_SQL: &str =
    "UPDATE today_results SET body=?,signatures=? WHERE local_date=?";
const CONDITION_PENDING_SQL: &str = "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='condition_removal_pending')";
const CONDITION_ITEMS_SQL: &str = "SELECT id,body,archived FROM items ORDER BY id";
const CONDITION_REWRITE_ITEM_SQL: &str = "UPDATE items SET body=? WHERE id=?";
const CONDITION_GENERATIONS_SQL: &str = "SELECT id,basis FROM generations ORDER BY id";
const CONDITION_REWRITE_GENERATION_SQL: &str = "UPDATE generations SET basis=? WHERE id=?";
const CONDITION_OPERATIONS_SQL: &str =
    "SELECT actor,key,request_hash,response FROM operations WHERE scope='sync'";

// Frozen 0.1.2 shapes are confined to the one-time migration. The explicit
// serializer preserves the old field order so its request digests remain
// verifiable; the current API never accepts or emits the removed fields.
#[derive(Clone, Deserialize)]
struct PreviousCatalogItem {
    #[serde(flatten)]
    item: CatalogItem,
    current_ownership_confirmed: bool,
    condition_at_purchase: Option<String>,
}
impl Serialize for PreviousCatalogItem {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let item = &self.item;
        let mut record = serializer.serialize_struct("CatalogItem", 16)?;
        record.serialize_field("id", &item.id)?;
        record.serialize_field("brand", &item.brand)?;
        record.serialize_field("name", &item.name)?;
        record.serialize_field("category", &item.category)?;
        record.serialize_field("color", &item.color)?;
        record.serialize_field("size", &item.size)?;
        record.serialize_field("quantity", &item.quantity)?;
        record.serialize_field("material", &item.material)?;
        record.serialize_field("sku", &item.sku)?;
        record.serialize_field("purchase_source", &item.purchase_source)?;
        record.serialize_field(
            "current_ownership_confirmed",
            &self.current_ownership_confirmed,
        )?;
        record.serialize_field("images", &item.images)?;
        record.serialize_field("condition_at_purchase", &self.condition_at_purchase)?;
        record.serialize_field("notes", &item.notes)?;
        record.serialize_field("price", &item.price)?;
        record.serialize_field("shop", &item.shop)?;
        record.end()
    }
}
#[derive(Serialize, Deserialize)]
struct PreviousTodayRequest {
    occasion: crate::models::Occasion,
    ownership_scope: PreviousScope,
    weather_mode: crate::models::WeatherMode,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PreviousScope {
    AllInventory,
    ConfirmedOnly,
}
#[derive(Deserialize)]
struct PreviousGenerationBasis {
    date: String,
    timezone: String,
    config_hash: String,
    items: Vec<PreviousCatalogItem>,
}
fn remember_previous_snapshot(
    previous: &[PreviousCatalogItem],
    snapshots: &mut BTreeMap<String, (String, String)>,
) -> Result<()> {
    let mut sorted: Vec<_> = previous.iter().collect();
    sorted.sort_by(|left, right| left.item.id.cmp(&right.item.id));
    let current: Vec<_> = sorted.iter().map(|previous| &previous.item).collect();
    let source_hash = hash(&serde_json::to_vec(&current)?);
    for allow_empty in [false, true] {
        snapshots.insert(
            hash(&serde_json::to_vec(&(&sorted, allow_empty))?),
            (
                hash(&serde_json::to_vec(&(&current, allow_empty))?),
                source_hash.clone(),
            ),
        );
    }
    Ok(())
}

// Frozen 0.1.6 catalog shape. Missing differs from null: an earlier migration
// in this startup may already have removed the field. Never invent a value to
// translate a historical operation hash that cannot actually be recovered.
#[derive(Deserialize)]
struct ConditionCatalogItem {
    #[serde(flatten)]
    item: CatalogItem,
    #[serde(default, deserialize_with = "previous_condition")]
    condition_at_purchase: Option<Option<String>>,
}
fn previous_condition<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(deserializer).map(Some)
}
impl Serialize for ConditionCatalogItem {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let item = &self.item;
        let mut record = serializer.serialize_struct("CatalogItem", 15)?;
        record.serialize_field("id", &item.id)?;
        record.serialize_field("brand", &item.brand)?;
        record.serialize_field("name", &item.name)?;
        record.serialize_field("category", &item.category)?;
        record.serialize_field("color", &item.color)?;
        record.serialize_field("size", &item.size)?;
        record.serialize_field("quantity", &item.quantity)?;
        record.serialize_field("material", &item.material)?;
        record.serialize_field("sku", &item.sku)?;
        record.serialize_field("purchase_source", &item.purchase_source)?;
        record.serialize_field("images", &item.images)?;
        if let Some(condition) = &self.condition_at_purchase {
            record.serialize_field("condition_at_purchase", condition)?;
        }
        record.serialize_field("notes", &item.notes)?;
        record.serialize_field("price", &item.price)?;
        record.serialize_field("shop", &item.shop)?;
        record.end()
    }
}
#[derive(Deserialize)]
struct ConditionGenerationBasis {
    date: String,
    timezone: String,
    config_hash: String,
    items: Vec<ConditionCatalogItem>,
}
fn remember_condition_snapshot(
    previous: &[ConditionCatalogItem],
    snapshots: &mut BTreeMap<String, (String, String)>,
) -> Result<()> {
    if previous
        .iter()
        .any(|item| item.condition_at_purchase.is_none())
    {
        return Ok(());
    }
    let mut sorted: Vec<_> = previous.iter().collect();
    sorted.sort_by(|left, right| left.item.id.cmp(&right.item.id));
    let current: Vec<_> = sorted.iter().map(|previous| &previous.item).collect();
    let source_hash = hash(&serde_json::to_vec(&current)?);
    for allow_empty in [false, true] {
        snapshots.insert(
            hash(&serde_json::to_vec(&(&sorted, allow_empty))?),
            (
                hash(&serde_json::to_vec(&(&current, allow_empty))?),
                source_hash.clone(),
            ),
        );
    }
    Ok(())
}

impl Store {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5));
        // One connection serializes the tiny local catalog's transactions. No
        // transaction holds this connection while calling Home Assistant.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        let store = Self {
            pool,
            tracking: std::sync::Arc::new(tokio::sync::Mutex::new(
                crate::tracking::engine::Engine::default(),
            )),
        };
        store.remove_ownership_confirmation().await?;
        store.remove_purchase_condition().await?;
        store.interrupt_running().await?;
        store
            .tracking_startup(Utc::now())
            .await
            .map_err(StoreError::from)?;
        Ok(store)
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    async fn remove_purchase_condition(&self) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let pending: bool = sqlx::query_scalar(CONDITION_PENDING_SQL)
            .fetch_one(&mut *tx)
            .await?;
        if !pending {
            tx.commit().await?;
            return Ok(());
        }
        let mut active = Vec::new();
        for row in sqlx::query(CONDITION_ITEMS_SQL).fetch_all(&mut *tx).await? {
            let previous: ConditionCatalogItem = serde_json::from_str(row.try_get("body")?)?;
            sqlx::query(CONDITION_REWRITE_ITEM_SQL)
                .bind(serde_json::to_string(&previous.item)?)
                .bind(row.try_get::<String, _>("id")?)
                .execute(&mut *tx)
                .await?;
            if !row.try_get::<bool, _>("archived")? {
                active.push(previous);
            }
        }
        let mut snapshots = BTreeMap::new();
        remember_condition_snapshot(&active, &mut snapshots)?;
        if state(&mut tx).await?.0 > 0 {
            let current: Vec<_> = active.iter().map(|previous| &previous.item).collect();
            sqlx::query(MIGRATION_REWRITE_STATE_SQL)
                .bind(hash(&serde_json::to_vec(&current)?))
                .execute(&mut *tx)
                .await?;
        }
        for row in sqlx::query(CONDITION_GENERATIONS_SQL)
            .fetch_all(&mut *tx)
            .await?
        {
            let previous: ConditionGenerationBasis = serde_json::from_str(row.try_get("basis")?)?;
            remember_condition_snapshot(&previous.items, &mut snapshots)?;
            let basis = GenerationBasis {
                date: previous.date,
                timezone: previous.timezone,
                config_hash: previous.config_hash,
                items: previous
                    .items
                    .into_iter()
                    .map(|previous| previous.item)
                    .collect(),
            };
            sqlx::query(CONDITION_REWRITE_GENERATION_SQL)
                .bind(serde_json::to_string(&basis)?)
                .bind(row.try_get::<String, _>("id")?)
                .execute(&mut *tx)
                .await?;
        }
        for row in sqlx::query(CONDITION_OPERATIONS_SQL)
            .fetch_all(&mut *tx)
            .await?
        {
            let previous_hash: String = row.try_get("request_hash")?;
            if let Some((request_hash, source_hash)) = snapshots.get(&previous_hash) {
                let mut report: SyncReport = serde_json::from_str(row.try_get("response")?)?;
                report.source_hash.clone_from(source_hash);
                sqlx::query(MIGRATION_REWRITE_SYNC_OPERATION_SQL)
                    .bind(request_hash)
                    .bind(serde_json::to_string(&report)?)
                    .bind(row.try_get::<String, _>("actor")?)
                    .bind(row.try_get::<String, _>("key")?)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        // Condition never contributed to styling, Today inputs or signatures.
        // Preserve those bodies and hashes byte-for-byte, including stale ones.
        sqlx::query("DROP TABLE condition_removal_pending")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn remove_ownership_confirmation(&self) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let pending: bool = sqlx::query_scalar(MIGRATION_PENDING_SQL)
            .fetch_one(&mut *tx)
            .await?;
        if !pending {
            tx.commit().await?;
            return Ok(());
        }
        let rows = sqlx::query(MIGRATION_ITEMS_SQL).fetch_all(&mut *tx).await?;
        let mut active = Vec::new();
        let mut signatures = BTreeMap::new();
        for row in rows {
            let previous: PreviousCatalogItem = serde_json::from_str(row.try_get("body")?)?;
            let id: String = row.try_get("id")?;
            let updated_hash = styling_hash(&previous.item)?;
            signatures.insert(
                id.clone(),
                (
                    row.try_get::<String, _>("styling_hash")?,
                    updated_hash.clone(),
                ),
            );
            sqlx::query(MIGRATION_REWRITE_ITEM_SQL)
                .bind(serde_json::to_string(&previous.item)?)
                .bind(updated_hash)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            if !row.try_get::<bool, _>("archived")? {
                active.push(previous);
            }
        }

        let mut snapshots = BTreeMap::new();
        remember_previous_snapshot(&active, &mut snapshots)?;
        // A schema-only rewrite is not an inventory edit. Preserve revisions,
        // but make the current source hash describe the new canonical shape.
        if state(&mut tx).await?.0 > 0 {
            let items: Vec<_> = active.iter().map(|previous| &previous.item).collect();
            sqlx::query(MIGRATION_REWRITE_STATE_SQL)
                .bind(hash(&serde_json::to_vec(&items)?))
                .execute(&mut *tx)
                .await?;
        }
        let rows = sqlx::query(MIGRATION_GENERATIONS_SQL)
            .fetch_all(&mut *tx)
            .await?;
        let mut today_hashes = BTreeMap::new();
        for row in rows {
            let id: String = row.try_get("id")?;
            let inputs: PreviousTodayRequest = serde_json::from_str(row.try_get("inputs")?)?;
            let generation: Generation = serde_json::from_str(row.try_get("body")?)?;
            let previous: PreviousGenerationBasis = serde_json::from_str(row.try_get("basis")?)?;
            remember_previous_snapshot(&previous.items, &mut snapshots)?;
            let basis = GenerationBasis {
                date: previous.date,
                timezone: previous.timezone,
                config_hash: previous.config_hash,
                items: previous
                    .items
                    .into_iter()
                    .map(|previous| previous.item)
                    .collect(),
            };
            sqlx::query(MIGRATION_REWRITE_GENERATION_SQL)
                .bind(serde_json::to_string(&generation)?)
                .bind(serde_json::to_string(&basis)?)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            today_hashes.insert(
                id,
                (
                    hash(&serde_json::to_vec(&inputs)?),
                    hash(&serde_json::to_vec(&generation.inputs)?),
                ),
            );
        }
        let rows = sqlx::query(MIGRATION_OPERATIONS_SQL)
            .fetch_all(&mut *tx)
            .await?;
        for row in rows {
            let previous_hash: String = row.try_get("request_hash")?;
            let scope: String = row.try_get("scope")?;
            if scope == "today" {
                let resource_id: Option<String> = row.try_get("resource_id")?;
                if let Some((previous, current)) =
                    resource_id.as_ref().and_then(|id| today_hashes.get(id))
                    && previous == &previous_hash
                {
                    sqlx::query(MIGRATION_REWRITE_TODAY_OPERATION_SQL)
                        .bind(current)
                        .bind(row.try_get::<String, _>("actor")?)
                        .bind(row.try_get::<String, _>("key")?)
                        .execute(&mut *tx)
                        .await?;
                }
                continue;
            }
            if scope != "sync" {
                continue;
            }
            let mut report: SyncReport = serde_json::from_str(row.try_get("response")?)?;
            let request_hash =
                if let Some((request_hash, source_hash)) = snapshots.get(&previous_hash) {
                    report.source_hash.clone_from(source_hash);
                    request_hash
                } else {
                    // Old operations did not retain requests. An unrecoverable
                    // historical digest must keep rejecting mismatched retries.
                    &previous_hash
                };
            sqlx::query(MIGRATION_REWRITE_SYNC_OPERATION_SQL)
                .bind(request_hash)
                .bind(serde_json::to_string(&report)?)
                .bind(row.try_get::<String, _>("actor")?)
                .bind(row.try_get::<String, _>("key")?)
                .execute(&mut *tx)
                .await?;
        }
        let rows = sqlx::query(MIGRATION_RESULTS_SQL)
            .fetch_all(&mut *tx)
            .await?;
        for row in rows {
            let mut result: TodayResult = serde_json::from_str(row.try_get("body")?)?;
            for suggestion in &mut result.suggestions {
                suggestion
                    .warnings
                    .retain(|warning| warning != "unconfirmed_ownership");
            }
            let mut saved: BTreeMap<String, String> =
                serde_json::from_str(row.try_get("signatures")?)?;
            for (id, signature) in &mut saved {
                if let Some((previous, current)) = signatures.get(id)
                    && signature == previous
                {
                    signature.clone_from(current);
                }
            }
            sqlx::query(MIGRATION_REWRITE_RESULT_SQL)
                .bind(serde_json::to_string(&result)?)
                .bind(serde_json::to_string(&saved)?)
                .bind(row.try_get::<String, _>("local_date")?)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DROP TABLE ownership_removal_pending")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn ready(&self) -> Result<()> {
        sqlx::query_scalar::<_, i64>("SELECT 1")
            .fetch_one(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn counts(&self) -> Result<(i64, i64, i64)> {
        let mut tx = self.pool.begin().await?;
        let revision = state(&mut tx).await?.0;
        let row = sqlx::query(COUNTS_SQL).fetch_one(&mut *tx).await?;
        let counts = (revision, row.try_get("active")?, row.try_get("archived")?);
        tx.commit().await?;
        Ok(counts)
    }

    pub async fn item(&self, id: &str) -> Result<Item> {
        let row = sqlx::query(ITEM_SQL)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        decode_item(&row)
    }

    pub async fn active_items(&self) -> Result<Vec<CatalogItem>> {
        let rows = sqlx::query(ACTIVE_ITEMS_SQL).fetch_all(&self.pool).await?;
        rows.iter()
            .map(|row| Ok(serde_json::from_str(row.try_get("body")?)?))
            .collect()
    }

    pub async fn list(&self, query: ListQuery) -> Result<ItemsPage> {
        let limit = query.limit.unwrap_or(50);
        if !(1..=100).contains(&limit) {
            return Err(StoreError::Invalid("invalid_limit"));
        }
        let mut filters = query.filters();
        validate_filters(&filters)?;
        let cursor = query.page_token.as_deref().map(decode_cursor).transpose()?;
        if let Some(cursor) = &cursor {
            if filters != ItemFilters::default() && filters != cursor.filters {
                return Err(StoreError::Invalid("invalid_page_token"));
            }
            filters = cursor.filters.clone();
        }
        let mut tx = self.pool.begin().await?;
        let revision = state(&mut tx).await?.0;
        let tracking_revision = if filters.uses_tracking() {
            Some(
                sqlx::query_scalar::<_, i64>(
                    "SELECT revision FROM tracking_state WHERE singleton=1",
                )
                .fetch_one(&mut *tx)
                .await?,
            )
        } else {
            None
        };
        if cursor.as_ref().is_some_and(|cursor| {
            cursor.revision != revision || cursor.tracking_revision != tracking_revision
        }) {
            return Err(StoreError::Conflict("page_invalidated"));
        }
        // A revision-bound ID still owns the same sort position. Resolve it in
        // this snapshot instead of duplicating large names/brands in the URL.
        let position = if let Some(cursor) = &cursor {
            Some(
                sqlx::query_as::<_, CursorPosition>(CURSOR_POSITION_SQL)
                    .bind(&cursor.id)
                    .fetch_optional(&mut *tx)
                    .await?
                    .ok_or(StoreError::Invalid("invalid_page_token"))?,
            )
        } else {
            None
        };
        let scope_total = sqlx::query_scalar(SCOPE_COUNT_SQL)
            .fetch_one(&mut *tx)
            .await?;
        let mut count = count_selection(&filters);
        let total = count
            .build_query_scalar::<i64>()
            .fetch_one(&mut *tx)
            .await?;
        let mut select = selection(&filters, position.as_ref(), limit);
        let mut rows = select.build().fetch_all(&mut *tx).await?;
        let has_more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let next_page = if has_more {
            rows.last()
                .map(|row| -> Result<String> {
                    encode_cursor(&Cursor {
                        revision,
                        tracking_revision,
                        filters: filters.clone(),
                        id: row.try_get("id")?,
                    })
                })
                .transpose()?
        } else {
            None
        };
        let items = rows.iter().map(decode_item).collect::<Result<Vec<_>>>()?;
        let variant_ids: BTreeSet<&str> = items
            .iter()
            .filter_map(|item| {
                item.catalog
                    .identity
                    .as_ref()
                    .map(|identity| identity.variant_id.as_str())
            })
            .collect();
        let variant_groups = variant_groups(&mut tx, &variant_ids).await?;
        let brands = sqlx::query_scalar(BRANDS_SQL).fetch_all(&mut *tx).await?;
        let categories = sqlx::query_scalar(CATEGORIES_SQL)
            .fetch_all(&mut *tx)
            .await?;
        // Facets describe the whole active projection, not just this page or
        // its selected filters. Source remains derived from authoritative JSON.
        let mut select_sources = source_facets();
        let sources = select_sources
            .build()
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .map(|row| -> Result<SourceFacet> {
                let source = row
                    .try_get::<String, _>("source")?
                    .parse::<Source>()
                    .map_err(|_| sqlx::Error::Decode("stored source is invalid".into()))?;
                let label = match &source {
                    Source::Shop(_) => row.try_get("shop_label")?,
                    Source::Purchase(value) if value == "self_bought" => "Self-bought".into(),
                    Source::Purchase(value) => value.clone(),
                };
                Ok(SourceFacet {
                    source,
                    label,
                    count: row.try_get("count")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        tx.commit().await?;
        Ok(ItemsPage {
            items,
            variant_groups,
            next_page,
            total,
            scope_total,
            facets: Facets {
                brands,
                categories,
                sources,
            },
            catalog_revision: revision,
            tracking_revision,
        })
    }

    pub async fn sync(
        &self,
        request: SyncRequest,
        expected: Option<i64>,
        actor: &str,
        key: Option<&str>,
    ) -> Result<SyncReport> {
        self.sync_fenced(request, expected, actor, key, None).await
    }

    pub async fn sync_fenced(
        &self,
        request: SyncRequest,
        expected: Option<i64>,
        actor: &str,
        key: Option<&str>,
        fence: Option<&FenceToken>,
    ) -> Result<SyncReport> {
        self.sync_fenced_at(request, expected, actor, key, fence, Utc::now())
            .await
    }

    /// Replay and tests supply time without opening or modifying a live store.
    pub async fn sync_at(
        &self,
        request: SyncRequest,
        expected: Option<i64>,
        actor: &str,
        key: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<SyncReport> {
        self.sync_fenced_at(request, expected, actor, key, None, now)
            .await
    }

    async fn sync_fenced_at(
        &self,
        request: SyncRequest,
        expected: Option<i64>,
        actor: &str,
        key: Option<&str>,
        fence: Option<&FenceToken>,
        now: DateTime<Utc>,
    ) -> Result<SyncReport> {
        let catalog = validate_catalog(request.items)?;
        if catalog.items().is_empty() && !request.allow_empty && !request.dry_run {
            return Err(StoreError::Invalid("empty_catalog_requires_allow_empty"));
        }
        let input_hash = hash(&serde_json::to_vec(&(
            catalog.items(),
            request.allow_empty,
        ))?);
        let mut tx = if request.dry_run {
            self.pool.begin().await?
        } else {
            self.pool.begin_with("BEGIN IMMEDIATE").await?
        };
        if !request.dry_run {
            check_sync_fence(&mut tx, actor, fence).await?;
            let key = key.ok_or(StoreError::Invalid("idempotency_key_required"))?;
            if let Some(row) = operation(&mut tx, "sync", actor, key).await? {
                if row.try_get::<String, _>("request_hash")? != input_hash {
                    return Err(StoreError::Conflict("idempotency_conflict"));
                }
                let report = serde_json::from_str(row.try_get("response")?)?;
                tx.commit().await?;
                return Ok(report);
            }
        }
        let (revision, previous_hash) = state(&mut tx).await?;
        if !request.dry_run && expected.ok_or(StoreError::Precondition)? != revision {
            return Err(StoreError::Revision);
        }
        let mut reconcile = reconciliation(catalog.items());
        let rows = reconcile.build().fetch_all(&mut *tx).await?;
        let existing: BTreeMap<String, (String, bool)> = rows
            .into_iter()
            .map(|row| {
                Ok((
                    row.try_get("id")?,
                    (row.try_get("body")?, row.try_get("archived")?),
                ))
            })
            .collect::<std::result::Result<_, sqlx::Error>>()?;
        let mut report = SyncReport {
            catalog_revision: revision,
            source_hash: catalog.report().source_hash.clone(),
            created: 0,
            updated: 0,
            archived: 0,
            unchanged: 0,
        };
        let source: BTreeMap<&str, &CatalogItem> = catalog
            .items()
            .iter()
            .map(|item| (item.id.as_str(), item))
            .collect();
        for item in catalog.items() {
            let body = serde_json::to_string(item)?;
            match existing.get(&item.id) {
                None => report.created += 1,
                Some((old, archived)) if old != &body || *archived => report.updated += 1,
                Some(_) => report.unchanged += 1,
            }
        }
        report.archived = existing
            .iter()
            .filter(|(id, (_, archived))| !archived && !source.contains_key(id.as_str()))
            .count();
        if request.dry_run {
            tx.commit().await?;
            return Ok(report);
        }
        let changed = report.created + report.updated + report.archived > 0
            || previous_hash != report.source_hash;
        if changed {
            for item in catalog.items() {
                let body = serde_json::to_string(item)?;
                if existing
                    .get(&item.id)
                    .is_some_and(|(old, archived)| old == &body && !archived)
                {
                    continue;
                }
                sqlx::query("INSERT INTO items (id, body, brand, sort_brand, name, sort_name, category, sku, archived, revision, styling_hash) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0, 1, ?) ON CONFLICT(id) DO UPDATE SET body=excluded.body, brand=excluded.brand, sort_brand=excluded.sort_brand, name=excluded.name, sort_name=excluded.sort_name, category=excluded.category, sku=excluded.sku, archived=0, revision=items.revision+1, styling_hash=excluded.styling_hash")
                    .bind(&item.id).bind(body).bind(&item.brand).bind(item.brand.as_deref().unwrap_or("").to_lowercase())
                    .bind(&item.name).bind(item.name.to_lowercase()).bind(&item.category).bind(item.sku.as_deref().map(str::to_lowercase))
                    .bind(styling_hash(item)?)
                    .execute(&mut *tx).await?;
            }
            for (id, (_, archived)) in &existing {
                if !archived && !source.contains_key(id.as_str()) {
                    sqlx::query("UPDATE items SET archived=1, revision=revision+1 WHERE id=?")
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                }
            }
            report.catalog_revision += 1;
            sqlx::query("UPDATE catalog_state SET revision=?, source_hash=? WHERE singleton=1")
                .bind(report.catalog_revision)
                .bind(&report.source_hash)
                .execute(&mut *tx)
                .await?;
            // RFID identities survive metadata edits, archive and restoration.
            // Reconcile against this same committed snapshot, never a second
            // catalog copy or an unlocked follow-up transaction.
            crate::tracking::store::reconcile_catalog(&mut tx, now)
                .await
                .map_err(StoreError::from)?;
        }
        sqlx::query("INSERT INTO operations (scope, actor, key, request_hash, response) VALUES ('sync', ?, ?, ?, ?)")
            .bind(actor).bind(key).bind(input_hash).bind(serde_json::to_string(&report)?).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(report)
    }

    pub async fn reserve_generation(
        &self,
        inputs: TodayRequest,
        basis: GenerationBasis,
        actor: &str,
        key: &str,
    ) -> Result<Reservation> {
        inputs.validate().map_err(StoreError::Invalid)?;
        let input_hash = hash(&serde_json::to_vec(&inputs)?);
        let mut tx = self.pool.begin().await?;
        if let Some(row) = operation(&mut tx, "today", actor, key).await? {
            if row.try_get::<String, _>("request_hash")? != input_hash {
                return Err(StoreError::Conflict("idempotency_conflict"));
            }
            let id: String = row.try_get("resource_id")?;
            let generation = generation(&mut tx, &id).await?;
            tx.commit().await?;
            return Ok(Reservation {
                generation,
                fresh: false,
            });
        }
        if sqlx::query_scalar::<_, bool>(RUNNING_EXISTS_SQL)
            .fetch_one(&mut *tx)
            .await?
        {
            return Err(StoreError::Conflict("generation_in_progress"));
        }
        let generation = Generation {
            id: uuid::Uuid::now_v7().to_string(),
            status: GenerationStatus::Running,
            inputs,
            started_at: Utc::now(),
            finished_at: None,
            error_code: None,
        };
        sqlx::query("INSERT INTO generations (id,local_date,timezone,config_hash,status,body,basis,started_at) VALUES (?,?,?,?,'running',?,?,?)")
            .bind(&generation.id).bind(&basis.date).bind(&basis.timezone).bind(&basis.config_hash)
            .bind(serde_json::to_string(&generation)?).bind(serde_json::to_string(&basis)?)
            .bind(generation.started_at.to_rfc3339()).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO operations (scope,actor,key,request_hash,resource_id) VALUES ('today',?,?,?,?)")
            .bind(actor).bind(key).bind(input_hash).bind(&generation.id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Reservation {
            generation,
            fresh: true,
        })
    }

    pub async fn finish_generation(
        &self,
        id: &str,
        result: std::result::Result<TodayResult, &str>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let mut operation = generation(&mut tx, id).await?;
        if operation.status != GenerationStatus::Running {
            return Ok(());
        }
        let row = sqlx::query(GENERATION_BASIS_SQL)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        let basis: GenerationBasis = serde_json::from_str(row.try_get("basis")?)?;
        let outcome = match result {
            Ok(result)
                if result.inputs != operation.inputs || result.generation_id != operation.id =>
            {
                Err("invalid_model_output")
            }
            Ok(result) => match referenced_signatures(&basis.items, &result) {
                Err(StoreError::Invalid(code)) => Err(code),
                Err(error) => return Err(error),
                Ok(signatures) => {
                    let availability = crate::tracking::store::availability(&mut tx)
                        .await
                        .map_err(StoreError::from)?;
                    if !signatures
                        .keys()
                        .all(|id| availability.eligible_catalog_ids.binary_search(id).is_ok())
                    {
                        Err("referenced_items_unavailable")
                    } else if signatures_current(&mut tx, &signatures, &result.inputs).await?
                        == ReferencedItems::Current
                    {
                        sqlx::query("INSERT INTO today_results (local_date,timezone,config_hash,body,signatures) VALUES (?,?,?,?,?) ON CONFLICT(local_date) DO UPDATE SET timezone=excluded.timezone, config_hash=excluded.config_hash, body=excluded.body, signatures=excluded.signatures")
                        .bind(&basis.date).bind(&basis.timezone).bind(&basis.config_hash).bind(serde_json::to_string(&result)?)
                        .bind(serde_json::to_string(&signatures)?).execute(&mut *tx).await?;
                        Ok(())
                    } else {
                        Err("referenced_items_changed")
                    }
                }
            },
            Err(code) => Err(code),
        };
        operation.status = if outcome.is_ok() {
            GenerationStatus::Succeeded
        } else {
            GenerationStatus::Failed
        };
        operation.error_code = outcome.err().map(str::to_owned);
        operation.finished_at = Some(Utc::now());
        save_generation(&mut tx, &operation).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn today(
        &self,
        date: &str,
        timezone: &str,
        config_hash: &str,
        now: DateTime<Utc>,
        footwear_requirement: Option<SafetyRequirement>,
    ) -> Result<TodayResponse> {
        let mut tx = self.pool.begin().await?;
        let generation: Option<Generation> =
            sqlx::query_scalar::<_, String>(CURRENT_GENERATION_SQL)
                .bind(date)
                .bind(timezone)
                .fetch_optional(&mut *tx)
                .await?
                .map(|body| serde_json::from_str(&body))
                .transpose()?;
        let generation = generation
            .filter(|generation| generation.inputs.footwear_requirement == footwear_requirement);
        let row = sqlx::query(TODAY_RESULT_SQL)
            .bind(date)
            .fetch_optional(&mut *tx)
            .await?;
        let result = if let Some(row) = row {
            let mut result: TodayResult = serde_json::from_str(row.try_get("body")?)?;
            if result.inputs.footwear_requirement != footwear_requirement {
                None
            } else {
                let signatures: BTreeMap<String, String> =
                    serde_json::from_str(row.try_get("signatures")?)?;
                if row.try_get::<String, _>("config_hash")? != config_hash
                    || row.try_get::<String, _>("timezone")? != timezone
                {
                    result.stale_reasons.push("configuration_changed".into());
                }
                if result.expires_at <= now {
                    result.stale_reasons.push("expired".into());
                }
                let referenced = signatures_current(&mut tx, &signatures, &result.inputs).await?;
                if referenced != ReferencedItems::Current {
                    result.stale_reasons.push("referenced_items_changed".into());
                }
                let availability = crate::tracking::store::availability(&mut tx)
                    .await
                    .map_err(StoreError::from)?;
                if !signatures
                    .keys()
                    .all(|id| availability.eligible_catalog_ids.binary_search(id).is_ok())
                {
                    result
                        .stale_reasons
                        .push("referenced_items_unavailable".into());
                }
                result.stale_reasons.sort();
                result.stale_reasons.dedup();
                if !result.stale_reasons.is_empty() {
                    result.freshness = Freshness::Stale;
                }
                // A stale styling choice may still be useful, but safety-work items
                // must never remain offered through an older cached outfit.
                (referenced != ReferencedItems::Restricted).then_some(result)
            }
        } else {
            None
        };
        tx.commit().await?;
        Ok(TodayResponse {
            date: date.into(),
            timezone: timezone.into(),
            result,
            generation,
        })
    }

    async fn interrupt_running(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query_scalar::<_, String>(RUNNING_GENERATIONS_SQL)
            .fetch_all(&mut *tx)
            .await?;
        for body in rows {
            let mut generation: Generation = serde_json::from_str(&body)?;
            generation.status = GenerationStatus::Interrupted;
            generation.finished_at = Some(Utc::now());
            generation.error_code = Some("interrupted".into());
            save_generation(&mut tx, &generation).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

#[derive(sqlx::FromRow)]
struct FenceRow {
    token_hash: String,
    actor: String,
    active: bool,
    pending: String,
    uploads: String,
}
impl FenceRow {
    fn owns(&self, actor: &str, token: &FenceToken) -> bool {
        use subtle::ConstantTimeEq;
        self.actor == actor
            && bool::from(
                self.token_hash
                    .as_bytes()
                    .ct_eq(hash(token.as_str().as_bytes()).as_bytes()),
            )
    }
    fn images(&self) -> Result<Vec<ManagedImage>> {
        Ok(serde_json::from_str(&self.pending)?)
    }
    fn uploads(&self) -> Result<Vec<crate::images::ImageSha>> {
        Ok(serde_json::from_str(&self.uploads)?)
    }
}
async fn fence_row(connection: &mut SqliteConnection) -> Result<Option<FenceRow>> {
    Ok(sqlx::query_as(FENCE_READ_SQL)
        .fetch_optional(connection)
        .await?)
}
async fn owned_fence(
    connection: &mut SqliteConnection,
    actor: &str,
    token: &FenceToken,
) -> Result<FenceRow> {
    let row = fence_row(connection)
        .await?
        .ok_or(StoreError::Conflict("sync_fence_expired"))?;
    if !row.active || !row.owns(actor, token) {
        return Err(StoreError::Conflict("sync_fence_expired"));
    }
    Ok(row)
}
async fn check_sync_fence(
    connection: &mut SqliteConnection,
    actor: &str,
    token: Option<&FenceToken>,
) -> Result<()> {
    let row = fence_row(connection).await?;
    if let Some(token) = token {
        let row = row.ok_or(StoreError::Conflict("sync_fence_expired"))?;
        if !row.active || !row.owns(actor, token) {
            return Err(StoreError::Conflict("sync_fence_expired"));
        }
        if !row.images()?.is_empty() {
            return Err(StoreError::Conflict("cleanup_pending"));
        }
    } else if let Some(row) = row {
        if !row.images()?.is_empty() {
            return Err(StoreError::Conflict("cleanup_pending"));
        }
        if row.active {
            return Err(StoreError::Conflict("sync_in_progress"));
        }
    }
    Ok(())
}
impl Store {
    pub async fn acquire_fence(&self, request: FenceAcquire, actor: &str) -> Result<FenceStatus> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let revision = state(&mut tx).await?.0;
        if revision != request.expected_revision {
            return Err(StoreError::Revision);
        }
        let row = fence_row(&mut tx).await?;
        if row
            .as_ref()
            .is_some_and(|row| row.active && !row.owns(actor, &request.token))
        {
            return Err(StoreError::Conflict("sync_in_progress"));
        }
        // A takeover retains every already-admitted delete. Those exact IDs
        // stay fenced across client crashes, expiration, and app restarts.
        let pending = row
            .as_ref()
            .map(|row| row.images())
            .transpose()?
            .unwrap_or_default();
        let uploads = row
            .as_ref()
            .map(|row| row.uploads())
            .transpose()?
            .unwrap_or_default();
        sqlx::query(FENCE_ACQUIRE_SQL)
            .bind(hash(request.token.as_str().as_bytes()))
            .bind(actor)
            .bind(serde_json::to_string(&pending)?)
            .bind(serde_json::to_string(&uploads)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(FenceStatus {
            catalog_revision: revision,
            pending,
            uploads,
            valid_for_seconds: 300,
        })
    }
    pub async fn prepare_prune(
        &self,
        request: FencePrepare,
        actor: &str,
        date: &str,
    ) -> Result<FenceStatus> {
        if request.images.len() > 10_000
            || request
                .images
                .iter()
                .map(|image| &image.id)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != request.images.len()
        {
            return Err(StoreError::Invalid("invalid_prune_batch"));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = owned_fence(&mut tx, actor, &request.token).await?;
        let revision = state(&mut tx).await?.0;
        if revision != request.catalog_revision {
            return Err(StoreError::Revision);
        }
        let pending = row.images()?;
        if !pending.is_empty() && pending != request.images {
            return Err(StoreError::Conflict("cleanup_pending"));
        }
        // One bounded proof covers the whole deletion batch, avoiding a full
        // catalog scan for each native request. The fence blocks every catalog
        // writer until these IDs are resolved. New Today results reference
        // active items, so cannot introduce an excluded archived reference.
        let rows = sqlx::query_scalar::<_, String>(LIVE_IMAGE_ITEMS_SQL)
            .bind(date)
            .fetch_all(&mut *tx)
            .await?;
        let mut referenced = std::collections::BTreeSet::new();
        for body in rows {
            let item: CatalogItem = serde_json::from_str(&body)?;
            referenced.extend(
                item.images
                    .iter()
                    .filter_map(|image| referenced_image_id(&image.url)),
            );
        }
        if request
            .images
            .iter()
            .any(|image| referenced.contains(&image.id))
        {
            return Err(StoreError::Conflict("image_referenced"));
        }
        sqlx::query(FENCE_PENDING_SQL)
            .bind(serde_json::to_string(&request.images)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(FenceStatus {
            catalog_revision: revision,
            pending: request.images,
            uploads: row.uploads()?,
            valid_for_seconds: 300,
        })
    }
    pub async fn resolve_pruned(&self, request: FenceResolve, actor: &str) -> Result<FenceStatus> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = owned_fence(&mut tx, actor, &request.token).await?;
        let mut pending = row.images()?;
        if pending
            .iter()
            .any(|image| image.id == request.image.id && image.sha256 != request.image.sha256)
        {
            return Err(StoreError::Conflict("invalid_prune_image"));
        }
        pending.retain(|image| image != &request.image);
        sqlx::query(FENCE_PENDING_SQL)
            .bind(serde_json::to_string(&pending)?)
            .execute(&mut *tx)
            .await?;
        let revision = state(&mut tx).await?.0;
        tx.commit().await?;
        Ok(FenceStatus {
            catalog_revision: revision,
            pending,
            uploads: row.uploads()?,
            valid_for_seconds: 300,
        })
    }
    pub async fn upload_claim(
        &self,
        request: FenceUpload,
        actor: &str,
    ) -> Result<UploadClaimStatus> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = owned_fence(&mut tx, actor, &request.token).await?;
        let mut uploads = row.uploads()?;
        let existing = uploads.contains(&request.sha256);
        let claimed = matches!(request.action, UploadClaimAction::Begin) && !existing;
        match request.action {
            UploadClaimAction::Begin if !existing => {
                if uploads.len() >= 10_000 {
                    return Err(StoreError::Conflict("image_upload_pending"));
                }
                uploads.push(request.sha256);
            }
            UploadClaimAction::Begin => {}
            UploadClaimAction::Verified | UploadClaimAction::Rejected => {
                uploads.retain(|sha| sha != &request.sha256)
            }
        }
        sqlx::query(FENCE_UPLOADS_SQL)
            .bind(serde_json::to_string(&uploads)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(UploadClaimStatus { claimed })
    }
    pub async fn release_fence(&self, token: &FenceToken, actor: &str) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(row) = fence_row(&mut tx).await? {
            if !row.owns(actor, token) {
                return Err(StoreError::Conflict("sync_fence_expired"));
            }
            if !row.images()?.is_empty() {
                return Err(StoreError::Conflict("cleanup_pending"));
            }
            sqlx::query(FENCE_RELEASE_SQL).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

async fn state(connection: &mut SqliteConnection) -> Result<(i64, String)> {
    let row = sqlx::query(STATE_SQL).fetch_one(connection).await?;
    Ok((row.try_get("revision")?, row.try_get("source_hash")?))
}
async fn operation(
    connection: &mut SqliteConnection,
    scope: &str,
    actor: &str,
    key: &str,
) -> Result<Option<SqliteRow>> {
    Ok(sqlx::query(OPERATION_SQL)
        .bind(scope)
        .bind(actor)
        .bind(key)
        .fetch_optional(connection)
        .await?)
}
async fn generation(connection: &mut SqliteConnection, id: &str) -> Result<Generation> {
    let body: String = sqlx::query_scalar(GENERATION_SQL)
        .bind(id)
        .fetch_optional(connection)
        .await?
        .ok_or(StoreError::NotFound)?;
    Ok(serde_json::from_str(&body)?)
}
async fn save_generation(connection: &mut SqliteConnection, generation: &Generation) -> Result<()> {
    let status = match generation.status {
        GenerationStatus::Running => "running",
        GenerationStatus::Succeeded => "succeeded",
        GenerationStatus::Failed => "failed",
        GenerationStatus::Interrupted => "interrupted",
    };
    sqlx::query("UPDATE generations SET body=?, status=? WHERE id=?")
        .bind(serde_json::to_string(generation)?)
        .bind(status)
        .bind(&generation.id)
        .execute(connection)
        .await?;
    Ok(())
}
fn decode_item(row: &SqliteRow) -> Result<Item> {
    Ok(Item {
        catalog: serde_json::from_str(row.try_get("body")?)?,
        revision: row.try_get("revision")?,
        archived: row.try_get("archived")?,
    })
}
fn validate_filters(filters: &ItemFilters) -> Result<()> {
    if filters
        .source
        .as_ref()
        .is_some_and(|source| !source.is_valid())
        || filters
            .q
            .as_ref()
            .is_some_and(|value| value.len() > 256 || value.chars().any(char::is_control))
        || filters
            .brand
            .as_ref()
            .is_some_and(|value| value.len() > 1024)
        || filters
            .category
            .as_ref()
            .is_some_and(|value| value.len() > 200)
    {
        return Err(StoreError::Invalid("invalid_filter"));
    }
    Ok(())
}
fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}
fn add_filters(query: &mut QueryBuilder<Sqlite>, filters: &ItemFilters) {
    if let Some(q) = filters.q.as_deref().filter(|q| !q.trim().is_empty()) {
        let pattern = format!("%{}%", escape_like(&q.to_lowercase()));
        query
            .push(" AND (sort_brand LIKE ")
            .push_bind(pattern.clone())
            .push(" ESCAPE '\\' OR sort_name LIKE ")
            .push_bind(pattern.clone())
            .push(" ESCAPE '\\' OR coalesce(sku,'') LIKE ")
            .push_bind(pattern)
            .push(" ESCAPE '\\')");
    }
    if let Some(brand) = &filters.brand {
        query
            .push(" AND sort_brand=")
            .push_bind(brand.to_lowercase());
    }
    if let Some(category) = &filters.category {
        query
            .push(" AND (category=")
            .push_bind(category.clone())
            .push(" OR category LIKE ")
            .push_bind(format!("{}.%", escape_like(category)))
            .push(" ESCAPE '\\')");
    }
    if let Some(source) = &filters.source {
        query
            .push(" AND (")
            .push(SOURCE_SQL)
            .push(")=")
            .push_bind(source.to_string());
    }
    if filters.ready_to_wear == Some(false) {
        // Negative readiness describes the whole catalog record: no active
        // copy is ready, even if another filter selects an unavailable copy.
        query.push(" AND NOT EXISTS (");
        query.push("SELECT 1 FROM tracking_units AS u WHERE u.catalog_id=items.id AND u.retired=0 AND u.archived=0 AND u.baseline_at IS NOT NULL AND u.status IN ('clean_ready','worn_ready'))");
    }
    if filters.ready_to_wear == Some(true) || filters.status.is_some() || filters.area.is_some() {
        query.push(" AND EXISTS (SELECT 1 FROM tracking_units AS u WHERE u.catalog_id=items.id AND u.retired=0 AND u.archived=0");
        if filters.ready_to_wear == Some(true) {
            query.push(
                " AND u.baseline_at IS NOT NULL AND u.status IN ('clean_ready','worn_ready')",
            );
        }
        if let Some(status) = filters.status {
            query.push(" AND u.status=").push_bind(status.as_str());
        }
        if let Some(area) = &filters.area {
            query
                .push(" AND u.current_area=")
                .push_bind(area.to_string());
        }
        query.push(")");
    }
}
fn source_facets() -> QueryBuilder<Sqlite> {
    let mut query = QueryBuilder::<Sqlite>::new("SELECT ");
    query
        .push(SOURCE_SQL)
        .push(" AS source, MIN(json_extract(body, '$.shop.label')) AS shop_label, COUNT(*) AS count FROM items WHERE archived=0 GROUP BY source HAVING source IS NOT NULL ORDER BY source");
    query
}
fn count_selection(filters: &ItemFilters) -> QueryBuilder<Sqlite> {
    let mut query = QueryBuilder::<Sqlite>::new(SCOPE_COUNT_SQL);
    add_filters(&mut query, filters);
    query
}
fn selection(
    filters: &ItemFilters,
    cursor: Option<&CursorPosition>,
    limit: u32,
) -> QueryBuilder<Sqlite> {
    let mut select = QueryBuilder::<Sqlite>::new(
        "SELECT id, body, revision, archived FROM items WHERE archived = 0",
    );
    add_filters(&mut select, filters);
    if let Some(cursor) = cursor {
        select
            .push(" AND (sort_brand, sort_name, id) > (")
            .push_bind(cursor.brand.clone())
            .push(", ")
            .push_bind(cursor.name.clone())
            .push(", ")
            .push_bind(cursor.id.clone())
            .push(")");
    }
    select
        .push(" ORDER BY sort_brand, sort_name, id LIMIT ")
        .push_bind(i64::from(limit) + 1);
    select
}
fn reconciliation(items: &[CatalogItem]) -> QueryBuilder<Sqlite> {
    // Reconciliation needs the active catalog and only source IDs being restored,
    // never unrelated archived records or their historical JSON bodies.
    let mut query = QueryBuilder::<Sqlite>::new(
        "SELECT id, CASE WHEN archived=0 THEN body ELSE '' END AS body, archived FROM items WHERE archived=0",
    );
    if !items.is_empty() {
        query.push(" OR id IN (");
        let mut values = query.separated(", ");
        for item in items {
            values.push_bind(item.id.clone());
        }
        values.push_unseparated(")");
    }
    query
}
fn encode_cursor(cursor: &Cursor) -> Result<String> {
    let token = URL_SAFE_NO_PAD.encode(serde_json::to_vec(cursor)?);
    if token.len() > MAX_PAGE_TOKEN_BYTES {
        return Err(StoreError::Invalid("invalid_filter"));
    }
    Ok(token)
}
fn decode_cursor(token: &str) -> Result<Cursor> {
    if token.len() > MAX_PAGE_TOKEN_BYTES {
        return Err(StoreError::Invalid("invalid_page_token"));
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(token)
        .map_err(|_| StoreError::Invalid("invalid_page_token"))?;
    let cursor: Cursor =
        serde_json::from_slice(&bytes).map_err(|_| StoreError::Invalid("invalid_page_token"))?;
    validate_filters(&cursor.filters)?;
    Ok(cursor)
}
fn referenced_signatures(
    items: &[CatalogItem],
    result: &TodayResult,
) -> Result<BTreeMap<String, String>> {
    let candidates: BTreeMap<&str, &CatalogItem> =
        items.iter().map(|item| (item.id.as_str(), item)).collect();
    let mut signatures = BTreeMap::new();
    for id in result
        .suggestions
        .iter()
        .flat_map(|suggestion| &suggestion.item_ids)
    {
        let item = candidates
            .get(id.as_str())
            .ok_or(StoreError::Invalid("invalid_model_output"))?;
        if !crate::today::eligible(item, &result.inputs) {
            return Err(StoreError::Invalid("invalid_model_output"));
        }
        signatures.insert(id.clone(), styling_hash(item)?);
    }
    Ok(signatures)
}
#[derive(PartialEq, Eq)]
enum ReferencedItems {
    Current,
    Changed,
    Restricted,
}
async fn signatures_current(
    connection: &mut SqliteConnection,
    signatures: &BTreeMap<String, String>,
    inputs: &TodayRequest,
) -> Result<ReferencedItems> {
    if signatures.is_empty() {
        return Ok(ReferencedItems::Current);
    }
    let mut select = signature_selection(signatures);
    let rows = select.build().fetch_all(connection).await?;
    let mut current = rows.len() == signatures.len();
    for row in rows {
        let item: CatalogItem = serde_json::from_str(row.try_get("body")?)?;
        if !crate::today::eligible(&item, inputs) {
            return Ok(ReferencedItems::Restricted);
        }
        let id: String = row.try_get("id")?;
        if signatures.get(&id) != Some(&row.try_get::<String, _>("styling_hash")?)
            || row.try_get::<bool, _>("archived")?
        {
            current = false;
        }
    }
    Ok(if current {
        ReferencedItems::Current
    } else {
        ReferencedItems::Changed
    })
}
fn signature_selection(signatures: &BTreeMap<String, String>) -> QueryBuilder<Sqlite> {
    let mut select = QueryBuilder::<Sqlite>::new(
        "SELECT id, body, styling_hash, archived FROM items WHERE id IN (",
    );
    let mut values = select.separated(", ");
    for id in signatures.keys() {
        values.push_bind(id.clone());
    }
    values.push_unseparated(")");
    select
}

fn variant_copy_selection(variant_ids: &BTreeSet<&str>) -> QueryBuilder<Sqlite> {
    let mut select = QueryBuilder::<Sqlite>::new(
        "SELECT id, revision, json_extract(body, '$.identity') AS identity, json_extract(body, '$.size') AS size, json_extract(body, '$.quantity') AS quantity, json_extract(body, '$.purchase_source') AS purchase_source, json_extract(body, '$.shop') AS shop, json_extract(body, '$.usage') AS usage FROM items WHERE archived=0 AND json_extract(body, '$.identity.variant_id') IN (",
    );
    let mut values = select.separated(", ");
    for id in variant_ids {
        values.push_bind((*id).to_owned());
    }
    values.push_unseparated(
        ") ORDER BY json_extract(body, '$.identity.variant_id'), sort_brand, sort_name, id LIMIT ",
    );
    select.push((MAX_CATALOG_ITEMS + 1).to_string());
    select
}

async fn variant_groups(
    connection: &mut SqliteConnection,
    variant_ids: &BTreeSet<&str>,
) -> Result<Vec<VariantGroup>> {
    if variant_ids.is_empty() {
        return Ok(Vec::new());
    }
    // The page chooses variants; one bounded read fills every active purchase
    // under the same revision. Source/readiness filters never truncate a group.
    let mut query = variant_copy_selection(variant_ids);
    let rows = query.build().fetch_all(connection).await?;
    if rows.len() > MAX_CATALOG_ITEMS {
        return Err(StoreError::Invalid("catalog_too_large"));
    }
    let mut groups = BTreeMap::<String, VariantGroup>::new();
    for row in rows {
        let identity: CatalogIdentity = serde_json::from_str(row.try_get("identity")?)?;
        let group = groups
            .entry(identity.variant_id.clone())
            .or_insert_with(|| VariantGroup {
                identity,
                quantity: Some(0),
                copies: Vec::new(),
            });
        let quantity: Option<i64> = row.try_get("quantity")?;
        let quantity = quantity
            .map(u32::try_from)
            .transpose()
            .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
        let shop: Option<String> = row.try_get("shop")?;
        let usage: Option<String> = row.try_get("usage")?;
        group.quantity = group
            .quantity
            .zip(quantity)
            .map(|(total, quantity)| total + u64::from(quantity));
        group.copies.push(InventoryCopy {
            id: row.try_get("id")?,
            size: row.try_get("size")?,
            quantity,
            purchase_source: row.try_get("purchase_source")?,
            shop: shop.as_deref().map(serde_json::from_str).transpose()?,
            usage: usage
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?
                .unwrap_or_default(),
            revision: row.try_get("revision")?,
        });
    }
    Ok(groups.into_values().collect())
}

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, collections::BTreeMap, path::Path};

    use chrono::{Duration, Utc};
    use sqlx::{
        Connection, Execute, QueryBuilder, Row, SqliteConnection, sqlite::SqliteConnectOptions,
    };

    use crate::{
        models::{
            CatalogItem, Freshness, Generation, GenerationStatus, ItemFilters, ListQuery, Occasion,
            Shop, ShopSeason, Source, Suggestion, SyncReport, SyncRequest, TodayRequest,
            TodayResult, WeatherMode, parse_catalog,
        },
        store::{
            ACTIVE_ITEMS_SQL, BRANDS_SQL, CATEGORIES_SQL, COUNTS_SQL, CURRENT_GENERATION_SQL,
            CURSOR_POSITION_SQL, ConditionCatalogItem, GENERATION_BASIS_SQL, GENERATION_SQL,
            GenerationBasis, ITEM_SQL, OPERATION_SQL, PreviousCatalogItem, PreviousScope,
            PreviousTodayRequest, RUNNING_EXISTS_SQL, RUNNING_GENERATIONS_SQL, SCOPE_COUNT_SQL,
            STATE_SQL, Store, StoreError, TODAY_RESULT_SQL, count_selection, hash, reconciliation,
            selection, signature_selection, source_facets,
        },
    };

    fn example(index: usize) -> CatalogItem {
        CatalogItem {
            id: format!("example-{index:04}"),
            brand: Some(format!("Label {}", index % 20)),
            name: format!("Piece {index:04}"),
            category: if index.is_multiple_of(2) {
                "clothing.top".into()
            } else {
                "footwear.shoes".into()
            },
            color: None,
            size: None,
            quantity: Some(1),
            usage: crate::models::Usage::Everyday,
            identity: None,
            material: None,
            sku: None,
            purchase_source: match index % 4 {
                1 => Some("self_bought".into()),
                2 => Some("Example gift".into()),
                3 => Some("Example exchange".into()),
                _ => None,
            },
            images: Vec::new(),
            notes: Vec::new(),
            price: None,
            shop: index.is_multiple_of(4).then(|| Shop {
                id: format!("example-shop-{}", index % 20),
                label: format!("Example shop {}", index % 20),
                season: Some(ShopSeason::SpringSummer),
                years: vec![2026],
            }),
        }
    }

    async fn previous_database(path: &Path) -> SqliteConnection {
        let mut connection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true),
        )
        .await
        .unwrap();
        // Apply the actual unchanged 0.1.2 migration, including SQLx's migration
        // history and checksum, before writing its persisted wire shapes.
        let mut previous = sqlx::migrate!("./migrations");
        previous.migrations = Cow::Owned(
            previous
                .iter()
                .filter(|migration| migration.version == 1)
                .cloned()
                .collect(),
        );
        previous.run(&mut connection).await.unwrap();
        let mut first = example(0);
        first.category = "clothing.dress".into();
        let items = [first, example(1), example(2)];
        let previous: Vec<_> = items
            .into_iter()
            .enumerate()
            .map(|(index, item)| PreviousCatalogItem {
                item,
                current_ownership_confirmed: index.is_multiple_of(2),
                condition_at_purchase: Some("Example condition".into()),
            })
            .collect();
        for (index, item) in previous.iter().enumerate() {
            sqlx::query("INSERT INTO items (id,body,brand,sort_brand,name,sort_name,category,sku,confirmed,archived,revision,styling_hash) VALUES (?,?,?,?,?,?,?,?,?,?,4,?)")
                .bind(&item.item.id).bind(serde_json::to_string(item).unwrap())
                .bind(&item.item.brand).bind(item.item.brand.as_deref().unwrap_or("").to_lowercase())
                .bind(&item.item.name).bind(item.item.name.to_lowercase()).bind(&item.item.category)
                .bind(&item.item.sku).bind(item.current_ownership_confirmed).bind(index == 2)
                .bind(format!("previous-style-{index}"))
                .execute(&mut connection).await.unwrap();
        }
        let active = &previous[..2];
        let source_hash = hash(&serde_json::to_vec(active).unwrap());
        sqlx::query("UPDATE catalog_state SET revision=7,source_hash=?")
            .bind(&source_hash)
            .execute(&mut connection)
            .await
            .unwrap();
        let report = SyncReport {
            catalog_revision: 7,
            source_hash,
            created: 2,
            updated: 0,
            archived: 0,
            unchanged: 0,
        };
        for (key, request_hash, response) in [
            (
                "current",
                hash(&serde_json::to_vec(&(active, false)).unwrap()),
                report.clone(),
            ),
            (
                "unrecoverable",
                "previous-unrecoverable-request-hash".into(),
                SyncReport {
                    source_hash: "previous-unrecoverable-source-hash".into(),
                    catalog_revision: 2,
                    ..report.clone()
                },
            ),
        ] {
            sqlx::query("INSERT INTO operations (scope,actor,key,request_hash,response) VALUES ('sync','example-owner',?,?,?)")
                .bind(key).bind(request_hash).bind(serde_json::to_string(&response).unwrap())
                .execute(&mut connection).await.unwrap();
        }
        let now = Utc::now();
        let date = now.date_naive().to_string();
        for (id, status, scope, weather, offset) in [
            (
                "previous-succeeded",
                GenerationStatus::Succeeded,
                PreviousScope::ConfirmedOnly,
                WeatherMode::Ignored,
                0,
            ),
            (
                "previous-running",
                GenerationStatus::Running,
                PreviousScope::AllInventory,
                WeatherMode::Required,
                1,
            ),
        ] {
            let inputs = PreviousTodayRequest {
                occasion: Occasion::Everyday,
                ownership_scope: scope,
                weather_mode: weather,
            };
            let generation = Generation {
                id: id.into(),
                status: status.clone(),
                inputs: TodayRequest {
                    occasion: inputs.occasion,
                    weather_mode: weather,
                    footwear_requirement: None,
                },
                started_at: now + Duration::seconds(offset),
                finished_at: (status == GenerationStatus::Succeeded).then_some(now),
                error_code: None,
            };
            let mut snapshot = active.to_vec();
            if status == GenerationStatus::Running {
                snapshot[0].item.name = "Before a historical edit".into();
                let report = SyncReport {
                    catalog_revision: 3,
                    source_hash: hash(&serde_json::to_vec(&snapshot).unwrap()),
                    created: 2,
                    updated: 0,
                    archived: 0,
                    unchanged: 0,
                };
                sqlx::query("INSERT INTO operations (scope,actor,key,request_hash,response) VALUES ('sync','example-owner','generation-snapshot',?,?)")
                    .bind(hash(&serde_json::to_vec(&(&snapshot,false)).unwrap()))
                    .bind(serde_json::to_string(&report).unwrap()).execute(&mut connection).await.unwrap();
            }
            let basis = GenerationBasis {
                date: date.clone(),
                timezone: "UTC".into(),
                config_hash: "example-config".into(),
                items: snapshot
                    .iter()
                    .map(|previous| previous.item.clone())
                    .collect(),
            };
            sqlx::query("INSERT INTO generations (id,local_date,timezone,config_hash,status,body,basis,started_at) VALUES (?,?,'UTC','example-config',?,json_set(?,'$.inputs.ownership_scope',?),json_set(?,'$.items',json(?)),?)")
                .bind(id).bind(&date).bind(if status == GenerationStatus::Running { "running" } else { "succeeded" })
                .bind(serde_json::to_string(&generation).unwrap()).bind(match inputs.ownership_scope { PreviousScope::AllInventory => "all_inventory", PreviousScope::ConfirmedOnly => "confirmed_only" })
                .bind(serde_json::to_string(&basis).unwrap()).bind(serde_json::to_string(&snapshot).unwrap())
                .bind(generation.started_at.to_rfc3339()).execute(&mut connection).await.unwrap();
            sqlx::query("INSERT INTO operations (scope,actor,key,request_hash,resource_id) VALUES ('today','example-owner',?,?,?)")
                .bind(id).bind(hash(&serde_json::to_vec(&inputs).unwrap())).bind(id).execute(&mut connection).await.unwrap();
        }
        let result = TodayResult {
            generation_id: "previous-succeeded".into(),
            inputs: TodayRequest {
                occasion: Occasion::Everyday,
                weather_mode: WeatherMode::Ignored,
                footwear_requirement: None,
            },
            generated_at: now,
            expires_at: now + Duration::hours(6),
            freshness: Freshness::Ready,
            stale_reasons: Vec::new(),
            weather: None,
            suggestions: vec![Suggestion {
                occasion: None,
                label: "An example outfit".into(),
                item_ids: active
                    .iter()
                    .map(|previous| previous.item.id.clone())
                    .collect(),
                advisory_reason: "A fictional combination.".into(),
                warnings: vec!["unconfirmed_ownership".into(), "unknown_quantity".into()],
            }],
        };
        let signatures = BTreeMap::from([
            ("example-0000", "previous-style-0"),
            ("example-0001", "previous-style-1"),
        ]);
        for (date, signatures) in [
            (date, signatures.clone()),
            (
                (now - Duration::days(1)).date_naive().to_string(),
                BTreeMap::from([("example-0000", "already-stale-signature")]),
            ),
        ] {
            sqlx::query("INSERT INTO today_results (local_date,timezone,config_hash,body,signatures) VALUES (?,'UTC','example-config',json_set(?,'$.inputs.ownership_scope','confirmed_only'),?)")
                .bind(date).bind(serde_json::to_string(&result).unwrap()).bind(serde_json::to_string(&signatures).unwrap()).execute(&mut connection).await.unwrap();
        }
        connection
    }

    #[test]
    fn previous_catalog_serializer_matches_the_released_digest() {
        let mut item = parse_catalog(include_bytes!("../tests/fixtures/catalog-seasons.json"))
            .unwrap()
            .into_items()
            .remove(0);
        // The released snapshot had an unknown count. Read compatibility must
        // preserve that fact, even though new writes now require quantities.
        item.quantity = None;
        let previous = PreviousCatalogItem {
            item,
            current_ownership_confirmed: false,
            condition_at_purchase: None,
        };
        let encoded = serde_json::to_vec(&vec![previous]).unwrap();
        assert_eq!(
            hash(&encoded),
            "4f82c38e155797950e58699ad3d33a7dd0a297d0681b73be51239cdec28be9cc"
        );
        let decoded: Vec<PreviousCatalogItem> = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded[0].item.id, "example-source");
        assert_eq!(decoded[0].item.quantity, None);
    }

    #[test]
    fn condition_serializer_matches_the_released_digest_without_guessing_missing_values() {
        let mut item = parse_catalog(include_bytes!("../tests/fixtures/catalog-seasons.json"))
            .unwrap()
            .into_items()
            .remove(0);
        item.quantity = None;
        let previous = ConditionCatalogItem {
            item,
            condition_at_purchase: Some(None),
        };
        let encoded = serde_json::to_vec(&vec![previous]).unwrap();
        assert_eq!(
            hash(&encoded),
            "6343da4793816f15138afa6c693be129a9cad5222e29f0f11d8aefe266e49cdc"
        );
        let decoded: Vec<ConditionCatalogItem> = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded[0].condition_at_purchase, Some(None));
        let current = serde_json::to_vec(&decoded[0].item).unwrap();
        let absent: ConditionCatalogItem = serde_json::from_slice(&current).unwrap();
        assert_eq!(absent.condition_at_purchase, None);
        let mut snapshots = BTreeMap::new();
        crate::store::remember_condition_snapshot(&[absent], &mut snapshots).unwrap();
        assert!(snapshots.is_empty());
    }

    async fn condition_database(path: &Path) -> Store {
        let pool = sqlx::SqlitePool::connect_with(
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true),
        )
        .await
        .unwrap();
        let mut previous = sqlx::migrate!("./migrations");
        previous.migrations = Cow::Owned(
            previous
                .iter()
                .filter(|migration| migration.version <= 2)
                .cloned()
                .collect(),
        );
        previous.run(&pool).await.unwrap();
        // The current writer needs an empty fence table while authoring this
        // historical fixture. Remove it below so the persisted database really
        // has only the old migrations when the upgrade is exercised.
        sqlx::raw_sql(include_str!("../migrations/0004_inventory_fence.sql"))
            .execute(&pool)
            .await
            .unwrap();
        // As with the fence, allow the current producer to author a real old
        // snapshot. Remove these temporary tables before exercising migration.
        sqlx::raw_sql(include_str!("../migrations/0005_tracking.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0006_tracking_history.sql"))
            .execute(&pool)
            .await
            .unwrap();
        // A fresh 0.1.6 store consumed the ownership marker before use.
        sqlx::query("DROP TABLE ownership_removal_pending")
            .execute(&pool)
            .await
            .unwrap();
        let store = Store {
            pool,
            tracking: std::sync::Arc::new(tokio::sync::Mutex::new(
                crate::tracking::engine::Engine::default(),
            )),
        };
        let mut items: Vec<_> = (0..3).map(example).collect();
        items[0].category = "clothing.dress".into();
        let old: Vec<_> = items
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, item)| ConditionCatalogItem {
                item,
                condition_at_purchase: Some((index != 0).then(|| "Example condition".into())),
            })
            .collect();
        for (key, count, revision) in [("historical", 3, 0), ("current", 2, 1)] {
            let mut report = store
                .sync(
                    SyncRequest {
                        items: items[..count].to_vec(),
                        dry_run: false,
                        allow_empty: false,
                    },
                    Some(revision),
                    "example-owner",
                    Some(key),
                )
                .await
                .unwrap();
            report.source_hash = hash(&serde_json::to_vec(&old[..count]).unwrap());
            sqlx::query(
                "UPDATE operations SET request_hash=?,response=? WHERE scope='sync' AND key=?",
            )
            .bind(hash(&serde_json::to_vec(&(&old[..count], false)).unwrap()))
            .bind(serde_json::to_string(&report).unwrap())
            .bind(key)
            .execute(&store.pool)
            .await
            .unwrap();
        }
        let now = Utc::now();
        let basis = GenerationBasis {
            date: now.date_naive().to_string(),
            timezone: "UTC".into(),
            config_hash: "example-config".into(),
            items: items.clone(),
        };
        let inputs = TodayRequest {
            occasion: Occasion::Everyday,
            weather_mode: WeatherMode::Ignored,
            footwear_requirement: None,
        };
        let generation = store
            .reserve_generation(inputs.clone(), basis.clone(), "example-owner", "saved")
            .await
            .unwrap()
            .generation;
        store
            .finish_generation(
                &generation.id,
                Ok(TodayResult {
                    generation_id: generation.id.clone(),
                    inputs: inputs.clone(),
                    generated_at: now,
                    expires_at: now + Duration::hours(6),
                    freshness: Freshness::Ready,
                    stale_reasons: Vec::new(),
                    weather: None,
                    suggestions: vec![Suggestion {
                        occasion: None,
                        label: "Example outfit".into(),
                        item_ids: vec![items[0].id.clone(), items[1].id.clone()],
                        advisory_reason: "An invented combination".into(),
                        warnings: Vec::new(),
                    }],
                }),
            )
            .await
            .unwrap();
        store
            .reserve_generation(inputs, basis, "example-owner", "running")
            .await
            .unwrap();
        for previous in &old {
            sqlx::query("UPDATE items SET body=? WHERE id=?")
                .bind(serde_json::to_string(previous).unwrap())
                .bind(&previous.item.id)
                .execute(&store.pool)
                .await
                .unwrap();
        }
        sqlx::query("UPDATE catalog_state SET source_hash=? WHERE singleton=1")
            .bind(hash(&serde_json::to_vec(&old[..2]).unwrap()))
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE generations SET basis=json_set(basis,'$.items',json(?))")
            .bind(serde_json::to_string(&old).unwrap())
            .execute(&store.pool)
            .await
            .unwrap();
        let report = SyncReport {
            catalog_revision: 1,
            source_hash: "unrecoverable-source-hash".into(),
            created: 1,
            updated: 0,
            archived: 0,
            unchanged: 0,
        };
        sqlx::query("INSERT INTO operations (scope,actor,key,request_hash,response) VALUES ('sync','example-owner','unrecoverable','unrecoverable-request-hash',?)")
            .bind(serde_json::to_string(&report).unwrap()).execute(&store.pool).await.unwrap();
        sqlx::query("DROP TABLE inventory_fence")
            .execute(&store.pool)
            .await
            .unwrap();
        for query in [
            "DROP TABLE tracking_coverage",
            "DROP TABLE tracking_bindings",
            "DROP TABLE tracking_history",
            "DROP TABLE tracking_sessions",
            "DROP TABLE tracking_units",
            "DROP TABLE tracking_inventory",
            "DROP TABLE tracking_reader_operations",
            "DROP TABLE tracking_reader_controls",
            "DROP TABLE tracking_configurations",
            "DROP TABLE tracking_state",
        ] {
            sqlx::query(query).execute(&store.pool).await.unwrap();
        }
        store
    }

    #[tokio::test]
    async fn condition_upgrade_keeps_archives_today_and_recoverable_retry_history() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("condition.sqlite3");
        let previous = condition_database(&path).await;
        let saved_before: (String, String) =
            sqlx::query_as("SELECT body,signatures FROM today_results")
                .fetch_one(&previous.pool)
                .await
                .unwrap();
        let generation_before: String =
            sqlx::query_scalar("SELECT body FROM generations WHERE status='succeeded'")
                .fetch_one(&previous.pool)
                .await
                .unwrap();
        let today_operations_before: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT key,request_hash,resource_id FROM operations WHERE scope='today' ORDER BY key",
        )
        .fetch_all(&previous.pool)
        .await
        .unwrap();
        previous.close().await;
        let store = Store::open(&path).await.unwrap();
        assert_eq!(store.counts().await.unwrap(), (2, 2, 1));
        assert_eq!(store.item("example-0000").await.unwrap().revision, 1);
        let archived = store.item("example-0002").await.unwrap();
        assert!(archived.archived);
        assert_eq!(archived.revision, 2);
        let items = store.active_items().await.unwrap();
        let request = || SyncRequest {
            items: items.clone(),
            dry_run: false,
            allow_empty: false,
        };
        let replay = store
            .sync(request(), Some(0), "example-owner", Some("current"))
            .await
            .unwrap();
        assert_eq!(replay.catalog_revision, 2);
        assert_eq!(
            replay.source_hash,
            parse_catalog(&serde_json::to_vec(&items).unwrap())
                .unwrap()
                .report()
                .source_hash
        );
        let mut historical = items.clone();
        historical.push(archived.catalog);
        let replay_history = store
            .sync(
                SyncRequest {
                    items: historical,
                    dry_run: false,
                    allow_empty: false,
                },
                Some(0),
                "example-owner",
                Some("historical"),
            )
            .await
            .unwrap();
        assert_eq!(replay_history.catalog_revision, 1);
        assert_eq!(
            store
                .sync(request(), Some(2), "example-owner", Some("new-key"))
                .await
                .unwrap()
                .catalog_revision,
            2
        );
        assert!(matches!(
            store
                .sync(request(), Some(2), "example-owner", Some("unrecoverable"))
                .await,
            Err(StoreError::Conflict("idempotency_conflict"))
        ));
        let original: (String,String) = sqlx::query_as("SELECT request_hash,json_extract(response,'$.source_hash') FROM operations WHERE key='unrecoverable'").fetch_one(&store.pool).await.unwrap();
        assert_eq!(
            original,
            (
                "unrecoverable-request-hash".into(),
                "unrecoverable-source-hash".into()
            )
        );
        let saved_after: (String, String) =
            sqlx::query_as("SELECT body,signatures FROM today_results")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(saved_before, saved_after);
        let generation_after: String =
            sqlx::query_scalar("SELECT body FROM generations WHERE status='succeeded'")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(generation_before, generation_after);
        let today_operations_after: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT key,request_hash,resource_id FROM operations WHERE scope='today' ORDER BY key",
        )
        .fetch_all(&store.pool)
        .await
        .unwrap();
        assert_eq!(today_operations_before, today_operations_after);
        let now = Utc::now();
        let today = store
            .today(
                &now.date_naive().to_string(),
                "UTC",
                "example-config",
                now,
                None,
            )
            .await
            .unwrap();
        assert!(matches!(today.result.unwrap().freshness, Freshness::Ready));
        assert_eq!(
            today.generation.unwrap().status,
            GenerationStatus::Interrupted
        );
        for (key, _, id) in today_operations_before {
            let reservation = store
                .reserve_generation(
                    TodayRequest {
                        occasion: Occasion::Everyday,
                        weather_mode: WeatherMode::Ignored,
                        footwear_requirement: None,
                    },
                    GenerationBasis {
                        date: now.date_naive().to_string(),
                        timezone: "UTC".into(),
                        config_hash: "example-config".into(),
                        items: items.clone(),
                    },
                    "example-owner",
                    &key,
                )
                .await
                .unwrap();
            assert!(!reservation.fresh);
            assert_eq!(reservation.generation.id, id);
        }
        let condition_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM (SELECT body AS document FROM items UNION ALL SELECT basis FROM generations) JOIN json_tree(document) WHERE key='condition_at_purchase'").fetch_one(&store.pool).await.unwrap();
        assert_eq!(condition_count, 0);
        assert!(
            !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='condition_removal_pending')"
            )
            .fetch_one(&store.pool)
            .await
            .unwrap()
        );
        store.close().await;
        let reopened = Store::open(&path).await.unwrap();
        assert_eq!(
            reopened
                .sync(request(), Some(0), "example-owner", Some("current"))
                .await
                .unwrap(),
            replay
        );
        reopened.close().await;
    }

    #[tokio::test]
    async fn condition_upgrade_rolls_back_a_malformed_snapshot_and_can_resume() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("condition-retry.sqlite3");
        let previous = condition_database(&path).await;
        let saved: Vec<(String, String)> = sqlx::query_as("SELECT id,basis FROM generations")
            .fetch_all(&previous.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE generations SET basis='invalid'")
            .execute(&previous.pool)
            .await
            .unwrap();
        previous.close().await;
        assert!(matches!(
            Store::open(&path).await,
            Err(StoreError::Serialization(_))
        ));
        let mut connection =
            SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
                .await
                .unwrap();
        assert!(
            sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM condition_removal_pending)")
                .fetch_one(&mut connection)
                .await
                .unwrap()
        );
        assert!(
            sqlx::query_scalar::<_, bool>(
                "SELECT json_type(body,'$.condition_at_purchase') IS NOT NULL FROM items LIMIT 1"
            )
            .fetch_one(&mut connection)
            .await
            .unwrap()
        );
        for (id, basis) in saved {
            sqlx::query("UPDATE generations SET basis=? WHERE id=?")
                .bind(basis)
                .bind(id)
                .execute(&mut connection)
                .await
                .unwrap();
        }
        connection.close().await.unwrap();
        let store = Store::open(&path).await.unwrap();
        assert_eq!(store.counts().await.unwrap(), (2, 2, 1));
        store.close().await;
    }

    #[tokio::test]
    async fn previous_database_upgrade_preserves_retries_revisions_and_result_freshness() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("previous.sqlite3");
        previous_database(&path).await.close().await.unwrap();
        let store = Store::open(&path).await.unwrap();
        assert_eq!(store.counts().await.unwrap(), (7, 2, 1));
        let items = store.active_items().await.unwrap();
        assert_eq!(store.item("example-0000").await.unwrap().revision, 4);
        assert!(store.item("example-0002").await.unwrap().archived);
        let request = || SyncRequest {
            items: items.clone(),
            dry_run: false,
            allow_empty: false,
        };
        let replay = store
            .sync(request(), Some(0), "example-owner", Some("current"))
            .await
            .unwrap();
        assert_eq!(replay.catalog_revision, 7);
        assert_eq!(
            replay.source_hash,
            parse_catalog(&serde_json::to_vec(&items).unwrap())
                .unwrap()
                .report()
                .source_hash
        );
        let mut historical = items.clone();
        historical[0].name = "Before a historical edit".into();
        let history = store
            .sync(
                SyncRequest {
                    items: historical.clone(),
                    dry_run: false,
                    allow_empty: false,
                },
                Some(0),
                "example-owner",
                Some("generation-snapshot"),
            )
            .await
            .unwrap();
        assert_eq!(history.catalog_revision, 3);
        assert_eq!(
            history.source_hash,
            parse_catalog(&serde_json::to_vec(&historical).unwrap())
                .unwrap()
                .report()
                .source_hash
        );
        let unchanged = store
            .sync(request(), Some(7), "example-owner", Some("new-key"))
            .await
            .unwrap();
        assert_eq!(
            (
                unchanged.catalog_revision,
                unchanged.unchanged,
                unchanged.updated
            ),
            (7, 2, 0)
        );
        assert!(matches!(
            store
                .sync(request(), Some(7), "example-owner", Some("unrecoverable"))
                .await,
            Err(StoreError::Conflict("idempotency_conflict"))
        ));
        let original: (String,String) = sqlx::query_as("SELECT request_hash,json_extract(response,'$.source_hash') FROM operations WHERE key='unrecoverable'").fetch_one(&store.pool).await.unwrap();
        assert_eq!(
            original,
            (
                "previous-unrecoverable-request-hash".into(),
                "previous-unrecoverable-source-hash".into()
            )
        );
        let now = Utc::now();
        let date = now.date_naive().to_string();
        let current = store
            .today(&date, "UTC", "example-config", now, None)
            .await
            .unwrap();
        let result = current.result.unwrap();
        assert!(matches!(result.freshness, Freshness::Ready));
        assert_eq!(result.suggestions[0].warnings, ["unknown_quantity"]);
        assert_eq!(
            current.generation.unwrap().status,
            GenerationStatus::Interrupted
        );
        let stale = store
            .today(
                &(now - Duration::days(1)).date_naive().to_string(),
                "UTC",
                "example-config",
                now,
                None,
            )
            .await
            .unwrap()
            .result
            .unwrap();
        assert!(matches!(stale.freshness, Freshness::Stale));
        assert!(
            stale
                .stale_reasons
                .contains(&"referenced_items_changed".into())
        );
        for (key, weather, status) in [
            (
                "previous-succeeded",
                WeatherMode::Ignored,
                GenerationStatus::Succeeded,
            ),
            (
                "previous-running",
                WeatherMode::Required,
                GenerationStatus::Interrupted,
            ),
        ] {
            let replay = store
                .reserve_generation(
                    TodayRequest {
                        occasion: Occasion::Everyday,
                        weather_mode: weather,
                        footwear_requirement: None,
                    },
                    GenerationBasis {
                        date: date.clone(),
                        timezone: "UTC".into(),
                        config_hash: "example-config".into(),
                        items: items.clone(),
                    },
                    "example-owner",
                    key,
                )
                .await
                .unwrap();
            assert!(!replay.fresh);
            assert_eq!(replay.generation.id, key);
            assert_eq!(replay.generation.status, status);
        }
        let columns: Vec<String> = sqlx::query("PRAGMA table_info(items)")
            .fetch_all(&store.pool)
            .await
            .unwrap()
            .iter()
            .map(|row| row.get("name"))
            .collect();
        assert!(!columns.iter().any(|name| name == "confirmed"));
        assert!(!sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name IN ('items_active_confirmed','ownership_removal_pending'))").fetch_one(&store.pool).await.unwrap());
        let json: Vec<String> = sqlx::query_scalar("SELECT body FROM items UNION ALL SELECT body FROM generations UNION ALL SELECT basis FROM generations UNION ALL SELECT body FROM today_results UNION ALL SELECT response FROM operations WHERE response IS NOT NULL").fetch_all(&store.pool).await.unwrap();
        for body in json {
            let legacy: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM json_tree(?) WHERE key IN ('current_ownership_confirmed','ownership_scope','confirmed_only','confirmed_count') OR value='unconfirmed_ownership')").bind(body).fetch_one(&store.pool).await.unwrap();
            assert!(!legacy);
        }
        let before = store.counts().await.unwrap();
        store.close().await;
        let reopened = Store::open(&path).await.unwrap();
        assert_eq!(reopened.counts().await.unwrap(), before);
        assert_eq!(
            reopened
                .sync(request(), Some(0), "example-owner", Some("current"))
                .await
                .unwrap(),
            replay
        );
        reopened.close().await;
    }

    #[tokio::test]
    async fn interrupted_json_rewrite_rolls_back_and_resumes_on_startup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("retry.sqlite3");
        let mut previous = previous_database(&path).await;
        let saved: String =
            sqlx::query_scalar("SELECT basis FROM generations WHERE id='previous-running'")
                .fetch_one(&mut previous)
                .await
                .unwrap();
        sqlx::query("UPDATE generations SET basis='invalid' WHERE id='previous-running'")
            .execute(&mut previous)
            .await
            .unwrap();
        previous.close().await.unwrap();
        assert!(matches!(
            Store::open(&path).await,
            Err(StoreError::Serialization(_))
        ));
        let mut connection =
            SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
                .await
                .unwrap();
        assert!(
            sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM ownership_removal_pending)")
                .fetch_one(&mut connection)
                .await
                .unwrap()
        );
        assert!(sqlx::query_scalar::<_, bool>("SELECT json_type(body,'$.current_ownership_confirmed') IS NOT NULL FROM items LIMIT 1").fetch_one(&mut connection).await.unwrap());
        sqlx::query("UPDATE generations SET basis=? WHERE id='previous-running'")
            .bind(saved)
            .execute(&mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();
        let store = Store::open(&path).await.unwrap();
        assert_eq!(store.counts().await.unwrap(), (7, 2, 1));
        assert_eq!(store.active_items().await.unwrap().len(), 2);
        store.close().await;
    }

    #[tokio::test]
    async fn actual_list_query_plans_on_a_representative_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("plans.sqlite3"))
            .await
            .unwrap();
        let grouped_example = |index| {
            let mut item = example(index);
            item.brand = Some(format!("Label {}", (index / 2) % 20));
            item.category = if (index / 2).is_multiple_of(2) {
                "clothing.top".into()
            } else {
                "footwear.shoes".into()
            };
            item.identity = Some(crate::models::CatalogIdentity {
                style_id: format!("example-style-{}", index / 20),
                variant_id: format!("example-variant-{}", index / 2),
            });
            item
        };
        store
            .sync(
                SyncRequest {
                    items: (0..1250).map(grouped_example).collect(),
                    dry_run: false,
                    allow_empty: false,
                },
                Some(0),
                "example-owner",
                Some("all"),
            )
            .await
            .unwrap();
        store
            .sync(
                SyncRequest {
                    items: (0..1000).map(grouped_example).collect(),
                    dry_run: false,
                    allow_empty: false,
                },
                Some(1),
                "example-owner",
                Some("active"),
            )
            .await
            .unwrap();
        let mut revision = 2;
        for batch in 0..4 {
            let start = 1250 + batch * 1000;
            let key = format!("history-batch-{batch}");
            let report = store
                .sync(
                    SyncRequest {
                        items: (start..start + 1000).map(grouped_example).collect(),
                        dry_run: false,
                        allow_empty: false,
                    },
                    Some(revision),
                    "example-owner",
                    Some(&key),
                )
                .await
                .unwrap();
            revision = report.catalog_revision;
        }
        let (_, active, archived) = store.counts().await.unwrap();
        assert_eq!((active, archived), (1000, 4250));
        let mut tx = store.pool.begin().await.unwrap();
        for day in 0..365 {
            let started_at = Utc::now() - Duration::days(day);
            let date = started_at.date_naive().to_string();
            let basis = GenerationBasis {
                date: date.clone(),
                timezone: "UTC".into(),
                config_hash: "example-config".into(),
                items: Vec::new(),
            };
            let generation = Generation {
                id: format!("example-generation-{day}"),
                status: GenerationStatus::Failed,
                inputs: TodayRequest::default(),
                started_at,
                finished_at: Some(started_at),
                error_code: Some("example-failure".into()),
            };
            sqlx::query("INSERT INTO generations (id,local_date,timezone,config_hash,status,body,basis,started_at) VALUES (?,?,?,?,'failed',?,?,?)")
                .bind(&generation.id).bind(&date).bind("UTC").bind("example-config").bind(serde_json::to_string(&generation).unwrap()).bind(serde_json::to_string(&basis).unwrap()).bind(started_at.to_rfc3339()).execute(&mut *tx).await.unwrap();
            sqlx::query("INSERT INTO operations (scope,actor,key,request_hash,resource_id) VALUES ('today',?,?,?,?)").bind("example-owner").bind(format!("example-operation-{day}")).bind("example-hash").bind(&generation.id).execute(&mut *tx).await.unwrap();
            let result = TodayResult {
                generation_id: generation.id.clone(),
                inputs: TodayRequest::default(),
                generated_at: started_at,
                expires_at: started_at + Duration::hours(6),
                freshness: Freshness::Ready,
                stale_reasons: Vec::new(),
                weather: None,
                suggestions: vec![Suggestion {
                    occasion: None,
                    label: "A historical example".into(),
                    item_ids: vec!["example-4300".into(), "example-4301".into()],
                    advisory_reason: "A fictional saved suggestion for query planning.".into(),
                    warnings: Vec::new(),
                }],
            };
            sqlx::query("INSERT INTO today_results (local_date,timezone,config_hash,body,signatures) VALUES (?,'UTC','example-config',?,?)")
                .bind(&date).bind(serde_json::to_string(&result).unwrap())
                .bind(serde_json::to_string(&BTreeMap::from([("example-4300","example-hash"),("example-4301","example-hash")])).unwrap())
                .execute(&mut *tx).await.unwrap();
        }
        tx.commit().await.unwrap();
        store
            .acquire_fence(
                crate::fence::FenceAcquire {
                    token: crate::fence::FenceToken::new().unwrap(),
                    expected_revision: revision,
                },
                "example-owner",
            )
            .await
            .unwrap();
        sqlx::query("ANALYZE").execute(&store.pool).await.unwrap();
        let (generation_body, generation_basis): (String, String) =
            sqlx::query_as("SELECT body,basis FROM generations WHERE id='example-generation-10'")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        let (result_body, result_signatures): (String, String) = sqlx::query_as(
            "SELECT body,signatures FROM today_results ORDER BY local_date DESC LIMIT 1",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        let sync_response: String = sqlx::query_scalar(
            "SELECT response FROM operations WHERE scope='sync' AND actor='example-owner' AND key='all'",
        ).fetch_one(&store.pool).await.unwrap();
        for (name, mut query) in [
            (
                "inventory fence read",
                sqlx::query(crate::store::FENCE_READ_SQL),
            ),
            (
                "inventory fence acquire",
                sqlx::query(crate::store::FENCE_ACQUIRE_SQL)
                    .bind("example-token-hash")
                    .bind("example-owner")
                    .bind("[]")
                    .bind("[]"),
            ),
            (
                "inventory fence pending",
                sqlx::query(crate::store::FENCE_PENDING_SQL).bind("[]"),
            ),
            (
                "inventory fence uploads",
                sqlx::query(crate::store::FENCE_UPLOADS_SQL).bind("[]"),
            ),
            (
                "inventory fence release",
                sqlx::query(crate::store::FENCE_RELEASE_SQL),
            ),
            (
                "complete current live image records",
                sqlx::query(crate::store::LIVE_IMAGE_ITEMS_SQL)
                    .bind(Utc::now().date_naive().to_string()),
            ),
            ("aggregate counts", sqlx::query(COUNTS_SQL)),
            ("item by ID", sqlx::query(ITEM_SQL).bind("example-4300")),
            (
                "cursor position",
                sqlx::query(CURSOR_POSITION_SQL).bind("example-4300"),
            ),
            ("active candidate input", sqlx::query(ACTIVE_ITEMS_SQL)),
            ("scope count", sqlx::query(SCOPE_COUNT_SQL)),
            ("brand facets", sqlx::query(BRANDS_SQL)),
            ("category facets", sqlx::query(CATEGORIES_SQL)),
            ("source facets", sqlx::query(source_facets().into_sql())),
            ("running existence", sqlx::query(RUNNING_EXISTS_SQL)),
            (
                "current daily attempt",
                sqlx::query(CURRENT_GENERATION_SQL)
                    .bind(Utc::now().date_naive().to_string())
                    .bind("UTC"),
            ),
            (
                "generation input",
                sqlx::query(GENERATION_BASIS_SQL).bind("example-generation-10"),
            ),
            (
                "daily result",
                sqlx::query(TODAY_RESULT_SQL).bind(Utc::now().date_naive().to_string()),
            ),
            ("startup interruption", sqlx::query(RUNNING_GENERATIONS_SQL)),
            ("catalog state", sqlx::query(STATE_SQL)),
            (
                "operation replay",
                sqlx::query(OPERATION_SQL)
                    .bind("today")
                    .bind("example-owner")
                    .bind("example-operation-10"),
            ),
            (
                "generation by ID",
                sqlx::query(GENERATION_SQL).bind("example-generation-10"),
            ),
            (
                "migration pending marker",
                sqlx::query(crate::store::MIGRATION_PENDING_SQL),
            ),
            (
                "migration catalog scan",
                sqlx::query(crate::store::MIGRATION_ITEMS_SQL),
            ),
            (
                "migration item rewrite",
                sqlx::query(crate::store::MIGRATION_REWRITE_ITEM_SQL)
                    .bind(serde_json::to_string(&example(4300)).unwrap())
                    .bind("example-new-hash")
                    .bind("example-4300"),
            ),
            (
                "migration catalog hash rewrite",
                sqlx::query(crate::store::MIGRATION_REWRITE_STATE_SQL).bind("example-new-hash"),
            ),
            (
                "migration generation scan",
                sqlx::query(crate::store::MIGRATION_GENERATIONS_SQL),
            ),
            (
                "migration generation rewrite",
                sqlx::query(crate::store::MIGRATION_REWRITE_GENERATION_SQL)
                    .bind(generation_body)
                    .bind(&generation_basis)
                    .bind("example-generation-10"),
            ),
            (
                "migration operation scan",
                sqlx::query(crate::store::MIGRATION_OPERATIONS_SQL),
            ),
            (
                "migration today retry rewrite",
                sqlx::query(crate::store::MIGRATION_REWRITE_TODAY_OPERATION_SQL)
                    .bind("example-new-hash")
                    .bind("example-owner")
                    .bind("example-operation-10"),
            ),
            (
                "migration sync retry rewrite",
                sqlx::query(crate::store::MIGRATION_REWRITE_SYNC_OPERATION_SQL)
                    .bind("example-new-hash")
                    .bind(sync_response)
                    .bind("example-owner")
                    .bind("all"),
            ),
            (
                "migration result scan",
                sqlx::query(crate::store::MIGRATION_RESULTS_SQL),
            ),
            (
                "migration result rewrite",
                sqlx::query(crate::store::MIGRATION_REWRITE_RESULT_SQL)
                    .bind(result_body)
                    .bind(result_signatures)
                    .bind(Utc::now().date_naive().to_string()),
            ),
            (
                "condition migration pending marker",
                sqlx::query(crate::store::CONDITION_PENDING_SQL),
            ),
            (
                "condition migration catalog scan",
                sqlx::query(crate::store::CONDITION_ITEMS_SQL),
            ),
            (
                "condition migration item rewrite",
                sqlx::query(crate::store::CONDITION_REWRITE_ITEM_SQL)
                    .bind(serde_json::to_string(&example(4300)).unwrap())
                    .bind("example-4300"),
            ),
            (
                "condition migration generation scan",
                sqlx::query(crate::store::CONDITION_GENERATIONS_SQL),
            ),
            (
                "condition migration generation rewrite",
                sqlx::query(crate::store::CONDITION_REWRITE_GENERATION_SQL)
                    .bind(&generation_basis)
                    .bind("example-generation-10"),
            ),
            (
                "condition migration sync operation scan",
                sqlx::query(crate::store::CONDITION_OPERATIONS_SQL),
            ),
        ] {
            let arguments = query.take_arguments().unwrap().unwrap();
            let sql = query.sql();
            let mut explain = QueryBuilder::<sqlx::Sqlite>::new("EXPLAIN QUERY PLAN ");
            explain.push(sql.as_str());
            let rows = sqlx::query_with::<sqlx::Sqlite, _>(explain.into_sql(), arguments.clone())
                .fetch_all(&store.pool)
                .await
                .unwrap();
            let plans: Vec<String> = rows
                .iter()
                .map(|row| sqlx::Row::get(row, "detail"))
                .collect();
            println!("{name}: {}", plans.join("; "));
            if plans.is_empty() {
                let mut native = QueryBuilder::<sqlx::Sqlite>::new("EXPLAIN ");
                native.push(sql.as_str());
                let vm = sqlx::query_with::<sqlx::Sqlite, _>(native.into_sql(), arguments)
                    .fetch_all(&store.pool)
                    .await
                    .unwrap();
                let opcodes: Vec<String> = vm.iter().map(|row| row.get("opcode")).collect();
                println!("{name} native VM: {}", opcodes.join(","));
            }
        }
        for (name, filters) in [
            ("first page", ItemFilters::default()),
            (
                "brand",
                ItemFilters {
                    brand: Some("Label 2".into()),
                    ..Default::default()
                },
            ),
            (
                "category descendants",
                ItemFilters {
                    category: Some("clothing.top".into()),
                    ..Default::default()
                },
            ),
            (
                "substring search",
                ItemFilters {
                    q: Some("Piece 430".into()),
                    ..Default::default()
                },
            ),
            (
                "shop source",
                ItemFilters {
                    source: Some(Source::Shop("example-shop-0".into())),
                    ..Default::default()
                },
            ),
            (
                "purchase source",
                ItemFilters {
                    source: Some(Source::Purchase("self_bought".into())),
                    ..Default::default()
                },
            ),
            (
                "ready units",
                ItemFilters {
                    ready_to_wear: Some(true),
                    ..Default::default()
                },
            ),
            (
                "not certified ready",
                ItemFilters {
                    ready_to_wear: Some(false),
                    ..Default::default()
                },
            ),
            (
                "unit status",
                ItemFilters {
                    status: Some(crate::tracking::models::UnitStatus::InWash),
                    ..Default::default()
                },
            ),
            (
                "physical area",
                ItemFilters {
                    area: Some("example-closet".parse().unwrap()),
                    ..Default::default()
                },
            ),
            (
                "combined tracking",
                ItemFilters {
                    ready_to_wear: Some(true),
                    status: Some(crate::tracking::models::UnitStatus::CleanReady),
                    area: Some("example-closet".parse().unwrap()),
                    ..Default::default()
                },
            ),
        ] {
            for (kind, mut builder) in [
                ("page", selection(&filters, None, 50)),
                ("count", count_selection(&filters)),
            ] {
                let mut query = builder.build();
                let arguments = query.take_arguments().unwrap().unwrap();
                let mut explain = QueryBuilder::<sqlx::Sqlite>::new("EXPLAIN QUERY PLAN ");
                explain.push(query.sql().as_str());
                let rows = sqlx::query_with::<sqlx::Sqlite, _>(explain.into_sql(), arguments)
                    .fetch_all(&store.pool)
                    .await
                    .unwrap();
                let plans: Vec<String> = rows
                    .iter()
                    .map(|row| sqlx::Row::get(row, "detail"))
                    .collect();
                assert!(!plans.is_empty());
                println!("{name} {kind}: {}", plans.join("; "));
            }
        }
        for source in [
            Source::Shop("example-shop-0".into()),
            Source::Purchase("self_bought".into()),
        ] {
            let started = std::time::Instant::now();
            let page = store
                .list(ListQuery {
                    source: Some(source.clone()),
                    limit: Some(50),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(page.scope_total, 1000);
            assert!(
                page.items
                    .iter()
                    .all(|item| item.catalog.source().as_ref() == Some(&source))
            );
            println!(
                "{source}: {} returned / {} matched / {} active, including global facets, {:?}",
                page.items.len(),
                page.total,
                page.scope_total,
                started.elapsed()
            );
        }
        let ids =
            std::collections::BTreeSet::from(["example-variant-2150", "example-variant-2600"]);
        // Compare the same production lookup on the same representative data
        // before and after adding its expression index.
        sqlx::query("DROP INDEX items_active_variant")
            .execute(&store.pool)
            .await
            .unwrap();
        let mut before_query = crate::store::variant_copy_selection(&ids);
        let mut query = before_query.build();
        let arguments = query.take_arguments().unwrap().unwrap();
        let mut explain = QueryBuilder::<sqlx::Sqlite>::new("EXPLAIN QUERY PLAN ");
        explain.push(query.sql().as_str());
        let before = sqlx::query_with::<sqlx::Sqlite, _>(explain.into_sql(), arguments)
            .fetch_all(&store.pool)
            .await
            .unwrap();
        let before_plans: Vec<String> = before
            .iter()
            .map(|row| sqlx::Row::get(row, "detail"))
            .collect();
        println!(
            "variant copies before expression index: {}",
            before_plans.join("; ")
        );
        let mut before_query = crate::store::variant_copy_selection(&ids);
        let started = std::time::Instant::now();
        let before_rows = before_query.build().fetch_all(&store.pool).await.unwrap();
        assert_eq!(before_rows.len(), 4);
        println!(
            "variant copies before expression index: {} returned, {:?}",
            before_rows.len(),
            started.elapsed()
        );
        sqlx::raw_sql(include_str!("../migrations/0007_variant_groups.sql"))
            .execute(&store.pool)
            .await
            .unwrap();
        let mut group_query = crate::store::variant_copy_selection(&ids);
        let mut query = group_query.build();
        let arguments = query.take_arguments().unwrap().unwrap();
        let mut explain = QueryBuilder::<sqlx::Sqlite>::new("EXPLAIN QUERY PLAN ");
        explain.push(query.sql().as_str());
        let rows = sqlx::query_with::<sqlx::Sqlite, _>(explain.into_sql(), arguments)
            .fetch_all(&store.pool)
            .await
            .unwrap();
        let group_plans: Vec<String> = rows
            .iter()
            .map(|row| sqlx::Row::get(row, "detail"))
            .collect();
        println!(
            "complete visible variant copies: {}",
            group_plans.join("; ")
        );
        assert!(
            group_plans
                .iter()
                .any(|plan| plan.contains("items_active_variant"))
        );
        assert!(!group_plans.iter().any(|plan| plan.contains("TEMP B-TREE")));
        let mut group_query = crate::store::variant_copy_selection(&ids);
        let started = std::time::Instant::now();
        let matched = group_query.build().fetch_all(&store.pool).await.unwrap();
        assert_eq!(matched.len(), 4);
        println!(
            "variant copies: {} returned / 1000 active / 4250 archived, {:?}",
            matched.len(),
            started.elapsed()
        );
        let signatures = BTreeMap::from([
            ("example-4300".into(), "example-hash".into()),
            ("example-4301".into(), "example-hash".into()),
        ]);
        let previous = sqlx::query(
            "EXPLAIN QUERY PLAN SELECT id, styling_hash, archived FROM items WHERE id IN (?, ?)",
        )
        .bind("example-4300")
        .bind("example-4301")
        .fetch_all(&store.pool)
        .await
        .unwrap();
        let previous_plans: Vec<String> = previous
            .iter()
            .map(|row| sqlx::Row::get(row, "detail"))
            .collect();
        println!(
            "previous referenced fact validation: {}",
            previous_plans.join("; ")
        );
        let mut builder = signature_selection(&signatures);
        let mut query = builder.build();
        let arguments = query.take_arguments().unwrap().unwrap();
        let mut explain = QueryBuilder::<sqlx::Sqlite>::new("EXPLAIN QUERY PLAN ");
        explain.push(query.sql().as_str());
        let rows = sqlx::query_with::<sqlx::Sqlite, _>(explain.into_sql(), arguments)
            .fetch_all(&store.pool)
            .await
            .unwrap();
        let plans: Vec<String> = rows
            .iter()
            .map(|row| sqlx::Row::get(row, "detail"))
            .collect();
        println!("referenced fact validation: {}", plans.join("; "));
        assert_eq!(plans, previous_plans);
        let subset: Vec<_> = [5, 4995, 9000].into_iter().map(grouped_example).collect();
        let previous =
            sqlx::query("EXPLAIN QUERY PLAN SELECT id, body, archived, revision FROM items")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        let before: Vec<String> = previous
            .iter()
            .map(|row| sqlx::Row::get(row, "detail"))
            .collect();
        println!("previous full-history sync: {}", before.join("; "));
        let mut builder = reconciliation(&subset);
        let mut query = builder.build();
        let arguments = query.take_arguments().unwrap().unwrap();
        let mut explain = QueryBuilder::<sqlx::Sqlite>::new("EXPLAIN QUERY PLAN ");
        explain.push(query.sql().as_str());
        let rows = sqlx::query_with::<sqlx::Sqlite, _>(explain.into_sql(), arguments)
            .fetch_all(&store.pool)
            .await
            .unwrap();
        let plans: Vec<String> = rows
            .iter()
            .map(|row| sqlx::Row::get(row, "detail"))
            .collect();
        println!("sync active/source IDs only: {}", plans.join("; "));
        store.close().await;
    }
    #[tokio::test]
    async fn fence_migration_upgrades_previous_database_and_has_a_native_sqlite_plan() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("previous-019.sqlite3");
        let store = Store::open(&path).await.unwrap();
        let mut source = vec![example(0), example(1)];
        source[0].category = "clothing.dress".into();
        store
            .sync(
                SyncRequest {
                    items: source.clone(),
                    dry_run: false,
                    allow_empty: false,
                },
                Some(0),
                "example-owner",
                Some("initial"),
            )
            .await
            .unwrap();
        let now = Utc::now();
        let date = now.date_naive().to_string();
        let inputs = TodayRequest {
            occasion: Occasion::Everyday,
            weather_mode: WeatherMode::Ignored,
            footwear_requirement: None,
        };
        let generation = store
            .reserve_generation(
                inputs.clone(),
                GenerationBasis {
                    date: date.clone(),
                    timezone: "UTC".into(),
                    config_hash: "example-config".into(),
                    items: source.clone(),
                },
                "example-owner",
                "old-today",
            )
            .await
            .unwrap()
            .generation;
        store
            .finish_generation(
                &generation.id,
                Ok(TodayResult {
                    generation_id: generation.id.clone(),
                    inputs,
                    generated_at: now,
                    expires_at: now + Duration::hours(6),
                    freshness: Freshness::Ready,
                    stale_reasons: Vec::new(),
                    weather: None,
                    suggestions: vec![Suggestion {
                        occasion: None,
                        label: "A saved example".into(),
                        item_ids: source.iter().map(|item| item.id.clone()).collect(),
                        advisory_reason: "A fictional combination".into(),
                        warnings: Vec::new(),
                    }],
                }),
            )
            .await
            .unwrap();
        source.truncate(1);
        let report = store
            .sync(
                SyncRequest {
                    items: source.clone(),
                    dry_run: false,
                    allow_empty: false,
                },
                Some(1),
                "example-owner",
                Some("current"),
            )
            .await
            .unwrap();
        store.close().await;
        let mut connection =
            SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
                .await
                .unwrap();
        // The additive fence is the sole schema difference from 0.1.9. Remove
        // that empty table/history entry only on this disposable fixture, while
        // retaining real writer-produced inventory, retry and Today records.
        sqlx::query("DROP TABLE inventory_fence")
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version=4")
            .execute(&mut connection)
            .await
            .unwrap();
        let before: Vec<(String, String)> =
            sqlx::query_as("SELECT scope,request_hash FROM operations ORDER BY scope,key")
                .fetch_all(&mut connection)
                .await
                .unwrap();
        let saved: String = sqlx::query_scalar("SELECT body FROM today_results")
            .fetch_one(&mut connection)
            .await
            .unwrap();
        let mut explain = QueryBuilder::<sqlx::Sqlite>::new("EXPLAIN ");
        explain.push(include_str!("../migrations/0004_inventory_fence.sql"));
        let plan = sqlx::query(explain.into_sql())
            .fetch_all(&mut connection)
            .await
            .unwrap();
        let opcodes: Vec<String> = plan.iter().map(|row| row.get("opcode")).collect();
        assert!(opcodes.iter().any(|opcode| opcode == "CreateBtree"));
        println!("fence migration native VM: {}", opcodes.join(","));
        connection.close().await.unwrap();
        let store = Store::open(&path).await.unwrap();
        assert_eq!(store.counts().await.unwrap(), (2, 1, 1));
        assert_eq!(store.active_items().await.unwrap(), source);
        assert!(store.item("example-0001").await.unwrap().archived);
        let replay = store
            .sync(
                SyncRequest {
                    items: source,
                    dry_run: false,
                    allow_empty: false,
                },
                Some(0),
                "example-owner",
                Some("current"),
            )
            .await
            .unwrap();
        assert_eq!(replay, report);
        let after: Vec<(String, String)> =
            sqlx::query_as("SELECT scope,request_hash FROM operations ORDER BY scope,key")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert_eq!(before, after);
        assert_eq!(
            saved,
            sqlx::query_scalar::<_, String>("SELECT body FROM today_results")
                .fetch_one(&store.pool)
                .await
                .unwrap()
        );
        let today = store
            .today(&date, "UTC", "example-config", now, None)
            .await
            .unwrap()
            .result
            .unwrap();
        assert_eq!(today.suggestions[0].item_ids.len(), 2);
        assert!(
            today
                .stale_reasons
                .iter()
                .any(|reason| reason == "referenced_items_changed")
        );
        let token = crate::fence::FenceToken::new().unwrap();
        store
            .acquire_fence(
                crate::fence::FenceAcquire {
                    token: token.clone(),
                    expected_revision: 2,
                },
                "example-owner",
            )
            .await
            .unwrap();
        store.release_fence(&token, "example-owner").await.unwrap();
        store.close().await;
    }
}
