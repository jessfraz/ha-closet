//! Local, SHA-addressed image sources. No credentials or network are needed here.
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

use crate::models::{CatalogItem, valid_id};

pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024 - 1024;
pub const MAX_IMAGE_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_IMAGES: usize = 512;
pub const MAX_MANIFEST_BYTES: u64 = 512 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub struct NativeImageId(#[schemars(with = "String")] String);
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub struct ImageSha(#[schemars(with = "String")] String);

fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
macro_rules! hex_type {
    ($name:ident, $length:expr, $error:literal) => {
        impl TryFrom<String> for $name {
            type Error = &'static str;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                if lower_hex(&value, $length) {
                    Ok(Self(value))
                } else {
                    Err($error)
                }
            }
        }
        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}
hex_type!(NativeImageId, 32, "invalid_native_image_id");
hex_type!(ImageSha, 64, "invalid_image_sha256");
impl ImageSha {
    pub fn digest(bytes: &[u8]) -> Self {
        Self(
            Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        )
    }
    pub fn upload_name(&self) -> String {
        format!("closet-{}.png", self.0)
    }
    pub fn from_upload_name(name: &str) -> Option<Self> {
        name.strip_prefix("closet-")?
            .strip_suffix(".png")?
            .to_owned()
            .try_into()
            .ok()
    }
}

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ImageError(pub &'static str);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageManifest {
    pub version: u8,
    pub images: Vec<ManifestImage>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestImage {
    pub item_id: String,
    pub sha256: ImageSha,
    /// Explicit image slot for a new local photo. A legacy native ID can locate
    /// the slot instead, but is never the authoritative byte identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_id: Option<NativeImageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_name: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ImageSlot {
    pub item_id: String,
    pub index: usize,
}
#[derive(Debug)]
pub struct LocalImage {
    pub sha256: ImageSha,
    pub path: PathBuf,
    pub size: u64,
    pub native_hint: Option<NativeImageId>,
    pub slots: Vec<ImageSlot>,
}
#[derive(Debug)]
pub struct LocalImages {
    pub assets: Vec<LocalImage>,
    pub native_origin: Option<Url>,
    manifest_path: PathBuf,
    manifest_hash: ImageSha,
    directory: PathBuf,
}
#[derive(Debug, Serialize)]
pub struct ImageValidationReport {
    pub images: usize,
    pub bytes: u64,
}

fn bounded_file(path: &Path, limit: u64) -> Result<Vec<u8>, ImageError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| ImageError("image_source_read_failed"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit {
        return Err(ImageError("invalid_image_source"));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|_| ImageError("image_source_read_failed"))?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ImageError("image_source_read_failed"))?;
    if bytes.len() as u64 > limit {
        return Err(ImageError("image_source_too_large"));
    }
    Ok(bytes)
}
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
fn word(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}
fn png_header(bytes: &[u8]) -> Result<(), ImageError> {
    // The SHA binds the reviewed file. This is a bounded format/dimension check,
    // not a second image decoder, resizer, or color-management pipeline.
    if bytes.len() < 45
        || bytes.get(..8) != Some(b"\x89PNG\r\n\x1a\n")
        || word(bytes, 8) != Some(13)
        || bytes.get(12..16) != Some(b"IHDR")
        || word(bytes, 29) != Some(crc32(&bytes[12..29]))
        || word(bytes, bytes.len() - 12) != Some(0)
        || bytes.get(bytes.len() - 8..bytes.len() - 4) != Some(b"IEND")
        || word(bytes, bytes.len() - 4) != Some(crc32(b"IEND"))
    {
        return Err(ImageError("invalid_png_header"));
    }
    let width = word(bytes, 16).ok_or(ImageError("invalid_png_header"))?;
    let height = word(bytes, 20).ok_or(ImageError("invalid_png_header"))?;
    if width == 0
        || height == 0
        || width > 16384
        || height > 16384
        || u64::from(width) * u64::from(height) > 40_000_000
    {
        return Err(ImageError("invalid_png_dimensions"));
    }
    Ok(())
}

pub fn native_image(url: &str) -> Option<(Url, NativeImageId)> {
    let mut url = Url::parse(url).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let id = url
        .path()
        .strip_prefix("/api/image/serve/")?
        .strip_suffix("/original")?
        .to_owned()
        .try_into()
        .ok()?;
    url.set_path("/");
    Some((url, id))
}
fn image_aliases(items: &[CatalogItem], url: &str) -> Vec<ImageSlot> {
    items
        .iter()
        .flat_map(|item| {
            item.images
                .iter()
                .enumerate()
                .filter(|(_, image)| image.url == url)
                .map(|(index, _)| ImageSlot {
                    item_id: item.id.clone(),
                    index,
                })
        })
        .collect()
}
impl LocalImage {
    pub fn read_verified(&self) -> Result<Vec<u8>, ImageError> {
        let bytes = bounded_file(&self.path, self.size)?;
        if bytes.len() as u64 != self.size || ImageSha::digest(&bytes) != self.sha256 {
            return Err(ImageError("image_source_changed"));
        }
        png_header(&bytes)?;
        Ok(bytes)
    }
}
impl LocalImages {
    pub fn read(
        manifest_path: &Path,
        directory: &Path,
        items: &[CatalogItem],
    ) -> Result<Self, ImageError> {
        let manifest_bytes = bounded_file(manifest_path, MAX_MANIFEST_BYTES)?;
        let manifest: ImageManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|_| ImageError("invalid_image_manifest"))?;
        if manifest.version != 1 || manifest.images.len() > MAX_IMAGES {
            return Err(ImageError("invalid_image_manifest"));
        }
        let metadata =
            fs::symlink_metadata(directory).map_err(|_| ImageError("invalid_image_directory"))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(ImageError("invalid_image_directory"));
        }
        let by_id: BTreeMap<_, _> = items.iter().map(|item| (item.id.as_str(), item)).collect();
        let mut hashes = BTreeSet::new();
        let mut hints = BTreeSet::new();
        let mut bound_slots = BTreeSet::new();
        let mut assets = Vec::new();
        let mut native_origin = None;
        let mut total = 0_u64;
        for entry in manifest.images {
            if !valid_id(&entry.item_id)
                || !hashes.insert(entry.sha256.clone())
                || entry
                    .native_id
                    .as_ref()
                    .is_some_and(|id| !hints.insert(id.clone()))
                || entry
                    .upload_name
                    .as_ref()
                    .is_some_and(|name| name != &entry.sha256.upload_name())
            {
                return Err(ImageError("duplicate_or_invalid_image_mapping"));
            }
            let item = by_id
                .get(entry.item_id.as_str())
                .ok_or(ImageError("missing_catalog_image"))?;
            let slots = if let Some(index) = entry.image_index {
                if index >= item.images.len() {
                    return Err(ImageError("invalid_image_index"));
                }
                if entry.native_id.as_ref().is_some_and(|hint| {
                    native_image(&item.images[index].url).is_none_or(|(_, id)| &id != hint)
                }) {
                    return Err(ImageError("image_binding_mismatch"));
                }
                if native_image(&item.images[index].url).is_some() {
                    image_aliases(items, &item.images[index].url)
                } else {
                    vec![ImageSlot {
                        item_id: entry.item_id,
                        index,
                    }]
                }
            } else {
                let hint = entry
                    .native_id
                    .as_ref()
                    .ok_or(ImageError("image_binding_required"))?;
                let references: Vec<_> = item
                    .images
                    .iter()
                    .filter(|image| native_image(&image.url).is_some_and(|(_, id)| &id == hint))
                    .collect();
                if references.len() != 1 {
                    return Err(ImageError("ambiguous_catalog_image"));
                }
                let previous_url = &references[0].url;
                image_aliases(items, previous_url)
            };
            for slot in &slots {
                if !bound_slots.insert(slot.clone()) {
                    return Err(ImageError("duplicate_image_slot"));
                }
            }
            let path = directory.join(format!("{}.png", entry.sha256));
            let metadata =
                fs::symlink_metadata(&path).map_err(|_| ImageError("image_source_read_failed"))?;
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() == 0
                || metadata.len() > MAX_IMAGE_BYTES
            {
                return Err(ImageError("invalid_image_source"));
            }
            total = total
                .checked_add(metadata.len())
                .ok_or(ImageError("image_total_too_large"))?;
            if total > MAX_IMAGE_TOTAL_BYTES {
                return Err(ImageError("image_total_too_large"));
            }
            assets.push(LocalImage {
                sha256: entry.sha256,
                path,
                size: metadata.len(),
                native_hint: entry.native_id,
                slots,
            });
        }
        for entry in fs::read_dir(directory).map_err(|_| ImageError("invalid_image_directory"))? {
            let entry = entry.map_err(|_| ImageError("invalid_image_directory"))?;
            let name = entry.file_name();
            let sha = name
                .to_str()
                .and_then(|name| name.strip_suffix(".png"))
                .and_then(|name| name.to_owned().try_into().ok());
            if !entry
                .file_type()
                .map_err(|_| ImageError("invalid_image_source"))?
                .is_file()
                || sha.as_ref().is_none_or(|sha| !hashes.contains(sha))
            {
                return Err(ImageError("unlisted_image_file"));
            }
        }
        for item in items {
            for image in &item.images {
                if let Some((origin, _)) = native_image(&image.url) {
                    if native_origin.as_ref().is_some_and(|old| old != &origin) {
                        return Err(ImageError("image_origin_mismatch"));
                    }
                    native_origin = Some(origin);
                }
            }
        }
        // Preflight every size first, then hash one file at a time. A supported
        // 512-MiB source never requires retaining all image buffers in memory.
        for asset in &assets {
            asset.read_verified()?;
        }
        Ok(Self {
            assets,
            native_origin,
            manifest_path: manifest_path.to_owned(),
            manifest_hash: ImageSha::digest(&manifest_bytes),
            directory: directory.to_owned(),
        })
    }
    pub fn verify_sources(&self, items: &[CatalogItem]) -> Result<(), ImageError> {
        if ImageSha::digest(&bounded_file(&self.manifest_path, MAX_MANIFEST_BYTES)?)
            != self.manifest_hash
        {
            return Err(ImageError("image_source_changed"));
        }
        Self::read(&self.manifest_path, &self.directory, items)?;
        Ok(())
    }
    pub fn report(&self) -> ImageValidationReport {
        ImageValidationReport {
            images: self.assets.len(),
            bytes: self.assets.iter().map(|asset| asset.size).sum(),
        }
    }
    pub fn materialize(
        &self,
        items: &[CatalogItem],
        origin: &Url,
        resolved: &BTreeMap<ImageSha, NativeImageId>,
    ) -> Result<Vec<CatalogItem>, ImageError> {
        if self
            .native_origin
            .as_ref()
            .is_some_and(|expected| expected != origin)
        {
            return Err(ImageError("image_origin_mismatch"));
        }
        let mut items = items.to_vec();
        for asset in &self.assets {
            let id = resolved
                .get(&asset.sha256)
                .ok_or(ImageError("unresolved_image"))?;
            let url = origin
                .join(&format!("/api/image/serve/{id}/original"))
                .map_err(|_| ImageError("image_origin_mismatch"))?;
            for slot in &asset.slots {
                let image = items
                    .iter_mut()
                    .find(|item| item.id == slot.item_id)
                    .and_then(|item| item.images.get_mut(slot.index))
                    .ok_or(ImageError("invalid_image_index"))?;
                image.url = url.to_string();
            }
        }
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        images::*,
        models::{ImageCrop, parse_catalog},
    };
    use base64::Engine;
    use std::fs;
    fn png() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=").unwrap()
    }
    fn items() -> Vec<CatalogItem> {
        parse_catalog(include_bytes!("../tests/fixtures/catalog-crop.json"))
            .unwrap()
            .into_items()
    }
    fn fixture(
        entries: Vec<ManifestImage>,
        items: &[CatalogItem],
        bytes: &[u8],
    ) -> (tempfile::TempDir, Result<LocalImages, ImageError>) {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("images");
        fs::create_dir(&directory).unwrap();
        for entry in &entries {
            fs::write(directory.join(format!("{}.png", entry.sha256)), bytes).unwrap();
        }
        let path = temp.path().join("images.json");
        fs::write(
            &path,
            serde_json::to_vec(&ImageManifest {
                version: 1,
                images: entries,
            })
            .unwrap(),
        )
        .unwrap();
        let result = LocalImages::read(&path, &directory, items);
        (temp, result)
    }
    fn entry(item: &CatalogItem) -> ManifestImage {
        ManifestImage {
            item_id: item.id.clone(),
            sha256: ImageSha::digest(&png()),
            image_index: Some(0),
            native_id: None,
            upload_name: None,
        }
    }
    #[test]
    fn gallery_binding_rejects_wrong_hint_index_duplicates_and_changed_sources() {
        let items = items();
        let entry = entry(&items[0]);
        let (_temp, local) = fixture(vec![entry.clone()], &items, &png());
        let local = local.unwrap();
        assert_eq!(local.report().images, 1);
        local.verify_sources(&items).unwrap();
        fs::write(&local.assets[0].path, b"changed").unwrap();
        assert_eq!(
            local.assets[0].read_verified().unwrap_err().0,
            "image_source_changed"
        );
        let mut bad = entry.clone();
        bad.image_index = Some(999);
        assert_eq!(
            fixture(vec![bad], &items, &png()).1.unwrap_err().0,
            "invalid_image_index"
        );
        let mut bad = entry.clone();
        bad.native_id = Some("a".repeat(32).try_into().unwrap());
        assert_eq!(
            fixture(vec![bad], &items, &png()).1.unwrap_err().0,
            "image_binding_mismatch"
        );
        assert_eq!(
            fixture(vec![entry.clone(), entry], &items, &png())
                .1
                .unwrap_err()
                .0,
            "duplicate_or_invalid_image_mapping"
        );
    }
    #[test]
    fn legacy_aliases_share_bytes_without_touching_crop_or_provenance() {
        let mut items = items();
        items.truncate(1);
        let id: NativeImageId = "1".repeat(32).try_into().unwrap();
        items[0].images[0].url =
            format!("https://ha.example.invalid/api/image/serve/{id}/original");
        items[0].images[0].crop = Some(ImageCrop {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        });
        let mut alias = items[0].clone();
        alias.id = "example-second-purchase".into();
        items.push(alias);
        let mut entry = entry(&items[0]);
        entry.image_index = None;
        entry.native_id = Some(id);
        let (_temp, local) = fixture(vec![entry], &items, &png());
        let local = local.unwrap();
        assert_eq!(local.assets[0].slots.len(), 2);
        let new_id: NativeImageId = "2".repeat(32).try_into().unwrap();
        let resolved = BTreeMap::from([(local.assets[0].sha256.clone(), new_id.clone())]);
        let mut materialized = local
            .materialize(
                &items,
                &"https://ha.example.invalid/".parse().unwrap(),
                &resolved,
            )
            .unwrap();
        for (before, after) in items.iter().zip(&mut materialized) {
            assert!(after.images[0].url.contains(new_id.as_str()));
            after.images[0].url = before.images[0].url.clone();
            assert_eq!(before, after);
        }
    }
    #[test]
    fn more_than_thirty_two_assets_are_valid_and_aggregate_preflight_is_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("images");
        fs::create_dir(&directory).unwrap();
        let mut items = items();
        items.truncate(1);
        let first = items[0].clone();
        items.clear();
        let mut entries = Vec::new();
        for index in 0..64 {
            let mut item = first.clone();
            item.id = format!("example-{index}");
            let mut bytes = png(); // A valid PNG ancillary text chunk gives each source a distinct identity.
            let text = format!("example\0{index}");
            let mut chunk = Vec::from(b"tEXt".as_slice());
            chunk.extend(text.as_bytes());
            let mut encoded = (text.len() as u32).to_be_bytes().to_vec();
            encoded.extend(&chunk);
            encoded.extend(crc32(&chunk).to_be_bytes());
            bytes.splice(33..33, encoded);
            let mut mapping = entry(&item);
            mapping.sha256 = ImageSha::digest(&bytes);
            fs::write(directory.join(format!("{}.png", mapping.sha256)), bytes).unwrap();
            entries.push(mapping);
            items.push(item);
        }
        let path = temp.path().join("images.json");
        fs::write(
            &path,
            serde_json::to_vec(&ImageManifest {
                version: 1,
                images: entries.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            LocalImages::read(&path, &directory, &items)
                .unwrap()
                .report()
                .images,
            64
        );
        // Sparse files prove the aggregate cap runs before any expensive read/hash.
        for mapping in &entries {
            fs::OpenOptions::new()
                .write(true)
                .open(directory.join(format!("{}.png", mapping.sha256)))
                .unwrap()
                .set_len(MAX_IMAGE_BYTES)
                .unwrap();
        }
        assert_eq!(
            LocalImages::read(&path, &directory, &items).unwrap_err().0,
            "image_total_too_large"
        );
    }
    #[test]
    fn malformed_png_symlinks_and_unlisted_files_are_rejected() {
        let items = items();
        let mut bytes = png();
        bytes[29] ^= 1;
        let mut mapping = entry(&items[0]);
        mapping.sha256 = ImageSha::digest(&bytes);
        assert_eq!(
            fixture(vec![mapping], &items, &bytes).1.unwrap_err().0,
            "invalid_png_header"
        );
        let (temp, local) = fixture(vec![entry(&items[0])], &items, &png());
        let local = local.unwrap();
        fs::write(temp.path().join("images/extra.png"), png()).unwrap();
        assert_eq!(
            local.verify_sources(&items).unwrap_err().0,
            "unlisted_image_file"
        );
        #[cfg(unix)]
        {
            let (temp, local) = fixture(vec![entry(&items[0])], &items, &png());
            let local = local.unwrap();
            let path = &local.assets[0].path;
            fs::remove_file(path).unwrap();
            std::os::unix::fs::symlink(temp.path().join("images.json"), path).unwrap();
            assert_eq!(
                local.verify_sources(&items).unwrap_err().0,
                "invalid_image_source"
            );
        }
    }
}
