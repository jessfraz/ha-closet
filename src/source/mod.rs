//! Offline authoring compiler. Registry evidence remains private; URLs are
//! materialized only after the existing deployment adapter resolves real assets.
mod semantics;
pub mod types;
use crate::source::types::*;
use crate::{
    images::{
        ImageSha, ImageSlot, LocalImage, MAX_IMAGE_BYTES, MAX_IMAGE_TOTAL_BYTES, MAX_IMAGES,
        PreparedImage, prepare_images,
    },
    models::{CatalogItem, ImageReference, Price, Shop, ValidatedCatalog, validate_catalog},
};
use icu_casemap::CaseMapper;
use icu_normalizer::{ComposingNormalizer, DecomposingNormalizer};
use icu_properties::{CodePointSetData, props::DefaultIgnorableCodePoint};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};
const MAX_SOURCE_BYTES: u64 = 1024 * 1024;
const MAX_REGISTRY_ROWS: usize = 5000;
#[derive(Clone, Debug, Serialize)]
pub struct SourceFieldError {
    pub field: String,
    pub code: &'static str,
}
#[derive(Clone, Debug, Serialize, thiserror::Error)]
#[error("{code}")]
pub struct SourceError {
    pub code: &'static str,
    pub errors: Vec<SourceFieldError>,
}
impl SourceError {
    fn single(field: impl Into<String>, code: &'static str) -> Self {
        Self {
            code,
            errors: vec![SourceFieldError {
                field: field.into(),
                code,
            }],
        }
    }
}
type Result<T> = std::result::Result<T, SourceError>;
#[derive(Default)]
pub(crate) struct Diagnostics {
    errors: Vec<SourceFieldError>,
}
impl Diagnostics {
    fn add(&mut self, field: impl Into<String>, code: &'static str) {
        if self.errors.len() < 256 {
            self.errors.push(SourceFieldError {
                field: field.into(),
                code,
            });
        }
    }
    fn check(&mut self, valid: bool, field: impl Into<String>, code: &'static str) {
        if !valid {
            self.add(field, code);
        }
    }
    fn finish(self) -> Result<()> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(SourceError {
                code: "invalid_source",
                errors: self.errors,
            })
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
struct SchemaVersion;
impl TryFrom<u8> for SchemaVersion {
    type Error = &'static str;
    fn try_from(v: u8) -> std::result::Result<Self, Self::Error> {
        if v == 1 {
            Ok(Self)
        } else {
            Err("unsupported_source_schema")
        }
    }
}
impl From<SchemaVersion> for u8 {
    fn from(_: SchemaVersion) -> Self {
        1
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Brands {
    schema: SchemaVersion,
    #[serde(default)]
    brands: BTreeMap<BrandId, Brand>,
    #[serde(default)]
    size_profiles: BTreeMap<SizingProfileId, SizingProfile>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Categories {
    schema: SchemaVersion,
    #[serde(default)]
    categories: BTreeMap<CategoryId, Category>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fittings {
    schema: Option<SchemaVersion>,
    #[serde(default)]
    fittings: BTreeMap<FittingId, Fitting>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wardrobe {
    schema: SchemaVersion,
    #[serde(default)]
    styles: Vec<Style>,
    #[serde(default)]
    variants: Vec<Variant>,
    #[serde(default)]
    inventory: Vec<InventoryEntry>,
}
#[derive(Serialize)]
struct CanonicalSource {
    brands: Brands,
    categories: Categories,
    fittings: Fittings,
    wardrobe: Wardrobe,
}

/// Identifier caseless matching: NFD input, per-character NFKC/full case fold,
/// removal of Default_Ignorable_Code_Point, then NFC. The compatibility pass
/// before folding handles decompositions that introduce cased characters.
/// UAX31/R5 require ignorable removal; lowercase/NFKC alone are not equivalent.
/// Explicit whitespace normalization is separate from Unicode normalization.
pub fn normalized_name(value: &str) -> String {
    let nfd = DecomposingNormalizer::new_nfd().normalize(value);
    let nfkc = ComposingNormalizer::new_nfkc();
    let fold = CaseMapper::new();
    let ignorable = CodePointSetData::new::<DefaultIgnorableCodePoint>();
    let mut mapped = String::new();
    for ch in nfd.chars() {
        if ignorable.contains(ch) {
            continue;
        }
        let original = ch.to_string();
        let compatibility = nfkc.normalize(&original);
        let folded = fold.fold_string(&compatibility);
        let normalized = nfkc.normalize(&folded);
        mapped.extend(normalized.chars().filter(|c| !ignorable.contains(*c)));
    }
    ComposingNormalizer::new_nfc()
        .normalize(&mapped)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
struct FileStamp {
    path: PathBuf,
    hash: ImageSha,
    limit: u64,
    field: String,
}
pub struct ReadSet {
    files: Vec<FileStamp>,
    directory: PathBuf,
    directory_names: Option<BTreeSet<String>>,
}
fn read_file(path: &Path, limit: u64, field: &str) -> Result<Vec<u8>> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| SourceError::single(field, "source_read_failed"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit {
        return Err(SourceError::single(field, "invalid_source_file"));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|_| SourceError::single(field, "source_read_failed"))?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| SourceError::single(field, "source_read_failed"))?;
    if bytes.len() as u64 > limit {
        return Err(SourceError::single(field, "source_too_large"));
    }
    Ok(bytes)
}
fn directory_names(path: &Path) -> Result<Option<BTreeSet<String>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(SourceError::single("assets", "source_read_failed")),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(SourceError::single("assets", "invalid_image_directory"));
    }
    let mut names = BTreeSet::new();
    for entry in
        fs::read_dir(path).map_err(|_| SourceError::single("assets", "source_read_failed"))?
    {
        let entry = entry.map_err(|_| SourceError::single("assets", "source_read_failed"))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| SourceError::single("assets", "invalid_image_file"))?;
        if !entry
            .file_type()
            .map_err(|_| SourceError::single("assets", "source_read_failed"))?
            .is_file()
        {
            return Err(SourceError::single("assets", "invalid_image_file"));
        }
        names.insert(name);
        if names.len() > MAX_IMAGES {
            return Err(SourceError::single("assets", "too_many_assets"));
        }
    }
    Ok(Some(names))
}
impl ReadSet {
    pub fn file_count(&self) -> usize {
        self.files.len()
    }
    pub fn verify_unchanged(&self) -> Result<()> {
        for file in &self.files {
            let bytes = read_file(&file.path, file.limit, &file.field)
                .map_err(|_| SourceError::single(&file.field, "catalog_source_changed"))?;
            if ImageSha::digest(&bytes) != file.hash {
                return Err(SourceError::single(&file.field, "catalog_source_changed"));
            }
        }
        if directory_names(&self.directory)
            .map_err(|_| SourceError::single("assets", "catalog_source_changed"))?
            != self.directory_names
        {
            return Err(SourceError::single("assets", "catalog_source_changed"));
        }
        Ok(())
    }
}
fn parse<T: DeserializeOwned>(bytes: &[u8], field: &str, errors: &mut Diagnostics) -> Option<T> {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            errors.add(field, "invalid_source_utf8");
            return None;
        }
    };
    match toml::from_str(text) {
        Ok(value) => Some(value),
        Err(error) => {
            let field = error.span().map_or_else(
                || field.to_owned(),
                |span| format!("{field}.bytes[{}]", span.start),
            );
            errors.add(field, "invalid_source_toml");
            None
        }
    }
}

/// Checked, URL-unresolved inventory facts. Asset photos are never replaced by
/// fake URLs or dropped to make a v1 validator accept an incomplete snapshot.
#[derive(Clone, Debug)]
pub struct CompiledItem {
    pub id: InventoryId,
    pub brand: NfcText,
    pub name: NfcText,
    pub category: CategoryId,
    pub color: String,
    pub size: String,
    pub quantity: std::num::NonZeroU32,
    pub material: String,
    pub sku: Option<MakerSku>,
    pub purchase_source: Option<NfcText>,
    pub images: Vec<SourceImage>,
    pub notes: Vec<PrivateNote>,
    pub price: SourcePrice,
    pub shop: Option<Shop>,
}
impl CompiledItem {
    fn project(&self, images: Vec<ImageReference>) -> CatalogItem {
        CatalogItem {
            id: self.id.to_string(),
            brand: Some(self.brand.to_string()),
            name: self.name.to_string(),
            category: self.category.to_string(),
            color: Some(self.color.clone()),
            size: Some(self.size.clone()),
            quantity: Some(self.quantity.get()),
            material: Some(self.material.clone()),
            sku: self.sku.as_ref().map(ToString::to_string),
            purchase_source: self.purchase_source.as_ref().map(ToString::to_string),
            images,
            notes: self.notes.iter().map(ToString::to_string).collect(),
            price: Some(Price {
                amount: self.price.amount.get(),
                currency: self.price.currency.to_string(),
                basis: self.price.basis.to_string(),
                quantity_basis: self.price.quantity_basis.to_string(),
            }),
            shop: self.shop.clone(),
        }
    }
}
#[derive(Default)]
struct JsonByteCount(usize);
impl Write for JsonByteCount {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("serialized_size_overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn json_size<T: Serialize + ?Sized>(value: &T) -> Result<usize> {
    let mut count = JsonByteCount::default();
    serde_json::to_writer(&mut count, value)
        .map_err(|_| SourceError::single("inventory", "source_serialization_failed"))?;
    Ok(count.0)
}
/// Count the actual v1 field serializers without constructing asset URLs or a
/// pretend ValidatedCatalog. The empty base gallery contributes only its array
/// delimiters; every ordered image's metadata/crop/URL width is then included.
/// Without an origin, an unresolved URL contributes just its JSON delimiters,
/// making this a lower bound. With the real origin, native IDs have a fixed
/// 32-hex width, so the complete native serve URL width is known before uploads.
fn projection_size(items: &[CompiledItem], origin: Option<&url::Url>) -> Result<usize> {
    let has_assets = items.iter().any(|item| {
        item.images
            .iter()
            .any(|image| matches!(image, SourceImage::Asset { .. }))
    });
    let native_url_size = match origin.filter(|_| has_assets) {
        Some(origin) => {
            if !crate::models::valid_https_url(origin.as_str()) {
                return Err(SourceError::single("inventory", "invalid_asset_origin"));
            }
            json_size(&origin.origin().ascii_serialization())?
                + "/api/image/serve/".len()
                + 32
                + "/original".len()
        }
        None => 2,
    };
    let mut total = 2 + items.len().saturating_sub(1); // Catalog array and commas.
    for item in items {
        total += json_size(&item.project(Vec::new()))?;
        total += item.images.len().saturating_sub(1);
        for image in &item.images {
            let (url_size, reference, crop) = match image {
                SourceImage::Asset {
                    reference, crop, ..
                } => (native_url_size, reference, crop),
                SourceImage::Remote {
                    url,
                    reference,
                    crop,
                } => (json_size(url)?, reference, crop),
            };
            // Source reference metadata and v1 reference metadata share their
            // exact typed serializers. Add the v1 URL field and optional crop.
            total += json_size(reference)? + ",\"url\":".len() + url_size;
            if let Some(crop) = crop {
                total += ",\"crop\":".len() + json_size(crop)?;
            }
        }
        if total > crate::models::MAX_CATALOG_BYTES {
            return Err(SourceError::single("inventory", "catalog_too_large"));
        }
    }
    Ok(total)
}
pub struct CompiledCatalog {
    items: Vec<CompiledItem>,
    assets: Vec<PreparedImage>,
    source_hash: String,
    read_set: ReadSet,
    brand_names: BTreeMap<String, BrandId>,
}
impl std::fmt::Debug for CompiledCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledCatalog")
            .field("items", &self.items.len())
            .field("assets", &self.assets.len())
            .finish_non_exhaustive()
    }
}
impl CompiledCatalog {
    pub fn source_hash(&self) -> &str {
        &self.source_hash
    }
    pub fn item_count(&self) -> usize {
        self.items.len()
    }
    pub fn items(&self) -> &[CompiledItem] {
        &self.items
    }
    pub fn assets(&self) -> &[PreparedImage] {
        &self.assets
    }
    pub fn read_set(&self) -> &ReadSet {
        &self.read_set
    }
    pub fn verify_unchanged(&self) -> Result<()> {
        self.read_set.verify_unchanged()
    }
    /// Exact v1 size preflight using the configured native origin, before any
    /// upload/list/delete requests. This does not resolve IDs or contact HA.
    pub fn validate_projection_size(&self, origin: &url::Url) -> Result<()> {
        projection_size(&self.items, Some(origin)).map(|_| ())
    }
    pub fn brand_by_name(&self, name: &str) -> Option<&BrandId> {
        self.brand_names.get(&normalized_name(name))
    }
    pub fn load(path: &Path) -> Result<Self> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let directory = parent.join("wardrobe-images");
        let mut errors = Diagnostics::default();
        let mut files = Vec::new();
        let mut input = Vec::new();
        for (name, file) in [
            ("brands", parent.join("brands.toml")),
            ("categories", parent.join("categories.toml")),
            ("fittings", parent.join("fittings.toml")),
            ("wardrobe", path.to_owned()),
        ] {
            match read_file(&file, MAX_SOURCE_BYTES, name) {
                Ok(bytes) => {
                    files.push(FileStamp {
                        path: file,
                        hash: ImageSha::digest(&bytes),
                        limit: MAX_SOURCE_BYTES,
                        field: name.into(),
                    });
                    input.push(Some(bytes));
                }
                Err(error) => {
                    errors.errors.extend(error.errors);
                    input.push(None);
                }
            }
        }
        let brands = input[0]
            .as_deref()
            .and_then(|b| parse::<Brands>(b, "brands", &mut errors));
        let categories = input[1]
            .as_deref()
            .and_then(|b| parse::<Categories>(b, "categories", &mut errors));
        let fittings = input[2]
            .as_deref()
            .and_then(|b| parse::<Fittings>(b, "fittings", &mut errors));
        let wardrobe = input[3]
            .as_deref()
            .and_then(|b| parse::<Wardrobe>(b, "wardrobe", &mut errors));
        let (Some(mut brands), Some(mut categories), Some(mut fittings), Some(mut wardrobe)) =
            (brands, categories, fittings, wardrobe)
        else {
            return Err(SourceError {
                code: "invalid_source",
                errors: errors.errors,
            });
        };
        let brand_names = validate_registries(&brands, &categories, &fittings, &mut errors);
        compile(&wardrobe, &brands, &categories, &fittings, &mut errors)?;
        // Validate ALL declared assets, including a variant whose gallery happens to
        // be overridden by an inventory entry. Upload only the reachable item slots.
        let mut declared = BTreeSet::new();
        for variant in &wardrobe.variants {
            for image in &variant.images {
                if let SourceImage::Asset { sha256, .. } = image {
                    declared.insert(sha256.clone());
                }
            }
        }
        for entry in &wardrobe.inventory {
            for image in entry.images.iter().flatten() {
                if let SourceImage::Asset { sha256, .. } = image {
                    declared.insert(sha256.clone());
                }
            }
        }
        errors.check(declared.len() <= MAX_IMAGES, "assets", "too_many_assets");
        errors.finish()?;
        let names = directory_names(&directory)?;
        let expected: BTreeSet<_> = declared.iter().map(|sha| format!("{sha}.png")).collect();
        if names.as_ref().is_none_or(|names| names != &expected)
            && (!expected.is_empty() || names.is_some())
        {
            return Err(SourceError::single(
                "assets",
                "image_files_do_not_match_source",
            ));
        }
        for variant in &mut wardrobe.variants {
            semantics::normalize_materials(&mut variant.materials);
        }
        for fitting in fittings.fittings.values_mut() {
            fitting.years.sort_by_key(|year| year.get());
        }
        let mut checked = Diagnostics::default();
        let items = compile(&wardrobe, &brands, &categories, &fittings, &mut checked)?;
        checked.finish()?;
        let mut all_assets = Vec::new();
        let mut total = 0_u64;
        for (index, sha) in declared.into_iter().enumerate() {
            let path = directory.join(format!("{sha}.png"));
            let metadata = fs::symlink_metadata(&path).map_err(|_| {
                SourceError::single(format!("assets[{index}]"), "image_source_read_failed")
            })?;
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() == 0
                || metadata.len() > MAX_IMAGE_BYTES
            {
                return Err(SourceError::single(
                    format!("assets[{index}]"),
                    "invalid_image_source",
                ));
            }
            total = total
                .checked_add(metadata.len())
                .ok_or_else(|| SourceError::single("assets", "image_total_too_large"))?;
            if total > MAX_IMAGE_TOTAL_BYTES {
                return Err(SourceError::single("assets", "image_total_too_large"));
            }
            let slots = items
                .iter()
                .flat_map(|item| {
                    item.images
                        .iter()
                        .enumerate()
                        .filter_map(|(index, image)| match image {
                            SourceImage::Asset { sha256, .. } if sha256 == &sha => {
                                Some(ImageSlot {
                                    item_id: item.id.to_string(),
                                    index,
                                })
                            }
                            _ => None,
                        })
                })
                .collect();
            all_assets.push(LocalImage {
                sha256: sha,
                path,
                size: metadata.len(),
                native_hint: None,
                slots,
            });
        }
        for (index, asset) in all_assets.iter().enumerate() {
            files.push(FileStamp {
                path: asset.path.clone(),
                hash: asset.sha256.clone(),
                limit: MAX_IMAGE_BYTES,
                field: format!("assets[{index}]"),
            });
        }
        let all_assets =
            prepare_images(all_assets).map_err(|error| SourceError::single("assets", error.0))?;
        wardrobe.styles.sort_by(|a, b| a.id.cmp(&b.id));
        wardrobe.variants.sort_by(|a, b| a.id.cmp(&b.id));
        wardrobe.inventory.sort_by(|a, b| a.id.cmp(&b.id));
        for variant in &mut wardrobe.variants {
            semantics::normalize_materials(&mut variant.materials);
        }
        for brand in brands.brands.values_mut() {
            brand.aliases.sort();
        }
        for category in categories.categories.values_mut() {
            category.aliases.sort();
        }
        for fitting in fittings.fittings.values_mut() {
            fitting.years.sort_by_key(|year| year.get());
        }
        let source_hash = crate::models::digest(
            &serde_json::to_vec(&CanonicalSource {
                brands,
                categories,
                fittings,
                wardrobe,
            })
            .map_err(|_| SourceError::single("source", "source_serialization_failed"))?,
        );
        let read_set = ReadSet {
            files,
            directory,
            directory_names: names,
        };
        read_set.verify_unchanged()?;
        Ok(Self {
            items,
            assets: all_assets
                .into_iter()
                .filter(|asset| !asset.source().slots.is_empty())
                .collect(),
            source_hash,
            read_set,
            brand_names,
        })
    }
    pub fn materialize(&self, resolved: &BTreeMap<ImageSha, url::Url>) -> Result<ValidatedCatalog> {
        self.verify_unchanged()?;
        let mut errors = Diagnostics::default();
        let mut items = Vec::new();
        for (i, item) in self.items.iter().enumerate() {
            let mut images = Vec::new();
            for (j, image) in item.images.iter().enumerate() {
                let (url, reference, crop) = match image {
                    SourceImage::Remote {
                        url,
                        reference,
                        crop,
                    } => (Some(url.clone()), reference, *crop),
                    SourceImage::Asset {
                        sha256,
                        reference,
                        crop,
                    } => {
                        let url = resolved
                            .get(sha256)
                            .and_then(|url| HttpsUrl::try_from(url.to_string()).ok());
                        errors.check(
                            url.is_some(),
                            format!("inventory[{i}].images[{j}]"),
                            "unresolved_or_invalid_asset_url",
                        );
                        (url, reference, *crop)
                    }
                };
                if let Some(url) = url {
                    images.push(ImageReference {
                        url: String::from(url),
                        source_url: reference.source_url.clone().map(String::from),
                        kind: reference.kind.token().into(),
                        confidence: reference.confidence,
                        notes: reference.notes.as_ref().map(ToString::to_string),
                        crop,
                    });
                }
            }
            items.push(item.project(images));
        }
        errors.finish()?;
        validate_catalog(items).map_err(|error| {
            if error.code == "catalog_too_large" {
                return SourceError::single("inventory", "catalog_too_large");
            }
            SourceError {
                code: "invalid_projection",
                errors: error
                    .errors
                    .into_iter()
                    .map(|error| SourceFieldError {
                        field: format!("inventory[{}].{}", error.index.unwrap_or(0), error.field),
                        code: "invalid_projection",
                    })
                    .collect(),
            }
        })
    }
}
fn validate_registries(
    brands: &Brands,
    categories: &Categories,
    fittings: &Fittings,
    errors: &mut Diagnostics,
) -> BTreeMap<String, BrandId> {
    errors.check(
        brands.brands.len() <= MAX_REGISTRY_ROWS && brands.size_profiles.len() <= MAX_REGISTRY_ROWS,
        "brands",
        "registry_too_large",
    );
    errors.check(
        categories.categories.len() <= MAX_REGISTRY_ROWS,
        "categories",
        "registry_too_large",
    );
    errors.check(
        fittings.fittings.len() <= MAX_REGISTRY_ROWS,
        "fittings",
        "registry_too_large",
    );
    let mut names = BTreeMap::new();
    for (index, (id, brand)) in brands.brands.iter().enumerate() {
        errors.check(
            brand.aliases.len() <= 100,
            format!("brands[{index}].aliases"),
            "too_many_aliases",
        );
        for (i, name) in std::iter::once(&brand.name)
            .chain(&brand.aliases)
            .enumerate()
        {
            let key = normalized_name(name.as_str());
            errors.check(
                !key.is_empty(),
                format!("brands[{index}]"),
                "empty_lookup_name",
            );
            errors.check(
                !names.contains_key(&key),
                format!("brands[{index}].names[{i}]"),
                "brand_name_collision",
            );
            names.entry(key).or_insert_with(|| id.clone());
        }
    }
    let mut labels = BTreeSet::new();
    let mut aliases = BTreeSet::new();
    let mut canonical_labels: BTreeMap<String, BTreeSet<&CategoryId>> = BTreeMap::new();
    for (id, category) in &categories.categories {
        canonical_labels
            .entry(normalized_name(category.label.as_str()))
            .or_default()
            .insert(id);
    }

    for (index, (id, category)) in categories.categories.iter().enumerate() {
        errors.check(
            !normalized_name(category.label.as_str()).is_empty(),
            format!("categories[{index}].label"),
            "empty_lookup_name",
        );
        errors.check(
            labels.insert((
                category.parent.clone(),
                normalized_name(category.label.as_str()),
            )),
            format!("categories[{index}].label"),
            "category_label_collision",
        );
        if let Some(kind) = category.sizing_kind
            && let Some(expected) = semantics::inferred_kind(id)
        {
            errors.check(
                kind == expected,
                format!("categories[{index}].sizing_kind"),
                "category_sizing_kind_mismatch",
            );
        }
        if let Some(parent) = &category.parent {
            errors.check(
                categories.categories.contains_key(parent),
                format!("categories[{index}].parent"),
                "unknown_category_parent",
            );
            errors.check(
                id.as_str().starts_with(&format!("{}.", parent.as_str())),
                format!("categories[{index}].parent"),
                "invalid_category_parent",
            );
        }
        errors.check(
            category.aliases.len() <= 100,
            format!("categories[{index}].aliases"),
            "too_many_aliases",
        );
        for (i, alias) in category.aliases.iter().enumerate() {
            let key = normalized_name(alias.as_str());
            let path = format!("categories[{index}].aliases[{i}]");
            errors.check(!key.is_empty(), &path, "empty_lookup_name");
            errors.check(
                key != normalized_name(category.label.as_str()),
                &path,
                "redundant_category_alias",
            );
            errors.check(
                canonical_labels
                    .get(&key)
                    .is_none_or(|owners| owners.iter().all(|owner| *owner == id)),
                &path,
                "category_alias_label_collision",
            );
            errors.check(
                aliases.insert(key),
                format!("categories[{index}].aliases[{i}]"),
                "category_alias_collision",
            );
        }
    }
    for (index, profile) in brands.size_profiles.values().enumerate() {
        let path = format!("size_profiles[{index}]");
        if let Some(brand) = &profile.brand {
            errors.check(
                brands.brands.contains_key(brand),
                format!("{path}.brand"),
                "unknown_brand",
            );
        }
        semantics::validate_profile(profile, &path, errors);
    }
    for (index, fitting) in fittings.fittings.values().enumerate() {
        let path = format!("fittings[{index}]");
        errors.check(
            fitting.label.as_str().len() <= 512,
            format!("{path}.label"),
            "projection_text_too_long",
        );
        let years: BTreeSet<_> = fitting.years.iter().map(|year| year.get()).collect();
        errors.check(
            years.len() == fitting.years.len() && years.len() <= 100,
            format!("{path}.years"),
            "invalid_fitting_years",
        );
    }
    names
}
fn validate_images(images: &[SourceImage], path: &str, errors: &mut Diagnostics) {
    errors.check(images.len() <= 20, path, "too_many_images");
    let mut seen = BTreeSet::new();
    for (index, image) in images.iter().enumerate() {
        let path = format!("{path}[{index}]");
        let (reference, crop) = match image {
            SourceImage::Asset {
                reference, crop, ..
            } => (reference, crop),
            SourceImage::Remote {
                url,
                reference,
                crop,
            } => {
                errors.check(
                    crate::fence::referenced_image_id(url.as_str()).is_none(),
                    format!("{path}.url"),
                    "native_url_not_source_identity",
                );
                (reference, crop)
            }
        };
        if let Some(url) = &reference.source_url {
            errors.check(
                crate::fence::referenced_image_id(url.as_str()).is_none(),
                format!("{path}.reference.source_url"),
                "native_url_not_source_identity",
            );
        }
        if let Some(crop) = crop {
            errors.check(
                crop.width > 0
                    && crop.height > 0
                    && u32::from(crop.x) + u32::from(crop.width) <= 10000
                    && u32::from(crop.y) + u32::from(crop.height) <= 10000,
                format!("{path}.crop"),
                "invalid_crop",
            );
        }
        match serde_json::to_vec(image) {
            Ok(key) => errors.check(seen.insert(key), &path, "duplicate_image_reference"),
            Err(_) => errors.add(&path, "source_serialization_failed"),
        }
    }
}
fn compile(
    wardrobe: &Wardrobe,
    brands: &Brands,
    categories: &Categories,
    fittings: &Fittings,
    errors: &mut Diagnostics,
) -> Result<Vec<CompiledItem>> {
    errors.check(
        wardrobe.inventory.len() <= crate::models::MAX_CATALOG_ITEMS,
        "inventory",
        "catalog_too_large",
    );
    errors.check(
        wardrobe.styles.len() <= MAX_REGISTRY_ROWS,
        "styles",
        "too_many_styles",
    );
    errors.check(
        wardrobe.variants.len() <= MAX_REGISTRY_ROWS,
        "variants",
        "too_many_variants",
    );
    let mut styles = BTreeMap::new();
    for (i, style) in wardrobe.styles.iter().enumerate() {
        errors.check(
            styles.insert(&style.id, style).is_none(),
            format!("styles[{i}].id"),
            "duplicate_style_id",
        );
        errors.check(
            brands.brands.contains_key(&style.brand),
            format!("styles[{i}].brand"),
            "unknown_brand",
        );
        errors.check(
            categories.categories.contains_key(&style.category),
            format!("styles[{i}].category"),
            "unknown_category",
        );
        errors.check(
            style.name.as_str().len() <= 512,
            format!("styles[{i}].name"),
            "projection_text_too_long",
        );
    }
    let mut variants = BTreeMap::new();
    let mut variant_facts = BTreeMap::new();
    for (i, variant) in wardrobe.variants.iter().enumerate() {
        errors.check(
            variants.insert(&variant.id, variant).is_none(),
            format!("variants[{i}].id"),
            "duplicate_variant_id",
        );
        errors.check(
            styles.contains_key(&variant.style),
            format!("variants[{i}].style"),
            "unknown_style",
        );
        let color = semantics::color(&variant.color, &format!("variants[{i}].color"), errors);
        let material = semantics::materials(
            &variant.materials,
            &format!("variants[{i}].materials"),
            errors,
        );
        variant_facts.insert(&variant.id, (color, material));
        validate_images(&variant.images, &format!("variants[{i}].images"), errors);
    }
    let mut ids = BTreeSet::new();
    let mut output = Vec::new();
    let mut output_bytes = 2;
    for (i, entry) in wardrobe.inventory.iter().enumerate() {
        let path = format!("inventory[{i}]");
        errors.check(
            ids.insert(&entry.id),
            format!("{path}.id"),
            "duplicate_inventory_id",
        );
        if let Some(images) = &entry.images {
            validate_images(images, &format!("{path}.images"), errors);
        }
        for (index, label) in std::iter::once(&entry.size.primary)
            .chain(&entry.size.also_reported)
            .enumerate()
        {
            let label_path = if index == 0 {
                format!("{path}.size.primary.profile")
            } else {
                format!("{path}.size.also_reported[{}].profile", index - 1)
            };
            errors.check(
                brands.size_profiles.contains_key(&label.profile),
                label_path,
                "unknown_size_profile",
            );
        }

        errors.check(
            entry.notes.len() <= 100,
            format!("{path}.notes"),
            "too_many_notes",
        );
        if let Some(name) = &entry.display_name {
            errors.check(
                name.as_str().len() <= 512,
                format!("{path}.display_name"),
                "projection_text_too_long",
            );
        }
        {
            let price = &entry.price;
            errors.check(
                price.basis.as_str().len() <= 200,
                format!("{path}.price.basis"),
                "projection_text_too_long",
            );
            errors.check(
                price.quantity_basis.as_str().len() <= 200,
                format!("{path}.price.quantity_basis"),
                "projection_text_too_long",
            );
        }
        let (purchase_source, shop) = match &entry.source {
            PurchaseSource::SelfBought => match NfcText::try_from("self_bought".to_owned()) {
                Ok(value) => (Some(value), None),
                Err(_) => {
                    errors.add(format!("{path}.source"), "invalid_purchase_source");
                    (None, None)
                }
            },
            PurchaseSource::Fitting { fitting } => match fittings.fittings.get(fitting) {
                Some(def) => (
                    def.legacy_api_purchase_source.clone(),
                    Some(Shop {
                        id: fitting.to_string(),
                        label: def.label.to_string(),
                        season: def.season,
                        years: def.years.iter().map(|year| year.get()).collect(),
                    }),
                ),
                None => {
                    errors.add(format!("{path}.source.fitting"), "unknown_fitting");
                    (None, None)
                }
            },
        };
        let Some(variant) = variants.get(&entry.variant) else {
            errors.add(format!("{path}.variant"), "unknown_variant");
            continue;
        };
        let Some(style) = styles.get(&variant.style) else {
            continue;
        };
        let Some(brand) = brands.brands.get(&style.brand) else {
            continue;
        };
        let size = semantics::size(
            &entry.size,
            &style.brand,
            &style.category,
            &brands.size_profiles,
            &categories.categories,
            &format!("{path}.size"),
            errors,
        );
        let Some((Some(color), Some(material))) = variant_facts.get(&variant.id) else {
            continue;
        };
        let images = entry
            .images
            .clone()
            .unwrap_or_else(|| variant.images.clone());
        errors.check(
            !images.is_empty(),
            format!("{path}.images"),
            "images_required",
        );
        let item = CompiledItem {
            id: entry.id.clone(),
            brand: brand.name.clone(),
            name: entry
                .display_name
                .clone()
                .unwrap_or_else(|| style.name.clone()),
            category: style.category.clone(),
            color: color.clone(),
            size,
            quantity: entry.quantity,
            material: material.clone(),
            sku: entry.sku.clone(),
            purchase_source,
            images,
            notes: entry.notes.clone(),
            price: entry.price.clone(),
            shop,
        };
        // Admit each projected item before expanding the next shared gallery.
        // A compact authored variant must not fan out into a huge allocation
        // before the v1 byte limit is checked.
        output_bytes += projection_size(std::slice::from_ref(&item), None)? - 2
            + usize::from(!output.is_empty());
        if output_bytes > crate::models::MAX_CATALOG_BYTES {
            return Err(SourceError::single("inventory", "catalog_too_large"));
        }
        output.push(item);
    }
    output.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(output)
}

