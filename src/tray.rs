use crate::CliRunTarget;
use async_channel::{Receiver, Sender, TrySendError};
use std::cell::{Cell, RefCell};
use std::sync::{mpsc, OnceLock};
use std::time::Duration;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

#[cfg(target_os = "linux")]
use crate::desktop::{resolved_tray_icon_to_sni_pixmap, rgba_to_argb_in_place, ResolvedTrayIcon};
#[cfg(target_os = "linux")]
use crate::APP_NAME;
#[cfg(target_os = "linux")]
use ksni::blocking::TrayMethods;

#[cfg(target_os = "linux")]
pub(crate) const SNI_MENU_ON_ACTIVATE: bool = true;

const TRAY_ACTION_QUEUE_CAPACITY: usize = 64;
#[cfg(target_os = "linux")]
const SNI_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);

const NAME_ID: &str = "name";
const START_STOP_ID: &str = "start-stop";
const LOGS_ID: &str = "logs";
const CONFIGURE_ID: &str = "configure";
const ABOUT_ID: &str = "about";
const EXIT_ID: &str = "exit";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrayAction {
    ToggleStartStop,
    ShowLogs,
    ShowConfiguration,
    ShowAbout,
    Exit,
}

static TRAY_ACTION_CHANNEL: OnceLock<(Sender<TrayAction>, Receiver<TrayAction>)> = OnceLock::new();

fn tray_action_channel() -> &'static (Sender<TrayAction>, Receiver<TrayAction>) {
    TRAY_ACTION_CHANNEL.get_or_init(|| async_channel::bounded(TRAY_ACTION_QUEUE_CAPACITY))
}

fn enqueue_tray_action(action: TrayAction) {
    let (tx, rx) = tray_action_channel();
    match tx.try_send(action) {
        Ok(()) | Err(TrySendError::Closed(_)) => {}
        Err(TrySendError::Full(action)) => {
            let _ = rx.try_recv();
            let _ = tx.try_send(action);
        }
    }
}

#[cfg(test)]
pub(crate) fn enqueue_tray_action_for_test(action: TrayAction) {
    enqueue_tray_action(action);
}

#[cfg(test)]
pub(crate) fn tray_action_queue_capacity_for_test() -> usize {
    TRAY_ACTION_QUEUE_CAPACITY
}

pub(crate) fn action_from_menu_id(id: &str) -> Option<TrayAction> {
    match id {
        START_STOP_ID => Some(TrayAction::ToggleStartStop),
        LOGS_ID => Some(TrayAction::ShowLogs),
        CONFIGURE_ID => Some(TrayAction::ShowConfiguration),
        ABOUT_ID => Some(TrayAction::ShowAbout),
        EXIT_ID => Some(TrayAction::Exit),
        _ => None,
    }
}

pub(crate) fn drain_tray_actions() -> Vec<TrayAction> {
    let mut actions = Vec::new();
    let (_tx, rx) = tray_action_channel();
    while let Ok(action) = rx.try_recv() {
        actions.push(action);
    }

    while let Ok(event) = MenuEvent::receiver().try_recv() {
        if let Some(action) = action_from_menu_id(event.id.as_ref()) {
            actions.push(action);
        }
    }
    actions
}

pub(crate) struct TrayHandle {
    backend: TrayBackend,
}

impl TrayHandle {
    pub(crate) fn set_display(&self, text: &str) {
        self.backend.set_display(text);
    }

    pub(crate) fn set_running(&self, running: bool) {
        self.backend.set_running(running);
    }
}

enum TrayBackend {
    TrayIcon(TrayIconHandle),
    #[cfg(target_os = "linux")]
    Sni(SniHandle),
}

impl TrayBackend {
    fn set_display(&self, text: &str) {
        match self {
            Self::TrayIcon(handle) => handle.set_display(text),
            #[cfg(target_os = "linux")]
            Self::Sni(handle) => handle.set_display(text),
        }
    }

