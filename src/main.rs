use async_channel::{Receiver, Sender};
use directories::{BaseDirs, ProjectDirs};
use glib::{ControlFlow, LogLevels, MainContext, Propagation};
use gtk::gdk;
use gtk::gdk_pixbuf::{InterpType, Pixbuf};
use gtk::prelude::*;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command, Stdio};
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use zeroize::Zeroizing;

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
    log_file_path: Option<PathBuf>,
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

fn main() {
    install_log_filters();

    let cli = match parse_cli_args() {
        Ok(cli) => cli,
        Err(err) => {
            eprintln!("{err}");
            print_help();
            process::exit(2);
        }
    };

    if let Err(err) = validate_runtime_mode(&cli) {
        eprintln!("{err}");
        process::exit(1);
    }

    match cli.mode.clone() {
        CliMode::DesktopFile {
            output_dir,
            autostart,
        } => {
            if let Err(err) = create_desktop_file_from_cli(&cli, output_dir, autostart) {
                eprintln!("{err}");
                process::exit(1);
            }
            return;
        }
        CliMode::Run => {}
    }

    let startup = match prepare_run_startup(&cli) {
        Ok(startup) => startup,
        Err(err) => {
            eprintln!("failed to prepare app startup: {err}");
            process::exit(1);
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
        log_file_path: startup.log_file_path,
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

fn detach_to_background_if_needed(cli: &CliOptions) -> Result<(), String> {
    if env::var_os(BG_CHILD_ENV).is_some() {
        return Ok(());
    }

    if !should_detach_for_terminal_launch() {
        return Ok(());
    }

    let executable =
        env::current_exe().map_err(|err| format!("unable to resolve executable path: {err}"))?;

    let mut command = Command::new(executable);
    command
        .args(build_detached_args(cli))
        .env(BG_CHILD_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    {
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let mut child = command
        .spawn()
        .map_err(|err| format!("unable to spawn detached process: {err}"))?;

    thread::sleep(Duration::from_millis(120));
    if let Ok(Some(status)) = child.try_wait() {
        return Err(format!("detached process exited early: {status}"));
    }

    process::exit(0);
}

fn build_detached_args(cli: &CliOptions) -> Vec<String> {
    match &cli.run_target {
        CliRunTarget::PersistentProfile { profile } => {
            let mut args = vec!["--config".to_string(), profile.clone()];
            if let Some(command_override) = &cli.command_override {
                args.push("--command".to_string());
                args.push(command_override.clone());
            }
            if let Some(icon_source) = &cli.icon_source {
                args.push("--icon".to_string());
                args.push(icon_source.display().to_string());
            }
            if let Some(log_file) = &cli.log_file {
                args.push("--log-file".to_string());
                args.push(log_file.display().to_string());
            }
            args
        }
        CliRunTarget::EphemeralArgv { argv } => {
            let mut args = Vec::with_capacity(argv.len() + 1);
            args.push("--".to_string());
            args.extend(argv.iter().cloned());
            args
        }
    }
}

fn tray_tooltip(run_target: &CliRunTarget) -> String {
    format!("{APP_NAME} ({})", run_target_label(run_target))
}

fn run_target_label(run_target: &CliRunTarget) -> String {
    match run_target {
        CliRunTarget::PersistentProfile { profile } => profile.clone(),
        CliRunTarget::EphemeralArgv { .. } => "ephemeral".to_string(),
    }
}

fn persistent_config_access(
    profile: Option<String>,
    config_path: Option<PathBuf>,
) -> Option<PersistentConfigAccess> {
    match (profile, config_path) {
        (Some(profile), Some(config_path)) => Some(PersistentConfigAccess {
            profile,
            config_path,
        }),
        _ => None,
    }
}

fn should_expose_configuration(access: Option<&PersistentConfigAccess>) -> bool {
    access.is_some()
}

fn ephemeral_command_text(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| shell_command_token(arg))
        .collect::<Vec<String>>()
        .join(" ")
}

fn shell_command_token(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '/' | '.' | '=' | ':'))
    {
        return arg.to_string();
    }

    let escaped = arg.replace('\'', "'\"'\"'");
    format!("'{escaped}'")
}

fn ephemeral_runtime_config(argv: &[String]) -> Config {
    Config {
        command: ephemeral_command_text(argv),
        autostart: false,
        icon_path: None,
        log_to_file: false,
        log_file_path: None,
    }
}

fn build_startup_state(cli: &CliOptions) -> Result<StartupState, String> {
    match &cli.run_target {
        CliRunTarget::PersistentProfile { profile } => {
            let config_path = config_path_for_profile(profile)
                .ok_or_else(|| "failed to resolve configuration path".to_string())?;
            let mut config = load_or_create_config(&config_path);

            match apply_cli_overrides_to_config(&mut config, cli) {
                Ok(true) => save_config(&config_path, &config)
                    .map_err(|err| format!("failed to save config overrides: {err}"))?,
                Ok(false) => {}
                Err(err) => return Err(format!("failed to apply CLI overrides: {err}")),
            }

            Ok(StartupState {
                profile_label: profile.clone(),
                persistent_config_access: persistent_config_access(
                    Some(profile.clone()),
                    Some(config_path),
                ),
                log_file_path: resolve_log_file_path(profile, &config),
                launch_on_startup: config.autostart,
                config,
            })
        }
        CliRunTarget::EphemeralArgv { argv } => Ok(StartupState {
            profile_label: run_target_label(&cli.run_target),
            persistent_config_access: None,
            config: ephemeral_runtime_config(argv),
            log_file_path: None,
            launch_on_startup: true,
        }),
    }
}

fn prepare_run_startup(cli: &CliOptions) -> Result<StartupState, String> {
    prepare_run_startup_with(cli, detach_to_background_if_needed)
}

fn prepare_run_startup_with<F>(cli: &CliOptions, detach: F) -> Result<StartupState, String>
where
    F: FnOnce(&CliOptions) -> Result<(), String>,
{
    let startup = build_startup_state(cli)?;
    detach(cli).map_err(|err| format!("failed to start background instance: {err}"))?;
    Ok(startup)
}

fn should_detach_for_terminal_launch() -> bool {
    unsafe {
        libc::isatty(libc::STDIN_FILENO) == 1
            || libc::isatty(libc::STDOUT_FILENO) == 1
            || libc::isatty(libc::STDERR_FILENO) == 1
    }
}

fn command_file_name(arg: &str) -> Option<&str> {
    Path::new(arg).file_name().and_then(|name| name.to_str())
}

fn sudo_option_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "-C" | "-D"
            | "-R"
            | "-T"
            | "-U"
            | "-g"
            | "-h"
            | "-p"
            | "-r"
            | "-t"
            | "-u"
            | "--chdir"
            | "--chroot"
            | "--close-from"
            | "--command-timeout"
            | "--group"
            | "--host"
            | "--other-user"
            | "--prompt"
            | "--role"
            | "--type"
            | "--user"
    )
}

fn is_environment_assignment(arg: &str) -> bool {
    let Some((name, _)) = arg.split_once('=') else {
        return false;
    };

    !name.is_empty() && !name.starts_with('-')
}

/// Returns the executable name that givetray will treat as the ephemeral
/// command target.
///
/// This is used to block recursive `givetray -- givetray ...` launches while
/// still recognizing the supported `sudo` prefixes: plain `sudo CMD`,
/// `sudo -- CMD`, `sudo -u root CMD`, and `sudo NAME=value CMD`.
fn effective_command_token(argv: &[String]) -> Option<&str> {
    let first = argv.first()?;
    let first_name = command_file_name(first)?;
    if first_name != "sudo" {
        return Some(first_name);
    }

    let mut skip_next = false;
    for arg in &argv[1..] {
        if skip_next {
            skip_next = false;
            continue;
        }

        if arg == "--" {
            continue;
        }

        if is_environment_assignment(arg) {
            continue;
        }

        if arg.starts_with("--") {
            if let Some((flag, _)) = arg.split_once('=') {
                if sudo_option_takes_value(flag) {
                    continue;
                }
            }
            if sudo_option_takes_value(arg) {
                skip_next = true;
                continue;
            }
        } else if arg.starts_with('-') {
            if arg.len() == 2 && sudo_option_takes_value(arg) {
                skip_next = true;
                continue;
            }
            if arg.len() > 2 && sudo_option_takes_value(&arg[..2]) {
                continue;
            }
            continue;
        }

        return command_file_name(arg);
    }

    None
}

fn parse_cli_args() -> Result<CliOptions, String> {
    match parse_cli_request_from(env::args())? {
        CliRequest::Run(cli) => Ok(cli),
        CliRequest::PrintHelp => {
            print_help();
            process::exit(0);
        }
        CliRequest::PrintVersion => {
            print_version();
            process::exit(0);
        }
    }
}

fn parse_cli_request_from<I, S>(args: I) -> Result<CliRequest, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let args: Vec<String> = args.into_iter().map(Into::into).collect();
    let raw_args = args.get(1..).unwrap_or(&[]);
    let option_args = match raw_args.iter().position(|arg| arg == "--") {
        Some(separator_index) => &raw_args[..separator_index],
        None => raw_args,
    };

    if option_args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Ok(CliRequest::PrintHelp);
    }
    if option_args
        .iter()
        .any(|arg| arg == "-V" || arg == "--version")
    {
        return Ok(CliRequest::PrintVersion);
    }

    Ok(CliRequest::Run(parse_cli_args_from(args)?))
}