#[cfg(test)]
mod tests {
    use crate::source::normalized_name;
    #[test]
    fn nfkc_casefold_matches_unicode_18_normative_character_mappings() {
        let mut tested = 0;
        for line in include_str!("../../tests/fixtures/unicode-nfkc-cf-18.txt")
            .lines()
            .filter(|line| !line.starts_with('#') && !line.is_empty())
        {
            let fields: Vec<_> = line.split(';').collect();
            let (start, end) = fields[0].split_once("..").unwrap_or((fields[0], fields[0]));
            let start = u32::from_str_radix(start, 16).unwrap();
            let end = u32::from_str_radix(end, 16).unwrap();
            let mapped: String = fields[2]
                .split_whitespace()
                .map(|hex| char::from_u32(u32::from_str_radix(hex, 16).unwrap()).unwrap())
                .collect();
            let expected = icu_normalizer::ComposingNormalizer::new_nfc()
                .normalize(&mapped)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            for scalar in start..=end {
                if let Some(ch) = char::from_u32(scalar) {
                    assert_eq!(
                        normalized_name(&ch.to_string()),
                        expected,
                        "Unicode18 mapping U+{scalar:04X}"
                    );
                    tested += 1;
                }
            }
        }
        assert!(tested > 10000);
    }
    #[test]
    fn identifier_folding_keeps_accents_punctuation_and_canonical_combining_order() {
        assert_eq!(normalized_name("  STRAẞE\u{a0}Studio  "), "strasse studio");
        assert_eq!(normalized_name("Ｆｕｌｌ\u{ad}width\u{fe0f}"), "fullwidth");
        assert_eq!(normalized_name("Élan"), normalized_name("E\u{301}LAN"));
        assert_ne!(normalized_name("Élan"), normalized_name("Elan"));
        assert_ne!(
            normalized_name("North-Studio"),
            normalized_name("NorthStudio")
        );
        assert_eq!(
            normalized_name("\u{1fb7}"),
            normalized_name("α\u{342}\u{345}")
        );
    }
    #[test]
    fn projection_budget_uses_the_real_wire_serializers_and_exact_native_width() {
        use crate::{
            images::ImageSha,
            models::{ImageConfidence, ImageCrop, ImageReference},
            source::{CompiledItem, projection_size, types::*},
        };
        let origin: url::Url = "https://[::1]:8443/base?ignored=yes".parse().unwrap();
        let remote: HttpsUrl = "https://IMAGES.example.invalid/Exact%2FCase.png?q=\"\\#Photo"
            .to_owned()
            .try_into()
            .unwrap();
        let metadata = ImageReferenceMetadata {
            kind: ImageKind::MerchantProductImage,
            confidence: ImageConfidence::Medium,
            source_url: Some(
                "https://maker.example.invalid/a?x=1&y=2"
                    .to_owned()
                    .try_into()
                    .unwrap(),
            ),
            notes: Some(
                "A note with \"quotes\", backslash \\ and\nnew line"
                    .to_owned()
                    .try_into()
                    .unwrap(),
            ),
        };
        let native = origin
            .join("/api/image/serve/0123456789abcdef0123456789abcdef/original")
            .unwrap();
        let crop = ImageCrop {
            x: 1000,
            y: 2000,
            width: 8000,
            height: 7000,
        };
        for count in 0..=3 {
            let images = [
                SourceImage::Remote {
                    url: remote.clone(),
                    reference: metadata.clone(),
                    crop: None,
                },
                SourceImage::Asset {
                    sha256: ImageSha::digest(b"fictional bytes"),
                    reference: metadata.clone(),
                    crop: Some(crop),
                },
                SourceImage::Asset {
                    sha256: ImageSha::digest(b"other fictional bytes"),
                    reference: metadata.clone(),
                    crop: None,
                },
            ][..count]
                .to_vec();
            let item = CompiledItem {
                id: "fictional-purchase".to_owned().try_into().unwrap(),
                brand: "Fictional \"Studio\"".to_owned().try_into().unwrap(),
                name: "Crew tee".to_owned().try_into().unwrap(),
                category: "clothing.top.tshirt".to_owned().try_into().unwrap(),
                color: "Blue".into(),
                size: "00".into(),
                quantity: std::num::NonZeroU32::new(2).unwrap(),
                material: "Cotton".into(),
                sku: None,
                purchase_source: Some("self_bought".to_owned().try_into().unwrap()),
                images,
                notes: vec![],
                price: SourcePrice {
                    amount: 42.5.try_into().unwrap(),
                    currency: "USD".to_owned().try_into().unwrap(),
                    basis: "receipt".to_owned().try_into().unwrap(),
                    quantity_basis: "per_item".to_owned().try_into().unwrap(),
                },
                shop: None,
            };
            let references = item
                .images
                .iter()
                .map(|image| {
                    let (url, reference, crop) = match image {
                        SourceImage::Remote {
                            url,
                            reference,
                            crop,
                        } => (url.as_str(), reference, *crop),
                        SourceImage::Asset {
                            reference, crop, ..
                        } => (native.as_str(), reference, *crop),
                    };
                    ImageReference {
                        url: url.to_owned(),
                        source_url: reference
                            .source_url
                            .as_ref()
                            .map(|url| url.as_str().to_owned()),
                        kind: reference.kind.token().into(),
                        confidence: reference.confidence,
                        notes: reference.notes.as_ref().map(ToString::to_string),
                        crop,
                    }
                })
                .collect();
            let wire = serde_json::to_vec(&vec![item.project(references)]).unwrap();
            assert_eq!(
                projection_size(std::slice::from_ref(&item), Some(&origin)).unwrap(),
                wire.len()
            );
            assert!(projection_size(std::slice::from_ref(&item), None).unwrap() <= wire.len());
        }
        assert_eq!(projection_size(&[], Some(&origin)).unwrap(), 2);
    }
}
