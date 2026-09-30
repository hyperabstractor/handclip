use std::sync::mpsc::{self, Receiver, Sender};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tao::{
    dpi::LogicalSize,
    event::WindowEvent,
    event_loop::EventLoopWindowTarget,
    window::{Window, WindowBuilder, WindowId},
};
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};
use wry::{WebView, WebViewBuilder};

use handclip_core::DeliveryTarget;

use crate::config::AppConfig;

const MENU_SHARE_PREFIX: &str = "handclip.share.";
const MENU_SHARE_ALL: &str = "handclip.share.all";
const MENU_SETTINGS: &str = "handclip.settings";
const MENU_RECEIVED: &str = "handclip.received";
const MENU_LOGS: &str = "handclip.logs";
const MENU_RESTART: &str = "handclip.restart";
const MENU_QUIT: &str = "handclip.quit";
const SETTINGS_HTML: &str = include_str!("../assets/settings.html");

#[derive(Clone, Debug, Serialize)]
pub struct RuntimeStatus {
    pub connected: bool,
    pub endpoint: Option<String>,
    pub summary: String,
    pub last_activity: String,
}

impl RuntimeStatus {
    pub fn starting() -> Self {
        Self {
            connected: false,
            endpoint: None,
            summary: "Connecting…".into(),
            last_activity: "No clipboard activity yet".into(),
        }
    }
}

#[derive(Debug)]
pub enum GuiAction {
    Share(DeliveryTarget),
    Settings,
    OpenReceived,
    OpenLogs,
    Save { config: AppConfig, token: String },
    CloseSettings,
    Restart,
    Quit,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum IpcMessage {
    Save {
        config: AppConfig,
        #[serde(default)]
        token: String,
    },
    CloseSettings,
}

pub struct DesktopUi {
    tray: TrayIcon,
    status_item: MenuItem,
    activity_item: MenuItem,
    settings_window: Option<SettingsWindow>,
    ipc_tx: Sender<String>,
    ipc_rx: Receiver<String>,
    status: RuntimeStatus,
}

struct SettingsWindow {
    window: Window,
    webview: WebView,
}

impl DesktopUi {
    pub fn new(config: &AppConfig) -> Result<Self> {
        let status_item = MenuItem::new("● Connecting…", false, None);
        let activity_item = MenuItem::new("No clipboard activity yet", false, None);
        let target_items = config
            .target_shortcuts
            .iter()
            .map(|target| {
                MenuItem::with_id(
                    format!("{MENU_SHARE_PREFIX}{}", target.device_id),
                    format!(
                        "Send Clipboard to {}  ({})",
                        target.device_id, target.shortcut
                    ),
                    true,
                    None,
                )
            })
            .collect::<Vec<_>>();
        let share_all_item = MenuItem::with_id(
            MENU_SHARE_ALL,
            format!("Send Clipboard to All  ({})", config.broadcast_shortcut),
            true,
            None,
        );
        let settings_item = MenuItem::with_id(MENU_SETTINGS, "Settings…", true, None);
        let received_item = MenuItem::with_id(MENU_RECEIVED, "Open Received Files", true, None);
        let logs_item = MenuItem::with_id(MENU_LOGS, "Open Logs", true, None);
        let restart_item = MenuItem::with_id(MENU_RESTART, "Restart Handclip", true, None);
        let quit_item = MenuItem::with_id(MENU_QUIT, "Quit Handclip", true, None);
        let separator_1 = PredefinedMenuItem::separator();
        let separator_2 = PredefinedMenuItem::separator();
        let separator_3 = PredefinedMenuItem::separator();
        let menu = Menu::new();
        menu.append(&status_item)?;
        menu.append(&activity_item)?;
        menu.append(&separator_1)?;
        for item in &target_items {
            menu.append(item)?;
        }
        menu.append(&share_all_item)?;
        menu.append(&settings_item)?;
        menu.append(&separator_2)?;
        menu.append(&received_item)?;
        menu.append(&logs_item)?;
        menu.append(&separator_3)?;
        menu.append(&restart_item)?;
        menu.append(&quit_item)
            .context("failed to construct tray menu")?;

        let tray = TrayIconBuilder::new()
            .with_tooltip("Handclip — Connecting")
            .with_icon(status_icon(false)?)
            .with_icon_as_template(false)
            .with_menu(Box::new(menu))
            .build()
            .context("failed to create tray icon")?;
        let (ipc_tx, ipc_rx) = mpsc::channel();

        Ok(Self {
            tray,
            status_item,
            activity_item,
            settings_window: None,
            ipc_tx,
            ipc_rx,
            status: RuntimeStatus::starting(),
        })
    }