fn parse_cli_args_from<I, S>(args: I) -> Result<CliOptions, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut args: Vec<String> = args.into_iter().map(Into::into).collect();
    if !args.is_empty() {
        args.remove(0);
    }

    let mut mode = CliMode::Run;
    if args.first().is_some_and(|arg| arg == "desktop-file") {
        mode = CliMode::DesktopFile {
            output_dir: None,
            autostart: false,
        };
        args.remove(0);
    }

    if let Some(separator_index) = args.iter().position(|arg| arg == "--") {
        if matches!(mode, CliMode::DesktopFile { .. }) {
            return Err("desktop-file does not support ephemeral mode".to_string());
        }

        let prefix = &args[..separator_index];
        let argv = args[(separator_index + 1)..].to_vec();
        if argv.is_empty() {
            return Err("ephemeral mode command cannot be empty".to_string());
        }
        if let Some(flag) = prefix
            .iter()
            .find(|arg| arg.as_str() == "-c" || arg.as_str() == "--config")
        {
            return Err(format!(
                "cannot mix persistent profile mode and ephemeral mode: {flag}"
            ));
        }
        if let Some(flag) = prefix
            .iter()
            .find(|arg| matches!(arg.as_str(), "-cmd" | "--command" | "--icon" | "--log-file"))
        {
            return Err(format!("{flag} is not allowed in ephemeral mode"));
        }
        if let Some(flag) = prefix
            .iter()
            .find(|arg| matches!(arg.as_str(), "--output-dir" | "--autostart"))
        {
            return Err(format!("{flag} is only valid with desktop-file"));
        }
        if let Some(unknown) = prefix.first() {
            return Err(format!("unknown argument: {unknown}"));
        }

        return Ok(CliOptions {
            run_target: CliRunTarget::EphemeralArgv { argv },
            command_override: None,
            icon_source: None,
            log_file: None,
            mode,
        });
    }

    let mut profile: Option<String> = None;
    let mut command_override: Option<String> = None;
    let mut icon_source = None;
    let mut log_file = None;

    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "-c" | "--config" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| "missing value for -c/--config".to_string())?;
                if profile.is_some() {
                    return Err("-c/--config provided more than once".to_string());
                }
                profile = Some(validate_profile_name(value)?);
                i += 2;
            }
            "-cmd" | "--command" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| "missing value for -cmd/--command".to_string())?;
                if command_override.is_some() {
                    return Err("-cmd/--command provided more than once".to_string());
                }
                command_override = Some(validate_command_override(value)?);
                i += 2;
            }
            "--icon" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| "missing value for --icon".to_string())?;
                icon_source = Some(PathBuf::from(value));
                i += 2;
            }
            "--log-file" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| "missing value for --log-file".to_string())?;
                if matches!(mode, CliMode::DesktopFile { .. }) {
                    return Err("--log-file is only valid in app mode".to_string());
                }
                log_file = Some(PathBuf::from(value));
                i += 2;
            }
            "--output-dir" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| "missing value for --output-dir".to_string())?;
                match &mut mode {
                    CliMode::DesktopFile {
                        output_dir,
                        autostart: _,
                    } => {
                        *output_dir = Some(PathBuf::from(value));
                        i += 2;
                    }
                    CliMode::Run => {
                        return Err("--output-dir is only valid with desktop-file".to_string());
                    }
                }
            }
            "--autostart" => match &mut mode {
                CliMode::DesktopFile {
                    output_dir: _,
                    autostart,
                } => {
                    *autostart = true;
                    i += 1;
                }
                CliMode::Run => {
                    return Err("--autostart is only valid with desktop-file".to_string());
                }
            },
            unknown => {
                return Err(format!("unknown argument: {unknown}"));
            }
        }
    }

    let profile =
        profile.ok_or_else(|| "missing required -c/--config PROFILE argument".to_string())?;

    Ok(CliOptions {
        run_target: CliRunTarget::PersistentProfile { profile },
        command_override,
        icon_source,
        log_file,
        mode,
    })
}

fn validate_runtime_mode(cli: &CliOptions) -> Result<(), String> {
    if let CliRunTarget::EphemeralArgv { ref argv } = cli.run_target {
        if effective_command_token(argv).is_some_and(|token| token == APP_NAME) {
            return Err("recursive ephemeral mode cannot launch givetray again".to_string());
        }
    }

    Ok(())
}

const HELP_USAGE_LINES: [&str; 3] = [
    "  {name} -c PROFILE [-cmd COMMAND|--command COMMAND] [--icon ICON_PATH] [--log-file LOG_PATH]",
    "  {name} -- <command...>",
    "  {name} desktop-file -c PROFILE [-cmd COMMAND|--command COMMAND] [--output-dir DIR] [--autostart] [--icon ICON_PATH]",
];

const HELP_MODE_LINES: [&str; 3] = [
    "  Persistent profile mode  Saved config, desktop entry support, and Configuration access",
    "  Ephemeral mode           Temporary, profile-free command launch via -- <command...>",
    "                           Starts immediately and keeps tray Start/Stop; Configuration is hidden",
];

const HELP_OPTION_LINES: [&str; 8] = [
    "  -c, --config PROFILE    Required profile name (letters, numbers, '-' or '_')",
    "  -cmd, --command COMMAND Set or overwrite saved command for the profile",
    "      --icon ICON_PATH    Copy icon into the selected profile and update config",
    "      --log-file LOG_PATH Enable log-to-file and set output path (app mode only)",
    "      --output-dir DIR    Output directory for desktop file (desktop-file mode only)",
    "      --autostart         Mark desktop file as autostart and default to ~/.config/autostart",
    "  -h, --help              Show this help",
    "  -V, --version           Show version",
];

fn render_help_lines(lines: &[&str]) -> String {
    lines
        .iter()
        .map(|line| line.replace("{name}", APP_NAME))
        .collect::<Vec<String>>()
        .join("\n")
}

fn help_text() -> String {
    format!(
        "{name}\n\nUsage:\n{usage}\n\nModes:\n{modes}\n\nOptions:\n{options}\n",
        name = APP_NAME,
        usage = render_help_lines(&HELP_USAGE_LINES),
        modes = render_help_lines(&HELP_MODE_LINES),
        options = render_help_lines(&HELP_OPTION_LINES),
    )
}

fn print_help() {
    println!("{}", help_text());
}

fn print_version() {
    println!("{APP_NAME} {}", env!("CARGO_PKG_VERSION"));
}

fn validate_profile_name(raw: &str) -> Result<String, String> {
    let profile = raw.trim();
    if profile.is_empty() {
        return Err("profile cannot be empty".to_string());
    }
    if profile.len() > MAX_PROFILE_LENGTH {
        return Err(format!(
            "profile is too long (max {MAX_PROFILE_LENGTH} characters)"
        ));
    }
    if !profile
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err("invalid profile name: use only letters, numbers, '-' or '_'".to_string());
    }
    Ok(profile.to_string())
}

fn validate_command_override(raw: &str) -> Result<String, String> {
    let command = raw.trim();
    if command.is_empty() {
        return Err("command cannot be empty".to_string());
    }
    if command.len() > MAX_COMMAND_LENGTH {
        return Err(format!(
            "command is too long (max {MAX_COMMAND_LENGTH} characters)"
        ));
    }
    if command.contains('\0') {
        return Err("command contains invalid null bytes".to_string());
    }

    match shell_words::split(command) {
        Ok(parts) if !parts.is_empty() => Ok(command.to_string()),
        Ok(_) => Err("command cannot be empty".to_string()),
        Err(err) => Err(format!("invalid -cmd/--command value: {err}")),
    }
}

fn apply_cli_overrides_to_config(config: &mut Config, cli: &CliOptions) -> Result<bool, String> {
    let mut changed = false;

    if let Some(command) = cli.command_override.as_ref() {
        if config.command != *command {
            config.command = command.clone();
            changed = true;
        }
    }

    if let Some(source_path) = cli.icon_source.as_ref() {
        let profile = cli
            .persistent_profile()
            .ok_or_else(|| "persistent profile is required for config overrides".to_string())?;
        let copied_path = copy_icon_to_profile(source_path, profile)?;
        let copied_path = copied_path.to_string_lossy().to_string();
        if config.icon_path.as_deref() != Some(copied_path.as_str()) {
            config.icon_path = Some(copied_path);
            changed = true;
        }
    }

    if let Some(log_file) = cli.log_file.as_ref() {
        let log_file = log_file.to_string_lossy().to_string();
        if !config.log_to_file || config.log_file_path.as_deref() != Some(log_file.as_str()) {
            config.log_to_file = true;
            config.log_file_path = Some(log_file);
            changed = true;
        }
    }

    if config.log_to_file && config.log_file_path.is_none() {
        let profile = cli
            .persistent_profile()
            .ok_or_else(|| "persistent profile is required for config overrides".to_string())?;
        if let Some(default_path) = default_log_file_path(profile) {
            config.log_file_path = Some(default_path.to_string_lossy().to_string());
            changed = true;
        }
    }

    Ok(changed)
}

fn create_desktop_file_from_cli(
    cli: &CliOptions,
    output_dir: Option<PathBuf>,
    autostart: bool,
) -> Result<(), String> {
    let profile = cli
        .persistent_profile()
        .ok_or_else(|| "desktop-file requires a persistent profile".to_string())?;
    let config_path = config_path_for_profile(profile)
        .ok_or_else(|| "unable to resolve configuration path".to_string())?;
    let mut config = load_or_create_config(&config_path);

    if apply_cli_overrides_to_config(&mut config, cli)? {
        save_config(&config_path, &config)
            .map_err(|err| format!("failed to save config overrides: {err}"))?;
    }

    let exec_path = env::current_exe()
        .map_err(|err| format!("unable to resolve executable path for desktop file: {err}"))?;
    let icon_path = resolve_icon_path_for_desktop(&config)
        .map_err(|err| format!("unable to resolve icon path for desktop file: {err}"))?;

    let desktop_path = if let Some(dir) = output_dir {
        dir.join(desktop_file_name(profile))
    } else if autostart {
        autostart_desktop_path(profile)
            .ok_or_else(|| "unable to resolve autostart desktop path".to_string())?
    } else {
        applications_desktop_path(profile)
            .ok_or_else(|| "unable to resolve Applications desktop path".to_string())?
    };

    let contents = desktop_entry(&exec_path, &icon_path, profile, autostart);
    write_desktop_file(&desktop_path, &contents)
        .map_err(|err| format!("failed to write desktop file: {err}"))?;

    println!("Desktop file created: {}", desktop_path.display());
    Ok(())
}

