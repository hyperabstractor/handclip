use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use handclip_core::{
    ClientMessage, ClipboardContent, ClipboardEvent, DeliveryTarget, PROTOCOL_VERSION,
    ServerMessage, read_frame, resolve_auth_token, validate_device_id, write_frame,
};
use tokio::{
    net::TcpStream,
    time::{sleep, timeout},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

#[derive(Debug, Parser)]
#[command(version, about = "Handclip text-bus diagnostic client")]
struct Args {
    #[arg(long, env = "HANDCLIP_SERVER", default_value = "127.0.0.1:24871")]
    server: String,

    #[arg(long, env = "HANDCLIP_DEVICE_ID")]
    device: String,

    #[arg(
        long,
        env = "HANDCLIP_TOKEN",
        hide_env_values = true,
        conflicts_with = "token_file"
    )]
    token: Option<String>,

    #[arg(long, env = "HANDCLIP_TOKEN_FILE")]
    token_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Publish one text event and wait for the relay acknowledgement.
    Send {
        /// Destination device ID, or "all".
        #[arg(long, default_value = "all")]
        target: String,

        /// Text to publish. Quote text containing spaces.
        text: String,
    },

    /// Print events delivered by the relay.
    Watch {
        /// Exit after receiving one event.
        #[arg(long)]
        once: bool,

        /// Print each delivered event as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Connect, authenticate, and print relay status.
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if !validate_device_id(&args.device) {
        bail!("invalid device ID; use 1-64 ASCII letters, numbers, dots, dashes, or underscores");
    }
    let auth_token = resolve_auth_token(args.token, args.token_file.as_deref())?;

    match args.command {
        Command::Send { target, text } => {
            send(&args.server, &args.device, &auth_token, target, text).await
        }
        Command::Watch { once, json } => {
            watch(&args.server, &args.device, &auth_token, once, json).await
        }
        Command::Status => status(&args.server, &args.device, &auth_token).await,
    }
}

async fn connect(server: &str, device: &str, token: &str) -> Result<TcpStream> {
    let mut stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(server))
        .await
        .with_context(|| format!("connection to {server} timed out"))?
        .with_context(|| format!("failed to connect to {server}"))?;
    stream
        .set_nodelay(true)
        .context("failed to set TCP_NODELAY")?;

    write_frame(
        &mut stream,
        &ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            device_id: device.to_owned(),
            auth_token: token.to_owned(),
        },
    )
    .await?;

    match timeout(CONNECT_TIMEOUT, read_frame::<_, ServerMessage>(&mut stream))
        .await
        .context("relay handshake timed out")??
        .ok_or_else(|| anyhow!("relay closed during handshake"))?
    {
        ServerMessage::Welcome {
            protocol_version,
            device_id,
        } if protocol_version == PROTOCOL_VERSION && device_id == device => Ok(stream),
        ServerMessage::Error { code, message } => {
            bail!("relay rejected connection ({code}): {message}")
        }
        message => bail!("unexpected handshake response: {message:?}"),
    }
}

async fn send(server: &str, device: &str, token: &str, target: String, text: String) -> Result<()> {
    let mut stream = connect(server, device, token).await?;
    let event = ClipboardEvent::text(device, text);
    let event_id = event.id;
    let target = if target == "all" {
        DeliveryTarget::All
    } else {
        if !validate_device_id(&target) {
            bail!("invalid target device ID");
        }
        DeliveryTarget::device(target)
    };

    write_frame(&mut stream, &ClientMessage::Publish { target, event }).await?;
    loop {
        let message = read_frame::<_, ServerMessage>(&mut stream)
            .await?
            .ok_or_else(|| anyhow!("relay closed before acknowledging the event"))?;
        match message {
            ServerMessage::Published {
                event_id: acknowledged_id,
                sequence,
                recipients,
            } if acknowledged_id == event_id => {
                println!(
                    "published event={event_id} sequence={sequence} recipients={}",
                    if recipients.is_empty() {
                        "none".into()
                    } else {
                        recipients.join(",")
                    }
                );
                return Ok(());
            }
            ServerMessage::Error { code, message } => {
                bail!("relay error ({code}): {message}");
            }
            _ => {}
        }
    }
}

async fn watch(server: &str, device: &str, token: &str, once: bool, json: bool) -> Result<()> {
    loop {
        match watch_connection(server, device, token, once, json).await {
            Ok(true) => return Ok(()),
            Ok(false) => eprintln!("relay disconnected; reconnecting in 2 seconds"),
            Err(error) => eprintln!("{error:#}; reconnecting in 2 seconds"),
        }
        sleep(RECONNECT_DELAY).await;
    }
}

async fn watch_connection(
    server: &str,
    device: &str,
    token: &str,
    once: bool,
    json: bool,
) -> Result<bool> {
    let mut stream = connect(server, device, token).await?;
    eprintln!("connected device={device} relay={server}");

    loop {
        let Some(message) = read_frame::<_, ServerMessage>(&mut stream).await? else {
            return Ok(false);
        };
        match message {
            ServerMessage::Event { sequence, event } => {
                print_event(sequence, &event, json)?;
                if once {
                    return Ok(true);
                }
            }
            ServerMessage::Ping { nonce } => {
                write_frame(&mut stream, &ClientMessage::Pong { nonce }).await?;
            }
            ServerMessage::Error { code, message } => {
                bail!("relay error ({code}): {message}");
            }
            _ => {}
        }
    }
}

fn print_event(sequence: u64, event: &ClipboardEvent, json: bool) -> Result<()> {
    if json {
        let value = serde_json::json!({
            "sequence": sequence,
            "event": event,
        });
        println!("{}", serde_json::to_string(&value)?);
        return Ok(());
    }

    match &event.content {
        ClipboardContent::Text { text } => {
            println!("[{sequence}] {}: {text}", event.origin);
        }
        ClipboardContent::ImagePng { png_base64 } => {
            println!(
                "[{sequence}] {}: PNG image ({} base64 bytes)",
                event.origin,
                png_base64.len()
            );
        }
        ClipboardContent::FilesStart {
            transfer_id,
            roots,
            entries,
            total_bytes,
        } => {
            println!(
                "[{sequence}] {}: file transfer {transfer_id} started ({} roots, {} entries, {} bytes)",
                event.origin,
                roots.len(),
                entries.len(),
                total_bytes
            );
        }
        ClipboardContent::FileChunk {
            transfer_id,
            file_index,
            offset,
            data_base64,
        } => {
            println!(
                "[{sequence}] {}: file transfer {transfer_id} chunk (file {file_index}, offset {offset}, {} base64 bytes)",
                event.origin,
                data_base64.len()
            );
        }
        ClipboardContent::FilesComplete {
            transfer_id,
            sha256,
        } => {
            println!(
                "[{sequence}] {}: file transfer {transfer_id} complete ({} entries)",
                event.origin,
                sha256.len()
            );
        }
    }
    Ok(())
}

async fn status(server: &str, device: &str, token: &str) -> Result<()> {
    let _stream = connect(server, device, token).await?;
    println!("connected device={device} relay={server} protocol={PROTOCOL_VERSION}");
    Ok(())
}