    fn set_running(&self, running: bool) {
        match self {
            Self::TrayIcon(handle) => handle.set_running(running),
            #[cfg(target_os = "linux")]
            Self::Sni(handle) => handle.set_running(running),
        }
    }
}

struct TrayIconHandle {
    tray_icon: TrayIcon,
    name_item: MenuItem,
    start_stop_item: MenuItem,
    mode: String,
    display_title: RefCell<String>,
    running: Cell<bool>,
}

impl TrayIconHandle {
    fn set_display(&self, text: &str) {
        self.display_title.replace(text.to_string());
        self.name_item
            .set_text(tray_menu_overview_label(text, &self.mode, self.running.get()));
        if let Err(err) = self.tray_icon.set_tooltip(Some(text)) {
            eprintln!("failed to update tray tooltip: {err}");
        }
        #[cfg(target_os = "linux")]
        self.tray_icon.set_title(Some(text));
    }

    fn set_running(&self, running: bool) {
        self.running.set(running);
        self.name_item.set_text(tray_menu_overview_label(
            &self.display_title.borrow(),
            &self.mode,
            running,
        ));
        self.start_stop_item
            .set_text(if running { "Stop" } else { "Start" });
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct SniState {
    id: String,
    title: String,
    tooltip: String,
    mode: String,
    show_configuration: bool,
    running: bool,
    icon_pixmap: Vec<ksni::Icon>,
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct SniTray {
    state: SniState,
}

#[cfg(target_os = "linux")]
impl ksni::Tray for SniTray {
    const MENU_ON_ACTIVATE: bool = SNI_MENU_ON_ACTIVATE;

    fn id(&self) -> String {
        self.state.id.clone()
    }

    fn title(&self) -> String {
        self.state.title.clone()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            icon_pixmap: self.state.icon_pixmap.clone(),
            title: self.state.tooltip.clone(),
            description: sni_tooltip_description(&self.state.mode, self.state.running),
            ..Default::default()
        }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.state.icon_pixmap.clone()
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;

        let mut menu = Vec::new();
        menu.push(
            StandardItem {
                label: sni_escape_menu_label(&tray_menu_overview_label(
                    &self.state.tooltip,
                    &self.state.mode,
                    self.state.running,
                )),
                enabled: false,
                ..Default::default()
            }
            .into(),
        );
        menu.push(
            StandardItem {
                label: if self.state.running {
                    "Stop".to_string()
                } else {
                    "Start".to_string()
                },
                activate: Box::new(|_| enqueue_tray_action(TrayAction::ToggleStartStop)),
                ..Default::default()
            }
            .into(),
        );
        menu.push(
            StandardItem {
                label: "Logs".to_string(),
                activate: Box::new(|_| enqueue_tray_action(TrayAction::ShowLogs)),
                ..Default::default()
            }
            .into(),
        );
        if self.state.show_configuration {
            menu.push(
                StandardItem {
                    label: "Configuration".to_string(),
                    activate: Box::new(|_| enqueue_tray_action(TrayAction::ShowConfiguration)),
                    ..Default::default()
                }
                .into(),
            );
        }
        menu.push(
            StandardItem {
                label: "About".to_string(),
                activate: Box::new(|_| enqueue_tray_action(TrayAction::ShowAbout)),
                ..Default::default()
            }
            .into(),
        );
        menu.push(ksni::MenuItem::Separator);
        menu.push(
            StandardItem {
                label: "Exit".to_string(),
                activate: Box::new(|_| enqueue_tray_action(TrayAction::Exit)),
                ..Default::default()
            }
            .into(),
        );
        menu
    }
}

#[cfg(target_os = "linux")]
struct SniHandle {
    handle: ksni::blocking::Handle<SniTray>,
}

#[cfg(target_os = "linux")]
impl SniHandle {
    fn set_display(&self, text: &str) {
        let text = text.to_string();
        let _ = self.handle.update(move |tray| {
            tray.state.tooltip = text.clone();
            tray.state.title = text.clone();
        });
    }

    fn set_running(&self, running: bool) {
        let _ = self.handle.update(move |tray| {
            tray.state.running = running;
        });
    }
}

#[cfg(target_os = "linux")]
impl Drop for SniHandle {
    fn drop(&mut self) {
        let awaiter = self.handle.shutdown();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            awaiter.wait();
            let _ = tx.send(());
        });

        if rx.recv_timeout(SNI_SHUTDOWN_TIMEOUT).is_err() {
            eprintln!("timed out waiting for SNI tray shutdown");
        }
    }
}

