#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use std::{
    borrow::Cow,
    collections::VecDeque,
    path::PathBuf,
    process::Command,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

mod config;
mod file_transfer;
mod gui;

use anyhow::{Context, Result, anyhow, bail};
use arboard::Clipboard;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use clap::Parser;
use handclip_core::{
    ClientMessage, ClipboardContent, ClipboardEvent, DeliveryTarget, PROTOCOL_VERSION,
    ServerMessage, read_frame, resolve_auth_token, write_frame,
};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState, hotkey::HotKey};
use image::{ColorType, ImageEncoder, ImageFormat, codecs::png::PngEncoder};
use tao::{
    event::{Event, StartCause},
    event_loop::{ControlFlow, EventLoopBuilder},
};
use tokio::{
    net::TcpStream,
    sync::mpsc::{Receiver, Sender, channel},
    time::{sleep, timeout},
};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

use crate::{
    config::{AppConfig, ConfigOverrides},
    gui::{DesktopUi, GuiAction},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const UI_POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const NETWORK_COMMAND_CAPACITY: usize = 32;
const UI_EVENT_CAPACITY: usize = 64;
// ponytail: fixed relay port on all interfaces (token-authenticated); make it a setting if it ever clashes.
const EMBEDDED_RELAY_LISTEN: &str = "0.0.0.0:24871";
#[cfg(target_os = "macos")]
const LAUNCH_AGENT_LABEL: &str = "io.github.handclip.agent";

#[derive(Debug, Parser)]
#[command(version, about = "Handclip native clipboard agent")]
struct Args {
    /// Settings file. Created from command-line values on first launch.
    #[arg(long, env = "HANDCLIP_CONFIG_FILE")]
    config_file: Option<PathBuf>,

    #[arg(long, env = "HANDCLIP_SERVER", value_delimiter = ',')]
    server: Vec<String>,

    #[arg(long, env = "HANDCLIP_DEVICE_ID")]
    device: Option<String>,

    #[arg(
        long,
        env = "HANDCLIP_TOKEN",
        hide_env_values = true,
        conflicts_with = "token_file"
    )]
    token: Option<String>,

    #[arg(long, env = "HANDCLIP_TOKEN_FILE")]
    token_file: Option<PathBuf>,

    /// Write daily rotating logs into this directory.
    #[arg(long, env = "HANDCLIP_LOG_DIR")]
    log_dir: Option<PathBuf>,

    /// Stage received files in this directory.
    #[arg(long, env = "HANDCLIP_RECEIVE_DIR")]
    receive_dir: Option<PathBuf>,

    /// Global broadcast shortcut. Target-specific shortcuts are configured in settings.
    #[arg(long, env = "HANDCLIP_BROADCAST_SHORTCUT", alias = "shortcut")]
    broadcast_shortcut: Option<String>,

    /// Ignore any existing settings file and use command-line values.
    #[arg(long)]
    ignore_config: bool,

    /// Do not create a menu-bar or system-tray interface.
    #[arg(long)]
    no_gui: bool,

    /// Open the settings window immediately after launch.
    #[arg(long)]
    open_settings: bool,

    #[arg(long, hide = true)]
    startup_delay_ms: Option<u64>,

    /// Do not register the global share shortcut.
    #[arg(long)]
    no_hotkey: bool,

    /// Publish the current text clipboard immediately after startup.
    #[arg(long)]
    share_on_start: bool,

    /// Exit after writing one remote text event to the clipboard.
    #[arg(long)]
    exit_after_receive: bool,
}

#[derive(Clone, Debug)]
enum NetworkCommand {
    Publish {
        target: DeliveryTarget,
        event: ClipboardEvent,
    },
}

#[derive(Debug)]
enum UiEvent {
    Connected {
        endpoint: String,
    },
    Disconnected(String),
    Published {
        event_id: Uuid,
        sequence: u64,
        kind: Option<ClipboardItemKind>,
        target: DeliveryTarget,
        recipients: Vec<String>,
    },
    Remote {
        sequence: u64,
        event: ClipboardEvent,
    },
}

#[derive(Clone, Copy, Debug)]
enum ClipboardItemKind {
    Text,
    Image,
    Files,
}

