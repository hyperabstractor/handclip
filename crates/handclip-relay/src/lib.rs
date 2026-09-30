use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use handclip_core::{
    ClientMessage, DeliveryTarget, PROTOCOL_VERSION, ServerMessage, read_frame, tokens_equal,
    validate_device_id, write_frame,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Mutex, broadcast},
    time::timeout,
};
use tracing::{info, warn};
use uuid::Uuid;

// 256 KiB file chunks keep this bounded ring at roughly 64 MiB plus framing overhead.
const EVENT_CHANNEL_CAPACITY: usize = 256;
const RECENT_EVENT_CAPACITY: usize = 4096;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct RelayState {
    auth_token: String,
    next_sequence: AtomicU64,
    events: broadcast::Sender<RoutedEvent>,
    recent_events: Mutex<RecentEvents>,
    connected_devices: Mutex<HashMap<String, usize>>,
}

#[derive(Clone, Debug)]
struct RoutedEvent {
    sequence: u64,
    event: handclip_core::ClipboardEvent,
    recipients: Vec<String>,
}

#[derive(Clone, Debug)]
struct RecentPublication {
    sequence: u64,
    target: DeliveryTarget,
    recipients: Vec<String>,
}

#[derive(Debug, Default)]
struct RecentEvents {
    publications: HashMap<Uuid, RecentPublication>,
    insertion_order: VecDeque<Uuid>,
}

impl RecentEvents {
    fn publication(&self, event_id: &Uuid) -> Option<RecentPublication> {
        self.publications.get(event_id).cloned()
    }

    fn insert(&mut self, event_id: Uuid, publication: RecentPublication) {
        self.publications.insert(event_id, publication);
        self.insertion_order.push_back(event_id);

        while self.insertion_order.len() > RECENT_EVENT_CAPACITY {
            if let Some(expired_id) = self.insertion_order.pop_front() {
                self.publications.remove(&expired_id);
            }
        }
    }
}

/// Accepts device connections on `listen` until the listener fails.
pub async fn serve(listen: SocketAddr, auth_token: String) -> Result<()> {
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("failed to listen on {listen}"))?;
    let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
    let state = Arc::new(RelayState {
        auth_token,
        next_sequence: AtomicU64::new(1),
        events,
        recent_events: Mutex::new(RecentEvents::default()),
        connected_devices: Mutex::new(HashMap::new()),
    });

    info!(%listen, "relay ready");

    loop {
        let (stream, peer) = listener.accept().await.context("accept failed")?;
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, peer, state).await {
                warn!(%peer, %error, "connection closed");
            }
        });
    }
}

