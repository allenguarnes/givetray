use crate::config::{
    apply_cli_overrides_to_config, atomic_write, config_path_for_profile, load_or_create_config,
    sanitize_profile_name, save_config,
};
use crate::logs::append_log;
use crate::{AppState, CliOptions, Config, APP_NAME, BUNDLED_ICON_FILE_NAME, ICON_FILE_NAME};
use directories::{BaseDirs, ProjectDirs};
use gtk::gdk_pixbuf::Pixbuf;
use std::cell::RefCell;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tray_icon::Icon;

pub(crate) fn create_desktop_file_from_cli(
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

pub(crate) fn apply_desktop_actions(
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

pub(crate) fn applications_desktop_path(profile: &str) -> Option<PathBuf> {
    BaseDirs::new().map(|dirs| {
        dirs.data_local_dir()
            .join("applications")
            .join(desktop_file_name(profile))
    })
}

pub(crate) fn autostart_desktop_path(profile: &str) -> Option<PathBuf> {
    BaseDirs::new().map(|dirs| {
        dirs.config_dir()
            .join("autostart")
            .join(desktop_file_name(profile))
    })
}

pub(crate) fn profile_icon_path(profile: &str) -> Option<PathBuf> {
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

pub(crate) fn copy_icon_to_profile(source_path: &Path, profile: &str) -> Result<PathBuf, String> {
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

pub(crate) fn resolve_icon_path_for_desktop(config: &Config) -> Result<PathBuf, std::io::Error> {
    if let Some(path) = config.icon_path.as_ref() {
        let icon = PathBuf::from(path);
        if icon.exists() {
            return Ok(icon);
        }
    }
    ensure_bundled_icon_file()
}

pub(crate) fn load_window_icon_pixbuf(config: &Config) -> Option<Pixbuf> {
    let icon_path = resolve_icon_path_for_desktop(config).ok()?;
    Pixbuf::from_file(icon_path).ok()
}

pub(crate) fn load_tray_icon(config: &Config) -> Result<Icon, Box<dyn std::error::Error>> {
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

pub(crate) fn desktop_file_name(profile: &str) -> String {
    format!("{APP_NAME}_{}.desktop", sanitize_profile_name(profile))
}

pub(crate) fn desktop_entry(
    exec_path: &Path,
    icon_path: &Path,
    profile: &str,
    autostart: bool,
) -> String {
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

fn write_desktop_file(path: &Path, contents: &str) -> Result<(), String> {
    atomic_write(path, contents)
}
