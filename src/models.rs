//! The catalog file and HTTP/CLI contracts share these owned types.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_CATALOG_BYTES: usize = 1_048_576;
pub const MAX_CATALOG_ITEMS: usize = 5_000;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CatalogItem {
    pub id: String,
    pub brand: Option<String>,
    pub name: String,
    pub category: String,
    pub color: Option<String>,
    pub size: Option<String>,
    /// Historical snapshots can omit this; every new catalog write requires it.
    pub quantity: Option<u32>,
    pub material: Option<String>,
    pub sku: Option<String>,
    /// Catalog imports require either a purchase source or a shop.
    pub purchase_source: Option<String>,
    pub images: Vec<ImageReference>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub price: Option<Price>,
    #[serde(default)]
    pub shop: Option<Shop>,
    #[serde(default, skip_serializing_if = "Usage::is_everyday")]
    pub usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<CatalogIdentity>,
}

/// Stable authoring IDs identify shared variants without combining purchases.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatalogIdentity {
    pub style_id: String,
    pub variant_id: String,
}

/// User-defined use, not a certification or a guarantee of protective performance.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Usage {
    #[default]
    Everyday,
    SafetyWorkOnly {
        requirement: SafetyRequirement,
    },
}
impl Usage {
    /// Whether ordinary, unrestricted wardrobe use is permitted.
    pub fn is_everyday(&self) -> bool {
        matches!(self, Self::Everyday)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SafetyRequirement {
    SteelToe,
}
impl FromStr for SafetyRequirement {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "steel_toe" => Ok(Self::SteelToe),
            _ => Err("invalid_safety_requirement"),
        }
    }
}
impl fmt::Display for SafetyRequirement {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        output.write_str(match self {
            Self::SteelToe => "steel_toe",
        })
    }
}
impl CatalogItem {
    pub fn source(&self) -> Option<Source> {
        if let Some(shop) = &self.shop {
            Some(Source::Shop(shop.id.clone()))
        } else {
            self.purchase_source
                .as_ref()
                .map(|purchase| Source::Purchase(purchase.clone()))
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ImageReference {
    pub url: String,
    #[serde(default)]
    pub source_url: Option<String>,
    pub kind: String,
    pub confidence: ImageConfidence,
    #[serde(default)]
    pub notes: Option<String>,
    /// Optional display-only viewport; the original URL and pixels stay unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<ImageCrop>,
}

/// A rectangle in 1/10,000 of the original image width and height.
/// The origin is the top-left; width and height must be positive, and the
/// rectangle must fit entirely inside the original image.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ImageCrop {
    #[schemars(range(min = 0, max = 9999))]
    pub x: u16,
    #[schemars(range(min = 0, max = 9999))]
    pub y: u16,
    #[schemars(range(min = 1, max = 10000))]
    pub width: u16,
    #[schemars(range(min = 1, max = 10000))]
    pub height: u16,
}
impl ImageCrop {
    fn is_valid(self) -> bool {
        self.width > 0
            && self.height > 0
            && u32::from(self.x) + u32::from(self.width) <= 10_000
            && u32::from(self.y) + u32::from(self.height) <= 10_000
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImageConfidence {
    High,
    Medium,
    Low,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Price {
    pub amount: f64,
    pub basis: String,
    pub currency: String,
    pub quantity_basis: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Shop {
    pub id: String,
    pub label: String,
    pub season: Option<ShopSeason>,
    pub years: Vec<u16>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum ShopSeason {
    Spring,
    Summer,
    Fall,
    Winter,
    #[serde(rename = "Spring/Summer")]
    SpringSummer,
    #[serde(rename = "Fall/Winter")]
    FallWinter,
}

/// A derived source key: `shop:<id>` or `purchase:<value>`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(try_from = "String", into = "String")]
pub enum Source {
    Shop(String),
    Purchase(String),
}
impl JsonSchema for Source {
    fn schema_name() -> String {
        "Source".into()
    }

    fn is_referenceable() -> bool {
        // Dropshot requires query parameters to have an inline scalar schema.
        false
    }

    fn json_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        String::json_schema(generator)
    }
}
impl Source {
    pub(crate) fn is_valid(&self) -> bool {
        match self {
            Self::Shop(id) => valid_text(id, 200),
            Self::Purchase(value) => valid_text(value, 1024),
        }
    }
}
impl FromStr for Source {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let source = if let Some(id) = value.strip_prefix("shop:") {
            Self::Shop(id.into())
        } else if let Some(purchase) = value.strip_prefix("purchase:") {
            Self::Purchase(purchase.into())
        } else {
            return Err("invalid_source");
        };
        if source.is_valid() {
            Ok(source)
        } else {
            Err("invalid_source")
        }
    }
}
impl TryFrom<String> for Source {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}
impl From<Source> for String {
    fn from(value: Source) -> Self {
        value.to_string()
    }
}
impl fmt::Display for Source {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Shop(id) => write!(output, "shop:{id}"),
            Self::Purchase(value) => write!(output, "purchase:{value}"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct SourceFacet {
    /// `shop:<id>` or `purchase:<value>`.
    pub source: Source,
    pub label: String,
    pub count: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct FieldError {
    pub index: Option<usize>,
    pub field: String,
    pub code: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ValidationError {
    pub code: String,
    pub errors: Vec<FieldError>,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        // serde's parse messages can echo input values. This error deliberately
        // exposes field locations and fixed codes, never the private catalog.
        write!(output, "{} ({} field errors)", self.code, self.errors.len())
    }
}
impl std::error::Error for ValidationError {}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ValidationReport {
    pub valid: bool,
    pub count: usize,
    pub source_hash: String,
}

#[derive(Clone, Debug)]
pub struct ValidatedCatalog {
    items: Vec<CatalogItem>,
    report: ValidationReport,
}
impl ValidatedCatalog {
    pub fn items(&self) -> &[CatalogItem] {
        &self.items
    }
    pub fn into_items(self) -> Vec<CatalogItem> {
        self.items
    }
    pub fn report(&self) -> &ValidationReport {
        &self.report
    }
}

pub fn parse_catalog(bytes: &[u8]) -> Result<ValidatedCatalog, ValidationError> {
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(validation_failure("catalog_too_large"));
    }
    let items: Vec<CatalogItem> =
        serde_json::from_slice(bytes).map_err(|error| ValidationError {
            code: "invalid_catalog_json".into(),
            errors: vec![FieldError {
                index: None,
                field: format!("line_{}_column_{}", error.line(), error.column()),
                code: "invalid_json_shape".into(),
            }],
        })?;
    validate_catalog(items)
}

pub fn validate_catalog(mut items: Vec<CatalogItem>) -> Result<ValidatedCatalog, ValidationError> {
    if items.len() > MAX_CATALOG_ITEMS {
        return Err(validation_failure("catalog_too_large"));
    }
    let mut errors = Vec::new();
    let mut ids = BTreeSet::new();
    let mut variants = BTreeMap::<&str, &CatalogItem>::new();
    for (index, item) in items.iter().enumerate() {
        let mut check = |field: &str, valid: bool, code: &str| {
            if !valid && errors.len() < 100 {
                errors.push(FieldError {
                    index: Some(index),
                    field: field.into(),
                    code: code.into(),
                });
            }
        };
        check("id", valid_id(&item.id), "invalid_id");
        check("id", ids.insert(item.id.clone()), "duplicate_id");
        if let Some(identity) = &item.identity {
            let previous = variants.insert(&identity.variant_id, item);
            check(
                "identity.style_id",
                valid_id(&identity.style_id),
                "invalid_id",
            );
            check(
                "identity.variant_id",
                valid_id(&identity.variant_id),
                "invalid_id",
            );
            check(
                "identity",
                previous.is_none_or(|previous| {
                    previous.identity.as_ref().map(|id| &id.style_id) == Some(&identity.style_id)
                }),
                "conflicting_variant_style",
            );
            check(
                "identity",
                previous.is_none_or(|previous| {
                    previous.brand == item.brand
                        && previous.category == item.category
                        && previous.color == item.color
                        && previous.material == item.material
                }),
                "conflicting_variant_metadata",
            );
        }
        check("name", valid_text(&item.name, 512), "invalid_text");
        check(
            "category",
            valid_category(&item.category),
            "invalid_category",
        );
        for (field, value) in [
            ("brand", &item.brand),
            ("color", &item.color),
            ("size", &item.size),
            ("material", &item.material),
            ("sku", &item.sku),
            ("purchase_source", &item.purchase_source),
        ] {
            check(
                field,
                value.as_deref().is_none_or(|v| valid_text(v, 1024)),
                "invalid_text",
            );
        }
        check(
            "purchase_source",
            item.shop.is_some() || item.purchase_source.is_some(),
            "source_required",
        );
        check("quantity", item.quantity.is_some(), "quantity_required");
        check(
            "quantity",
            item.quantity != Some(0),
            "quantity_must_be_positive",
        );
        check(
            "notes",
            item.notes.len() <= 100 && item.notes.iter().all(|v| valid_note(v, 4096)),
            "invalid_notes",
        );
        check("images", item.images.len() <= 20, "too_many_images");
        for image in &item.images {
            check(
                "images.url",
                valid_https_url(&image.url),
                "invalid_https_url",
            );
            check(
                "images.source_url",
                image.source_url.as_deref().is_none_or(valid_https_url),
                "invalid_https_url",
            );
            check("images.kind", valid_text(&image.kind, 100), "invalid_text");
            check(
                "images.notes",
                image.notes.as_deref().is_none_or(|v| valid_note(v, 4096)),
                "invalid_notes",
            );
            check(
                "images.crop",
                image.crop.is_none_or(ImageCrop::is_valid),
                "invalid_crop",
            );
        }
        if let Some(price) = &item.price {
            check(
                "price.amount",
                price.amount.is_finite() && price.amount >= 0.0,
                "invalid_amount",
            );
            check(
                "price.currency",
                price.currency.len() == 3 && price.currency.bytes().all(|b| b.is_ascii_uppercase()),
                "invalid_currency",
            );
            check("price.basis", valid_text(&price.basis, 200), "invalid_text");
            check(
                "price.quantity_basis",
                valid_text(&price.quantity_basis, 200),
                "invalid_text",
            );
        }
        if let Some(shop) = &item.shop {
            check("shop.id", valid_text(&shop.id, 200), "invalid_text");
            check("shop.label", valid_text(&shop.label, 512), "invalid_text");
            check(
                "shop.years",
                shop.years.len() <= 100
                    && shop.years.iter().all(|year| (1000..=9999).contains(year)),
                "invalid_year",
            );
        }
    }
    if !errors.is_empty() {
        return Err(ValidationError {
            code: "invalid_catalog".into(),
            errors,
        });
    }
    // File ordering is not identity. Reordering a source file is a no-op sync.
    items.sort_by(|left, right| left.id.cmp(&right.id));
    let bytes = serde_json::to_vec(&items).map_err(|_| validation_failure("invalid_catalog"))?;
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(validation_failure("catalog_too_large"));
    }
    let report = ValidationReport {
        valid: true,
        count: items.len(),
        source_hash: digest(&bytes),
    };
    Ok(ValidatedCatalog { items, report })
}

fn validation_failure(code: &str) -> ValidationError {
    ValidationError {
        code: code.into(),
        errors: Vec::new(),
    }
}
pub(crate) fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
pub fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && !matches!(value, "." | "..")
        && value.len() <= 200
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}
pub fn valid_text(value: &str, limit: usize) -> bool {
    !value.trim().is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}
fn valid_note(value: &str, limit: usize) -> bool {
    !value.trim().is_empty()
        && value.len() <= limit
        && !value
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t' | '\r'))
}
fn valid_category(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value.split('.').all(|part| {
            !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        })
}
pub fn valid_https_url(value: &str) -> bool {
    value.len() <= 8192
        && url::Url::parse(value).is_ok_and(|url| {
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
        })
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct Item {
    #[serde(flatten)]
    pub catalog: CatalogItem,
    pub revision: i64,
    pub archived: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ItemFilters {
    pub q: Option<String>,
    pub brand: Option<String>,
    pub category: Option<String>,
    pub source: Option<Source>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_to_wear: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<crate::tracking::models::UnitStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<crate::tracking::models::AreaId>,
}
impl ItemFilters {
    pub fn uses_tracking(&self) -> bool {
        self.ready_to_wear.is_some() || self.status.is_some() || self.area.is_some()
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    pub q: Option<String>,
    pub brand: Option<String>,
    pub category: Option<String>,
    /// `shop:<id>` or `purchase:<value>`; omit for all sources.
    pub source: Option<Source>,
    /// Match at least one physical unit with this readiness, not ownership.
    pub ready_to_wear: Option<bool>,
    pub status: Option<crate::tracking::models::UnitStatus>,
    pub area: Option<crate::tracking::models::AreaId>,
    pub limit: Option<u32>,
    pub page_token: Option<String>,
}
impl ListQuery {
    pub fn filters(&self) -> ItemFilters {
        ItemFilters {
            q: self.q.clone(),
            brand: self.brand.clone(),
            category: self.category.clone(),
            source: self.source.clone(),
            ready_to_wear: self.ready_to_wear,
            status: self.status,
            area: self.area.clone(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Facets {
    pub brands: Vec<String>,
    pub categories: Vec<String>,
    pub sources: Vec<SourceFacet>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ItemsPage {
    pub items: Vec<Item>,
    /// Complete active copies for variants visible on this flat page.
    #[serde(default)]
    pub variant_groups: Vec<VariantGroup>,
    pub next_page: Option<String>,
    pub total: i64,
    pub scope_total: i64,
    pub facets: Facets,
    pub catalog_revision: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracking_revision: Option<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct VariantGroup {
    pub identity: CatalogIdentity,
    /// Sum of physical copies, unknown if any historical quantity is unknown.
    pub quantity: Option<u64>,
    pub copies: Vec<InventoryCopy>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct InventoryCopy {
    pub id: String,
    pub size: Option<String>,
    pub quantity: Option<u32>,
    pub purchase_source: Option<String>,
    pub shop: Option<Shop>,
    #[serde(default, skip_serializing_if = "Usage::is_everyday")]
    pub usage: Usage,
    pub revision: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SyncRequest {
    #[schemars(schema_with = "catalog_items_schema")]
    pub items: Vec<CatalogItem>,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub allow_empty: bool,
}

// Historical read snapshots remain nullable. New imports require a positive
// count, so their schema is stricter without rewriting stored JSON.
pub(crate) fn catalog_items_schema(
    generator: &mut schemars::r#gen::SchemaGenerator,
) -> schemars::schema::Schema {
    let mut item = CatalogItem::json_schema(generator).into_object();
    let object = item.object.get_or_insert_with(Default::default);
    object.required.insert("quantity".into());
    object.properties.insert(
        "quantity".into(),
        generator.subschema_for::<std::num::NonZeroU32>(),
    );
    let mut array = Vec::<CatalogItem>::json_schema(generator).into_object();
    array.array.get_or_insert_with(Default::default).items =
        Some(schemars::schema::SingleOrVec::Single(Box::new(item.into())));
    array.into()
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct SyncReport {
    pub catalog_revision: i64,
    pub source_hash: String,
    pub created: usize,
    pub updated: usize,
    pub archived: usize,
    pub unchanged: usize,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccessRole {
    Owner,
    Integration,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct System {
    pub version: String,
    pub role: AccessRole,
    pub catalog_revision: i64,
    pub item_count: i64,
    pub archived_count: i64,
    pub today_configured: bool,
}

macro_rules! choice {
    ($name:ident, $default:ident, {$($variant:ident => $value:literal),+ $(,)?}) => {
        #[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
        pub enum $name { $(#[serde(rename = $value)] $variant),+ }
        impl Default for $name { fn default() -> Self { Self::$default } }
        impl FromStr for $name {
            type Err = &'static str;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value { $($value => Ok(Self::$variant),)+ _ => Err("invalid_choice") }
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
                output.write_str(match self { $(Self::$variant => $value),+ })
            }
        }
    };
}
choice!(Occasion, All, {All => "all", Everyday => "everyday", Work => "work", Dinner => "dinner", Bedtime => "bedtime"});
choice!(WeatherMode, Required, {Required => "required", Ignored => "ignored"});

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TodayRequest {
    #[serde(default)]
    pub occasion: Occasion,
    #[serde(default)]
    pub weather_mode: WeatherMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footwear_requirement: Option<SafetyRequirement>,
}
impl TodayRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.occasion == Occasion::Bedtime && self.footwear_requirement.is_some() {
            Err("footwear_requirement_not_applicable")
        } else {
            Ok(())
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GenerationStatus {
    Running,
    Succeeded,
    Failed,
    Interrupted,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Generation {
    pub id: String,
    pub status: GenerationStatus,
    pub inputs: TodayRequest,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub error_code: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    Ready,
    Stale,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct TodayResponse {
    pub date: String,
    pub timezone: String,
    pub result: Option<TodayResult>,
    pub generation: Option<Generation>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct TodayResult {
    pub generation_id: String,
    pub inputs: TodayRequest,
    pub generated_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub freshness: Freshness,
    pub stale_reasons: Vec<String>,
    pub weather: Option<WeatherContext>,
    pub suggestions: Vec<Suggestion>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Suggestion {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occasion: Option<Occasion>,
    pub label: String,
    pub item_ids: Vec<String>,
    pub advisory_reason: String,
    #[serde(default)]
    pub warnings: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WeatherContext {
    pub retrieved_at: DateTime<Utc>,
    pub forecast_type: String,
    pub window_start: DateTime<Utc>,
    pub window_end: DateTime<Utc>,
    pub temperature_unit: String,
    pub coverage: ForecastCoverage,
    pub entries: Vec<Forecast>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ForecastCoverage {
    Complete,
    Partial,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Forecast {
    pub datetime: DateTime<Utc>,
    pub condition: Option<String>,
    pub temperature: Option<f64>,
    pub templow: Option<f64>,
    pub precipitation_probability: Option<f64>,
}

#[cfg(test)]
mod tests {
    use crate::models::{ShopSeason, Source, parse_catalog};

    #[test]
    fn typed_seasons_preserve_the_catalog_wire_shape_and_hash() {
        let input = include_bytes!("../tests/fixtures/catalog-seasons.json");
        let catalog = parse_catalog(input).unwrap();
        assert_eq!(
            catalog.report().source_hash,
            "67d1bb16e1a0bd615a6cfa3d2d13dda1dba5e405dbae093b2d5876ca9261a8e0"
        );
        let item = &catalog.items()[0];
        assert_eq!(
            item.shop.as_ref().unwrap().season,
            Some(ShopSeason::SpringSummer)
        );
        let encoded = serde_json::to_vec(catalog.items()).unwrap();
        assert_eq!(parse_catalog(&encoded).unwrap().report(), catalog.report());
        for (wire, season) in [
            ("Spring", ShopSeason::Spring),
            ("Summer", ShopSeason::Summer),
            ("Fall", ShopSeason::Fall),
            ("Winter", ShopSeason::Winter),
            ("Spring/Summer", ShopSeason::SpringSummer),
            ("Fall/Winter", ShopSeason::FallWinter),
        ] {
            assert_eq!(
                serde_json::to_string(&season).unwrap(),
                format!("\"{wire}\"")
            );
            assert_eq!(
                serde_json::from_str::<ShopSeason>(&format!("\"{wire}\"")).unwrap(),
                season
            );
        }
        assert_eq!(
            serde_json::to_string(&Option::<ShopSeason>::None).unwrap(),
            "null"
        );
        assert!(serde_json::from_str::<ShopSeason>("\"spring_summer\"").is_err());
        let invalid = String::from_utf8(input.to_vec())
            .unwrap()
            .replace("Spring/Summer", "spring_summer");
        assert_eq!(
            parse_catalog(invalid.as_bytes()).unwrap_err().code,
            "invalid_catalog_json"
        );
    }

    #[test]
    fn derived_sources_have_canonical_scalar_keys_without_changing_catalog_fields() {
        for (key, source) in [
            ("shop:example-shop", Source::Shop("example-shop".into())),
            ("shop:example:shop", Source::Shop("example:shop".into())),
            (
                "purchase:Example gift / café",
                Source::Purchase("Example gift / café".into()),
            ),
            (
                "purchase:Example exchange",
                Source::Purchase("Example exchange".into()),
            ),
        ] {
            assert_eq!(source.to_string(), key);
            assert_eq!(key.parse::<Source>().unwrap(), source);
            let encoded = serde_json::to_string(&source).unwrap();
            assert_eq!(serde_json::from_str::<String>(&encoded).unwrap(), key);
            assert_eq!(serde_json::from_str::<Source>(&encoded).unwrap(), source);
        }
        for invalid in [
            "all",
            "shop:",
            "purchase:",
            "unspecified",
            "unspecified:any",
            "shop: ",
            "purchase:\nexample",
        ] {
            assert_eq!(invalid.parse::<Source>(), Err("invalid_source"));
        }
        assert!(
            format!("shop:{}", "a".repeat(201))
                .parse::<Source>()
                .is_err()
        );
        assert!(
            format!("purchase:{}", "a".repeat(1025))
                .parse::<Source>()
                .is_err()
        );
        let mut item = parse_catalog(include_bytes!("../tests/fixtures/catalog-seasons.json"))
            .unwrap()
            .into_items()
            .remove(0);
        assert_eq!(item.source(), Some(Source::Shop("example-shop".into())));
        item.shop = None;
        assert_eq!(item.source(), Some(Source::Purchase("self_bought".into())));
        item.purchase_source = None;
        assert_eq!(item.source(), None);
    }
}
