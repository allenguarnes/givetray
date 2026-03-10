use crate::desktop::copy_icon_to_profile;
use crate::logs::append_log;
use crate::{AppState, CliOptions, Config, APP_NAME, DEFAULT_COMMAND, DEFAULT_PROFILE};
use directories::ProjectDirs;
use gtk::prelude::*;
use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;

pub(crate) fn config_path_for_profile(profile: &str) -> Option<PathBuf> {
    ProjectDirs::from("com", APP_NAME, APP_NAME).map(|proj| {
        proj.config_dir()
            .join("configs")
            .join(format!("{}.toml", sanitize_profile_name(profile)))
    })
}

pub(crate) fn default_log_file_path(profile: &str) -> Option<PathBuf> {
    ProjectDirs::from("com", APP_NAME, APP_NAME).map(|proj| {
        proj.data_local_dir()
            .join("logs")
            .join(format!("{}.log", sanitize_profile_name(profile)))
    })
}

pub(crate) fn resolve_log_file_path(profile: &str, config: &Config) -> Option<PathBuf> {
    if !config.log_to_file {
        return None;
    }
    config
        .log_file_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| default_log_file_path(profile))
}

pub(crate) fn load_or_create_config(path: &PathBuf) -> Config {
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

pub(crate) fn save_config(path: &PathBuf, config: &Config) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("failed to create config dir: {err}"))?;
    }

    let payload = toml::to_string_pretty(config)
        .map_err(|err| format!("failed to serialize config: {err}"))?;
    fs::write(path, payload).map_err(|err| format!("failed to write config: {err}"))?;
    Ok(())
}

pub(crate) fn sanitize_profile_name(profile: &str) -> String {
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

pub(crate) fn apply_cli_overrides_to_config(
    config: &mut Config,
    cli: &CliOptions,
) -> Result<bool, String> {
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

pub(crate) fn save_configuration(
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
    let next_log_file_path = if log_to_file_enabled {
        state.saved_log_file_path.as_ref().map(PathBuf::from)
    } else {
        None
    };
    if state.log_file_path != next_log_file_path {
        state.log_file_writer = None;
    }
    state.log_file_path = next_log_file_path;

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