impl ClipboardItemKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Text => "Text",
            Self::Image => "Image",
            Self::Files => "Files",
        }
    }
}

#[derive(Clone, Debug)]
struct RegisteredHotkey {
    hotkey: HotKey,
    target: DeliveryTarget,
}

fn main() {
    let args = Args::parse();
    if let Some(delay_ms) = args.startup_delay_ms {
        thread::sleep(Duration::from_millis(delay_ms.min(10_000)));
    }
    let (config_path, app_config) = match load_startup_config(&args) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("failed to load settings: {error:#}");
            return;
        }
    };
    let log_guard = match initialize_logging(Some(&app_config.log_dir)) {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("failed to initialize logging: {error:#}");
            return;
        }
    };
    if let Err(error) = run(args, app_config, config_path) {
        error!(%error, error_chain = %format!("{error:#}"), "agent stopped");
    }
    drop(log_guard);
}

fn load_startup_config(args: &Args) -> Result<(PathBuf, AppConfig)> {
    let config_path = args
        .config_file
        .clone()
        .unwrap_or_else(config::default_config_file);
    let overrides = ConfigOverrides {
        device_id: args.device.clone(),
        relay_endpoints: args.server.clone(),
        token_file: args.token_file.clone(),
        receive_dir: args.receive_dir.clone(),
        log_dir: args.log_dir.clone(),
        broadcast_shortcut: args.broadcast_shortcut.clone(),
    };
    let config = if config_path.exists() && !args.ignore_config {
        AppConfig::load(&config_path)?
    } else if overrides.device_id.is_none() {
        // First launch: the settings window finishes the setup.
        AppConfig::draft()
    } else {
        let config = AppConfig::from_overrides(overrides)?;
        if !args.ignore_config {
            config.save(&config_path)?;
        }
        config
    };
    Ok((config_path, config))
}