    pub fn poll_actions(&self) -> Vec<GuiAction> {
        let mut actions = Vec::new();
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            let event_id = event.id.as_ref();
            let action = match event_id {
                MENU_SHARE_ALL => Some(GuiAction::Share(DeliveryTarget::All)),
                MENU_SETTINGS => Some(GuiAction::Settings),
                MENU_RECEIVED => Some(GuiAction::OpenReceived),
                MENU_LOGS => Some(GuiAction::OpenLogs),
                MENU_RESTART => Some(GuiAction::Restart),
                MENU_QUIT => Some(GuiAction::Quit),
                _ => event_id
                    .strip_prefix(MENU_SHARE_PREFIX)
                    .map(|device_id| GuiAction::Share(DeliveryTarget::device(device_id))),
            };
            actions.extend(action);
        }
        while let Ok(message) = self.ipc_rx.try_recv() {
            match serde_json::from_str::<IpcMessage>(&message) {
                Ok(IpcMessage::Save { config, token }) => {
                    actions.push(GuiAction::Save { config, token });
                }
                Ok(IpcMessage::CloseSettings) => actions.push(GuiAction::CloseSettings),
                Err(error) => tracing::warn!(%error, "ignored invalid settings message"),
            }
        }
        actions
    }

    pub fn show_settings(
        &mut self,
        event_loop: &EventLoopWindowTarget<()>,
        config: &AppConfig,
    ) -> Result<()> {
        if let Some(settings) = &self.settings_window {
            settings.window.set_visible(true);
            settings.window.set_focus();
            self.sync_settings(config)?;
            return Ok(());
        }

        let model = SettingsModel::new(config, &self.status);
        let model_json =
            serde_json::to_string(&model).context("failed to encode settings model")?;
        let initialization_script = format!("window.__HANDCLIP_MODEL__ = {model_json};");
        let window = WindowBuilder::new()
            .with_title("Handclip Settings")
            .with_inner_size(LogicalSize::new(720.0, 780.0))
            .with_min_inner_size(LogicalSize::new(620.0, 620.0))
            .with_visible(true)
            .build(event_loop)
            .context("failed to create settings window")?;
        let ipc_tx = self.ipc_tx.clone();
        let webview = WebViewBuilder::new()
            .with_html(SETTINGS_HTML)
            .with_initialization_script(initialization_script)
            .with_ipc_handler(move |request| {
                let _ = ipc_tx.send(request.body().clone());
            })
            .build(&window)
            .context("failed to create settings webview")?;
        window.set_focus();
        self.settings_window = Some(SettingsWindow { window, webview });
        Ok(())
    }

    pub fn handle_window_event(&mut self, window_id: WindowId, event: &WindowEvent<'_>) -> bool {
        let Some(settings) = &self.settings_window else {
            return false;
        };
        if settings.window.id() != window_id {
            return false;
        }
        if matches!(event, WindowEvent::CloseRequested) {
            settings.window.set_visible(false);
        }
        true
    }

    pub fn set_connected(&mut self, endpoint: String) {
        self.status.connected = true;
        self.status.endpoint = Some(endpoint.clone());
        self.status.summary = format!("Connected to {endpoint}");
        self.status_item
            .set_text(format!("● Connected — {endpoint}"));
        let _ = self
            .tray
            .set_tooltip(Some(format!("Handclip — Connected to {endpoint}")));
        let _ = self
            .tray
            .set_icon(Some(status_icon(true).expect("valid icon")));
        self.sync_runtime_status();
    }