pub(crate) fn build_tray(
    tooltip: &str,
    show_configuration: bool,
    restored_running: bool,
    run_target: &CliRunTarget,
    owns_profile_lock: bool,
    icon: Icon,
    resolved_icon: Option<&ResolvedTrayIcon>,
) -> Result<TrayHandle, String> {
    #[cfg(target_os = "linux")]
    {
        match build_sni_tray(
            tooltip,
            show_configuration,
            restored_running,
            run_target,
            owns_profile_lock,
            resolved_icon,
        ) {
            Ok(handle) => {
                return Ok(TrayHandle {
                    backend: TrayBackend::Sni(handle),
                });
            }
            Err(err) => {
                eprintln!(
                    "failed to start native SNI tray backend: {err}; falling back to tray-icon"
                );
            }
        }
    }

    let tray_icon_handle = build_tray_icon_backend(
        tooltip,
        show_configuration,
        restored_running,
        run_target,
        icon,
    )?;
    Ok(TrayHandle {
        backend: TrayBackend::TrayIcon(tray_icon_handle),
    })
}

#[cfg(target_os = "linux")]
fn build_sni_tray(
    tooltip: &str,
    show_configuration: bool,
    restored_running: bool,
    run_target: &CliRunTarget,
    owns_profile_lock: bool,
    resolved_icon: Option<&ResolvedTrayIcon>,
) -> Result<SniHandle, String> {
    let icon_pixmap = resolved_icon
        .map(resolved_tray_icon_to_sni_pixmap)
        .unwrap_or_else(default_sni_icon_pixmap);

    let tray = SniTray {
        state: SniState {
            id: sni_id_for_run_target(run_target, owns_profile_lock),
            title: sni_title_for_tooltip(tooltip),
            tooltip: tooltip.to_string(),
            mode: sni_mode_text(run_target).to_string(),
            show_configuration,
            running: restored_running,
            icon_pixmap,
        },
    };

    let handle = tray
        .spawn()
        .map_err(|err| format!("failed to create SNI tray: {err}"))?;

    Ok(SniHandle { handle })
}

#[cfg(target_os = "linux")]
fn default_sni_icon_pixmap() -> Vec<ksni::Icon> {
    let image = match image::load_from_memory(include_bytes!("../assets/icon.png")) {
        Ok(image) => image.to_rgba8(),
        Err(err) => {
            eprintln!("failed to decode bundled SNI icon: {err}");
            return Vec::new();
        }
    };

    let (width, height) = image.dimensions();
    let mut argb = image.into_raw();
    rgba_to_argb_in_place(&mut argb);

    vec![ksni::Icon {
        width: width as i32,
        height: height as i32,
        data: argb,
    }]
}

#[cfg(target_os = "linux")]
fn sni_title_for_tooltip(tooltip: &str) -> String {
    tooltip.to_string()
}

#[cfg(target_os = "linux")]
pub(crate) fn sni_mode_text(run_target: &CliRunTarget) -> &'static str {
    tray_mode_text(run_target)
}

pub(crate) fn tray_mode_text(run_target: &CliRunTarget) -> &'static str {
    match run_target {
        CliRunTarget::PersistentProfile { .. } => "Persistent",
        CliRunTarget::EphemeralArgv { .. } => "Ephemeral",
    }
}

pub(crate) fn tray_status_text(running: bool) -> &'static str {
    if running { "Running" } else { "Stopped" }
}

pub(crate) fn tray_menu_overview_label(title: &str, mode: &str, running: bool) -> String {
    format!("{title}\nMode: {mode}\nStatus: {}", tray_status_text(running))
}

