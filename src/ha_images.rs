//! Home Assistant's native image APIs, using library WebSocket and multipart transports.
use futures_util::{SinkExt, StreamExt};
use reqwest::{
    header::{AUTHORIZATION, CONTENT_TYPE},
    multipart::{Form, Part},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{collections::BTreeSet, time::Duration};
use tokio::{
    net::TcpStream,
    time::{Instant, timeout, timeout_at},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};

use crate::{
    client::{ApiClient, Failure, Result, Transport},
    images::{ImageSha, LocalImage, MAX_IMAGE_BYTES, NativeImageId},
};
const READ_TIMEOUT: Duration = Duration::from_secs(15);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeImage {
    pub id: NativeImageId,
    pub name: String,
    pub content_type: String,
    pub filesize: u64,
}
#[derive(Serialize)]
#[serde(tag = "type")]
enum Command<'a> {
    #[serde(rename = "auth")]
    Auth { access_token: &'a str },
    #[serde(rename = "image/list")]
    List { id: u64 },
    #[serde(rename = "image/delete")]
    Delete {
        id: u64,
        image_id: &'a NativeImageId,
    },
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Reply<T> {
    AuthRequired {},
    AuthOk {},
    AuthInvalid {},
    Result {
        id: u64,
        success: bool,
        result: Option<T>,
    },
}
async fn receive<T: DeserializeOwned>(socket: &mut Socket) -> Result<Reply<T>> {
    for _ in 0..8 {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(&text)
                    .map_err(|_| Failure::local("invalid_native_image_response"));
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            _ => return Err(Failure::local("native_image_connection_failed")),
        }
    }
    Err(Failure::local("invalid_native_image_response"))
}
async fn send(socket: &mut Socket, command: &Command<'_>) -> Result<()> {
    let json = serde_json::to_string(command)
        .map_err(|_| Failure::local("native_image_request_failed"))?;
    socket
        .send(Message::Text(json.into()))
        .await
        .map_err(|_| Failure::local("native_image_connection_failed"))
}

pub struct HaImages<'a> {
    api: &'a ApiClient,
}
impl<'a> HaImages<'a> {
    pub fn new(api: &'a ApiClient) -> Result<Self> {
        if api.transport != Transport::HomeAssistant {
            return Err(Failure::local("image_transport_required"));
        }
        Ok(Self { api })
    }
    async fn command<T: DeserializeOwned>(&self, command: Command<'_>) -> Result<Option<T>> {
        let mut url = self
            .api
            .origin
            .join("/api/websocket")
            .map_err(|_| Failure::local("invalid_url"))?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|_| Failure::local("invalid_url"))?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(2 * 1024 * 1024))
            .max_frame_size(Some(2 * 1024 * 1024));
        let deadline = Instant::now() + READ_TIMEOUT;
        let (mut socket, _) = timeout_at(
            deadline,
            connect_async_tls_with_config(
                url.as_str(),
                Some(config),
                true,
                self.api.connector.clone(),
            ),
        )
        .await
        .map_err(|_| Failure::local("native_image_timeout"))?
        .map_err(|_| Failure::local("native_image_connection_failed"))?;
        let result = timeout_at(deadline, async {
            if !matches!(receive::<T>(&mut socket).await?, Reply::AuthRequired {}) {
                return Err(Failure::local("native_image_auth_failed"));
            }
            let token = self
                .api
                .authorization
                .to_str()
                .ok()
                .and_then(|value| value.strip_prefix("Bearer "))
                .ok_or_else(|| Failure::local("invalid_credential"))?;
            send(
                &mut socket,
                &Command::Auth {
                    access_token: token,
                },
            )
            .await?;
            if !matches!(receive::<T>(&mut socket).await?, Reply::AuthOk {}) {
                return Err(Failure::local("native_image_auth_failed"));
            }
            send(&mut socket, &command).await?;
            match receive(&mut socket).await? {
                Reply::Result {
                    id: 1,
                    success: true,
                    result,
                } => Ok(result),
                _ => Err(Failure::local("native_image_command_failed")),
            }
        })
        .await
        .map_err(|_| Failure::local("native_image_timeout"))
        .and_then(|result| result);
        // Explicit close is bounded; dropping either future also drops its TCP
        // stream, so cancellation cannot leave a background connection owned here.
        let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
        result
    }
    pub async fn list(&self) -> Result<Vec<NativeImage>> {
        let records: Vec<NativeImage> = self
            .command(Command::List { id: 1 })
            .await?
            .ok_or_else(|| Failure::local("invalid_native_image_response"))?;
        if records.len() > 10_000
            || records
                .iter()
                .map(|record| &record.id)
                .collect::<BTreeSet<_>>()
                .len()
                != records.len()
        {
            return Err(Failure::local("invalid_native_image_response"));
        }
        Ok(records)
    }
    /// Exactly one native delete request. The caller reconciles by listing;
    /// only an already-admitted durable pending ID may be resumed on another run.
    pub async fn delete(&self, image: &NativeImageId) -> Result<()> {
        self.command::<()>(Command::Delete {
            id: 1,
            image_id: image,
        })
        .await?;
        Ok(())
    }
    pub async fn verify(&self, image: &NativeImage, sha: &ImageSha) -> Result<()> {
        if image.content_type != "image/png"
            || image.filesize == 0
            || image.filesize > MAX_IMAGE_BYTES
        {
            return Err(Failure::local("native_image_content_mismatch"));
        }
        let url = self
            .api
            .origin
            .join(&format!("/api/image/serve/{}/original", image.id))
            .map_err(|_| Failure::local("invalid_url"))?;
        // Native originals are unauthenticated behind the configured HA origin.
        // No token is sent and redirects are never followed.
        let mut response = self
            .api
            .client
            .get(url)
            .timeout(READ_TIMEOUT)
            .send()
            .await
            .map_err(|_| Failure::local("native_image_read_failed"))?;
        if !response.status().is_success()
            || response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(';').next())
                != Some("image/png")
        {
            return Err(Failure::local("native_image_unavailable"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::local("native_image_read_failed"))?
        {
            if bytes.len() as u64 + chunk.len() as u64 > image.filesize {
                return Err(Failure::local("native_image_content_mismatch"));
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.len() as u64 != image.filesize || &ImageSha::digest(&bytes) != sha {
            return Err(Failure::local("native_image_content_mismatch"));
        }
        Ok(())
    }
    pub async fn matching(
        &self,
        asset: &LocalImage,
        records: &[NativeImage],
    ) -> Result<Option<NativeImageId>> {
        let mut matches: Vec<_> = records
            .iter()
            .filter(|image| image.name == asset.sha256.upload_name())
            .collect();
        matches.sort_by_key(|image| {
            (
                asset.native_hint.as_ref() != Some(&image.id),
                image.id.clone(),
            )
        });
        if let Some(image) = matches.first() {
            if image.filesize != asset.size {
                return Err(Failure::local("native_image_content_mismatch"));
            }
            self.verify(image, &asset.sha256).await?;
            return Ok(Some(image.id.clone()));
        }
        Ok(None)
    }
    /// Never retried. An ambiguous response requires list + exact-byte GET;
    /// absence is not permission to create another random native ID.
    pub async fn upload(&self, asset: &LocalImage) -> Result<UploadOutcome> {
        let bytes = asset.read_verified()?;
        let result = async {
            let part = Part::bytes(bytes)
                .file_name(asset.sha256.upload_name())
                .mime_str("image/png")
                .map_err(|_| Failure::local("invalid_image_source"))?;
            let url = self
                .api
                .origin
                .join("/api/image/upload")
                .map_err(|_| Failure::local("invalid_url"))?;
            let mut response = self
                .api
                .client
                .post(url)
                .header(AUTHORIZATION, self.api.authorization.clone())
                .multipart(Form::new().part("file", part))
                .timeout(UPLOAD_TIMEOUT)
                .send()
                .await
                .map_err(|_| Failure::local("image_upload_pending"))?;
            if matches!(
                response.status().as_u16(),
                400 | 401 | 403 | 404 | 405 | 413 | 415 | 422
            ) {
                return Ok(UploadOutcome::Rejected);
            }
            let mut count = 0;
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| Failure::local("image_upload_pending"))?
            {
                count += chunk.len();
                if count > 65_536 {
                    break;
                }
            }
            Ok::<_, Failure>(UploadOutcome::Reconcile)
        }
        .await;
        Ok(match result {
            Ok(outcome) => outcome,
            Err(_) => UploadOutcome::Reconcile,
        })
    }
}
pub enum UploadOutcome {
    Rejected,
    Reconcile,
}