fn run(args: Args, mut app_config: AppConfig, config_path: PathBuf) -> Result<()> {
    let ready = app_config.validate().and_then(|()| {
        let token = if args.token.is_some() {
            resolve_auth_token(args.token, None)
        } else {
            resolve_auth_token(None, Some(&app_config.token_file))
        };
        token.map_err(Into::into)
    });
    let auth_token = match ready {
        Ok(token) => Some(token),
        Err(error) if args.no_gui => return Err(error),
        Err(error) => {
            warn!(error = %format!("{error:#}"), "setup required; opening settings");
            None
        }
    };
    let needs_setup = auth_token.is_none();

    if let Some(token) = &auth_token
        && app_config.run_relay
    {
        spawn_relay_thread(token.clone())?;
    }

    let mut clipboard = Clipboard::new().context("failed to open the system clipboard")?;
    let mut incoming_transfers =
        file_transfer::IncomingTransfers::new(app_config.receive_dir.clone())?;
    let (network_tx, network_rx) = channel(NETWORK_COMMAND_CAPACITY);
    let (ui_tx, ui_rx) = mpsc::sync_channel(UI_EVENT_CAPACITY);

    if let Some(token) = auth_token {
        spawn_network_thread(
            app_config.relay_endpoints.clone(),
            app_config.device_id.clone(),
            token,
            network_rx,
            ui_tx,
        )?;
    } else {
        // Not set up: make shares fail fast instead of queueing forever.
        drop(network_rx);
    }

    let event_loop = build_event_loop();
    let (hotkey_manager, share_hotkeys) = if args.no_hotkey || needs_setup {
        (None, Vec::new())
    } else {
        let manager = GlobalHotKeyManager::new().context("failed to initialize global hotkeys")?;
        let mut registrations = app_config
            .target_shortcuts
            .iter()
            .map(|target| {
                Ok(RegisteredHotkey {
                    hotkey: target.shortcut.parse().with_context(|| {
                        format!("failed to parse shortcut for {}", target.device_id)
                    })?,
                    target: DeliveryTarget::device(&target.device_id),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        registrations.push(RegisteredHotkey {
            hotkey: app_config
                .broadcast_shortcut
                .parse()
                .context("failed to parse configured broadcast shortcut")?,
            target: DeliveryTarget::All,
        });
        let hotkeys = registrations
            .iter()
            .map(|registration| registration.hotkey)
            .collect::<Vec<_>>();
        manager
            .register_all(&hotkeys)
            .context("failed to register the Handclip share shortcuts")?;
        for registration in &registrations {
            info!(
                shortcut = %registration.hotkey,
                target = %delivery_target_label(&registration.target),
                "share shortcut ready"
            );
        }
        (Some(manager), registrations)
    };

    if args.share_on_start {
        publish_current_clipboard(
            &mut clipboard,
            &app_config.device_id,
            DeliveryTarget::All,
            &network_tx,
        );
    }

    let mut device_id = app_config.device_id.clone();
    let exit_after_receive = args.exit_after_receive;
    let no_gui = args.no_gui;
    let open_settings_on_start = args.open_settings || needs_setup;
    let hotkey_events = GlobalHotKeyEvent::receiver();
    let mut desktop_ui: Option<DesktopUi> = None;

    event_loop.run(move |event, event_loop, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + UI_POLL_INTERVAL);

        if matches!(event, Event::NewEvents(StartCause::Init)) && !no_gui {
            match DesktopUi::new(&app_config) {
                Ok(mut ui) => {
                    if needs_setup {
                        ui.set_setup_required();
                    }
                    if open_settings_on_start
                        && let Err(error) = ui.show_settings(event_loop, &app_config)
                    {
                        error!(%error, "failed to open settings on startup");
                    }
                    desktop_ui = Some(ui);
                }
                Err(error) => error!(%error, "failed to create desktop interface"),
            }
        }
        if let Event::WindowEvent {
            window_id,
            ref event,
            ..
        } = event
            && desktop_ui
                .as_mut()
                .is_some_and(|ui| ui.handle_window_event(window_id, event))
        {
            return;
        }

        while let Ok(event) = hotkey_events.try_recv() {
            if event.state == HotKeyState::Pressed
                && let Some(registration) = share_hotkeys
                    .iter()
                    .find(|registration| registration.hotkey.id() == event.id)
            {
                request_clipboard_share(
                    &mut clipboard,
                    &device_id,
                    registration.target.clone(),
                    &network_tx,
                    &mut desktop_ui,
                    app_config.notifications,
                );
            }
        }

        while let Ok(event) = ui_rx.try_recv() {
            match event {
                UiEvent::Connected { endpoint } => {
                    info!(%endpoint, "connected to relay");
                    if let Some(ui) = &mut desktop_ui {
                        ui.set_connected(endpoint);
                    }
                }
                UiEvent::Disconnected(message) => {
                    warn!(%message, "relay disconnected");
                    if let Some(ui) = &mut desktop_ui {
                        ui.set_disconnected(&message);
                    }
                }
                UiEvent::Published {
                    event_id,
                    sequence,
                    kind,
                    target,
                    recipients,
                } => {
                    info!(
                        %event_id,
                        sequence,
                        target = %delivery_target_label(&target),
                        recipients = ?recipients,
                        "clipboard published"
                    );
                    if let Some(kind) = kind {
                        let item = kind.label();
                        let (activity, notification_title, notification_body) =
                            publication_feedback(item, &target, &recipients);
                        if let Some(ui) = &mut desktop_ui {
                            ui.set_activity(activity);
                        }
                        if app_config.notifications {
                            notify_user(notification_title, &notification_body);
                        }
                    }
                }
                UiEvent::Remote { sequence, event } => {
                    if event.origin == device_id {
                        continue;
                    }
                    match event.content {
                        ClipboardContent::Text { text } => {
                            if let Err(error) = clipboard.set_text(text) {
                                error!(
                                    %error,
                                    sequence,
                                    origin = %event.origin,
                                    "failed to set clipboard"
                                );
                            } else {
                                info!(
                                    sequence,
                                    origin = %event.origin,
                                    "remote text copied locally"
                                );
                                if let Some(ui) = &mut desktop_ui {
                                    ui.set_activity(format!("Received text from {}", event.origin));
                                }
                                if app_config.notifications {
                                    notify_user(
                                        "Clipboard received",
                                        &format!("Text from {}", event.origin),
                                    );
                                }
                                if exit_after_receive {
                                    *control_flow = ControlFlow::Exit;
                                }
                            }
                        }
                        ClipboardContent::ImagePng { png_base64 } => {
                            match set_clipboard_image(&mut clipboard, &png_base64) {
                                Ok(()) => {
                                    info!(
                                        sequence,
                                        origin = %event.origin,
                                        "remote image copied locally"
                                    );
                                    if let Some(ui) = &mut desktop_ui {
                                        ui.set_activity(format!(
                                            "Received image from {}",
                                            event.origin
                                        ));
                                    }
                                    if app_config.notifications {
                                        notify_user(
                                            "Clipboard received",
                                            &format!("Image from {}", event.origin),
                                        );
                                    }
                                    if exit_after_receive {
                                        *control_flow = ControlFlow::Exit;
                                    }
                                }
                                Err(error) => {
                                    error!(
                                        %error,
                                        sequence,
                                        origin = %event.origin,
                                        "failed to set clipboard image"
                                    );
                                }
                            }
                        }
                        content @ (ClipboardContent::FilesStart { .. }
                        | ClipboardContent::FileChunk { .. }
                        | ClipboardContent::FilesComplete { .. }) => {
                            match incoming_transfers.handle(&event.origin, content) {
                                Ok(Some(paths)) => {
                                    if let Err(error) = clipboard.set().file_list(&paths) {
                                        error!(
                                            %error,
                                            sequence,
                                            origin = %event.origin,
                                            "failed to set clipboard files"
                                        );
                                    } else {
                                        info!(
                                            sequence,
                                            origin = %event.origin,
                                            count = paths.len(),
                                            "remote files copied locally"
                                        );
                                        if let Some(ui) = &mut desktop_ui {
                                            ui.set_activity(format!(
                                                "Received {} file item(s) from {}",
                                                paths.len(),
                                                event.origin
                                            ));
                                        }
                                        if app_config.notifications {
                                            notify_user(
                                                "Clipboard received",
                                                &format!(
                                                    "{} file item(s) from {}",
                                                    paths.len(),
                                                    event.origin
                                                ),
                                            );
                                        }
                                        if exit_after_receive {
                                            *control_flow = ControlFlow::Exit;
                                        }
                                    }
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    error!(
                                        %error,
                                        sequence,
                                        origin = %event.origin,
                                        "failed to receive clipboard files"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        let actions = desktop_ui
            .as_ref()
            .map(DesktopUi::poll_actions)
            .unwrap_or_default();
        for action in actions {
            match action {
                GuiAction::Share(target) => {
                    request_clipboard_share(
                        &mut clipboard,
                        &device_id,
                        target,
                        &network_tx,
                        &mut desktop_ui,
                        app_config.notifications,
                    );
                }
                GuiAction::Settings => {
                    if let Some(ui) = &mut desktop_ui
                        && let Err(error) = ui.show_settings(event_loop, &app_config)
                    {
                        error!(%error, "failed to open settings");
                    }
                }
                GuiAction::OpenReceived => {
                    open_directory(&app_config.receive_dir);
                }
                GuiAction::OpenLogs => {
                    open_directory(&app_config.log_dir);
                }
                GuiAction::Save {
                    config: new_config,
                    token,
                } => {
                    let save_result = new_config
                        .validate()
                        .and_then(|_| config::save_token(&new_config.token_file, &token))
                        .and_then(|_| {
                            resolve_auth_token(None, Some(&new_config.token_file))
                                .map(|_| ())
                                .map_err(Into::into)
                        })
                        .and_then(|_| new_config.save(&config_path));
                    match save_result {
                        Ok(()) => {
                            app_config = new_config;
                            device_id.clone_from(&app_config.device_id);
                            info!(path = %config_path.display(), "settings saved");
                            restart_agent(&config_path);
                            *control_flow = ControlFlow::Exit;
                        }
                        Err(error) => {
                            warn!(%error, "settings were not saved");
                            if let Some(ui) = &desktop_ui {
                                ui.save_error(&format!("{error:#}"));
                            }
                        }
                    }
                }
                GuiAction::CloseSettings => {
                    if let Some(ui) = &desktop_ui {
                        ui.hide_settings();
                    }
                }
                GuiAction::Restart => {
                    restart_agent(&config_path);
                    *control_flow = ControlFlow::Exit;
                }
                GuiAction::Quit => {
                    quit_agent();
                    *control_flow = ControlFlow::Exit;
                }
            }
        }

        let _keep_manager_alive = &hotkey_manager;
    })
}

fn initialize_logging(
    log_dir: Option<&std::path::Path>,
) -> Result<Option<tracing_appender::non_blocking::WorkerGuard>> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    if let Some(log_dir) = log_dir {
        std::fs::create_dir_all(log_dir)
            .with_context(|| format!("failed to create log directory {}", log_dir.display()))?;
        let appender = tracing_appender::rolling::daily(log_dir, "agent.log");
        let (writer, guard) = tracing_appender::non_blocking(appender);
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_ansi(false)
            .with_writer(writer)
            .compact()
            .init();
        Ok(Some(guard))
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .compact()
            .init();
        Ok(None)
    }
}

fn publish_current_clipboard(
    clipboard: &mut Clipboard,
    device_id: &str,
    target: DeliveryTarget,
    network_tx: &Sender<NetworkCommand>,
) {
    if let Ok(paths) = clipboard.get().file_list()
        && !paths.is_empty()
    {
        publish_clipboard_files(paths, device_id.to_owned(), target, network_tx.clone());
        return;
    }

    match clipboard_event(clipboard, device_id) {
        Ok(event) => {
            if let Err(error) = network_tx.try_send(NetworkCommand::Publish { target, event }) {
                error!(%error, "could not queue clipboard event");
            }
        }
        Err(error) => warn!(%error, "clipboard does not contain supported content"),
    }
}

fn publish_clipboard_files(
    paths: Vec<PathBuf>,
    device_id: String,
    target: DeliveryTarget,
    network_tx: Sender<NetworkCommand>,
) {
    let spawn_result = thread::Builder::new()
        .name("handclip-file-sender".into())
        .spawn(move || {
            info!(count = paths.len(), "preparing clipboard files");
            match file_transfer::stream_paths(paths, &device_id, |event| {
                network_tx
                    .blocking_send(NetworkCommand::Publish {
                        target: target.clone(),
                        event,
                    })
                    .map_err(|_| anyhow!("network worker is not running"))
            }) {
                Ok(summary) => info!(
                    transfer_id = %summary.transfer_id,
                    entries = summary.entry_count,
                    bytes = summary.total_bytes,
                    "clipboard files queued"
                ),
                Err(error) => error!(%error, "failed to share clipboard files"),
            }
        });
    if let Err(error) = spawn_result {
        error!(%error, "failed to start file sender");
    }
}

fn request_clipboard_share(
    clipboard: &mut Clipboard,
    device_id: &str,
    target: DeliveryTarget,
    network_tx: &Sender<NetworkCommand>,
    desktop_ui: &mut Option<DesktopUi>,
    notifications: bool,
) {
    if matches!(
        &target,
        DeliveryTarget::Device {
            device_id: target_device
        } if target_device == device_id
    ) {
        let message = format!("{device_id} is this device — clipboard unchanged");
        info!(%device_id, "ignored clipboard share to the current device");
        if let Some(ui) = desktop_ui {
            ui.set_activity(&message);
        }
        if notifications {
            notify_user("Clipboard unchanged", &message);
        }
        return;
    }

    let target_label = delivery_target_label(&target).to_owned();
    publish_current_clipboard(clipboard, device_id, target, network_tx);
    if let Some(ui) = desktop_ui {
        ui.set_activity(format!("Sending current clipboard to {target_label}…"));
    }
}

fn delivery_target_label(target: &DeliveryTarget) -> &str {
    match target {
        DeliveryTarget::Device { device_id } => device_id,
        DeliveryTarget::All => "all devices",
    }
}

fn publication_feedback(
    item: &str,
    target: &DeliveryTarget,
    recipients: &[String],
) -> (String, &'static str, String) {
    if recipients.is_empty() {
        return match target {
            DeliveryTarget::Device { device_id } => (
                format!("{device_id} is offline — nothing sent"),
                "Clipboard not sent",
                format!("{device_id} is offline"),
            ),
            DeliveryTarget::All => (
                "No other devices are online".into(),
                "Clipboard not sent",
                "No other devices are online".into(),
            ),
        };
    }

    let recipient_list = recipients.join(", ");
    (
        format!("{item} sent to {recipient_list}"),
        "Clipboard sent",
        format!("{item} to {recipient_list}"),
    )
}

fn clipboard_event(clipboard: &mut Clipboard, device_id: &str) -> Result<ClipboardEvent> {
    if let Ok(text) = clipboard.get_text() {
        return Ok(ClipboardEvent::text(device_id, text));
    }

    let image = clipboard
        .get_image()
        .context("clipboard contains neither text nor an image")?;
    let width = u32::try_from(image.width).context("image width exceeds protocol limits")?;
    let height = u32::try_from(image.height).context("image height exceeds protocol limits")?;
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(&image.bytes, width, height, ColorType::Rgba8.into())
        .context("failed to encode clipboard image as PNG")?;
    if png.len() > MAX_IMAGE_BYTES {
        bail!(
            "encoded image is {} MiB; the current limit is {} MiB",
            png.len() / (1024 * 1024),
            MAX_IMAGE_BYTES / (1024 * 1024)
        );
    }

    Ok(ClipboardEvent {
        id: Uuid::new_v4(),
        origin: device_id.to_owned(),
        created_at_ms: unix_time_ms(),
        content: ClipboardContent::ImagePng {
            png_base64: BASE64.encode(png),
        },
    })
}

fn set_clipboard_image(clipboard: &mut Clipboard, png_base64: &str) -> Result<()> {
    let png = BASE64
        .decode(png_base64)
        .context("remote image is not valid base64")?;
    if png.len() > MAX_IMAGE_BYTES {
        bail!("remote image exceeds the configured size limit");
    }
    let rgba = image::load_from_memory_with_format(&png, ImageFormat::Png)
        .context("remote payload is not a valid PNG")?
        .into_rgba8();
    let width = usize::try_from(rgba.width()).context("image width exceeds platform limits")?;
    let height = usize::try_from(rgba.height()).context("image height exceeds platform limits")?;

    clipboard
        .set_image(arboard::ImageData {
            width,
            height,
            bytes: Cow::Owned(rgba.into_raw()),
        })
        .context("platform rejected the remote clipboard image")
}

fn unix_time_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn spawn_relay_thread(token: String) -> Result<()> {
    let listen = EMBEDDED_RELAY_LISTEN.parse().expect("valid relay address");
    thread::Builder::new()
        .name("handclip-relay".into())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("failed to start relay runtime")
                .and_then(|runtime| runtime.block_on(handclip_relay::serve(listen, token)));
            if let Err(error) = result {
                error!(%error, error_chain = %format!("{error:#}"), "embedded relay stopped");
            }
        })
        .context("failed to spawn relay thread")?;
    Ok(())
}

fn spawn_network_thread(
    relay_endpoints: Vec<String>,
    device_id: String,
    token: String,
    command_rx: Receiver<NetworkCommand>,
    ui_tx: mpsc::SyncSender<UiEvent>,
) -> Result<()> {
    thread::Builder::new()
        .name("handclip-network".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = ui_tx.send(UiEvent::Disconnected(format!(
                        "failed to start network runtime: {error}"
                    )));
                    return;
                }
            };
            runtime.block_on(network_loop(
                relay_endpoints,
                device_id,
                token,
                command_rx,
                ui_tx,
            ));
        })
        .context("failed to spawn network thread")?;
    Ok(())
}

async fn network_loop(
    relay_endpoints: Vec<String>,
    device_id: String,
    token: String,
    mut command_rx: Receiver<NetworkCommand>,
    ui_tx: mpsc::SyncSender<UiEvent>,
) {
    let mut pending = VecDeque::new();

    loop {
        let (stream, endpoint) = match connect_to_relay(&relay_endpoints, &device_id, &token).await
        {
            Ok(connection) => connection,
            Err(error) => {
                let _ = ui_tx.send(UiEvent::Disconnected(format!("{error:#}")));
                tokio::select! {
                    _ = sleep(RECONNECT_DELAY) => {}
                    command = command_rx.recv() => {
                        let Some(command @ NetworkCommand::Publish { .. }) = command else {
                            return;
                        };
                        pending.push_back(command);
                    }
                }
                continue;
            }
        };

        let _ = ui_tx.send(UiEvent::Connected { endpoint });
        let (reader, writer) = stream.into_split();
        match connected_session(
            reader,
            writer,
            &device_id,
            &mut command_rx,
            &ui_tx,
            &mut pending,
        )
        .await
        {
            Ok(()) => return,
            Err(error) => {
                let _ = ui_tx.send(UiEvent::Disconnected(format!("{error:#}")));
            }
        }
    }
}

async fn connect_to_relay(
    relay_endpoints: &[String],
    device_id: &str,
    token: &str,
) -> Result<(TcpStream, String)> {
    let mut failures = Vec::with_capacity(relay_endpoints.len());
    for endpoint in relay_endpoints {
        match connect(endpoint, device_id, token).await {
            Ok(stream) => return Ok((stream, endpoint.clone())),
            Err(error) => failures.push(format!("{endpoint}: {error:#}")),
        }
    }
    bail!("all relay endpoints failed: {}", failures.join("; "))
}

async fn connect(endpoint: &str, device_id: &str, token: &str) -> Result<TcpStream> {
    let mut stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(endpoint))
        .await
        .with_context(|| format!("connection to {endpoint} timed out"))?
        .with_context(|| format!("failed to connect to {endpoint}"))?;
    stream
        .set_nodelay(true)
        .context("failed to set TCP_NODELAY")?;

    write_frame(
        &mut stream,
        &ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            device_id: device_id.to_owned(),
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
            device_id: welcomed_device,
        } if protocol_version == PROTOCOL_VERSION && welcomed_device == device_id => Ok(stream),
        ServerMessage::Error { code, message } => {
            bail!("relay rejected connection ({code}): {message}")
        }
        message => bail!("unexpected handshake response: {message:?}"),
    }
}

async fn connected_session(
    mut reader: tokio::net::tcp::OwnedReadHalf,
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    device_id: &str,
    command_rx: &mut Receiver<NetworkCommand>,
    ui_tx: &mpsc::SyncSender<UiEvent>,
    pending: &mut VecDeque<NetworkCommand>,
) -> Result<()> {
    let mut outstanding = VecDeque::new();
    let result = async {
        while let Some(command) = pending.pop_front() {
            let NetworkCommand::Publish { target, event } = &command;
            outstanding.push_back(command.clone());
            write_frame(
                &mut writer,
                &ClientMessage::Publish {
                    target: target.clone(),
                    event: event.clone(),
                },
            )
            .await?;
        }

        loop {
            tokio::select! {
                command = command_rx.recv() => {
                    let Some(command @ NetworkCommand::Publish { .. }) = command else {
                        return Ok(());
                    };
                    let NetworkCommand::Publish { target, event } = &command;
                    outstanding.push_back(command.clone());
                    write_frame(
                        &mut writer,
                        &ClientMessage::Publish {
                            target: target.clone(),
                            event: event.clone(),
                        },
                    )
                    .await?;
                }
                message = read_frame::<_, ServerMessage>(&mut reader) => {
                    let Some(message) = message? else {
                        bail!("relay closed the connection");
                    };
                    match message {
                        ServerMessage::Published {
                            event_id,
                            sequence,
                            recipients,
                        } => {
                            let publication = outstanding
                                .iter()
                                .position(|command| matches!(
                                    command,
                                    NetworkCommand::Publish { event, .. } if event.id == event_id
                                ))
                                .and_then(|position| outstanding.remove(position));
                            let Some(publication) = publication else {
                                warn!(%event_id, sequence, "relay acknowledged an unknown event");
                                continue;
                            };
                            let NetworkCommand::Publish { target, event } = publication;
                            let kind = match event.content {
                                ClipboardContent::Text { .. } => Some(ClipboardItemKind::Text),
                                ClipboardContent::ImagePng { .. } => {
                                    Some(ClipboardItemKind::Image)
                                }
                                ClipboardContent::FilesComplete { .. } => {
                                    Some(ClipboardItemKind::Files)
                                }
                                ClipboardContent::FilesStart { .. }
                                | ClipboardContent::FileChunk { .. } => None,
                            };
                            let _ = ui_tx.send(UiEvent::Published {
                                event_id,
                                sequence,
                                kind,
                                target,
                                recipients,
                            });
                        }
                        ServerMessage::Event { sequence, event } => {
                            if event.origin != device_id {
                                let _ = ui_tx.send(UiEvent::Remote { sequence, event });
                            }
                        }
                        ServerMessage::Ping { nonce } => {
                            write_frame(&mut writer, &ClientMessage::Pong { nonce }).await?;
                        }
                        ServerMessage::Error { code, message } => {
                            bail!("relay error ({code}): {message}");
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    .await;
    if result.is_err() {
        requeue_outstanding(outstanding, pending);
    }
    result
}

fn requeue_outstanding(
    mut outstanding: VecDeque<NetworkCommand>,
    pending: &mut VecDeque<NetworkCommand>,
) {
    while let Some(event) = outstanding.pop_back() {
        pending.push_front(event);
    }
}

fn build_event_loop() -> tao::event_loop::EventLoop<()> {
    let mut builder = EventLoopBuilder::new();
    let event_loop = builder.build();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};

        let mut event_loop = event_loop;
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
        event_loop.set_activate_ignoring_other_apps(true);
        event_loop
    }
    #[cfg(not(target_os = "macos"))]
    {
        event_loop
    }
}

fn open_directory(path: &std::path::Path) {
    if let Err(error) = std::fs::create_dir_all(path) {
        error!(%error, path = %path.display(), "failed to create directory");
        return;
    }
    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let result = Command::new("explorer.exe").arg(path).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let result = Command::new("xdg-open").arg(path).spawn();
    match result {
        // Reap the launcher so it doesn't linger as a zombie process.
        Ok(mut child) => {
            let _ = thread::Builder::new()
                .name("handclip-open".into())
                .spawn(move || child.wait());
        }
        Err(error) => error!(%error, path = %path.display(), "failed to open directory"),
    }
}

fn notify_user(summary: &str, body: &str) {
    let summary = summary.to_owned();
    let body = body.to_owned();
    let _ = thread::Builder::new()
        .name("handclip-notification".into())
        .spawn(move || {
            #[cfg(target_os = "macos")]
            {
                let script = r#"display notification (system attribute "HANDCLIP_NOTIFICATION_BODY") with title "Handclip" subtitle (system attribute "HANDCLIP_NOTIFICATION_SUMMARY")"#;
                if let Err(error) = Command::new("osascript")
                    .arg("-e")
                    .arg(script)
                    .env("HANDCLIP_NOTIFICATION_SUMMARY", &summary)
                    .env("HANDCLIP_NOTIFICATION_BODY", &body)
                    .status()
                {
                    warn!(%error, "failed to show desktop notification");
                }
            }
            #[cfg(target_os = "windows")]
            if let Err(error) = notify_rust::Notification::new()
                .appname("Handclip")
                .summary(&summary)
                .body(&body)
                .show()
            {
                warn!(%error, "failed to show desktop notification");
            }
        });
}

fn restart_agent(config_path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    // Under the launch agent, KeepAlive restarts us; a Finder launch must respawn itself.
    let should_spawn =
        std::env::var("XPC_SERVICE_NAME").as_deref() != Ok(LAUNCH_AGENT_LABEL);
    #[cfg(not(target_os = "macos"))]
    let should_spawn = true;

    if should_spawn {
        match std::env::current_exe().and_then(|executable| {
            Command::new(executable)
                .arg("--config-file")
                .arg(config_path)
                .arg("--startup-delay-ms")
                .arg("750")
                .spawn()
                .map(|_| ())
        }) {
            Ok(()) => {}
            Err(error) => error!(%error, "failed to start replacement Handclip process"),
        }
    }
}

fn quit_agent() {
    #[cfg(target_os = "macos")]
    {
        let user_id = Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|value| value.trim().to_owned());
        if let Some(user_id) = user_id {
            let target = format!("gui/{user_id}/{LAUNCH_AGENT_LABEL}");
            if let Err(error) = Command::new("launchctl").arg("bootout").arg(target).spawn() {
                error!(%error, "failed to unload Handclip launch agent");
            }
        }
    }
}
