use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

// Retain protocol 4 compatibility with deployed 0.2 peers, including their
// saved-file delivery mode and receiver confirmations.
pub const PROTOCOL_VERSION: u16 = 4;
pub const DEFAULT_PORT: u16 = 24_871;
pub const MAX_FRAME_SIZE: usize = 32 * 1024 * 1024;
pub const MAX_DEVICE_ID_LEN: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardEvent {
    pub id: Uuid,
    pub origin: String,
    pub created_at_ms: u64,
    pub content: ClipboardContent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeliveryTarget {
    Device { device_id: String },
    All,
}

impl DeliveryTarget {
    pub fn device(device_id: impl Into<String>) -> Self {
        Self::Device {
            device_id: device_id.into(),
        }
    }
}

impl ClipboardEvent {
    pub fn new(origin: impl Into<String>, content: ClipboardContent) -> Self {
        Self {
            id: Uuid::new_v4(),
            origin: origin.into(),
            created_at_ms: unix_time_ms(),
            content,
        }
    }

    pub fn text(origin: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(origin, ClipboardContent::Text { text: text.into() })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileEntryKind {
    File,
    Directory,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Portable, slash-separated path relative to the transfer's staging directory.
    pub relative_path: String,
    pub kind: FileEntryKind,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClipboardContent {
    Text {
        text: String,
    },
    ImagePng {
        png_base64: String,
    },
    FilesStart {
        transfer_id: Uuid,
        delivery: FileDelivery,
        roots: Vec<String>,
        entries: Vec<FileEntry>,
        total_bytes: u64,
    },
    FileChunk {
        transfer_id: Uuid,
        file_index: u32,
        offset: u64,
        data_base64: String,
    },
    FilesComplete {
        transfer_id: Uuid,
        /// One SHA-256 digest per entry; directories have no digest.
        sha256: Vec<Option<String>>,
    },
    /// Sent by the receiver only after all bytes have passed integrity checks.
    FilesReceived {
        transfer_id: Uuid,
        item_count: usize,
        delivery: FileDelivery,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileDelivery {
    Clipboard,
    Save,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        protocol_version: u16,
        device_id: String,
        auth_token: String,
    },
    Publish {
        target: DeliveryTarget,
        event: ClipboardEvent,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome {
        protocol_version: u16,
        device_id: String,
    },
    Event {
        sequence: u64,
        event: ClipboardEvent,
    },
    Published {
        event_id: Uuid,
        sequence: u64,
        recipients: Vec<String>,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    Error {
        code: String,
        message: String,
    },
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid JSON frame: {0}")]
    Json(#[from] serde_json::Error),

    #[error("frame size {actual} exceeds the {maximum}-byte limit")]
    FrameTooLarge { actual: usize, maximum: usize },
}

#[derive(Debug, Error)]
pub enum AuthTokenError {
    #[error("set HANDCLIP_TOKEN or HANDCLIP_TOKEN_FILE")]
    Missing,

    #[error("failed to read token file {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },

    #[error("authentication token must not be empty")]
    Empty,

    #[error("set either HANDCLIP_TOKEN or HANDCLIP_TOKEN_FILE, not both")]
    ConflictingSources,
}

pub fn resolve_auth_token(
    inline: Option<String>,
    token_file: Option<&Path>,
) -> Result<String, AuthTokenError> {
    let token = match (inline, token_file) {
        (Some(_), Some(_)) => return Err(AuthTokenError::ConflictingSources),
        (Some(token), None) => token,
        (None, Some(path)) => {
            std::fs::read_to_string(path).map_err(|source| AuthTokenError::Read {
                path: path.display().to_string(),
                source,
            })?
        }
        (None, None) => return Err(AuthTokenError::Missing),
    };
    let token = token.trim().to_owned();
    if token.is_empty() {
        return Err(AuthTokenError::Empty);
    }
    Ok(token)
}

pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<(), ProtocolError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = serde_json::to_vec(value)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge {
            actual: payload.len(),
            maximum: MAX_FRAME_SIZE,
        });
    }

    writer
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Stateful, cancellation-safe framing for use inside `tokio::select!`.
/// Keep this object for the entire connection; dropping it discards partial data.
pub struct FrameReader<R> {
    reader: R,
    header: [u8; 4],
    header_read: usize,
    payload: Vec<u8>,
    payload_read: usize,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            header: [0; 4],
            header_read: 0,
            payload: Vec::new(),
            payload_read: 0,
        }
    }

    pub async fn next<T: DeserializeOwned>(&mut self) -> Result<Option<T>, ProtocolError> {
        while self.header_read < 4 {
            let count = self
                .reader
                .read(&mut self.header[self.header_read..])
                .await?;
            if count == 0 {
                if self.header_read == 0 {
                    return Ok(None);
                }
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
            }
            self.header_read += count;
        }
        let length = u32::from_be_bytes(self.header) as usize;
        if length > MAX_FRAME_SIZE {
            return Err(ProtocolError::FrameTooLarge {
                actual: length,
                maximum: MAX_FRAME_SIZE,
            });
        }
        self.payload.resize(length, 0);
        while self.payload_read < length {
            let count = self
                .reader
                .read(&mut self.payload[self.payload_read..])
                .await?;
            if count == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
            }
            self.payload_read += count;
        }
        let value = serde_json::from_slice(&self.payload)?;
        self.header_read = 0;
        self.payload_read = 0;
        self.payload.clear();
        Ok(Some(value))
    }
}

/// One-shot framing. For reads that can be cancelled and retried, use FrameReader.
pub async fn read_frame<R, T>(reader: &mut R) -> Result<Option<T>, ProtocolError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    FrameReader::new(reader).next().await
}

pub fn validate_device_id(device_id: &str) -> bool {
    !device_id.is_empty()
        && device_id.len() <= MAX_DEVICE_ID_LEN
        && device_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

pub fn tokens_equal(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }

    left.as_bytes()
        .iter()
        .zip(right.as_bytes())
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resumes_partial_headers_and_payloads_after_cancellation() {
        use tokio::time::{Duration, timeout};
        let message = ClientMessage::Ping { nonce: 1234 };
        let payload = serde_json::to_vec(&message).unwrap();
        let mut bytes = (payload.len() as u32).to_be_bytes().to_vec();
        bytes.extend(payload);
        for split in [1, 3, 4, 5, bytes.len() - 1] {
            let (mut writer, reader) = tokio::io::duplex(1024);
            let mut reader = FrameReader::new(reader);
            writer.write_all(&bytes[..split]).await.unwrap();
            assert!(
                timeout(Duration::from_millis(2), reader.next::<ClientMessage>())
                    .await
                    .is_err()
            );
            writer.write_all(&bytes[split..]).await.unwrap();
            writer.write_all(&bytes).await.unwrap();
            assert_eq!(
                reader.next::<ClientMessage>().await.unwrap(),
                Some(message.clone())
            );
            assert_eq!(
                reader.next::<ClientMessage>().await.unwrap(),
                Some(message.clone())
            );
            drop(writer);
            assert_eq!(reader.next::<ClientMessage>().await.unwrap(), None);
        }
    }

    #[test]
    fn validates_stable_device_ids() {
        assert!(validate_device_id("air"));
        assert!(validate_device_id("mac-mini"));
        assert!(validate_device_id("pc.office_1"));
        assert!(!validate_device_id(""));
        assert!(!validate_device_id("contains a space"));
        assert!(!validate_device_id(&"a".repeat(MAX_DEVICE_ID_LEN + 1)));
    }

    #[test]
    fn compares_tokens_without_early_byte_exit() {
        assert!(tokens_equal("same-token", "same-token"));
        assert!(!tokens_equal("same-token", "same-taken"));
        assert!(!tokens_equal("short", "a-longer-token"));
    }

    #[test]
    fn resolves_inline_tokens() {
        assert_eq!(
            resolve_auth_token(Some("  secret-token\n".into()), None).unwrap(),
            "secret-token"
        );
        assert!(matches!(
            resolve_auth_token(None, None),
            Err(AuthTokenError::Missing)
        ));
    }

    #[test]
    fn constructs_file_transfer_events() {
        let transfer_id = Uuid::new_v4();
        let event = ClipboardEvent::new(
            "air",
            ClipboardContent::FilesStart {
                transfer_id,
                delivery: FileDelivery::Clipboard,
                roots: vec!["report.pdf".into()],
                entries: vec![FileEntry {
                    relative_path: "report.pdf".into(),
                    kind: FileEntryKind::File,
                    size: 42,
                }],
                total_bytes: 42,
            },
        );

        assert_eq!(event.origin, "air");
        assert!(matches!(
            event.content,
            ClipboardContent::FilesStart {
                transfer_id: actual,
                ..
            } if actual == transfer_id
        ));
    }

    #[test]
    fn constructs_targeted_delivery() {
        assert_eq!(
            DeliveryTarget::device("mini"),
            DeliveryTarget::Device {
                device_id: "mini".into()
            }
        );
    }

    #[tokio::test]
    async fn framed_messages_round_trip() {
        let (mut writer, mut reader) = tokio::io::duplex(4096);
        let expected = ClientMessage::Publish {
            target: DeliveryTarget::device("mini"),
            event: ClipboardEvent::text("air", "hello\nfrom the Air"),
        };

        write_frame(&mut writer, &expected).await.unwrap();
        let actual: ClientMessage = read_frame(&mut reader).await.unwrap().unwrap();

        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn clean_eof_returns_none() {
        let (writer, mut reader) = tokio::io::duplex(64);
        drop(writer);

        let actual: Option<ClientMessage> = read_frame(&mut reader).await.unwrap();
        assert!(actual.is_none());
    }
}