#[cfg(target_os = "linux")]
pub(crate) fn sni_tooltip_description(mode: &str, running: bool) -> String {
    format!("Mode: {mode}\nStatus: {}", tray_status_text(running))
}

#[cfg(target_os = "linux")]
pub(crate) fn sni_escape_menu_label(label: &str) -> String {
    label.replace('_', "__")
}

#[cfg(target_os = "linux")]
pub(crate) fn sni_id_for_run_target(run_target: &CliRunTarget, owns_profile_lock: bool) -> String {
    match run_target {
        CliRunTarget::PersistentProfile { profile } => {
            let sanitized = crate::config::sanitize_profile_name(profile);
            if owns_profile_lock {
                format!("{APP_NAME}.profile.{sanitized}")
            } else {
                let pid = std::process::id();
                format!("{APP_NAME}.profile.{sanitized}.secondary.{pid}")
            }
        }
        CliRunTarget::EphemeralArgv { argv } => {
            let pid = std::process::id();
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis())
                .unwrap_or(0);
            let token = argv.first().map(String::as_str).unwrap_or("cmd");
            let basename = std::path::Path::new(token)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("cmd");
            let mut cleaned = basename
                .chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() {
                        ch.to_ascii_lowercase()
                    } else {
                        '_'
                    }
                })
                .collect::<String>();
            if cleaned.is_empty() {
                cleaned.push_str("cmd");
            }
            format!("{APP_NAME}.ephemeral.{pid}.{nonce}.{cleaned}")
        }
    }
}

fn build_tray_icon_backend(
    tooltip: &str,
    show_configuration: bool,
    restored_running: bool,
    run_target: &CliRunTarget,
    icon: Icon,
) -> Result<TrayIconHandle, String> {
    let mode = tray_mode_text(run_target).to_string();
    let name_item = MenuItem::with_id(
        NAME_ID,
        tray_menu_overview_label(tooltip, &mode, restored_running),
        false,
        None,
    );
    let start_stop_item = MenuItem::with_id(
        START_STOP_ID,
        if restored_running { "Stop" } else { "Start" },
        true,
        None,
    );
    let logs_item = MenuItem::with_id(LOGS_ID, "Logs", true, None);
    let configure_item = MenuItem::with_id(CONFIGURE_ID, "Configuration", true, None);
    let about_item = MenuItem::with_id(ABOUT_ID, "About", true, None);
    let exit_item = MenuItem::with_id(EXIT_ID, "Exit", true, None);

    let tray_menu = Menu::new();
    tray_menu
        .append(&name_item)
        .map_err(|err| format!("menu append failed: {err}"))?;
    tray_menu
        .append(&start_stop_item)
        .map_err(|err| format!("menu append failed: {err}"))?;
    tray_menu
        .append(&logs_item)
        .map_err(|err| format!("menu append failed: {err}"))?;
    if show_configuration {
        tray_menu
            .append(&configure_item)
            .map_err(|err| format!("menu append failed: {err}"))?;
    }
    tray_menu
        .append(&about_item)
        .map_err(|err| format!("menu append failed: {err}"))?;
    tray_menu
        .append(&PredefinedMenuItem::separator())
        .map_err(|err| format!("menu append failed: {err}"))?;
    tray_menu
        .append(&exit_item)
        .map_err(|err| format!("menu append failed: {err}"))?;

    let mut tray_icon_builder = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_menu_on_left_click(true)
        .with_tooltip(tooltip)
        .with_icon(icon);
    #[cfg(target_os = "linux")]
    {
        tray_icon_builder = tray_icon_builder.with_title(tooltip);
    }

    let tray_icon = tray_icon_builder
        .build()
        .map_err(|err| format!("failed to create tray icon: {err}"))?;

    Ok(TrayIconHandle {
        tray_icon,
        name_item,
        start_stop_item,
        mode,
        display_title: RefCell::new(tooltip.to_string()),
        running: Cell::new(restored_running),
    })
}
