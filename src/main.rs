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
use std::io::{BufRead, BufReader, Write};
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
const ICON_FILE_NAME: &str = "icon.png";
const BUNDLED_ICON_FILE_NAME: &str = "default-icon.png";

#[derive(Debug, Clone)]
struct CliOptions {
    profile: String,
    icon_source: Option<PathBuf>,
    log_file: Option<PathBuf>,
    mode: CliMode,
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
    profile: String,
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
    config_path: PathBuf,
}

fn main() {
    install_log_filters();

    let cli = parse_cli_args().unwrap_or_else(|err| {
        eprintln!("{err}");
        print_help();
        process::exit(2);
    });

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

    let config_path =
        config_path_for_profile(&cli.profile).expect("failed to resolve configuration path");
    let mut config = load_or_create_config(&config_path);

    if let Some(source_path) = cli.icon_source.as_ref() {
        match copy_icon_to_profile(source_path, &cli.profile) {
            Ok(stored_path) => {
                config.icon_path = Some(stored_path.to_string_lossy().to_string());
                save_config(&config_path, &config);
            }
            Err(err) => eprintln!("failed to set icon: {err}"),
        }
    }

    if let Some(log_file) = cli.log_file.as_ref() {
        config.log_to_file = true;
        config.log_file_path = Some(log_file.to_string_lossy().to_string());
        save_config(&config_path, &config);
    }

    if config.log_to_file && config.log_file_path.is_none() {
        if let Some(default_path) = default_log_file_path(&cli.profile) {
            config.log_file_path = Some(default_path.to_string_lossy().to_string());
            save_config(&config_path, &config);
        }
    }

    let log_file_path = resolve_log_file_path(&cli.profile, &config);

    gtk::init().expect("failed to initialize GTK");
    install_css();

    let window_icon = load_window_icon_pixbuf(&config);
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
        &cli.profile,
        &config.command,
        config.autostart,
        config.log_to_file,
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
    tray_menu
        .append(&configure_item)
        .expect("menu append failed");
    tray_menu.append(&about_item).expect("menu append failed");
    tray_menu
        .append(&PredefinedMenuItem::separator())
        .expect("menu append failed");
    tray_menu.append(&exit_item).expect("menu append failed");

    let tray_icon = load_tray_icon(&config).expect("failed to load tray icon");
    let tooltip = format!("{APP_NAME} ({})", cli.profile);
    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip(&tooltip)
        .with_icon(tray_icon)
        .build()
        .expect("failed to create tray icon");

    let state = Rc::new(RefCell::new(AppState {
        profile: cli.profile,
        command: config.command.clone(),
        saved_command: config.command.clone(),
        saved_autostart: config.autostart,
        saved_icon_path: config.icon_path.clone(),
        saved_log_to_file: config.log_to_file,
        saved_log_file_path: config.log_file_path.clone(),
        child: None,
        log_lines: VecDeque::new(),
        log_file_path,
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
        config_last: config.command,
        config_ignore: false,
        start_stop_item,
        config_path,
    }));

    {
        let (apps_toggle, system_autostart_toggle) = {
            let app = state.borrow();
            (
                app.config_applications.clone(),
                app.config_system_autostart.clone(),
            )
        };
        refresh_desktop_toggles(state.clone(), &apps_toggle, &system_autostart_toggle);
    }

    setup_config_handlers(state.clone());
    setup_logs_handlers(state.clone());
    setup_log_receiver(state.clone(), ui_rx);
    setup_menu_polling(state.clone(), ui_tx.clone());
    setup_process_watcher(state.clone(), ui_tx.clone());

    if config.autostart {
        start_command(state.clone(), ui_tx);
    }

    gtk::main();
}

fn parse_cli_args() -> Result<CliOptions, String> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_help();
        process::exit(0);
    }
    if args.iter().any(|arg| arg == "-V" || arg == "--version") {
        print_version();
        process::exit(0);
    }

    let mut mode = CliMode::Run;
    if args.first().is_some_and(|arg| arg == "desktop-file") {
        mode = CliMode::DesktopFile {
            output_dir: None,
            autostart: false,
        };
        args.remove(0);
    }

    let mut profile: Option<String> = None;
    let mut icon_source = None;
    let mut log_file = None;

    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "-c" | "--config" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| "missing value for --config".to_string())?;
                profile = Some(sanitize_profile_name(value));
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
        profile,
        icon_source,
        log_file,
        mode,
    })
}

