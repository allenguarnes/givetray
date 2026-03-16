mod cli;
mod config;
mod desktop;
mod logs;
mod process;
#[cfg(test)]
mod tests;
mod ui;

use crate::cli::{
    parse_cli_args, prepare_run_startup, should_expose_configuration, tray_tooltip,
    validate_runtime_mode,
};
use crate::desktop::{create_desktop_file_from_cli, load_tray_icon, load_window_icon_pixbuf};
use crate::logs::{build_logs_window, setup_log_receiver, setup_logs_handlers};
use crate::process::start_command;
use crate::ui::{
    build_about_window, build_config_window, install_css, install_log_filters,
    refresh_desktop_toggles, setup_config_handlers, setup_menu_polling, setup_process_watcher,
};
use gtk::prelude::*;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::{exit, Child};
use std::rc::Rc;
use tray_icon::menu::{Menu, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::TrayIconBuilder;

const APP_NAME: &str = "givetray";
const DEFAULT_PROFILE: &str = "default";
const DEFAULT_COMMAND: &str = "echo configure command";
const MAX_LOG_LINES: usize = 5000;
const MAX_UNDO: usize = 200;
const MAX_COMMAND_LENGTH: usize = 8192;
const MAX_PROFILE_LENGTH: usize = 128;
const ICON_FILE_NAME: &str = "icon.png";
const BUNDLED_ICON_FILE_NAME: &str = "default-icon.png";
const BG_CHILD_ENV: &str = "GIVETRAY_BG_CHILD";
const LOG_LINK_TAG_NAME: &str = "log-link";
const LOG_LINK_CLICK_SLOP: f64 = 4.0;

#[derive(Debug, Clone)]
struct CliOptions {
    run_target: CliRunTarget,
    command_override: Option<String>,
    icon_source: Option<PathBuf>,
    log_file: Option<PathBuf>,
    mode: CliMode,
}

impl CliOptions {
    fn persistent_profile(&self) -> Option<&str> {
        match &self.run_target {
            CliRunTarget::PersistentProfile { profile } => Some(profile),
            CliRunTarget::EphemeralArgv { .. } => None,
        }
    }
}

#[derive(Debug, Clone)]
enum CliRequest {
    Run(CliOptions),
    PrintHelp,
    PrintVersion,
}

#[derive(Debug, Clone)]
enum CliRunTarget {
    PersistentProfile { profile: String },
    EphemeralArgv { argv: Vec<String> },
}

#[derive(Debug, Clone)]
enum CliMode {
    Run,
    DesktopFile {
        output_dir: Option<PathBuf>,
        autostart: bool,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Config {
    command: String,
    #[serde(default)]
    autostart: bool,
    #[serde(default)]
    icon_path: Option<String>,
    #[serde(default)]
    log_to_file: bool,
    #[serde(default)]
    log_file_path: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct RuntimeOwnershipState {
    pid: u32,
    pgid: i32,
    started_at_unix_ms: u64,
    command_label: String,
    profile_name: Option<String>,
    ephemeral: bool,
}

enum UiEvent {
    AppendLog(String),
    ProcessExited(Option<i32>),
    SetRunning(bool),
}

struct AppState {
    persistent_config_access: Option<PersistentConfigAccess>,
    command: String,
    saved_command: String,
    saved_autostart: bool,
    saved_icon_path: Option<String>,
    saved_log_to_file: bool,
    saved_log_file_path: Option<String>,
    child: Option<Child>,
    log_lines: VecDeque<String>,
    log_links: VecDeque<Vec<LogLink>>,
    log_file_path: Option<PathBuf>,
    log_file_writer: Option<std::io::LineWriter<std::fs::File>>,
    logs_window: gtk::Window,
    logs_view: gtk::TextView,
    logs_buffer: gtk::TextBuffer,
    logs_clear_button: gtk::Button,
    logs_copy_button: gtk::Button,
    logs_status_label: gtk::Label,
    about_window: gtk::Window,
    config_window: gtk::Window,
    config_view: gtk::TextView,
    config_buffer: gtk::TextBuffer,
    config_autostart: gtk::CheckButton,
    config_log_to_file: gtk::CheckButton,
    config_applications: gtk::CheckButton,
    config_system_autostart: gtk::CheckButton,
    config_save_button: gtk::Button,
    config_status_label: gtk::Label,
    config_saved_applications: bool,
    config_saved_system_autostart: bool,
    config_undo: Vec<String>,
    config_redo: Vec<String>,
    config_last: String,
    config_ignore: bool,
    start_stop_item: MenuItem,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogLink {
    start_char: i32,
    end_char: i32,
    uri: String,
}

#[derive(Debug, Clone)]
struct PendingLogLink {
    uri: String,
    x: f64,
    y: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PersistentConfigAccess {
    profile: String,
    config_path: PathBuf,
}

struct StartupState {
    profile_label: String,
    persistent_config_access: Option<PersistentConfigAccess>,
    config: Config,
    log_file_path: Option<PathBuf>,
    launch_on_startup: bool,
}

enum ConfigCloseAction {
    Save,
    Discard,
    Cancel,
}

fn main() {
    install_log_filters();

    let cli = match parse_cli_args() {
        Ok(cli) => cli,
        Err(err) => {
            eprintln!("{err}");
            exit(2);
        }
    };

    if let Err(err) = validate_runtime_mode(&cli) {
        eprintln!("{err}");
        exit(1);
    }

    match cli.mode.clone() {
        CliMode::DesktopFile {
            output_dir,
            autostart,
        } => {
            if let Err(err) = create_desktop_file_from_cli(&cli, output_dir, autostart) {
                eprintln!("{err}");
                exit(1);
            }
            return;
        }
        CliMode::Run => {}
    }

    let startup = match prepare_run_startup(&cli) {
        Ok(startup) => startup,
        Err(err) => {
            eprintln!("failed to prepare app startup: {err}");
            exit(1);
        }
    };

    gtk::init().expect("failed to initialize GTK");
    install_css();

    let window_icon = load_window_icon_pixbuf(&startup.config);
    if let Some(icon) = window_icon.as_ref() {
        gtk::Window::set_default_icon(icon);
    }

    let (
        logs_window,
        logs_view,
        logs_buffer,
        logs_clear_button,
        logs_copy_button,
        logs_status_label,
    ) = build_logs_window();
    let (
        config_window,
        config_view,
        config_buffer,
        config_autostart,
        config_log_to_file,
        config_applications,
        config_system_autostart,
        config_save_button,
        config_status_label,
    ) = build_config_window(
        &startup.profile_label,
        &startup.config.command,
        startup.config.autostart,
        startup.config.log_to_file,
    );
    let about_window = build_about_window(window_icon.as_ref());

    if let Some(icon) = window_icon.as_ref() {
        logs_window.set_icon(Some(icon));
        config_window.set_icon(Some(icon));
        about_window.set_icon(Some(icon));
    }

    let (ui_tx, ui_rx) = async_channel::unbounded::<UiEvent>();

    let start_stop_id = MenuId::new("start-stop");
    let logs_id = MenuId::new("logs");
    let configure_id = MenuId::new("configure");
    let about_id = MenuId::new("about");
    let exit_id = MenuId::new("exit");

    let start_stop_item = MenuItem::with_id(start_stop_id.clone(), "Start", true, None);
    let logs_item = MenuItem::with_id(logs_id.clone(), "Logs", true, None);
    let configure_item = MenuItem::with_id(configure_id.clone(), "Configuration", true, None);
    let about_item = MenuItem::with_id(about_id.clone(), "About", true, None);
    let exit_item = MenuItem::with_id(exit_id.clone(), "Exit", true, None);

    let tray_menu = Menu::new();
    tray_menu
        .append(&start_stop_item)
        .expect("menu append failed");
    tray_menu.append(&logs_item).expect("menu append failed");
    if should_expose_configuration(startup.persistent_config_access.as_ref()) {
        tray_menu
            .append(&configure_item)
            .expect("menu append failed");
    }
    tray_menu.append(&about_item).expect("menu append failed");
    tray_menu
        .append(&PredefinedMenuItem::separator())
        .expect("menu append failed");
    tray_menu.append(&exit_item).expect("menu append failed");

    let tray_icon = load_tray_icon(&startup.config).expect("failed to load tray icon");
    let tooltip = tray_tooltip(&cli.run_target);
    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip(&tooltip)
        .with_icon(tray_icon)
        .build()
        .expect("failed to create tray icon");

    let state = Rc::new(RefCell::new(AppState {
        persistent_config_access: startup.persistent_config_access,
        command: startup.config.command.clone(),
        saved_command: startup.config.command.clone(),
        saved_autostart: startup.config.autostart,
        saved_icon_path: startup.config.icon_path.clone(),
        saved_log_to_file: startup.config.log_to_file,
        saved_log_file_path: startup.config.log_file_path.clone(),
        child: None,
        log_lines: VecDeque::new(),
        log_links: VecDeque::new(),
        log_file_path: startup.log_file_path,
        log_file_writer: None,
        logs_window,
        logs_view,
        logs_buffer,
        logs_clear_button,
        logs_copy_button,
        logs_status_label,
        about_window,
        config_window,
        config_view,
        config_buffer,
        config_autostart,
        config_log_to_file,
        config_applications,
        config_system_autostart,
        config_save_button,
        config_status_label,
        config_saved_applications: false,
        config_saved_system_autostart: false,
        config_undo: Vec::new(),
        config_redo: Vec::new(),
        config_last: startup.config.command,
        config_ignore: false,
        start_stop_item,
    }));

    if state.borrow().persistent_config_access.is_some() {
        let (apps_toggle, system_autostart_toggle) = {
            let app = state.borrow();
            (
                app.config_applications.clone(),
                app.config_system_autostart.clone(),
            )
        };
        refresh_desktop_toggles(state.clone(), &apps_toggle, &system_autostart_toggle);
        setup_config_handlers(state.clone());
    }
    setup_logs_handlers(state.clone());
    setup_log_receiver(state.clone(), ui_rx);
    setup_menu_polling(state.clone(), ui_tx.clone());
    setup_process_watcher(state.clone(), ui_tx.clone());

    if startup.launch_on_startup {
        start_command(state.clone(), ui_tx);
    }

    gtk::main();
}