fn build_logs_window() -> (
    gtk::Window,
    gtk::TextView,
    gtk::TextBuffer,
    gtk::Button,
    gtk::Button,
    gtk::Label,
) {
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("Logs");
    window.set_default_size(820, 520);

    let buffer = gtk::TextBuffer::new(None::<&gtk::TextTagTable>);
    let text_view = gtk::TextView::with_buffer(&buffer);
    text_view.set_editable(false);
    text_view.set_monospace(true);
    text_view.set_cursor_visible(false);

    text_view.set_left_margin(8);
    text_view.set_right_margin(8);
    text_view.set_top_margin(8);
    text_view.set_bottom_margin(8);

    let clear_button = gtk::Button::new();
    let clear_icon = gtk::Image::from_icon_name(Some("edit-clear"), gtk::IconSize::Button);
    let clear_label = gtk::Label::new(Some("Clear"));
    let clear_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    clear_box.pack_start(&clear_icon, false, false, 0);
    clear_box.pack_start(&clear_label, false, false, 0);
    clear_button.add(&clear_box);

    let copy_button = gtk::Button::new();
    let copy_icon = gtk::Image::from_icon_name(Some("edit-copy"), gtk::IconSize::Button);
    let copy_label = gtk::Label::new(Some("Copy All"));
    let copy_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    copy_box.pack_start(&copy_icon, false, false, 0);
    copy_box.pack_start(&copy_label, false, false, 0);
    copy_button.add(&copy_box);

    let status_label = gtk::Label::new(Some("0 lines"));
    status_label.set_halign(gtk::Align::Start);
    status_label.set_xalign(0.0);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    actions.set_halign(gtk::Align::Fill);
    actions.set_margin_start(8);
    actions.set_margin_end(8);
    actions.set_margin_top(8);
    actions.set_margin_bottom(4);
    actions.pack_start(&status_label, true, true, 0);
    actions.pack_start(&copy_button, false, false, 0);
    actions.pack_start(&clear_button, false, false, 0);

    let scroller = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    scroller.set_hexpand(true);
    scroller.set_vexpand(true);
    scroller.add(&text_view);

    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    container.set_hexpand(true);
    container.set_vexpand(true);
    container.pack_start(&actions, false, false, 0);
    container.pack_start(&scroller, true, true, 0);

    window.add(&container);
    window.connect_delete_event(|window, _| {
        window.hide();
        Propagation::Stop
    });

    window.show_all();
    window.hide();

    (
        window,
        text_view,
        buffer,
        clear_button,
        copy_button,
        status_label,
    )
}

fn build_config_window(
    profile: &str,
    command: &str,
    autostart: bool,
    log_to_file: bool,
) -> (
    gtk::Window,
    gtk::TextView,
    gtk::TextBuffer,
    gtk::CheckButton,
    gtk::CheckButton,
    gtk::CheckButton,
    gtk::CheckButton,
    gtk::Button,
    gtk::Label,
) {
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title(&format!("Configuration ({profile})"));
    window.set_default_size(860, 300);

    let buffer = gtk::TextBuffer::new(None::<&gtk::TextTagTable>);
    buffer.set_text(command);
    let text_view = gtk::TextView::with_buffer(&buffer);
    text_view.set_monospace(true);
    text_view.set_wrap_mode(gtk::WrapMode::WordChar);
    text_view.set_hexpand(true);
    text_view.set_vexpand(true);
    text_view.set_left_margin(8);
    text_view.set_right_margin(8);
    text_view.set_top_margin(8);
    text_view.set_bottom_margin(8);

    let label = gtk::Label::new(Some("Command or script"));
    label.set_halign(gtk::Align::Start);
    label.set_margin_start(8);
    label.set_margin_end(8);
    label.set_margin_top(12);
    label.set_margin_bottom(4);

    let scroller = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    scroller.set_hexpand(true);
    scroller.set_vexpand(true);
    scroller.add(&text_view);

    let hint = gtk::Label::new(Some("Enter the terminal command to run for this profile."));
    hint.set_halign(gtk::Align::Start);
    hint.set_xalign(0.0);
    hint.set_margin_start(8);
    hint.set_margin_end(8);
    hint.set_margin_bottom(4);

    let autostart_toggle =
        gtk::CheckButton::with_label("Run command automatically when givetray launches");
    autostart_toggle.set_active(autostart);
    autostart_toggle.set_halign(gtk::Align::Start);
    autostart_toggle.set_tooltip_text(Some(
        "Runs this profile command when the givetray instance starts.",
    ));

    let log_to_file_toggle = gtk::CheckButton::with_label("Write logs to file");
    log_to_file_toggle.set_active(log_to_file);
    log_to_file_toggle.set_halign(gtk::Align::Start);
    log_to_file_toggle.set_tooltip_text(Some(
        "When enabled, command logs are appended to a profile log file.",
    ));

    let apps_toggle = gtk::CheckButton::with_label("Create Applications entry (.desktop)");
    apps_toggle.set_halign(gtk::Align::Start);
    apps_toggle.set_tooltip_text(Some(
        "Creates or removes ~/.local/share/applications desktop entry for this profile.",
    ));

    let autostart_desktop_toggle =
        gtk::CheckButton::with_label("Enable desktop session autostart (~/.config/autostart)");
    autostart_desktop_toggle.set_halign(gtk::Align::Start);
    autostart_desktop_toggle.set_tooltip_text(Some(
        "Creates or removes ~/.config/autostart desktop entry for this profile.",
    ));

    let save_button = gtk::Button::new();
    let save_icon = gtk::Image::from_icon_name(Some("media-floppy"), gtk::IconSize::Button);
    let save_label = gtk::Label::new(Some("Save"));
    let save_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    save_box.pack_start(&save_icon, false, false, 0);
    save_box.pack_start(&save_label, false, false, 0);
    save_button.add(&save_box);

    let options = gtk::Box::new(gtk::Orientation::Vertical, 4);
    options.pack_start(&autostart_toggle, false, false, 0);
    options.pack_start(&log_to_file_toggle, false, false, 0);
    options.pack_start(&apps_toggle, false, false, 0);
    options.pack_start(&autostart_desktop_toggle, false, false, 0);

    let status_label = gtk::Label::new(Some("Saved"));
    status_label.set_halign(gtk::Align::End);
    status_label.set_xalign(1.0);

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    footer.set_halign(gtk::Align::Fill);
    footer.set_valign(gtk::Align::Start);
    footer.set_margin_start(8);
    footer.set_margin_end(8);
    footer.set_margin_top(6);
    footer.set_margin_bottom(8);
    footer.pack_start(&options, true, true, 0);
    footer.pack_start(&status_label, false, false, 0);
    footer.pack_start(&save_button, false, false, 0);
    save_button.set_valign(gtk::Align::Center);
    save_button.set_halign(gtk::Align::End);
    save_button.set_vexpand(false);
    save_button.set_hexpand(false);

    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    container.set_hexpand(true);
    container.set_vexpand(true);
    container.pack_start(&label, false, false, 0);
    container.pack_start(&hint, false, false, 0);
    container.pack_start(&scroller, true, true, 0);
    container.pack_start(&footer, false, false, 0);

    window.add(&container);

    window.show_all();
    window.hide();

    (
        window,
        text_view,
        buffer,
        autostart_toggle,
        log_to_file_toggle,
        apps_toggle,
        autostart_desktop_toggle,
        save_button,
        status_label,
    )
}

fn build_about_window(window_icon: Option<&Pixbuf>) -> gtk::Window {
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("About");
    window.set_default_size(460, 300);
    window.set_resizable(true);

    let title = gtk::Label::new(None);
    title.set_markup("<b>givetray</b>");
    title.set_halign(gtk::Align::Start);
    title.set_xalign(0.0);

    let subtitle = gtk::Label::new(Some("Run terminal commands from the Linux system tray"));
    subtitle.set_halign(gtk::Align::Start);
    subtitle.set_xalign(0.0);
    subtitle.set_margin_bottom(6);

    let version = gtk::Label::new(Some(&format!("Version: {}", env!("CARGO_PKG_VERSION"))));
    version.set_halign(gtk::Align::Start);
    version.set_xalign(0.0);

    let author = gtk::Label::new(Some("Author: Allen Guarnes"));
    author.set_halign(gtk::Align::Start);
    author.set_xalign(0.0);

    let github = gtk::LinkButton::with_label("https://github.com/allenguarnes/givetray", "GitHub");
    github.set_halign(gtk::Align::Start);
    github.set_margin_top(2);

    let coffee =
        gtk::LinkButton::with_label("https://buymeacoffee.com/allenguarnes", "Buy Me a Coffee");
    coffee.set_halign(gtk::Align::Start);

    let description = gtk::Label::new(Some(
        "Run terminal commands from a tray icon with profile-based settings and logs.",
    ));
    description.set_halign(gtk::Align::Start);
    description.set_xalign(0.0);
    description.set_line_wrap(true);

    let licenses = gtk::Label::new(Some("License: MIT OR Apache-2.0"));
    licenses.set_halign(gtk::Align::Start);
    licenses.set_xalign(0.0);
    licenses.set_line_wrap(true);

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    header.set_halign(gtk::Align::Start);

    if let Some(icon) = window_icon {
        let about_icon = icon
            .scale_simple(56, 56, InterpType::Bilinear)
            .unwrap_or_else(|| icon.clone());
        let icon_image = gtk::Image::from_pixbuf(Some(&about_icon));
        icon_image.set_halign(gtk::Align::Start);
        header.pack_start(&icon_image, false, false, 0);
    }

    let title_block = gtk::Box::new(gtk::Orientation::Vertical, 2);
    title_block.pack_start(&title, false, false, 0);
    title_block.pack_start(&subtitle, false, false, 0);
    header.pack_start(&title_block, false, false, 0);

    let links = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    links.pack_start(&github, false, false, 0);
    links.pack_start(&coffee, false, false, 0);

    let divider = gtk::Separator::new(gtk::Orientation::Horizontal);

    let container = gtk::Box::new(gtk::Orientation::Vertical, 6);
    container.set_margin_start(12);
    container.set_margin_end(12);
    container.set_margin_top(12);
    container.set_margin_bottom(12);
    container.pack_start(&header, false, false, 0);
    container.pack_start(&description, false, false, 0);
    container.pack_start(&divider, false, false, 4);
    container.pack_start(&version, false, false, 0);
    container.pack_start(&author, false, false, 0);
    container.pack_start(&links, false, false, 0);
    container.pack_start(&licenses, false, false, 0);

    window.add(&container);
    window.connect_delete_event(|window, _| {
        window.hide();
        Propagation::Stop
    });

    window.show_all();
    window.hide();

    window
}

