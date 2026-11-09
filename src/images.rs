//! Local, SHA-addressed image sources. No credentials or network are needed here.
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::{self, File},
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
};

use image::{DynamicImage, ImageDecoder, ImageEncoder, Limits, imageops::FilterType};
use moxcms::{ColorProfile, DataColorSpace, Layout, TransformOptions};
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
pub const MAX_DELIVERY_BYTES: usize = 2 * 1024 * 1024;
pub const DELIVERY_POLICY: &str = "closet-v1";

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
        let (stem, modern) = name.strip_prefix("closet-v1-").map_or_else(
            || name.strip_prefix("closet-").map(|name| (name, false)),
            |name| Some((name, true)),
        )?;
        stem.strip_suffix(".png")
            .or_else(|| modern.then(|| stem.strip_suffix(".jpg")).flatten())?
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryFormat {
    Png,
    Jpeg,
}
impl DeliveryFormat {
    pub fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
        }
    }
    fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
        }
    }
    pub fn from_upload_name(name: &str) -> Option<Self> {
        ImageSha::from_upload_name(name)?;
        if name.ends_with(".png") {
            Some(Self::Png)
        } else {
            Some(Self::Jpeg)
        }
    }
}
/// The only uploadable image. Source identity and actual delivery identity are
/// distinct; private fields prevent callers from labeling raw bytes as optimized.
#[derive(Debug)]
pub struct PreparedImage {
    source: LocalImage,
    wire_sha: ImageSha,
    format: DeliveryFormat,
    bytes: Vec<u8>,
    width: u32,
    height: u32,
}
impl PreparedImage {
    pub fn source(&self) -> &LocalImage {
        &self.source
    }
    pub fn wire_sha(&self) -> &ImageSha {
        &self.wire_sha
    }
    pub fn format(&self) -> DeliveryFormat {
        self.format
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    pub fn upload_name(&self) -> String {
        format!(
            "{DELIVERY_POLICY}-{}.{}",
            self.wire_sha,
            self.format.extension()
        )
    }
}
pub fn prepare_images(assets: Vec<LocalImage>) -> Result<Vec<PreparedImage>, ImageError> {
    if assets.len() > MAX_IMAGES
        || assets
            .iter()
            .try_fold(0_u64, |total, asset| total.checked_add(asset.size))
            .is_none_or(|total| total > MAX_IMAGE_TOTAL_BYTES)
    {
        return Err(ImageError("image_total_too_large"));
    }
    let mut prepared = Vec::with_capacity(assets.len());
    let mut total = 0_u64;
    for asset in assets {
        let image = asset.prepare()?;
        total += image.bytes.len() as u64;
        if total > MAX_IMAGE_TOTAL_BYTES {
            return Err(ImageError("image_delivery_total_too_large"));
        }
        prepared.push(image);
    }
    Ok(prepared)
}
#[derive(Debug)]
pub struct LocalImages {
    pub assets: Vec<PreparedImage>,
    pub native_origin: Option<Url>,
    manifest_path: PathBuf,
    manifest_hash: ImageSha,
    directory: PathBuf,
}
struct ImageSources {
    assets: Vec<LocalImage>,
    native_origin: Option<Url>,
    manifest_path: PathBuf,
    manifest_hash: ImageSha,
    directory: PathBuf,
}
#[derive(Debug, Serialize)]
pub struct ImageValidationReport {
    pub images: usize,
    pub bytes: u64,
    pub delivery_policy: &'static str,
    pub upload_bytes: u64,
    pub upload_fingerprint: ImageSha,
}
impl ImageValidationReport {
    pub fn from_assets(assets: &[PreparedImage]) -> Self {
        let mut identities: Vec<_> = assets.iter().collect();
        identities.sort_by_key(|image| &image.source.sha256);
        let mut fingerprint = Vec::from(DELIVERY_POLICY.as_bytes());
        for image in identities {
            fingerprint.extend(image.source.sha256.as_str().as_bytes());
            fingerprint.extend(image.wire_sha.as_str().as_bytes());
        }
        Self {
            images: assets.len(),
            bytes: assets.iter().map(|image| image.source.size).sum(),
            delivery_policy: DELIVERY_POLICY,
            upload_bytes: assets
                .iter()
                .map(|image| (&image.wire_sha, image.bytes.len() as u64))
                .collect::<BTreeMap<_, _>>()
                .values()
                .sum(),
            upload_fingerprint: ImageSha::digest(&fingerprint),
        }
    }
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
    // Check dimensions before the codec can allocate a decompressed image.
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

fn png_orientation(
    bytes: &[u8],
    has_icc: bool,
) -> Result<image::metadata::Orientation, ImageError> {
    // PNG permits eXIf after IDAT. Use image's TIFF parser on a bounded chunk,
    // including that placement, rather than depending on decoder read-ahead.
    let mut position = 8_usize;
    let mut exif = None;
    let mut non_srgb = false;
    while position < bytes.len() {
        let length = word(bytes, position).ok_or(ImageError("invalid_png_data"))? as usize;
        let end = position
            .checked_add(length)
            .and_then(|end| end.checked_add(12))
            .filter(|end| *end <= bytes.len())
            .ok_or(ImageError("invalid_png_data"))?;
        if word(bytes, end - 4) != Some(crc32(&bytes[position + 4..end - 4]))
            || (bytes.get(position + 4..position + 8) == Some(b"IEND") && end != bytes.len())
        {
            return Err(ImageError("invalid_png_data"));
        }
        match bytes.get(position + 4..position + 8) {
            Some(b"cICP")
                if !has_icc && bytes.get(position + 8..end - 4) == Some(&[1, 13, 0, 1]) => {}
            Some(b"cICP") => return Err(ImageError("unsupported_image_color_space")),
            Some(b"gAMA") => {
                if length != 4 {
                    return Err(ImageError("invalid_image_metadata"));
                }
                non_srgb |= word(bytes, position + 8) != Some(45455);
            }
            Some(b"cHRM") => {
                if length != 32 {
                    return Err(ImageError("invalid_image_metadata"));
                }
                non_srgb |= [31270, 32900, 64000, 33000, 30000, 60000, 15000, 6000]
                    .iter()
                    .enumerate()
                    .any(|(index, value)| word(bytes, position + 8 + index * 4) != Some(*value));
            }
            Some(b"iCCP") if !has_icc => return Err(ImageError("invalid_image_metadata")),
            _ => {}
        }
        if bytes.get(position + 4..position + 8) == Some(b"eXIf") {
            if exif.is_some()
                || length > 1024 * 1024
                || word(bytes, end - 4) != Some(crc32(&bytes[position + 4..end - 4]))
            {
                return Err(ImageError("invalid_image_metadata"));
            }
            exif = Some(&bytes[position + 8..end - 4]);
        }
        position = end;
    }
    // ICC takes precedence over legacy gamma/chromaticity. Without a profile,
    // unsupported color interpretation must not silently become untagged sRGB.
    if non_srgb && !has_icc {
        return Err(ImageError("unsupported_image_color_space"));
    }
    Ok(exif
        .and_then(image::metadata::Orientation::from_exif_chunk)
        .unwrap_or(image::metadata::Orientation::NoTransforms))
}

#[derive(Default)]
struct DeliveryBuffer {
    bytes: Vec<u8>,
    exceeded: bool,
}
impl Write for DeliveryBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len() + bytes.len() > MAX_DELIVERY_BYTES {
            self.exceeded = true;
            return Err(std::io::Error::other("image delivery budget exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encode_delivery(
    image: &DynamicImage,
    format: DeliveryFormat,
    icc: &[u8],
) -> Result<Option<Vec<u8>>, ImageError> {
    let mut output = DeliveryBuffer::default();
    let result = match format {
        DeliveryFormat::Png => {
            let mut encoder = image::codecs::png::PngEncoder::new_with_quality(
                &mut output,
                image::codecs::png::CompressionType::Best,
                image::codecs::png::FilterType::Adaptive,
            );
            encoder
                .set_icc_profile(icc.to_vec())
                .map_err(|_| ImageError("invalid_image_metadata"))?;
            encoder.write_image(
                image.as_bytes(),
                image.width(),
                image.height(),
                image.color().into(),
            )
        }
        DeliveryFormat::Jpeg => {
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, 92);
            encoder
                .set_icc_profile(icc.to_vec())
                .map_err(|_| ImageError("invalid_image_metadata"))?;
            encoder.encode_image(&image.to_rgb8())
        }
    };
    if output.exceeded {
        return Ok(None);
    }
    result.map_err(|_| ImageError("image_encode_failed"))?;
    Ok(Some(output.bytes))
}
fn has_transparency(image: &DynamicImage) -> bool {
    match image {
        DynamicImage::ImageRgba8(image) => image.pixels().any(|pixel| pixel[3] != u8::MAX),
        DynamicImage::ImageLumaA8(image) => image.pixels().any(|pixel| pixel[1] != u8::MAX),
        DynamicImage::ImageRgba16(image) => image.pixels().any(|pixel| pixel[3] != u16::MAX),
        DynamicImage::ImageLumaA16(image) => image.pixels().any(|pixel| pixel[1] != u16::MAX),
        _ => false,
    }
}

struct LinearImage {
    pixels: DynamicImage,
    to_srgb: std::sync::Arc<moxcms::TransformF32Executor>,
    color: image::ColorType,
}
impl LinearImage {
    fn new(image: DynamicImage, profile: &ColorProfile) -> Result<Self, ImageError> {
        let color = image.color();
        let layout = if profile.color_space == DataColorSpace::Gray {
            Layout::GrayAlpha
        } else {
            Layout::Rgba
        };
        // Broad, scene-linear RGB avoids clipping ordinary wide-gamut sources
        // during interpolation. The resizer supports finite SDR samples [0,1].
        let working = ColorProfile::new_aces_aces_2065_1_linear();
        let options = TransformOptions {
            allow_use_cicp_transfer: false,
            prefer_fixed_point: false,
            ..Default::default()
        };
        let forward = profile
            .create_transform_f32(layout, &working, Layout::Rgba, options)
            .map_err(|_| ImageError("unsupported_image_color_space"))?;
        let to_srgb = working
            .create_transform_f32(
                Layout::Rgba,
                &ColorProfile::new_srgb(),
                Layout::Rgba,
                options,
            )
            .map_err(|_| ImageError("unsupported_image_color_space"))?;
        let mut pixels = image.into_rgba32f();
        let width = pixels.width() as usize;
        let mut input = vec![0_f32; width * layout.channels()];
        for row in pixels.as_mut().chunks_exact_mut(width * 4) {
            if layout == Layout::GrayAlpha {
                for (source, target) in row
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(input.as_chunks_mut::<2>().0.iter_mut())
                {
                    target.copy_from_slice(&[source[0], source[3]]);
                }
            } else {
                input.copy_from_slice(row);
            }
            forward
                .transform(&input, row)
                .map_err(|_| ImageError("image_color_transform_failed"))?;
            for pixel in row.as_chunks_mut::<4>().0.iter_mut() {
                let alpha = pixel[3];
                for channel in &mut pixel[..3] {
                    if !channel.is_finite()
                        || (alpha > 0. && !(-0.00001..=1.00001).contains(channel))
                    {
                        return Err(ImageError("unsupported_image_color_space"));
                    }
                    *channel *= alpha;
                }
            }
        }
        Ok(Self {
            pixels: DynamicImage::ImageRgba32F(pixels),
            to_srgb,
            color,
        })
    }
    fn resize(&self, edge: u32) -> Result<DynamicImage, ImageError> {
        let mut resized = self
            .pixels
            .resize(edge, edge, FilterType::Lanczos3)
            .into_rgba32f();
        let width = resized.width() as usize;
        let mut encoded = vec![0_f32; width * 4];
        for row in resized.as_mut().chunks_exact_mut(width * 4) {
            for pixel in row.as_chunks_mut::<4>().0.iter_mut() {
                let alpha = pixel[3];
                for channel in &mut pixel[..3] {
                    *channel = if alpha > 0. {
                        (*channel / alpha).clamp(0., 1.)
                    } else {
                        0.
                    };
                }
            }
            self.to_srgb
                .transform(row, &mut encoded)
                .map_err(|_| ImageError("image_color_transform_failed"))?;
            row.copy_from_slice(&encoded);
        }
        let resized = DynamicImage::ImageRgba32F(resized);
        Ok(match self.color {
            image::ColorType::L8 | image::ColorType::Rgb8 => {
                DynamicImage::ImageRgb8(resized.into_rgb8())
            }
            image::ColorType::La8 | image::ColorType::Rgba8 => {
                DynamicImage::ImageRgba8(resized.into_rgba8())
            }
            image::ColorType::L16 | image::ColorType::Rgb16 => {
                DynamicImage::ImageRgb16(resized.into_rgb16())
            }
            image::ColorType::La16 | image::ColorType::Rgba16 => {
                DynamicImage::ImageRgba16(resized.into_rgba16())
            }
            _ => return Err(ImageError("unsupported_image_color_space")),
        })
    }
}
impl LocalImage {
    fn prepare(self) -> Result<PreparedImage, ImageError> {
        let bytes = self.read_verified()?;
        let mut limits = Limits::default();
        limits.max_image_width = Some(16384);
        limits.max_image_height = Some(16384);
        // PNG's decoder accounts for pixel and ancillary decompression buffers.
        // 40M RGBA16 pixels need 320 MB; retained deliveries are separately capped.
        limits.max_alloc = Some(384 * 1024 * 1024);
        let mut decoder = image::codecs::png::PngDecoder::with_limits(Cursor::new(&bytes), limits)
            .map_err(|_| ImageError("invalid_png_data"))?;
        if decoder
            .is_apng()
            .map_err(|_| ImageError("invalid_png_data"))?
        {
            return Err(ImageError("animated_image_not_supported"));
        }
        let icc = decoder
            .icc_profile()
            .map_err(|_| ImageError("invalid_image_metadata"))?
            .unwrap_or_default();
        if icc.len() > 1024 * 1024 {
            return Err(ImageError("image_metadata_too_large"));
        }
        let orientation = png_orientation(&bytes, !icc.is_empty())?;
        let mut decoded =
            DynamicImage::from_decoder(decoder).map_err(|_| ImageError("invalid_png_data"))?;
        decoded.apply_orientation(orientation);
        let profile = if icc.is_empty() {
            ColorProfile::new_srgb()
        } else {
            ColorProfile::new_from_slice(&icc).map_err(|_| ImageError("invalid_image_metadata"))?
        };
        if profile.cicp.as_ref().is_some_and(|cicp| {
            matches!(
                cicp.transfer_characteristics,
                moxcms::TransferCharacteristics::Smpte2084 | moxcms::TransferCharacteristics::Hlg
            )
        }) {
            return Err(ImageError("unsupported_image_color_space"));
        }
        if !icc.is_empty()
            && !matches!(
                (profile.color_space, decoded.color()),
                (
                    DataColorSpace::Rgb,
                    image::ColorType::Rgb8
                        | image::ColorType::Rgba8
                        | image::ColorType::Rgb16
                        | image::ColorType::Rgba16
                ) | (
                    DataColorSpace::Gray,
                    image::ColorType::L8
                        | image::ColorType::La8
                        | image::ColorType::L16
                        | image::ColorType::La16
                )
            )
        {
            return Err(ImageError("unsupported_image_color_space"));
        }
        let transparent = has_transparency(&decoded);
        let long_edge = decoded.width().max(decoded.height());
        let mut original = Some(decoded);
        let mut linear = None;
        let mut previous = 0;
        // Small originals get one conservative HD fallback instead of repeating
        // their unchanged dimensions at every larger policy edge.
        let final_edge = if long_edge < 1280 { 1024 } else { 1280 };
        for edge in [2048, 1792, 1536, 1280, final_edge] {
            let edge = edge.min(long_edge);
            if edge == previous {
                continue;
            }
            previous = edge;
            let resized = if long_edge > edge || !icc.is_empty() {
                if linear.is_none() {
                    linear = Some(LinearImage::new(
                        original.take().ok_or(ImageError("image_encode_failed"))?,
                        &profile,
                    )?);
                }
                linear
                    .as_ref()
                    .ok_or(ImageError("image_encode_failed"))?
                    .resize(edge)?
            } else {
                original
                    .as_ref()
                    .ok_or(ImageError("image_encode_failed"))?
                    .clone()
            };
            let mut candidate = encode_delivery(&resized, DeliveryFormat::Png, &[])?
                .map(|bytes| (DeliveryFormat::Png, bytes));
            if !transparent
                && let Some(jpeg) = encode_delivery(&resized, DeliveryFormat::Jpeg, &[])?
                && candidate
                    .as_ref()
                    .is_none_or(|(_, png)| jpeg.len() < png.len())
            {
                candidate = Some((DeliveryFormat::Jpeg, jpeg));
            }
            if let Some((format, bytes)) = candidate {
                return Ok(PreparedImage {
                    source: self,
                    wire_sha: ImageSha::digest(&bytes),
                    format,
                    width: resized.width(),
                    height: resized.height(),
                    bytes,
                });
            }
        }
        Err(ImageError("image_delivery_budget_exceeded"))
    }
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
        if self.size == 0 || self.size > MAX_IMAGE_BYTES {
            return Err(ImageError("invalid_image_source"));
        }
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
        let sources = Self::sources(manifest_path, directory, items)?;
        Ok(Self {
            assets: prepare_images(sources.assets)?,
            native_origin: sources.native_origin,
            manifest_path: sources.manifest_path,
            manifest_hash: sources.manifest_hash,
            directory: sources.directory,
        })
    }
    fn sources(
        manifest_path: &Path,
        directory: &Path,
        items: &[CatalogItem],
    ) -> Result<ImageSources, ImageError> {
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
        Ok(ImageSources {
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
        Self::sources(&self.manifest_path, &self.directory, items)?;
        Ok(())
    }
    pub fn report(&self) -> ImageValidationReport {
        ImageValidationReport::from_assets(&self.assets)
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
            let asset = asset.source();
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
    use moxcms::ToneReprCurve;
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
    fn full_decode_rejects_corrupted_image_before_materializing_or_uploading() {
        let items = items();
        let mut bytes = png();
        bytes[45] ^= 0xff;
        let mut mapping = entry(&items[0]);
        mapping.sha256 = ImageSha::digest(&bytes);
        assert_eq!(
            fixture(vec![mapping], &items, &bytes).1.unwrap_err().0,
            "invalid_png_data"
        );
    }
    #[test]
    fn valid_png_envelope_does_not_hide_corrupt_compressed_pixels_or_excessive_dimensions() {
        let items = items();
        let mut bytes = png();
        bytes[41] ^= 0xff;
        let crc = crc32(&bytes[37..52]).to_be_bytes();
        bytes[52..56].copy_from_slice(&crc);
        let mut mapping = entry(&items[0]);
        mapping.sha256 = ImageSha::digest(&bytes);
        assert_eq!(
            fixture(vec![mapping], &items, &bytes).1.unwrap_err().0,
            "invalid_png_data"
        );
        let mut bytes = png();
        bytes[16..20].copy_from_slice(&10_000_u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&5_000_u32.to_be_bytes());
        let crc = crc32(&bytes[12..29]).to_be_bytes();
        bytes[29..33].copy_from_slice(&crc);
        let mut mapping = entry(&items[0]);
        mapping.sha256 = ImageSha::digest(&bytes);
        assert_eq!(
            fixture(vec![mapping], &items, &bytes).1.unwrap_err().0,
            "invalid_png_dimensions"
        );
    }
    fn encoded_png(image: &DynamicImage, icc: &[u8], orientation: Option<u8>) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::png::PngEncoder::new_with_quality(
            &mut bytes,
            image::codecs::png::CompressionType::Fast,
            image::codecs::png::FilterType::Adaptive,
        );
        encoder.set_icc_profile(icc.to_vec()).unwrap();
        if let Some(orientation) = orientation {
            let mut exif =
                b"II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x01\0\0\0\0\0\0\0".to_vec();
            exif[18] = orientation;
            encoder.set_exif_metadata(exif).unwrap();
        }
        encoder
            .write_image(
                image.as_bytes(),
                image.width(),
                image.height(),
                image.color().into(),
            )
            .unwrap();
        bytes
    }
    fn prepare(bytes: &[u8]) -> (tempfile::TempDir, PreparedImage) {
        let items = items();
        let mut mapping = entry(&items[0]);
        mapping.sha256 = ImageSha::digest(bytes);
        let (temp, local) = fixture(vec![mapping], &items, bytes);
        (temp, local.unwrap().assets.remove(0))
    }
    fn noise(width: u32, height: u32, alpha: bool) -> DynamicImage {
        let mut state = 123456789_u32;
        let pixels = image::RgbaImage::from_fn(width, height, |_, _| {
            let mut channel = || {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            };
            image::Rgba([
                channel(),
                channel(),
                channel(),
                if alpha { channel().min(254) } else { 255 },
            ])
        });
        DynamicImage::ImageRgba8(pixels)
    }
    fn profile(space: &[u8; 4]) -> Vec<u8> {
        if space == b"GRAY" {
            ColorProfile::new_gray_with_gamma(2.2)
        } else {
            ColorProfile::new_srgb()
        }
        .encode()
        .unwrap()
    }
    #[test]
    fn unsupported_png_color_interpretation_fails_before_delivery_instead_of_recoloring() {
        let image = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            32,
            32,
            image::Rgb([64, 128, 192]),
        ));
        for (kind, payload) in [
            (b"gAMA", 100000_u32.to_be_bytes().to_vec()),
            (b"cICP", vec![1, 16, 0, 1]),
            (b"cHRM", vec![0; 32]),
        ] {
            let mut bytes = encoded_png(&image, &[], None);
            let mut chunk = (payload.len() as u32).to_be_bytes().to_vec();
            chunk.extend(kind);
            chunk.extend(payload);
            chunk.extend(crc32(&chunk[4..]).to_be_bytes());
            bytes.splice(33..33, chunk);
            let items = items();
            let mut mapping = entry(&items[0]);
            mapping.sha256 = ImageSha::digest(&bytes);
            assert_eq!(
                fixture(vec![mapping], &items, &bytes).1.unwrap_err().0,
                "unsupported_image_color_space"
            );
        }
    }
    #[test]
    fn delivery_normalizes_icc_preserves_sixteen_bit_alpha_and_applies_orientation_once() {
        let mut pixels = image::ImageBuffer::new(2, 1);
        pixels.put_pixel(0, 0, image::Rgba([65535_u16, 0, 0, 65534]));
        pixels.put_pixel(1, 0, image::Rgba([0, 65535, 0, 12345]));
        let original = DynamicImage::ImageRgba16(pixels);
        let profile = profile(b"RGB ");
        let mut bytes = encoded_png(&original, &profile, Some(6));
        // eXIf is legal after IDAT, so exercise that placement as well.
        let mut pos = 8;
        while bytes.get(pos + 4..pos + 8) != Some(b"eXIf") {
            pos += word(&bytes, pos).unwrap() as usize + 12;
        }
        let end = pos + word(&bytes, pos).unwrap() as usize + 12;
        let exif: Vec<_> = bytes.drain(pos..end).collect();
        let insertion = bytes.len() - 12;
        bytes.splice(insertion..insertion, exif);
        let (temp, prepared) = prepare(&bytes);
        assert_eq!(prepared.format(), DeliveryFormat::Png);
        assert_eq!(prepared.dimensions(), (1, 2));
        let mut decoder =
            image::codecs::png::PngDecoder::new(Cursor::new(prepared.bytes())).unwrap();
        assert!(decoder.icc_profile().unwrap().is_none());
        assert!(decoder.exif_metadata().unwrap().is_none());
        assert_eq!(
            decoder.orientation().unwrap(),
            image::metadata::Orientation::NoTransforms
        );
        let decoded = DynamicImage::from_decoder(decoder).unwrap().into_rgba16();
        for (index, expected) in [[65535_u16, 0, 0, 65534], [0, 65535, 0, 12345]]
            .into_iter()
            .enumerate()
        {
            let actual = decoded.get_pixel(0, index as u32);
            assert_eq!(actual[3], expected[3]);
            // ICC's quantized matrix can change sub-display-precision RGB while
            // conversion preserves the rendered primary and all alpha bits.
            assert!(
                actual.0[..3]
                    .iter()
                    .zip(expected)
                    .all(|(actual, expected)| actual.abs_diff(expected) <= 128)
            );
        }
        assert_eq!(std::fs::read(&prepared.source().path).unwrap(), bytes);
        assert_eq!(ImageSha::digest(prepared.bytes()), *prepared.wire_sha());
        assert_eq!(ImageSha::digest(&bytes), prepared.source().sha256);
        let (_again, repeat) = prepare(&bytes);
        assert_eq!(repeat.bytes(), prepared.bytes());
        drop(temp);
    }
    #[test]
    fn opaque_delivery_uses_jpeg_without_upscaling_and_has_distinct_byte_identity() {
        let icc = ColorProfile::new_display_p3().encode().unwrap();
        let bytes = encoded_png(&noise(768, 512, false), &icc, None);
        let (_temp, prepared) = prepare(&bytes);
        assert_eq!(prepared.format(), DeliveryFormat::Jpeg);
        assert_eq!(prepared.dimensions(), (768, 512));
        assert!(prepared.bytes().len() < bytes.len());
        assert!(prepared.bytes().len() <= MAX_DELIVERY_BYTES);
        assert_ne!(*prepared.wire_sha(), prepared.source().sha256);
        assert_eq!(
            ImageSha::from_upload_name(&prepared.upload_name()),
            Some(prepared.wire_sha().clone())
        );
        let mut decoder =
            image::codecs::jpeg::JpegDecoder::new(Cursor::new(prepared.bytes())).unwrap();
        assert!(decoder.icc_profile().unwrap().is_none());
        assert_eq!(decoder.dimensions(), (768, 512));
    }
    #[test]
    fn wide_gamut_icc_is_normalized_for_native_thumbnails_without_profile_dependency() {
        let profile = ColorProfile::new_display_p3().encode().unwrap();
        let pixels = image::RgbImage::from_pixel(32, 32, image::Rgb([200, 80, 30]));
        let bytes = encoded_png(&DynamicImage::ImageRgb8(pixels), &profile, None);
        let (_temp, prepared) = prepare(&bytes);
        assert_eq!(prepared.format(), DeliveryFormat::Png);
        let mut decoder =
            image::codecs::png::PngDecoder::new(Cursor::new(prepared.bytes())).unwrap();
        assert!(decoder.icc_profile().unwrap().is_none());
        let decoded = DynamicImage::from_decoder(decoder).unwrap().into_rgb8();
        // Independent Chromium canvas reference for this Display-P3 patch.
        assert!(decoded.pixels().all(|pixel| {
            pixel
                .0
                .iter()
                .zip([216_u8, 69, 0])
                .all(|(actual, expected)| actual.abs_diff(expected) <= 1)
        }));
    }
    #[test]
    fn gray_icc_normalizes_to_sixteen_bit_srgb_and_hdr_is_explicitly_unsupported() {
        let icc = profile(b"GRAY");
        let pixels = image::ImageBuffer::from_pixel(3072, 4, image::Luma([16384_u16]));
        let bytes = encoded_png(&DynamicImage::ImageLuma16(pixels), &icc, None);
        let (_temp, prepared) = prepare(&bytes);
        assert_eq!(prepared.format(), DeliveryFormat::Png);
        let mut decoder =
            image::codecs::png::PngDecoder::new(Cursor::new(prepared.bytes())).unwrap();
        assert!(decoder.icc_profile().unwrap().is_none());
        assert_eq!(decoder.color_type(), image::ColorType::Rgb16);
        let decoded = DynamicImage::from_decoder(decoder).unwrap().into_rgb16();
        assert!(
            decoded
                .pixels()
                .all(|pixel| pixel.0.iter().all(|value| value.abs_diff(15797) <= 8)),
            "{:?}",
            decoded.get_pixel(0, 0)
        );
        let profile = ColorProfile::new_display_p3_pq().encode().unwrap();
        let image = DynamicImage::ImageRgb16(image::ImageBuffer::from_pixel(
            2,
            2,
            image::Rgb([10000_u16; 3]),
        ));
        let bytes = encoded_png(&image, &profile, None);
        let items = items();
        let mut mapping = entry(&items[0]);
        mapping.sha256 = ImageSha::digest(&bytes);
        assert_eq!(
            fixture(vec![mapping], &items, &bytes).1.unwrap_err().0,
            "unsupported_image_color_space"
        );
    }
    #[test]
    fn resized_garment_edges_ignore_all_rgb_hidden_under_zero_alpha() {
        let mut outputs = Vec::new();
        for hidden in [[255, 0, 255], [0, 0, 0]] {
            let pixels = image::RgbaImage::from_fn(3072, 1536, |x, _| {
                if x < 2048 {
                    image::Rgba([255, 255, 255, 255])
                } else {
                    image::Rgba([hidden[0], hidden[1], hidden[2], 0])
                }
            });
            let bytes = encoded_png(&DynamicImage::ImageRgba8(pixels), &[], None);
            let (_temp, prepared) = prepare(&bytes);
            assert_eq!(prepared.dimensions(), (2048, 1024));
            let decoded = image::load_from_memory(prepared.bytes())
                .unwrap()
                .into_rgba8();
            assert!(
                decoded
                    .pixels()
                    .filter(|pixel| pixel[3] > 0)
                    .all(|pixel| pixel[0] >= 254 && pixel[1] >= 254 && pixel[2] >= 254)
            );
            outputs.push(prepared.bytes().to_vec());
        }
        assert_eq!(outputs[0], outputs[1]);
    }
    #[test]
    fn nontrivial_icc_gamma_is_normalized_to_srgb_after_linear_resampling() {
        let mut profile = ColorProfile::new_display_p3();
        profile.red_trc = Some(ToneReprCurve::Parametric(vec![1.8]));
        profile.green_trc = profile.red_trc.clone();
        profile.blue_trc = profile.red_trc.clone();
        profile.cicp = None;
        let icc = profile.encode().unwrap();
        for checker in [false, true] {
            let pixels = image::RgbImage::from_fn(4096, 4, |x, _| {
                if checker {
                    image::Rgb([if x % 2 == 0 { 64 } else { 192 }; 3])
                } else {
                    image::Rgb([128; 3])
                }
            });
            let bytes = encoded_png(&DynamicImage::ImageRgb8(pixels), &icc, None);
            let (_temp, prepared) = prepare(&bytes);
            assert_eq!(prepared.format(), DeliveryFormat::Png);
            let mut decoder =
                image::codecs::png::PngDecoder::new(Cursor::new(prepared.bytes())).unwrap();
            assert!(decoder.icc_profile().unwrap().is_none());
            let decoded = DynamicImage::from_decoder(decoder).unwrap().into_rgb8();
            let pixel = decoded.get_pixel(1000, 0);
            if checker {
                assert!(
                    pixel.0.iter().all(|value| (156..=160).contains(value)),
                    "{pixel:?}"
                );
            } else {
                for actual in pixel.0.iter() {
                    assert!(actual.abs_diff(146) <= 2, "{pixel:?}");
                }
            }
        }
    }
    #[test]
    fn delivery_resizes_with_aspect_ratio_and_fails_instead_of_discarding_alpha_detail() {
        let pixels = image::RgbaImage::from_fn(3072, 1536, |x, y| {
            image::Rgba([(x % 251) as u8, (y % 251) as u8, 80, 123])
        });
        let bytes = encoded_png(&DynamicImage::ImageRgba8(pixels), &[], None);
        let (_temp, prepared) = prepare(&bytes);
        assert_eq!(prepared.format(), DeliveryFormat::Png);
        assert_eq!(prepared.dimensions(), (2048, 1024));
        let decoded = image::load_from_memory(prepared.bytes())
            .unwrap()
            .into_rgba8();
        assert!(decoded.pixels().all(|pixel| pixel[3] == 123));
        let oversized = encoded_png(&noise(1024, 1024, true), &[], None);
        let items = items();
        let mut mapping = entry(&items[0]);
        mapping.sha256 = ImageSha::digest(&oversized);
        assert_eq!(
            fixture(vec![mapping], &items, &oversized).1.unwrap_err().0,
            "image_delivery_budget_exceeded"
        );
    }
    #[test]
    fn small_originals_get_one_hd_fallback_when_lossless_alpha_exceeds_delivery_budget() {
        let mut pixels = noise(1094, 1237, false).into_rgba8();
        for (x, _, pixel) in pixels.enumerate_pixels_mut() {
            if x >= 650 {
                *pixel = image::Rgba([0, 0, 0, 0]);
            }
        }
        let bytes = encoded_png(&DynamicImage::ImageRgba8(pixels), &[], None);
        assert!(bytes.len() > MAX_DELIVERY_BYTES);
        let (_temp, prepared) = prepare(&bytes);
        assert_eq!(prepared.format(), DeliveryFormat::Png);
        assert_eq!(prepared.dimensions().0.max(prepared.dimensions().1), 1024);
        assert!(prepared.bytes().len() <= MAX_DELIVERY_BYTES);
    }
    #[test]
    fn gallery_binding_rejects_wrong_hint_index_duplicates_and_changed_sources() {
        let items = items();
        let entry = entry(&items[0]);
        let (_temp, local) = fixture(vec![entry.clone()], &items, &png());
        let local = local.unwrap();
        assert_eq!(local.report().images, 1);
        local.verify_sources(&items).unwrap();
        fs::write(&local.assets[0].source().path, b"changed").unwrap();
        assert_eq!(
            local.assets[0].source().read_verified().unwrap_err().0,
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
        assert_eq!(local.assets[0].source().slots.len(), 2);
        let new_id: NativeImageId = "2".repeat(32).try_into().unwrap();
        let resolved = BTreeMap::from([(local.assets[0].source().sha256.clone(), new_id.clone())]);
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
            let path = &local.assets[0].source().path;
            fs::remove_file(path).unwrap();
            std::os::unix::fs::symlink(temp.path().join("images.json"), path).unwrap();
            assert_eq!(
                local.verify_sources(&items).unwrap_err().0,
                "invalid_image_source"
            );
        }
    }
}
