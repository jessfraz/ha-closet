//! A short-lived sync owner and a durable, exact pending deletion.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::images::{ImageSha, NativeImageId};

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub struct FenceToken(#[schemars(with = "String")] String);
impl FenceToken {
    pub fn new() -> Result<Self, getrandom::Error> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes)?;
        Ok(Self(ImageSha::digest(&bytes).to_string()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Debug for FenceToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FenceToken([redacted])")
    }
}
impl TryFrom<String> for FenceToken {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        ImageSha::try_from(value.clone()).map_err(|_| "invalid_fence_token")?;
        Ok(Self(value))
    }
}
impl From<FenceToken> for String {
    fn from(value: FenceToken) -> Self {
        value.0
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ManagedImage {
    pub id: NativeImageId,
    pub sha256: ImageSha,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FenceAcquire {
    pub token: FenceToken,
    pub expected_revision: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FenceStatus {
    pub catalog_revision: i64,
    pub pending: Vec<ManagedImage>,
    pub uploads: Vec<ImageSha>,
    pub valid_for_seconds: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FencePrepare {
    pub token: FenceToken,
    pub catalog_revision: i64,
    pub images: Vec<ManagedImage>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FenceResolve {
    pub token: FenceToken,
    pub image: ManagedImage,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FenceRelease {
    pub token: FenceToken,
}

/// HA decodes route escapes and ignores queries/fragments for originals and
/// thumbnails. Every native serve filename protects its object ID, irrespective
/// of its hostname. Unknown filenames deliberately over-protect rather than
/// guessing which thumbnail sizes a future HA version supports.
/// This deliberately over-protects an unrelated host with the same random ID.
pub fn referenced_image_id(value: &str) -> Option<NativeImageId> {
    let url = url::Url::parse(value).ok()?;
    let path = percent_encoding::percent_decode_str(url.path())
        .decode_utf8()
        .ok()?;
    let (id, filename) = path.strip_prefix("/api/image/serve/")?.split_once('/')?;
    if filename.is_empty() || filename.contains('/') {
        return None;
    }
    id.to_owned().try_into().ok()
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FenceReleased {
    pub released: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UploadClaimAction {
    Begin,
    Verified,
    Rejected,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FenceUpload {
    pub token: FenceToken,
    pub sha256: ImageSha,
    pub action: UploadClaimAction,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct UploadClaimStatus {
    pub claimed: bool,
}