fn setup_config_handlers(state: Rc<RefCell<AppState>>) {
    let view = state.borrow().config_view.clone();
    let buffer = state.borrow().config_buffer.clone();
    let window = state.borrow().config_window.clone();
    let autostart_toggle = state.borrow().config_autostart.clone();
    let log_to_file_toggle = state.borrow().config_log_to_file.clone();
    let save_button = state.borrow().config_save_button.clone();
    let apps_toggle = state.borrow().config_applications.clone();
    let system_autostart_toggle = state.borrow().config_system_autostart.clone();

    let state_close = state.clone();
    let buffer_close = buffer.clone();
    let autostart_toggle_close = autostart_toggle.clone();
    let log_to_file_toggle_close = log_to_file_toggle.clone();
    let apps_toggle_close = apps_toggle.clone();
    let system_autostart_toggle_close = system_autostart_toggle.clone();
    window.connect_delete_event(move |window, _| {
        let current_text = buffer_text(&buffer_close);
        let has_unsaved = {
            let app = state_close.borrow();
            config_has_unsaved_changes(
                &app,
                &current_text,
                autostart_toggle_close.is_active(),
                log_to_file_toggle_close.is_active(),
                apps_toggle_close.is_active(),
                system_autostart_toggle_close.is_active(),
            )
        };

        if !has_unsaved {
            window.hide();
            return Propagation::Stop;
        }

        match show_config_close_dialog(window) {
            ConfigCloseAction::Save => {
                save_from_config_widgets(
                    state_close.clone(),
                    &buffer_close,
                    &log_to_file_toggle_close,
                    &apps_toggle_close,
                    &system_autostart_toggle_close,
                );
                window.hide();
            }
            ConfigCloseAction::Discard => {
                refresh_config_dirty_status(state_close.clone());
                window.hide();
            }
            ConfigCloseAction::Cancel => {}
        }

        Propagation::Stop
    });

    let state_save = state.clone();
    let buffer_save = buffer.clone();
    let log_to_file_toggle_save = log_to_file_toggle.clone();
    let apps_toggle_save = apps_toggle.clone();
    let system_autostart_save = system_autostart_toggle.clone();
    save_button.connect_clicked(move |_| {
        save_from_config_widgets(
            state_save.clone(),
            &buffer_save,
            &log_to_file_toggle_save,
            &apps_toggle_save,
            &system_autostart_save,
        );
    });

    let state_changed = state.clone();
    let buffer_changed = buffer.clone();
    buffer.connect_changed(move |_| {
        let text = buffer_text(&buffer_changed);
        let mut state = state_changed.borrow_mut();
        if state.config_ignore || text == state.config_last {
            return;
        }
        let last = state.config_last.clone();
        state.config_undo.push(last);
        if state.config_undo.len() > MAX_UNDO {
            state.config_undo.remove(0);
        }
        state.config_last = text;
        state.config_redo.clear();
        drop(state);
        refresh_config_dirty_status(state_changed.clone());
    });

    let state_autostart_toggled = state.clone();
    autostart_toggle.connect_toggled(move |_| {
        refresh_config_dirty_status(state_autostart_toggled.clone());
    });

    let state_logfile_toggled = state.clone();
    log_to_file_toggle.connect_toggled(move |_| {
        refresh_config_dirty_status(state_logfile_toggled.clone());
    });

    let state_apps_toggled = state.clone();
    apps_toggle.connect_toggled(move |_| {
        refresh_config_dirty_status(state_apps_toggled.clone());
    });

    let state_system_toggled = state.clone();
    system_autostart_toggle.connect_toggled(move |_| {
        refresh_config_dirty_status(state_system_toggled.clone());
    });

    let state_keys = state.clone();
    let buffer_keys = buffer.clone();
    view.connect_key_press_event(move |_, event| {
        let key = event.keyval();
        let modifiers = event.state();
        let ctrl = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
        let shift = modifiers.contains(gdk::ModifierType::SHIFT_MASK);

        if ctrl && shift && (key == gdk::keys::constants::Z || key == gdk::keys::constants::z) {
            if let Some(next) = {
                let mut state = state_keys.borrow_mut();
                let next = state.config_redo.pop();
                if let Some(ref value) = next {
                    let last = state.config_last.clone();
                    state.config_undo.push(last);
                    state.config_last = value.clone();
                    state.config_ignore = true;
                }
                next
            } {
                buffer_keys.set_text(&next);
                state_keys.borrow_mut().config_ignore = false;
                refresh_config_dirty_status(state_keys.clone());
                return Propagation::Stop;
            }
            return Propagation::Proceed;
        }

        if ctrl && !shift && (key == gdk::keys::constants::z || key == gdk::keys::constants::Z) {
            if let Some(prev) = {
                let mut state = state_keys.borrow_mut();
                let prev = state.config_undo.pop();
                if let Some(ref value) = prev {
                    let last = state.config_last.clone();
                    state.config_redo.push(last);
                    state.config_last = value.clone();
                    state.config_ignore = true;
                }
                prev
            } {
                buffer_keys.set_text(&prev);
                state_keys.borrow_mut().config_ignore = false;
                refresh_config_dirty_status(state_keys.clone());
                return Propagation::Stop;
            }
            return Propagation::Proceed;
        }

        if ctrl && (key == gdk::keys::constants::y || key == gdk::keys::constants::Y) {
            if let Some(next) = {
                let mut state = state_keys.borrow_mut();
                let next = state.config_redo.pop();
                if let Some(ref value) = next {
                    let last = state.config_last.clone();
                    state.config_undo.push(last);
                    state.config_last = value.clone();
                    state.config_ignore = true;
                }
                next
            } {
                buffer_keys.set_text(&next);
                state_keys.borrow_mut().config_ignore = false;
                refresh_config_dirty_status(state_keys.clone());
                return Propagation::Stop;
            }
            return Propagation::Proceed;
        }

        Propagation::Proceed
    });
}

enum ConfigCloseAction {
    Save,
    Discard,
    Cancel,
}

fn show_config_close_dialog(parent: &gtk::Window) -> ConfigCloseAction {
    let dialog = gtk::MessageDialog::new(
        Some(parent),
        gtk::DialogFlags::MODAL,
        gtk::MessageType::Question,
        gtk::ButtonsType::None,
        "You have unsaved configuration changes.",
    );
    dialog.set_secondary_text(Some("Save changes before closing this window?"));
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    dialog.add_button("Discard", gtk::ResponseType::No);
    dialog.add_button("Save", gtk::ResponseType::Yes);
    dialog.set_default_response(gtk::ResponseType::Yes);

    let response = dialog.run();
    dialog.close();

    match response {
        gtk::ResponseType::Yes => ConfigCloseAction::Save,
        gtk::ResponseType::No => ConfigCloseAction::Discard,
        _ => ConfigCloseAction::Cancel,
    }
}

fn save_from_config_widgets(
    state: Rc<RefCell<AppState>>,
    buffer: &gtk::TextBuffer,
    log_to_file_toggle: &gtk::CheckButton,
    apps_toggle: &gtk::CheckButton,
    system_autostart_toggle: &gtk::CheckButton,
) {
    let text = buffer_text(buffer);
    let saved = save_configuration(state.clone(), text, log_to_file_toggle.is_active());
    if saved {
        apply_desktop_actions(
            state.clone(),
            apps_toggle.is_active(),
            system_autostart_toggle.is_active(),
        );
        refresh_desktop_toggles(state.clone(), apps_toggle, system_autostart_toggle);
    }
    refresh_config_dirty_status(state);
}

fn config_has_unsaved_changes(
    state: &AppState,
    current_command: &str,
    current_autostart: bool,
    current_log_to_file: bool,
    current_applications: bool,
    current_system_autostart: bool,
) -> bool {
    current_command != state.saved_command
        || current_autostart != state.saved_autostart
        || current_log_to_file != state.saved_log_to_file
        || current_applications != state.config_saved_applications
        || current_system_autostart != state.config_saved_system_autostart
}

fn refresh_config_dirty_status(state: Rc<RefCell<AppState>>) {
    let (status_label, status_text) = {
        let app = state.borrow();
        if app.config_ignore {
            return;
        }

        let command = buffer_text(&app.config_buffer);
        let unsaved = config_has_unsaved_changes(
            &app,
            &command,
            app.config_autostart.is_active(),
            app.config_log_to_file.is_active(),
            app.config_applications.is_active(),
            app.config_system_autostart.is_active(),
        );
        (
            app.config_status_label.clone(),
            if unsaved { "Unsaved changes" } else { "Saved" },
        )
    };

    status_label.set_text(status_text);
}

fn setup_logs_handlers(state: Rc<RefCell<AppState>>) {
    let clear_button = state.borrow().logs_clear_button.clone();
    let copy_button = state.borrow().logs_copy_button.clone();
    let buffer = state.borrow().logs_buffer.clone();
    let status_label = state.borrow().logs_status_label.clone();

    let state_clear = state.clone();
    let buffer_clear = buffer.clone();
    let status_clear = status_label.clone();
    clear_button.connect_clicked(move |_| {
        let mut state = state_clear.borrow_mut();
        state.log_lines.clear();
        buffer_clear.set_text("");
        set_logs_status(&status_clear, 0, Some("cleared"));
    });

    let buffer_copy = buffer.clone();
    let state_copy = state.clone();
    let status_copy = status_label.clone();
    copy_button.connect_clicked(move |_| {
        let text = buffer_text(&buffer_copy);
        let clipboard = gtk::Clipboard::get(&gdk::SELECTION_CLIPBOARD);
        clipboard.set_text(&text);
        let line_count = state_copy.borrow().log_lines.len();
        set_logs_status(&status_copy, line_count, Some("copied"));
    });
}

fn set_logs_status(label: &gtk::Label, line_count: usize, detail: Option<&str>) {
    let text = match detail {
        Some(detail) => format!("{line_count} lines | {detail}"),
        None => format!("{line_count} lines"),
    };
    label.set_text(&text);
}

fn setup_log_receiver(state: Rc<RefCell<AppState>>, receiver: Receiver<UiEvent>) {
    MainContext::default().spawn_local(async move {
        while let Ok(event) = receiver.recv().await {
            let mut state = state.borrow_mut();
            match event {
                UiEvent::AppendLog(line) => append_log(&mut state, line),
                UiEvent::ProcessExited(code) => {
                    state.child = None;
                    state.start_stop_item.set_text("Start");
                    let msg = match code {
                        Some(code) => format!("command exited with code {code}"),
                        None => "command exited".to_string(),
                    };
                    append_log(&mut state, msg);
                }
                UiEvent::SetRunning(running) => {
                    state
                        .start_stop_item
                        .set_text(if running { "Stop" } else { "Start" });
                }
            }
        }
    });
}