async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    state: Arc<RelayState>,
) -> Result<()> {
    stream
        .set_nodelay(true)
        .context("failed to set TCP_NODELAY")?;
    let (mut reader, mut writer) = stream.into_split();

    let hello = timeout(
        HANDSHAKE_TIMEOUT,
        read_frame::<_, ClientMessage>(&mut reader),
    )
    .await
    .context("handshake timed out")??
    .ok_or_else(|| anyhow!("connection closed before handshake"))?;

    let device_id = match hello {
        ClientMessage::Hello {
            protocol_version,
            device_id,
            auth_token,
        } => {
            if protocol_version != PROTOCOL_VERSION {
                write_frame(
                    &mut writer,
                    &ServerMessage::Error {
                        code: "protocol_version".into(),
                        message: format!(
                            "relay uses protocol {PROTOCOL_VERSION}; client uses {protocol_version}"
                        ),
                    },
                )
                .await?;
                bail!("unsupported protocol version {protocol_version}");
            }
            if !validate_device_id(&device_id) {
                write_frame(
                    &mut writer,
                    &ServerMessage::Error {
                        code: "device_id".into(),
                        message: "device ID contains unsupported characters".into(),
                    },
                )
                .await?;
                bail!("invalid device ID");
            }
            if !tokens_equal(&auth_token, &state.auth_token) {
                write_frame(
                    &mut writer,
                    &ServerMessage::Error {
                        code: "authentication".into(),
                        message: "authentication failed".into(),
                    },
                )
                .await?;
                bail!("authentication failed");
            }
            device_id
        }
        _ => bail!("first client message was not hello"),
    };

    write_frame(
        &mut writer,
        &ServerMessage::Welcome {
            protocol_version: PROTOCOL_VERSION,
            device_id: device_id.clone(),
        },
    )
    .await?;

    let mut event_rx = state.events.subscribe();
    register_device(&state, &device_id).await;
    info!(%peer, %device_id, "device connected");

    let result = async {
        loop {
            tokio::select! {
                message = read_frame::<_, ClientMessage>(&mut reader) => {
                    let Some(message) = message? else {
                        info!(%peer, %device_id, "device disconnected");
                        break Ok(());
                    };
                    handle_client_message(&device_id, message, &state, &mut writer).await?;
                }
                routed = event_rx.recv() => {
                    match routed {
                        Ok(routed) => {
                            if routed.recipients.iter().any(|recipient| recipient == &device_id) {
                                write_frame(
                                    &mut writer,
                                    &ServerMessage::Event {
                                        sequence: routed.sequence,
                                        event: routed.event,
                                    },
                                )
                                .await?;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            write_frame(
                                &mut writer,
                                &ServerMessage::Error {
                                    code: "event_lag".into(),
                                    message: format!("client missed {skipped} events"),
                                },
                            )
                            .await?;
                            bail!("client lagged by {skipped} events");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            bail!("relay event channel closed");
                        }
                    }
                }
            }
        }
    }
    .await;

    unregister_device(&state, &device_id).await;
    result
}

async fn handle_client_message<W>(
    device_id: &str,
    message: ClientMessage,
    state: &RelayState,
    writer: &mut W,
) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    match message {
        ClientMessage::Publish { target, event } => {
            if event.origin != device_id {
                write_frame(
                    writer,
                    &ServerMessage::Error {
                        code: "origin".into(),
                        message: "event origin must match the authenticated device".into(),
                    },
                )
                .await?;
                bail!("event origin does not match device ID");
            }
            if let DeliveryTarget::Device {
                device_id: target_device,
            } = &target
                && !validate_device_id(target_device)
            {
                write_frame(
                    writer,
                    &ServerMessage::Error {
                        code: "target".into(),
                        message: "target device ID contains unsupported characters".into(),
                    },
                )
                .await?;
                bail!("invalid target device ID");
            }

            let candidate_recipients = {
                let connected_devices = state.connected_devices.lock().await;
                recipients_for_target(device_id, &target, &connected_devices)
            };
            let publication_result = {
                let mut recent_events = state.recent_events.lock().await;
                if let Some(publication) = recent_events.publication(&event.id) {
                    if publication.target != target {
                        None
                    } else {
                        Some((publication, false))
                    }
                } else {
                    let sequence = state.next_sequence.fetch_add(1, Ordering::Relaxed);
                    let publication = RecentPublication {
                        sequence,
                        target: target.clone(),
                        recipients: candidate_recipients,
                    };
                    recent_events.insert(event.id, publication.clone());
                    Some((publication, true))
                }
            };
            let Some((publication, is_new)) = publication_result else {
                write_frame(
                    writer,
                    &ServerMessage::Error {
                        code: "duplicate_target".into(),
                        message: "an event ID cannot be republished to a different target".into(),
                    },
                )
                .await?;
                bail!("duplicate event ID used with a different target");
            };
            if is_new {
                let _receiver_count = state.events.send(RoutedEvent {
                    sequence: publication.sequence,
                    event: event.clone(),
                    recipients: publication.recipients.clone(),
                });
            }
            write_frame(
                writer,
                &ServerMessage::Published {
                    event_id: event.id,
                    sequence: publication.sequence,
                    recipients: publication.recipients,
                },
            )
            .await?;
        }
        ClientMessage::Ping { nonce } => {
            write_frame(writer, &ServerMessage::Pong { nonce }).await?;
        }
        ClientMessage::Pong { .. } => {}
        ClientMessage::Hello { .. } => {
            write_frame(
                writer,
                &ServerMessage::Error {
                    code: "handshake".into(),
                    message: "hello may only be sent once".into(),
                },
            )
            .await?;
            bail!("client sent a second hello");
        }
    }
    Ok(())
}

async fn register_device(state: &RelayState, device_id: &str) {
    let mut connected_devices = state.connected_devices.lock().await;
    *connected_devices.entry(device_id.to_owned()).or_default() += 1;
}

async fn unregister_device(state: &RelayState, device_id: &str) {
    let mut connected_devices = state.connected_devices.lock().await;
    let Some(connection_count) = connected_devices.get_mut(device_id) else {
        return;
    };
    *connection_count -= 1;
    if *connection_count == 0 {
        connected_devices.remove(device_id);
    }
}

fn recipients_for_target(
    origin: &str,
    target: &DeliveryTarget,
    connected_devices: &HashMap<String, usize>,
) -> Vec<String> {
    let mut recipients: Vec<String> = match target {
        DeliveryTarget::Device { device_id } => connected_devices
            .contains_key(device_id)
            .then(|| device_id.clone())
            .into_iter()
            .collect(),
        DeliveryTarget::All => connected_devices.keys().cloned().collect(),
    };
    recipients.retain(|device_id| device_id != origin);
    recipients.sort();
    recipients
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_to_online_target_only() {
        let connected = HashMap::from([
            ("air".to_owned(), 1),
            ("mini".to_owned(), 1),
            ("pc".to_owned(), 1),
        ]);
        assert_eq!(
            recipients_for_target("air", &DeliveryTarget::device("pc"), &connected),
            ["pc"]
        );
        assert!(
            recipients_for_target("air", &DeliveryTarget::device("offline"), &connected).is_empty()
        );
    }

    #[test]
    fn broadcast_excludes_the_sender() {
        let connected = HashMap::from([
            ("air".to_owned(), 1),
            ("mini".to_owned(), 2),
            ("pc".to_owned(), 1),
        ]);
        assert_eq!(
            recipients_for_target("mini", &DeliveryTarget::All, &connected),
            ["air", "pc"]
        );
    }
}