fn print_help() {
    println!(
        "{name}\n\nUsage:\n  {name} -c PROFILE [--icon ICON_PATH] [--log-file LOG_PATH]\n  {name} desktop-file -c PROFILE [--output-dir DIR] [--autostart] [--icon ICON_PATH]\n\nOptions:\n  -c, --config PROFILE   Required profile name to load or create\n      --icon ICON_PATH   Copy icon into the selected profile and update config\n      --log-file LOG_PATH  Enable log-to-file and set output path\n      --output-dir DIR   Output directory for desktop file (desktop-file mode only)\n      --autostart        Mark desktop file as autostart and default to ~/.config/autostart\n  -h, --help             Show this help\n  -V, --version          Show version\n",
        name = APP_NAME,
    );
}

fn print_version() {
    println!("{APP_NAME} {}", env!("CARGO_PKG_VERSION"));
}

fn create_desktop_file_from_cli(
    cli: &CliOptions,
    output_dir: Option<PathBuf>,
    autostart: bool,
) -> Result<(), String> {
    let config_path = config_path_for_profile(&cli.profile)
        .ok_or_else(|| "unable to resolve configuration path".to_string())?;
    let mut config = load_or_create_config(&config_path);

    if let Some(source_path) = cli.icon_source.as_ref() {
        let copied_path = copy_icon_to_profile(source_path, &cli.profile)?;
        config.icon_path = Some(copied_path.to_string_lossy().to_string());
        save_config(&config_path, &config);
    }

    if let Some(log_file) = cli.log_file.as_ref() {
        config.log_to_file = true;
        config.log_file_path = Some(log_file.to_string_lossy().to_string());
        save_config(&config_path, &config);
    }

    let exec_path = env::current_exe()
        .map_err(|err| format!("unable to resolve executable path for desktop file: {err}"))?;
    let icon_path = resolve_icon_path_for_desktop(&config)
        .map_err(|err| format!("unable to resolve icon path for desktop file: {err}"))?;

    let desktop_path = if let Some(dir) = output_dir {
        dir.join(desktop_file_name(&cli.profile))
    } else if autostart {
        autostart_desktop_path(&cli.profile)
            .ok_or_else(|| "unable to resolve autostart desktop path".to_string())?
    } else {
        applications_desktop_path(&cli.profile)
            .ok_or_else(|| "unable to resolve Applications desktop path".to_string())?
    };

    let contents = desktop_entry(&exec_path, &icon_path, &cli.profile, autostart);
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

    let hint = gtk::Label::new(Some(
        "Use the exact terminal command you want to run for this profile.",
    ));
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

    let apps_toggle = gtk::CheckButton::with_label("Create Applications launcher (.desktop)");
    apps_toggle.set_halign(gtk::Align::Start);
    apps_toggle.set_tooltip_text(Some(
        "Creates or removes ~/.local/share/applications desktop entry for this profile.",
    ));

    let autostart_desktop_toggle =
        gtk::CheckButton::with_label("Enable desktop session autostart (.config/autostart)");
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

    let subtitle = gtk::Label::new(Some("System tray wrapper for terminal commands"));
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

    let licenses = gtk::Label::new(Some("License: MIT"));
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
    save_configuration(state.clone(), text, log_to_file_toggle.is_active());
    apply_desktop_actions(
        state.clone(),
        apps_toggle.is_active(),
        system_autostart_toggle.is_active(),
    );
    refresh_desktop_toggles(state.clone(), apps_toggle, system_autostart_toggle);
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

fn save_configuration(state: Rc<RefCell<AppState>>, text: String, log_to_file_enabled: bool) {
    let mut state = state.borrow_mut();
    state.command = text.clone();
    state.config_last = text.clone();
    state.saved_command = text.clone();
    state.saved_autostart = state.config_autostart.is_active();
    state.saved_log_to_file = log_to_file_enabled;
    if log_to_file_enabled && state.saved_log_file_path.is_none() {
        state.saved_log_file_path =
            default_log_file_path(&state.profile).map(|path| path.to_string_lossy().to_string());
    }
    state.log_file_path = if log_to_file_enabled {
        state.saved_log_file_path.as_ref().map(PathBuf::from)
    } else {
        None
    };
    save_config(
        &state.config_path,
        &Config {
            command: text,
            autostart: state.saved_autostart,
            icon_path: state.saved_icon_path.clone(),
            log_to_file: state.saved_log_to_file,
            log_file_path: state.saved_log_file_path.clone(),
        },
    );
    append_log(&mut state, "Configuration updated".to_string());
    if state.saved_log_to_file {
        if let Some(path) = state.saved_log_file_path.clone() {
            append_log(&mut state, format!("Log file enabled: {path}"));
        }
    } else {
        append_log(&mut state, "Log file output disabled".to_string());
    }
}

fn refresh_desktop_toggles(
    state: Rc<RefCell<AppState>>,
    apps_toggle: &gtk::CheckButton,
    system_autostart_toggle: &gtk::CheckButton,
) {
    let profile = state.borrow().profile.clone();
    let apps_exists = applications_desktop_path(&profile).is_some_and(|path| path.exists());
    let autostart_exists = autostart_desktop_path(&profile).is_some_and(|path| path.exists());

    {
        let mut app = state.borrow_mut();
        app.config_ignore = true;
        app.config_saved_applications = apps_exists;
        app.config_saved_system_autostart = autostart_exists;
    }

    apps_toggle.set_active(apps_exists);
    system_autostart_toggle.set_active(autostart_exists);

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

    let (profile, icon_path, config_path) = {
        let app = state.borrow();
        let config = load_or_create_config(&app.config_path);
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
        (app.profile.clone(), icon_path, app.config_path.clone())
    };

    let desktop_name = desktop_file_name(&profile);

    if let Some(path) = applications_desktop_path(&profile) {
        if apps_enabled {
            let content = desktop_entry(&exec_path, &icon_path, &profile, false);
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

    if let Some(path) = autostart_desktop_path(&profile) {
        if autostart_enabled {
            let content = desktop_entry(&exec_path, &icon_path, &profile, true);
            if let Err(err) = write_desktop_file(&path, &content) {
                append_log(
                    &mut state.borrow_mut(),
                    format!("Failed to add system auto-start entry: {err}"),
                );
            } else {
                append_log(
                    &mut state.borrow_mut(),
                    format!("System auto-start entry updated: {desktop_name}"),
                );
            }
        } else if path.exists() {
            match fs::remove_file(&path) {
                Ok(_) => append_log(
                    &mut state.borrow_mut(),
                    format!("System auto-start entry removed: {desktop_name}"),
                ),
                Err(err) => append_log(
                    &mut state.borrow_mut(),
                    format!("Failed to remove system auto-start entry: {err}"),
                ),
            }
        }
    } else {
        append_log(
            &mut state.borrow_mut(),
            "Unable to resolve system auto-start entry path".to_string(),
        );
    }

    let config = load_or_create_config(&config_path);
    save_config(&config_path, &config);
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
            save_config(path, &default);
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

fn save_config(path: &PathBuf, config: &Config) {
    if let Some(parent) = path.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            eprintln!("failed to create config dir: {err}");
            return;
        }
    }

    match toml::to_string_pretty(config) {
        Ok(payload) => {
            if let Err(err) = fs::write(path, payload) {
                eprintln!("failed to write config: {err}");
            }
        }
        Err(err) => eprintln!("failed to serialize config: {err}"),
    }
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
        "{} -c {}",
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

fn append_log(state: &mut AppState, line: String) {
    let mut rebuild = false;
    if state.log_lines.len() >= MAX_LOG_LINES {
        state.log_lines.pop_front();
        rebuild = true;
    }
    state.log_lines.push_back(line.clone());

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
        state.logs_buffer.insert(&mut end_iter, &line);
        state.logs_buffer.insert(&mut end_iter, "\n");
    }

    let mut end_iter = state.logs_buffer.end_iter();
    state
        .logs_view
        .scroll_to_iter(&mut end_iter, 0.0, false, 0.0, 0.0);

    set_logs_status(&state.logs_status_label, state.log_lines.len(), None);

    if let Some(path) = state.log_file_path.as_ref() {
        if let Err(err) = append_log_to_file(path, &line) {
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
    args.first().is_some_and(|arg| {
        Path::new(arg)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "sudo")
    })
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