fn setup_menu_polling(state: Rc<RefCell<AppState>>, ui_tx: Sender<UiEvent>) {
    glib::timeout_add_local(Duration::from_millis(150), move || {
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            let id = event.id;
            if id == "start-stop" {
                let running = state.borrow().child.is_some();
                if running {
                    stop_command(state.clone(), ui_tx.clone());
                } else {
                    start_command(state.clone(), ui_tx.clone());
                }
            } else if id == "logs" {
                let window = state.borrow().logs_window.clone();
                window.show_all();
                window.resize(820, 520);
            } else if id == "configure" {
                let can_configure = {
                    let state = state.borrow();
                    should_expose_configuration(state.persistent_config_access.as_ref())
                };
                if !can_configure {
                    append_log(
                        &mut state.borrow_mut(),
                        "configuration window is unavailable in ephemeral mode".to_string(),
                    );
                    continue;
                }
                let (
                    window,
                    view,
                    buffer,
                    autostart_toggle,
                    log_to_file_toggle,
                    command,
                    autostart,
                    log_to_file,
                ) = {
                    let state = state.borrow();
                    (
                        state.config_window.clone(),
                        state.config_view.clone(),
                        state.config_buffer.clone(),
                        state.config_autostart.clone(),
                        state.config_log_to_file.clone(),
                        state.saved_command.clone(),
                        state.saved_autostart,
                        state.saved_log_to_file,
                    )
                };
                let (apps_toggle, system_autostart_toggle) = {
                    let state = state.borrow();
                    (
                        state.config_applications.clone(),
                        state.config_system_autostart.clone(),
                    )
                };
                {
                    let mut state = state.borrow_mut();
                    state.config_ignore = true;
                    state.config_last = command.clone();
                    state.config_undo.clear();
                    state.config_redo.clear();
                }
                buffer.set_text(&command);
                autostart_toggle.set_active(autostart);
                log_to_file_toggle.set_active(log_to_file);
                refresh_desktop_toggles(state.clone(), &apps_toggle, &system_autostart_toggle);
                refresh_config_dirty_status(state.clone());
                window.show_all();
                view.grab_focus();
            } else if id == "about" {
                let window = state.borrow().about_window.clone();
                window.show_all();
            } else if id == "exit" {
                stop_command_blocking(state.clone());
                gtk::main_quit();
            }
        }

        ControlFlow::Continue
    });
}

fn install_log_filters() {
    glib::log_set_handler(
        Some("libayatana-appindicator"),
        LogLevels::LEVEL_WARNING,
        false,
        false,
        |_domain, _level, _message| {},
    );
}

fn install_css() {
    let provider = gtk::CssProvider::new();
    let css = b"
        textview,
        textview text {
            font-family: monospace;
            font-size: 11pt;
        }
    ";
    provider.load_from_data(css).expect("failed to load CSS");

    if let Some(screen) = gdk::Screen::default() {
        gtk::StyleContext::add_provider_for_screen(
            &screen,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn setup_process_watcher(state: Rc<RefCell<AppState>>, ui_tx: Sender<UiEvent>) {
    glib::timeout_add_local(Duration::from_millis(500), move || {
        let mut should_emit = None;
        {
            let mut state = state.borrow_mut();
            if let Some(child) = state.child.as_mut() {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        should_emit = Some(status.code());
                        state.child = None;
                    }
                    Ok(None) => {}
                    Err(err) => {
                        append_log(&mut state, format!("failed to check command status: {err}"));
                    }
                }
            }
        }

        if let Some(code) = should_emit {
            let _ = ui_tx.send_blocking(UiEvent::ProcessExited(code));
        }

        ControlFlow::Continue
    });
}

fn save_configuration(
    state: Rc<RefCell<AppState>>,
    text: String,
    log_to_file_enabled: bool,
) -> bool {
    let mut state = state.borrow_mut();
    let Some(access) = state.persistent_config_access.clone() else {
        append_log(
            &mut state,
            "configuration save is unavailable in ephemeral mode".to_string(),
        );
        return false;
    };
    let new_autostart = state.config_autostart.is_active();
    let mut new_log_file_path = state.saved_log_file_path.clone();
    if log_to_file_enabled && new_log_file_path.is_none() {
        new_log_file_path =
            default_log_file_path(&access.profile).map(|path| path.to_string_lossy().to_string());
    }
    let new_config = Config {
        command: text.clone(),
        autostart: new_autostart,
        icon_path: state.saved_icon_path.clone(),
        log_to_file: log_to_file_enabled,
        log_file_path: new_log_file_path.clone(),
    };

    if let Err(err) = save_config(&access.config_path, &new_config) {
        append_log(&mut state, format!("Failed to save configuration: {err}"));
        return false;
    }

    state.command = text.clone();
    state.config_last = text.clone();
    state.saved_command = text;
    state.saved_autostart = new_autostart;
    state.saved_log_to_file = log_to_file_enabled;
    state.saved_log_file_path = new_log_file_path;
    state.log_file_path = if log_to_file_enabled {
        state.saved_log_file_path.as_ref().map(PathBuf::from)
    } else {
        None
    };

    let saved_log_file_path = state.saved_log_file_path.clone();
    let saved_log_to_file = state.saved_log_to_file;

    append_log(&mut state, "Configuration updated".to_string());
    if saved_log_to_file {
        if let Some(path) = saved_log_file_path {
            append_log(&mut state, format!("Log file enabled: {path}"));
        }
    } else {
        append_log(&mut state, "Log file output disabled".to_string());
    }

    true
}

fn refresh_desktop_toggles(
    state: Rc<RefCell<AppState>>,
    apps_toggle: &gtk::CheckButton,
    system_autostart_toggle: &gtk::CheckButton,
) {
    let access = state.borrow().persistent_config_access.clone();
    let enabled = access.is_some();
    let (apps_exists, autostart_exists) = if let Some(access) = access {
        (
            applications_desktop_path(&access.profile).is_some_and(|path| path.exists()),
            autostart_desktop_path(&access.profile).is_some_and(|path| path.exists()),
        )
    } else {
        (false, false)
    };

    {
        let mut app = state.borrow_mut();
        app.config_ignore = true;
        app.config_saved_applications = apps_exists;
        app.config_saved_system_autostart = autostart_exists;
    }

    apps_toggle.set_active(apps_exists);
    system_autostart_toggle.set_active(autostart_exists);
    apps_toggle.set_sensitive(enabled);
    system_autostart_toggle.set_sensitive(enabled);

    {
        let mut app = state.borrow_mut();
        app.config_ignore = false;
    }

    refresh_config_dirty_status(state);
}

fn apply_desktop_actions(
    state: Rc<RefCell<AppState>>,
    apps_enabled: bool,
    autostart_enabled: bool,
) {
    let access = {
        let app = state.borrow();
        app.persistent_config_access.clone()
    };
    let Some(access) = access else {
        append_log(
            &mut state.borrow_mut(),
            "desktop entry actions are unavailable in ephemeral mode".to_string(),
        );
        return;
    };

    let exec_path = match env::current_exe() {
        Ok(path) => path,
        Err(err) => {
            append_log(
                &mut state.borrow_mut(),
                format!("Failed to resolve executable path: {err}"),
            );
            return;
        }
    };

    let icon_path = {
        let app = state.borrow();
        let config = load_or_create_config(&access.config_path);
        let icon_path = match resolve_icon_path_for_desktop(&config) {
            Ok(path) => path,
            Err(err) => {
                drop(app);
                append_log(
                    &mut state.borrow_mut(),
                    format!("Failed to prepare icon path: {err}"),
                );
                return;
            }
        };
        icon_path
    };

    let desktop_name = desktop_file_name(&access.profile);

    if let Some(path) = applications_desktop_path(&access.profile) {
        if apps_enabled {
            let content = desktop_entry(&exec_path, &icon_path, &access.profile, false);
            if let Err(err) = write_desktop_file(&path, &content) {
                append_log(
                    &mut state.borrow_mut(),
                    format!("Failed to add Applications entry: {err}"),
                );
            } else {
                append_log(
                    &mut state.borrow_mut(),
                    format!("Applications entry updated: {desktop_name}"),
                );
            }
        } else if path.exists() {
            match fs::remove_file(&path) {
                Ok(_) => append_log(
                    &mut state.borrow_mut(),
                    format!("Applications entry removed: {desktop_name}"),
                ),
                Err(err) => append_log(
                    &mut state.borrow_mut(),
                    format!("Failed to remove Applications entry: {err}"),
                ),
            }
        }
    } else {
        append_log(
            &mut state.borrow_mut(),
            "Unable to resolve Applications entry path".to_string(),
        );
    }

    if let Some(path) = autostart_desktop_path(&access.profile) {
        if autostart_enabled {
            let content = desktop_entry(&exec_path, &icon_path, &access.profile, true);
            if let Err(err) = write_desktop_file(&path, &content) {
                append_log(
                    &mut state.borrow_mut(),
                    format!("Failed to add system autostart entry: {err}"),
                );
            } else {
                append_log(
                    &mut state.borrow_mut(),
                    format!("System autostart entry updated: {desktop_name}"),
                );
            }
        } else if path.exists() {
            match fs::remove_file(&path) {
                Ok(_) => append_log(
                    &mut state.borrow_mut(),
                    format!("System autostart entry removed: {desktop_name}"),
                ),
                Err(err) => append_log(
                    &mut state.borrow_mut(),
                    format!("Failed to remove system autostart entry: {err}"),
                ),
            }
        }
    } else {
        append_log(
            &mut state.borrow_mut(),
            "Unable to resolve system autostart entry path".to_string(),
        );
    }

    let config = load_or_create_config(&access.config_path);
    if let Err(err) = save_config(&access.config_path, &config) {
        append_log(
            &mut state.borrow_mut(),
            format!("Failed to sync configuration file: {err}"),
        );
    }
}

fn applications_desktop_path(profile: &str) -> Option<PathBuf> {
    BaseDirs::new().map(|dirs| {
        dirs.data_local_dir()
            .join("applications")
            .join(desktop_file_name(profile))
    })
}

fn autostart_desktop_path(profile: &str) -> Option<PathBuf> {
    BaseDirs::new().map(|dirs| {
        dirs.config_dir()
            .join("autostart")
            .join(desktop_file_name(profile))
    })
}

fn config_path_for_profile(profile: &str) -> Option<PathBuf> {
    ProjectDirs::from("com", APP_NAME, APP_NAME).map(|proj| {
        proj.config_dir()
            .join("configs")
            .join(format!("{}.toml", sanitize_profile_name(profile)))
    })
}

fn default_log_file_path(profile: &str) -> Option<PathBuf> {
    ProjectDirs::from("com", APP_NAME, APP_NAME).map(|proj| {
        proj.data_local_dir()
            .join("logs")
            .join(format!("{}.log", sanitize_profile_name(profile)))
    })
}

fn resolve_log_file_path(profile: &str, config: &Config) -> Option<PathBuf> {
    if !config.log_to_file {
        return None;
    }
    config
        .log_file_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| default_log_file_path(profile))
}