    pub fn set_disconnected(&mut self, detail: &str) {
        self.status.connected = false;
        self.status.endpoint = None;
        self.status.summary = "Disconnected — reconnecting".into();
        self.status_item.set_text("● Disconnected — reconnecting");
        let _ = self.tray.set_tooltip(Some("Handclip — Disconnected"));
        let _ = self
            .tray
            .set_icon(Some(status_icon(false).expect("valid icon")));
        tracing::debug!(%detail, "updated tray disconnect status");
        self.sync_runtime_status();
    }

    pub fn set_setup_required(&mut self) {
        self.status.summary = "Setup required".into();
        self.status_item.set_text("● Setup required — open Settings");
        let _ = self.tray.set_tooltip(Some("Handclip — Setup required"));
        self.sync_runtime_status();
    }

    pub fn set_activity(&mut self, activity: impl Into<String>) {
        let activity = activity.into();
        self.status.last_activity = activity.clone();
        self.activity_item.set_text(activity);
        self.sync_runtime_status();
    }

    pub fn save_error(&self, message: &str) {
        let script = format!(
            "window.handclipSaveResult(false, {});",
            serde_json::to_string(message).expect("string serializes")
        );
        if let Some(settings) = &self.settings_window {
            let _ = settings.webview.evaluate_script(&script);
        }
    }

    pub fn hide_settings(&self) {
        if let Some(settings) = &self.settings_window {
            settings.window.set_visible(false);
        }
    }

    fn sync_settings(&self, config: &AppConfig) -> Result<()> {
        let model = SettingsModel::new(config, &self.status);
        let json = serde_json::to_string(&model).context("failed to encode settings model")?;
        if let Some(settings) = &self.settings_window {
            settings
                .webview
                .evaluate_script(&format!("window.handclipLoad({json});"))
                .context("failed to update settings window")?;
        }
        Ok(())
    }

    fn sync_runtime_status(&self) {
        let json = serde_json::to_string(&self.status).expect("runtime status serializes");
        if let Some(settings) = &self.settings_window {
            let _ = settings
                .webview
                .evaluate_script(&format!("window.handclipRuntime({json});"));
        }
    }
}

#[derive(Serialize)]
struct SettingsModel<'a> {
    config: &'a AppConfig,
    status: &'a RuntimeStatus,
    platform: &'static str,
    shortcut_help: &'static str,
    token_saved: bool,
}

impl<'a> SettingsModel<'a> {
    fn new(config: &'a AppConfig, status: &'a RuntimeStatus) -> Self {
        Self {
            config,
            status,
            platform: platform_name(),
            shortcut_help: shortcut_help(),
            token_saved: config.token_file.exists(),
        }
    }
}

fn status_icon(connected: bool) -> Result<Icon> {
    const SIZE: u32 = 32;
    let mut rgba = vec![0_u8; (SIZE * SIZE * 4) as usize];
    let status_color = if connected {
        [45, 212, 156, 255]
    } else {
        [245, 158, 66, 255]
    };

    for y in 0..SIZE {
        for x in 0..SIZE {
            let index = ((y * SIZE + x) * 4) as usize;
            let dx = x as f32 - 15.5;
            let dy = y as f32 - 15.5;
            if dx * dx + dy * dy <= 13.5 * 13.5 {
                rgba[index..index + 4].copy_from_slice(&[41, 70, 116, 255]);
            }
            let link_a = ((x as i32 - 12).pow(2) + (y as i32 - 16).pow(2)) as f32;
            let link_b = ((x as i32 - 19).pow(2) + (y as i32 - 16).pow(2)) as f32;
            if (9.0..=25.0).contains(&link_a) || (9.0..=25.0).contains(&link_b) {
                rgba[index..index + 4].copy_from_slice(&[242, 247, 255, 255]);
            }
            let sdx = x as f32 - 25.0;
            let sdy = y as f32 - 25.0;
            if sdx * sdx + sdy * sdy <= 5.0 * 5.0 {
                rgba[index..index + 4].copy_from_slice(&status_color);
            }
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).context("failed to create status icon")
}

fn platform_name() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "macOS"
    }
    #[cfg(target_os = "windows")]
    {
        "Windows"
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        "Desktop"
    }
}

fn shortcut_help() -> &'static str {
    "Use each device's Device ID. Shortcuts like Control+Alt+1 work the same on macOS and Windows."
}