fn load_or_create_config(path: &PathBuf) -> Config {
    let default = Config {
        command: DEFAULT_COMMAND.to_string(),
        autostart: false,
        icon_path: None,
        log_to_file: false,
        log_file_path: None,
    };

    let content = match fs::read_to_string(path) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if let Err(save_err) = save_config(path, &default) {
                eprintln!(
                    "failed to initialize default config at {}: {save_err}",
                    path.display()
                );
            }
            return default;
        }
        Err(err) => {
            eprintln!("failed to read config at {}: {err}", path.display());
            return default;
        }
    };

    match toml::from_str(&content) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("failed to parse config at {}: {err}", path.display());
            default
        }
    }
}

fn save_config(path: &PathBuf, config: &Config) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("failed to create config dir: {err}"))?;
    }

    let payload = toml::to_string_pretty(config)
        .map_err(|err| format!("failed to serialize config: {err}"))?;
    fs::write(path, payload).map_err(|err| format!("failed to write config: {err}"))?;
    Ok(())
}

fn sanitize_profile_name(profile: &str) -> String {
    let mut cleaned = profile
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();

    if cleaned.is_empty() {
        cleaned = DEFAULT_PROFILE.to_string();
    }
    cleaned
}

fn profile_icon_path(profile: &str) -> Option<PathBuf> {
    ProjectDirs::from("com", APP_NAME, APP_NAME).map(|proj| {
        proj.data_local_dir()
            .join("profiles")
            .join(sanitize_profile_name(profile))
            .join(ICON_FILE_NAME)
    })
}

fn bundled_icon_path() -> Option<PathBuf> {
    ProjectDirs::from("com", APP_NAME, APP_NAME)
        .map(|proj| proj.data_local_dir().join(BUNDLED_ICON_FILE_NAME))
}

fn copy_icon_to_profile(source_path: &Path, profile: &str) -> Result<PathBuf, String> {
    let bytes = fs::read(source_path).map_err(|err| format!("unable to read icon file: {err}"))?;
    image::load_from_memory(&bytes).map_err(|err| format!("invalid icon image: {err}"))?;

    let target_path = profile_icon_path(profile)
        .ok_or_else(|| "unable to resolve icon storage path".to_string())?;
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("unable to create icon dir: {err}"))?;
    }
    fs::write(&target_path, bytes).map_err(|err| format!("unable to store icon copy: {err}"))?;
    Ok(target_path)
}

fn ensure_bundled_icon_file() -> Result<PathBuf, std::io::Error> {
    let icon_path = bundled_icon_path()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "project dirs"))?;
    if let Some(parent) = icon_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&icon_path, include_bytes!("../assets/icon.png"))?;
    Ok(icon_path)
}

fn resolve_icon_path_for_desktop(config: &Config) -> Result<PathBuf, std::io::Error> {
    if let Some(path) = config.icon_path.as_ref() {
        let icon = PathBuf::from(path);
        if icon.exists() {
            return Ok(icon);
        }
    }
    ensure_bundled_icon_file()
}

fn load_window_icon_pixbuf(config: &Config) -> Option<Pixbuf> {
    let icon_path = resolve_icon_path_for_desktop(config).ok()?;
    Pixbuf::from_file(icon_path).ok()
}

fn load_tray_icon(config: &Config) -> Result<Icon, Box<dyn std::error::Error>> {
    if let Some(path) = config.icon_path.as_ref() {
        let icon_path = PathBuf::from(path);
        if icon_path.exists() {
            match fs::read(&icon_path)
                .map_err(|err| err.to_string())
                .and_then(|bytes| image::load_from_memory(&bytes).map_err(|err| err.to_string()))
            {
                Ok(image) => {
                    let rgba = image.to_rgba8();
                    let (width, height) = rgba.dimensions();
                    return Ok(Icon::from_rgba(rgba.into_raw(), width, height)?);
                }
                Err(err) => eprintln!(
                    "failed to load profile icon at {}: {err}. falling back to bundled icon",
                    icon_path.display()
                ),
            }
        }
    }

    let bytes = include_bytes!("../assets/icon.png");
    let image = image::load_from_memory(bytes)?;
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    Ok(Icon::from_rgba(rgba.into_raw(), width, height)?)
}

fn desktop_file_name(profile: &str) -> String {
    format!("{APP_NAME}_{}.desktop", sanitize_profile_name(profile))
}

fn desktop_entry(exec_path: &Path, icon_path: &Path, profile: &str, autostart: bool) -> String {
    let mut entry = String::from("[Desktop Entry]\n");
    entry.push_str("Type=Application\n");

    let display_name = format!("{} ({APP_NAME})", sanitize_profile_name(profile));
    entry.push_str(&format!("Name={display_name}\n"));

    let exec = format!(
        "{} --config {}",
        desktop_escape_arg(&exec_path.to_string_lossy()),
        desktop_escape_arg(profile)
    );
    entry.push_str(&format!("Exec={exec}\n"));
    entry.push_str(&format!("Icon={}\n", icon_path.to_string_lossy()));
    entry.push_str("Terminal=false\n");
    entry.push_str("Categories=Utility;\n");
    if autostart {
        entry.push_str("X-GNOME-Autostart-enabled=true\n");
    }
    entry
}

fn desktop_escape_arg(value: &str) -> String {
    let percent_escaped = value.replace('%', "%%");
    let needs_quotes = value
        .chars()
        .any(|ch| ch.is_whitespace() || ch == '"' || ch == '\\');
    if !needs_quotes {
        return percent_escaped;
    }

    let mut escaped = String::with_capacity(percent_escaped.len() + 2);
    escaped.push('"');
    for ch in percent_escaped.chars() {
        if ch == '"' || ch == '\\' {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped.push('"');
    escaped
}

fn write_desktop_file(path: &PathBuf, contents: &str) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)
}

fn buffer_text(buffer: &gtk::TextBuffer) -> String {
    let start = buffer.start_iter();
    let end = buffer.end_iter();
    buffer
        .text(&start, &end, true)
        .unwrap_or_default()
        .to_string()
}

fn append_log_to_file(path: &Path, line: &str) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")
}

fn strip_ansi_codes(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut result = String::with_capacity(line.len());
    let mut chunk_start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }

        if chunk_start < i {
            result.push_str(&line[chunk_start..i]);
        }

        i += 1;
        if i >= bytes.len() {
            chunk_start = i;
            break;
        }

        match bytes[i] {
            b'[' => {
                i += 1;
                while i < bytes.len() {
                    if (0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b']' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            b'P' | b'X' | b'^' | b'_' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            _ => {
                i += 1;
            }
        }

        chunk_start = i;
    }

    if chunk_start < line.len() {
        result.push_str(&line[chunk_start..]);
    }

    result
}

fn append_log(state: &mut AppState, line: String) {
    let clean_line = strip_ansi_codes(&line);
    let mut rebuild = false;
    if state.log_lines.len() >= MAX_LOG_LINES {
        state.log_lines.pop_front();
        rebuild = true;
    }
    state.log_lines.push_back(clean_line.clone());

    if rebuild {
        let payload = state
            .log_lines
            .iter()
            .cloned()
            .collect::<Vec<String>>()
            .join("\n");
        state.logs_buffer.set_text(&payload);
    } else {
        let mut end_iter = state.logs_buffer.end_iter();
        state.logs_buffer.insert(&mut end_iter, &clean_line);
        state.logs_buffer.insert(&mut end_iter, "\n");
    }

    let mut end_iter = state.logs_buffer.end_iter();
    state
        .logs_view
        .scroll_to_iter(&mut end_iter, 0.0, false, 0.0, 0.0);

    set_logs_status(&state.logs_status_label, state.log_lines.len(), None);

    if let Some(path) = state.log_file_path.as_ref() {
        if let Err(err) = append_log_to_file(path, &clean_line) {
            eprintln!("failed to write log file at {}: {err}", path.display());
        }
    }
}

fn start_command(state: Rc<RefCell<AppState>>, ui_tx: Sender<UiEvent>) {
    if state.borrow().child.is_some() {
        let _ = ui_tx.send_blocking(UiEvent::AppendLog("command is already running".to_string()));
        return;
    }

    let command = state.borrow().command.clone();
    let mut args = match shell_words::split(&command) {
        Ok(parts) if !parts.is_empty() => parts,
        Ok(_) => {
            let _ = ui_tx.send_blocking(UiEvent::AppendLog("command is empty".to_string()));
            return;
        }
        Err(err) => {
            let _ = ui_tx.send_blocking(UiEvent::AppendLog(format!("command parse error: {err}")));
            return;
        }
    };

    let sudo_password = if is_sudo_command(&args) {
        ensure_sudo_stdin_flag(&mut args);
        match prompt_sudo_password() {
            Some(password) => Some(password),
            None => {
                let _ = ui_tx.send_blocking(UiEvent::AppendLog(
                    "sudo password prompt cancelled".to_string(),
                ));
                return;
            }
        }
    } else {
        None
    };

    let mut cmd = Command::new(&args[0]);
    cmd.env_remove(BG_CHILD_ENV);
    if args.len() > 1 {
        cmd.args(&args[1..]);
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    if sudo_password.is_some() {
        cmd.stdin(Stdio::piped());
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            let _ = ui_tx.send_blocking(UiEvent::AppendLog(format!(
                "failed to start command: {err}"
            )));
            return;
        }
    };

    if let Some(password) = sudo_password {
        if let Some(mut stdin) = child.stdin.take() {
            if let Err(err) = stdin
                .write_all(password.as_bytes())
                .and_then(|_| stdin.write_all(b"\n"))
            {
                let _ = ui_tx.send_blocking(UiEvent::AppendLog(format!(
                    "failed to send sudo password to process: {err}"
                )));
            }
        } else {
            let _ = ui_tx.send_blocking(UiEvent::AppendLog(
                "unable to access sudo stdin pipe".to_string(),
            ));
        }
    }

    if let Some(stdout) = child.stdout.take() {
        spawn_reader(stdout, ui_tx.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_reader(stderr, ui_tx.clone());
    }

    state.borrow_mut().child = Some(child);
    let _ = ui_tx.send_blocking(UiEvent::SetRunning(true));
    let _ = ui_tx.send_blocking(UiEvent::AppendLog("command started".to_string()));
}

fn stop_command(state: Rc<RefCell<AppState>>, ui_tx: Sender<UiEvent>) {
    let child = state.borrow_mut().child.take();
    if let Some(mut child) = child {
        thread::spawn(move || {
            terminate_child(&mut child, Duration::from_secs(2));
            let code = child.wait().ok().and_then(|status| status.code());
            let _ = ui_tx.send_blocking(UiEvent::ProcessExited(code));
        });
    }
}

fn stop_command_blocking(state: Rc<RefCell<AppState>>) {
    let child = state.borrow_mut().child.take();
    if let Some(mut child) = child {
        terminate_child(&mut child, Duration::from_secs(2));
        let _ = child.wait();
    }
}

fn terminate_child(child: &mut Child, timeout: Duration) {
    if let Ok(Some(_)) = child.try_wait() {
        return;
    }
    let pid = child.id();
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(_) => break,
        }
        if start.elapsed() > timeout {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    let _ = child.kill();
}

fn spawn_reader<R: std::io::Read + Send + 'static>(reader: R, ui_tx: Sender<UiEvent>) {
    thread::spawn(move || {
        let buf = BufReader::new(reader);
        for line in buf.lines() {
            match line {
                Ok(line) => {
                    let _ = ui_tx.send_blocking(UiEvent::AppendLog(line));
                }
                Err(err) => {
                    let _ =
                        ui_tx.send_blocking(UiEvent::AppendLog(format!("log read error: {err}")));
                    break;
                }
            }
        }
    });
}

fn is_sudo_command(args: &[String]) -> bool {
    args.first()
        .and_then(|arg| command_file_name(arg))
        .is_some_and(|name| name == "sudo")
}

fn ensure_sudo_stdin_flag(args: &mut Vec<String>) {
    if args
        .iter()
        .any(|arg| arg == "-S" || arg == "--stdin" || arg == "--askpass")
    {
        return;
    }

    if args.len() == 1 {
        args.push("-S".to_string());
        return;
    }

    args.insert(1, "-S".to_string());
}

fn prompt_sudo_password() -> Option<Zeroizing<String>> {
    let dialog = gtk::Dialog::with_buttons(
        Some("Sudo Password"),
        None::<&gtk::Window>,
        gtk::DialogFlags::MODAL,
        &[
            ("Cancel", gtk::ResponseType::Cancel),
            ("Start", gtk::ResponseType::Accept),
        ],
    );
    dialog.set_default_response(gtk::ResponseType::Accept);

    let content = dialog.content_area();
    content.set_spacing(8);

    let description = gtk::Label::new(Some("Enter sudo password to start this command:"));
    description.set_halign(gtk::Align::Start);
    description.set_xalign(0.0);
    content.pack_start(&description, false, false, 0);

    let password_entry = gtk::Entry::new();
    password_entry.set_visibility(false);
    password_entry.set_invisible_char(Some('*'));
    password_entry.set_activates_default(true);
    content.pack_start(&password_entry, false, false, 0);

    dialog.show_all();
    password_entry.grab_focus();

    let response = dialog.run();
    let password = if response == gtk::ResponseType::Accept {
        let text = password_entry.text().to_string();
        if text.is_empty() {
            None
        } else {
            Some(Zeroizing::new(text))
        }
    } else {
        None
    };

    dialog.close();
    password
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct TestEnvGuard {
        home: Option<String>,
        xdg_config_home: Option<String>,
        xdg_data_home: Option<String>,
    }

    impl TestEnvGuard {
        fn set(temp_root: &Path) -> Self {
            let guard = Self {
                home: env::var("HOME").ok(),
                xdg_config_home: env::var("XDG_CONFIG_HOME").ok(),
                xdg_data_home: env::var("XDG_DATA_HOME").ok(),
            };

            let home = temp_root.join("home");
            let config = temp_root.join("config");
            let data = temp_root.join("data");
            fs::create_dir_all(&home).expect("test home dir should be created");
            fs::create_dir_all(&config).expect("test config dir should be created");
            fs::create_dir_all(&data).expect("test data dir should be created");

            env::set_var("HOME", &home);
            env::set_var("XDG_CONFIG_HOME", &config);
            env::set_var("XDG_DATA_HOME", &data);

            guard
        }
    }

    impl Drop for TestEnvGuard {
        fn drop(&mut self) {
            match self.home.as_ref() {
                Some(value) => env::set_var("HOME", value),
                None => env::remove_var("HOME"),
            }
            match self.xdg_config_home.as_ref() {
                Some(value) => env::set_var("XDG_CONFIG_HOME", value),
                None => env::remove_var("XDG_CONFIG_HOME"),
            }
            match self.xdg_data_home.as_ref() {
                Some(value) => env::set_var("XDG_DATA_HOME", value),
                None => env::remove_var("XDG_DATA_HOME"),
            }
        }
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        env::temp_dir().join(format!("givetray-{name}-{}-{nonce}", process::id()))
    }

    #[test]
    fn help_text_documents_ephemeral_mode() {
        let help = help_text();

        assert!(help.contains("Usage:\n  givetray -c PROFILE [-cmd COMMAND|--command COMMAND] [--icon ICON_PATH] [--log-file LOG_PATH]\n  givetray -- <command...>\n  givetray desktop-file -c PROFILE [-cmd COMMAND|--command COMMAND] [--output-dir DIR] [--autostart] [--icon ICON_PATH]"));
        assert!(help.contains("Modes:\n  Persistent profile mode  Saved config, desktop entry support, and Configuration access\n  Ephemeral mode           Temporary, profile-free command launch via -- <command...>\n                           Starts immediately and keeps tray Start/Stop; Configuration is hidden"));
    }

    #[test]
    fn parse_persistent_profile_mode() {
        let cli = parse_cli_args_from(["givetray", "-c", "default"])
            .expect("persistent profile mode should parse");

        assert!(matches!(
            cli.run_target,
            CliRunTarget::PersistentProfile { ref profile } if profile == "default"
        ));
        assert!(matches!(cli.mode, CliMode::Run));
    }

    #[test]
    fn parse_ephemeral_mode() {
        let cli = parse_cli_args_from(["givetray", "--", "echo", "hello world"])
            .expect("ephemeral mode should parse");

        assert!(matches!(
            cli.run_target,
            CliRunTarget::EphemeralArgv { ref argv }
                if argv == &["echo".to_string(), "hello world".to_string()]
        ));
        assert!(matches!(cli.mode, CliMode::Run));
    }

    #[test]
    fn parse_ephemeral_mode_passes_through_help_flag() {
        let request = parse_cli_request_from(["givetray", "--", "echo", "--help"])
            .expect("ephemeral help arg should parse");

        let cli = match request {
            CliRequest::Run(cli) => cli,
            CliRequest::PrintHelp => panic!("help flag after -- must not be intercepted"),
            CliRequest::PrintVersion => panic!("version flag after -- must not be intercepted"),
        };

        assert!(matches!(
            cli.run_target,
            CliRunTarget::EphemeralArgv { ref argv }
                if argv == &["echo".to_string(), "--help".to_string()]
        ));
    }

    #[test]
    fn parse_ephemeral_mode_passes_through_version_flag() {
        let request = parse_cli_request_from(["givetray", "--", "echo", "--version"])
            .expect("ephemeral version arg should parse");

        let cli = match request {
            CliRequest::Run(cli) => cli,
            CliRequest::PrintHelp => panic!("help flag after -- must not be intercepted"),
            CliRequest::PrintVersion => panic!("version flag after -- must not be intercepted"),
        };

        assert!(matches!(
            cli.run_target,
            CliRunTarget::EphemeralArgv { ref argv }
                if argv == &["echo".to_string(), "--version".to_string()]
        ));
    }

    #[test]
    fn reject_empty_ephemeral_mode() {
        let err =
            parse_cli_args_from(["givetray", "--"]).expect_err("empty ephemeral mode must fail");

        assert!(err.contains("ephemeral"));
        assert!(err.contains("empty"));
    }

    #[test]
    fn reject_mixed_profile_and_ephemeral_mode() {
        let err = parse_cli_args_from(["givetray", "-c", "default", "--", "echo", "hi"])
            .expect_err("mixed mode must fail");

        assert!(err.contains("cannot mix"));
    }

    #[test]
    fn reject_profile_only_flags_in_ephemeral_mode() {
        let err = parse_cli_args_from([
            "givetray",
            "--log-file",
            "/tmp/givetray.log",
            "--",
            "echo",
            "hi",
        ])
        .expect_err("profile-only flags must fail in ephemeral mode");

        assert!(err.contains("ephemeral mode"));
        assert!(err.contains("--log-file"));
    }

    #[test]
    fn parse_desktop_file_profile_mode() {
        let cli = parse_cli_args_from(["givetray", "desktop-file", "-c", "scrcpy"])
            .expect("desktop-file profile mode should parse");

        assert!(matches!(
            cli.run_target,
            CliRunTarget::PersistentProfile { ref profile } if profile == "scrcpy"
        ));
        assert!(matches!(
            cli.mode,
            CliMode::DesktopFile {
                output_dir: None,
                autostart: false
            }
        ));
    }

    #[test]
    fn reject_desktop_file_ephemeral_mode() {
        let err = parse_cli_args_from(["givetray", "desktop-file", "--", "echo", "hi"])
            .expect_err("desktop-file must reject ephemeral mode");

        assert!(err.contains("desktop-file"));
        assert!(err.contains("ephemeral mode"));
    }

    #[test]
    fn allow_ephemeral_runtime_when_non_recursive() {
        let cli = parse_cli_args_from(["givetray", "--", "echo", "hello"])
            .expect("ephemeral mode should parse");

        validate_runtime_mode(&cli).expect("ephemeral runtime should now be allowed");
    }

    #[test]
    fn reject_recursive_ephemeral_givetray_command() {
        let cli = parse_cli_args_from(["givetray", "--", "givetray", "-c", "default"])
            .expect("recursive ephemeral mode should still parse");

        let err = validate_runtime_mode(&cli)
            .expect_err("recursive ephemeral givetray command must be rejected");

        assert!(err.contains("recursive"));
        assert!(err.contains("givetray"));
    }

    #[test]
    fn reject_recursive_ephemeral_sudo_givetray_command() {
        let cli = parse_cli_args_from(["givetray", "--", "sudo", "givetray", "-c", "default"])
            .expect("sudo recursive ephemeral mode should still parse");

        let err = validate_runtime_mode(&cli)
            .expect_err("sudo recursive ephemeral givetray command must be rejected");

        assert!(err.contains("recursive"));
        assert!(err.contains("givetray"));
    }

    #[test]
    fn build_detached_args_for_ephemeral_mode() {
        let args = build_detached_args(&CliOptions {
            run_target: CliRunTarget::EphemeralArgv {
                argv: vec![
                    "echo".to_string(),
                    "hello world".to_string(),
                    "--flag=value".to_string(),
                ],
            },
            command_override: None,
            icon_source: None,
            log_file: None,
            mode: CliMode::Run,
        });

        assert_eq!(
            args,
            vec![
                "--".to_string(),
                "echo".to_string(),
                "hello world".to_string(),
                "--flag=value".to_string(),
            ]
        );
    }

    #[test]
    fn build_detached_args_for_persistent_mode() {
        let args = build_detached_args(&CliOptions {
            run_target: CliRunTarget::PersistentProfile {
                profile: "default".to_string(),
            },
            command_override: None,
            icon_source: None,
            log_file: None,
            mode: CliMode::Run,
        });

        assert_eq!(args, vec!["--config".to_string(), "default".to_string()]);
    }

    #[test]
    fn build_detached_args_for_persistent_mode_preserves_overrides() {
        let args = build_detached_args(&CliOptions {
            run_target: CliRunTarget::PersistentProfile {
                profile: "default".to_string(),
            },
            command_override: Some("echo detached run".to_string()),
            icon_source: Some(PathBuf::from("/tmp/icon.png")),
            log_file: Some(PathBuf::from("/tmp/givetray.log")),
            mode: CliMode::Run,
        });

        assert_eq!(
            args,
            vec![
                "--config".to_string(),
                "default".to_string(),
                "--command".to_string(),
                "echo detached run".to_string(),
                "--icon".to_string(),
                "/tmp/icon.png".to_string(),
                "--log-file".to_string(),
                "/tmp/givetray.log".to_string(),
            ]
        );
    }

    #[test]
    fn effective_command_token_supports_sudo_then_givetray() {
        let argv = vec!["/usr/bin/sudo".to_string(), "givetray".to_string()];

        assert_eq!(effective_command_token(&argv), Some("givetray"));
    }

    #[test]
    fn effective_command_token_supports_direct_givetray_path() {
        let argv = vec!["/path/to/givetray".to_string()];

        assert_eq!(effective_command_token(&argv), Some("givetray"));
    }

    #[test]
    fn effective_command_token_supports_sudo_double_dash() {
        let argv = vec!["sudo".to_string(), "--".to_string(), "givetray".to_string()];

        assert_eq!(effective_command_token(&argv), Some("givetray"));
    }

    #[test]
    fn effective_command_token_supports_sudo_user_option() {
        let argv = vec![
            "sudo".to_string(),
            "-u".to_string(),
            "root".to_string(),
            "givetray".to_string(),
        ];

        assert_eq!(effective_command_token(&argv), Some("givetray"));
    }

    #[test]
    fn effective_command_token_supports_sudo_environment_assignment() {
        let argv = vec![
            "sudo".to_string(),
            "FOO=bar".to_string(),
            "givetray".to_string(),
        ];

        assert_eq!(effective_command_token(&argv), Some("givetray"));
    }

    #[test]
    fn ephemeral_tooltip_uses_mode_label() {
        let tooltip = tray_tooltip(&CliRunTarget::EphemeralArgv {
            argv: vec!["echo".to_string(), "hello".to_string()],
        });

        assert_eq!(tooltip, "givetray (ephemeral)");
    }

    #[test]
    fn ephemeral_command_text_preserves_args() {
        let command = ephemeral_command_text(&[
            "printf".to_string(),
            "hello world".to_string(),
            "--flag=value".to_string(),
            "quote\"me".to_string(),
        ]);

        assert_eq!(command, "printf 'hello world' --flag=value 'quote\"me'");
    }

    #[test]
    fn ephemeral_command_text_round_trips_backslashes() {
        let argv = vec![
            "printf".to_string(),
            "C:\\temp\\file.txt".to_string(),
            "path with \\ slash".to_string(),
        ];

        let command = ephemeral_command_text(&argv);
        let reparsed = shell_words::split(&command).expect("generated command should parse");

        assert_eq!(reparsed, argv);
    }

    #[test]
    fn ephemeral_runtime_config_defaults_are_non_persistent() {
        let config = ephemeral_runtime_config(&["echo".to_string(), "hello world".to_string()]);

        assert_eq!(config.command, "echo 'hello world'");
        assert!(!config.autostart);
        assert_eq!(config.icon_path, None);
        assert!(!config.log_to_file);
        assert_eq!(config.log_file_path, None);
    }

    #[test]
    fn ephemeral_mode_hides_configuration_menu() {
        let startup = StartupState {
            profile_label: "ephemeral".to_string(),
            persistent_config_access: None,
            config: ephemeral_runtime_config(&["echo".to_string()]),
            log_file_path: None,
            launch_on_startup: true,
        };

        assert!(!should_expose_configuration(
            startup.persistent_config_access.as_ref()
        ));
    }

    #[test]
    fn persistent_mode_exposes_configuration_menu() {
        let startup = StartupState {
            profile_label: "default".to_string(),
            persistent_config_access: Some(PersistentConfigAccess {
                profile: "default".to_string(),
                config_path: PathBuf::from("/tmp/default.toml"),
            }),
            config: Config {
                command: DEFAULT_COMMAND.to_string(),
                autostart: false,
                icon_path: None,
                log_to_file: false,
                log_file_path: None,
            },
            log_file_path: None,
            launch_on_startup: false,
        };

        assert!(should_expose_configuration(
            startup.persistent_config_access.as_ref()
        ));
    }

    #[test]
    fn persistent_configuration_access_source_of_truth_requires_profile_and_config_path() {
        assert!(persistent_config_access(
            Some("default".to_string()),
            Some(PathBuf::from("/tmp/default.toml")),
        )
        .is_some());
        assert!(persistent_config_access(Some("default".to_string()), None).is_none());
        assert!(persistent_config_access(None, Some(PathBuf::from("/tmp/default.toml"))).is_none());
        assert!(persistent_config_access(None, None).is_none());
    }

    #[test]
    fn ephemeral_startup_state_has_no_persistent_configuration_access() {
        let cli = parse_cli_args_from(["givetray", "--", "echo", "hello"])
            .expect("ephemeral mode should parse");
        let startup = build_startup_state(&cli).expect("ephemeral startup should build");

        assert!(startup.persistent_config_access.is_none());
        assert!(!should_expose_configuration(
            startup.persistent_config_access.as_ref()
        ));
    }

    #[test]
    fn persistent_startup_state_exposes_configuration_from_source_of_truth() {
        let startup = StartupState {
            profile_label: "default".to_string(),
            persistent_config_access: Some(PersistentConfigAccess {
                profile: "default".to_string(),
                config_path: PathBuf::from("/tmp/default.toml"),
            }),
            config: Config {
                command: DEFAULT_COMMAND.to_string(),
                autostart: false,
                icon_path: None,
                log_to_file: false,
                log_file_path: None,
            },
            log_file_path: None,
            launch_on_startup: false,
        };

        assert!(should_expose_configuration(
            startup.persistent_config_access.as_ref()
        ));
    }

    #[test]
    fn startup_preflight_runs_before_detach_for_persistent_profiles() {
        let _env_lock = ENV_LOCK.lock().expect("env lock should be acquired");
        let temp_root = unique_test_dir("startup-preflight-order");
        fs::create_dir_all(&temp_root).expect("temp root should be created");
        let _env = TestEnvGuard::set(&temp_root);

        let cli = parse_cli_args_from(["givetray", "-c", "preflight", "--command", "printf ready"])
            .expect("persistent cli should parse");

        let startup = prepare_run_startup_with(&cli, |cli| {
            let profile = cli
                .persistent_profile()
                .expect("profile should still be persistent");
            let config_path = config_path_for_profile(profile).expect("config path should resolve");
            let saved = fs::read_to_string(&config_path)
                .expect("config should already be written before detach");

            assert!(saved.contains("printf ready"));

            Ok(())
        })
        .expect("startup preparation should succeed");

        assert_eq!(startup.config.command, "printf ready");
        assert!(startup.persistent_config_access.is_some());
    }

    #[test]
    fn strip_ansi_codes_removes_csi_sequences() {
        let line = "[\x1b[32mok\x1b[0m] uv                 /usr/bin/uv";

        assert_eq!(strip_ansi_codes(line), "[ok] uv                 /usr/bin/uv");
    }

    #[test]
    fn strip_ansi_codes_removes_osc_sequences() {
        let line = "prefix \x1b]0;givetray test\x07suffix";

        assert_eq!(strip_ansi_codes(line), "prefix suffix");
    }

    #[test]
    fn strip_ansi_codes_removes_st_terminated_escape_sequences() {
        let line = "left \x1b]8;;https://example.com\x1b\\label\x1b]8;;\x1b\\ right";

        assert_eq!(strip_ansi_codes(line), "left label right");
    }
}
