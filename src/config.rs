use crate::desktop::copy_icon_to_profile;
use crate::logs::{append_log, profile_lock_action_blocked_message};
use crate::{
    AppState, CliOptions, Config, RuntimeOwnershipState, APP_NAME, DEFAULT_COMMAND,
    DEFAULT_PROFILE, MAX_COMMAND_LENGTH, MAX_DISPLAY_NAME_LENGTH, RUNTIME_INVALID_CLEARED_MESSAGE,
    RUNTIME_RESTORED_MESSAGE, RUNTIME_STALE_CLEARED_MESSAGE,
};
use directories::ProjectDirs;
use std::cell::RefCell;
use std::fs;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub(crate) struct ProfileLockHandle {
    // Keep the descriptor open so the advisory flock remains held.
    pub(crate) lock_file: fs::File,
}

impl Drop for ProfileLockHandle {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.lock_file.as_raw_fd(), libc::LOCK_UN) };
    }
}

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
        name: None,
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

pub(crate) fn save_config(path: &Path, config: &Config) -> Result<(), String> {
    let payload = toml::to_string_pretty(config)
        .map_err(|err| format!("failed to serialize config: {err}"))?;
    atomic_write(path, &payload).map_err(|err| format!("failed to write config: {err}"))
}

pub(crate) fn atomic_write(path: &Path, contents: &str) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Err("file has no parent directory".to_string());
    };

    fs::create_dir_all(parent).map_err(|err| format!("failed to create directory: {err}"))?;
    // Durability note: we sync only `parent` after rename. If create_dir_all had to
    // create missing ancestors on first run, those ancestor directory entries may not
    // be individually synced to their own parents before a crash.
    let existing_permissions = path.metadata().ok().map(|metadata| metadata.permissions());
    // Permission note: this preserves standard file permission bits via
    // std::fs::Permissions, but not ownership, ACLs, or xattrs.

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temp_path = parent.join(format!(".tmp.{file_name}.{}.{}", std::process::id(), nonce));

    let write_result = (|| {
        let mut file = fs::File::create(&temp_path)
            .map_err(|err| format!("failed to create temp file: {err}"))?;
        if let Some(permissions) = existing_permissions.as_ref() {
            file.set_permissions(permissions.clone())
                .map_err(|err| format!("failed to set temp file permissions: {err}"))?;
        }
        file.write_all(contents.as_bytes())
            .map_err(|err| format!("failed to write temp file: {err}"))?;
        file.sync_all()
            .map_err(|err| format!("failed to sync temp file: {err}"))
    })();

    if let Err(err) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }

    fs::rename(&temp_path, path).map_err(|err| {
        let _ = fs::remove_file(&temp_path);
        format!("failed to rename temp file: {err}")
    })?;

    sync_directory(parent)
}

fn sync_directory(path: &Path) -> Result<(), String> {
    let directory =
        fs::File::open(path).map_err(|err| format!("failed to open directory for sync: {err}"))?;
    directory
        .sync_all()
        .map_err(|err| format!("failed to sync directory: {err}"))
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

pub(crate) fn validate_saved_command_text(raw: &str) -> Result<String, String> {
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
        Err(err) => Err(format!("invalid command: {err}")),
    }
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

    if let Some(name) = cli.name_override.as_ref() {
        if config.name.as_deref() != Some(name.as_str()) {
            config.name = Some(name.clone());
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
    name_text: String,
    autostart_enabled: bool,
    log_to_file_enabled: bool,
) -> bool {
    let mut state = state.borrow_mut();
    if !can_save_profile_configuration(
        state.persistent_config_access.as_ref(),
        state.owns_profile_lock,
    ) {
        append_log(&mut state, profile_lock_action_blocked_message());
        return false;
    }

    let Some(access) = state.persistent_config_access.clone() else {
        append_log(
            &mut state,
            "configuration save is unavailable in ephemeral mode".to_string(),
        );
        return false;
    };
    let computed = match build_saved_configuration(
        &access.profile,
        state.saved_icon_path.clone(),
        state.saved_log_file_path.clone(),
        &text,
        &name_text,
        autostart_enabled,
        log_to_file_enabled,
    ) {
        Ok(computed) => computed,
        Err(err) => {
            append_log(&mut state, err);
            return false;
        }
    };

    if let Err(err) = save_config(&access.config_path, &computed.config) {
        append_log(&mut state, format!("Failed to save configuration: {err}"));
        return false;
    }

    state.command = computed.command.clone();
    state.config_last = computed.command.clone();
    state.saved_command = computed.command;
    state.saved_autostart = computed.autostart;
    state.saved_name = computed.name;
    state.display_name = state.saved_name.clone();
    state.saved_log_to_file = computed.log_to_file;
    state.saved_log_file_path = computed.log_file_path;
    let next_log_file_path = if computed.log_to_file {
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

#[derive(Debug)]
pub(crate) struct SavedConfiguration {
    pub(crate) command: String,
    pub(crate) autostart: bool,
    pub(crate) name: Option<String>,
    pub(crate) log_to_file: bool,
    pub(crate) log_file_path: Option<String>,
    pub(crate) config: Config,
}

pub(crate) fn build_saved_configuration(
    profile: &str,
    icon_path: Option<String>,
    saved_log_file_path: Option<String>,
    command_text: &str,
    name_text: &str,
    autostart_enabled: bool,
    log_to_file_enabled: bool,
) -> Result<SavedConfiguration, String> {
    let command = validate_saved_command_text(command_text)?;
    let name = parse_optional_display_name(name_text)?;

    let mut log_file_path = saved_log_file_path;
    if log_to_file_enabled && log_file_path.is_none() {
        log_file_path =
            default_log_file_path(profile).map(|path| path.to_string_lossy().to_string());
    }

    Ok(SavedConfiguration {
        config: Config {
            command: command.clone(),
            autostart: autostart_enabled,
            name: name.clone(),
            icon_path,
            log_to_file: log_to_file_enabled,
            log_file_path: log_file_path.clone(),
        },
        command,
        autostart: autostart_enabled,
        name,
        log_to_file: log_to_file_enabled,
        log_file_path,
    })
}

pub(crate) fn normalize_display_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    if name.is_empty() {
        return None;
    }
    if name.len() > MAX_DISPLAY_NAME_LENGTH {
        return None;
    }
    if name.chars().any(|ch| ch.is_control()) {
        return None;
    }

    Some(name.to_string())
}

pub(crate) fn normalize_saved_display_name(raw: Option<&str>) -> Option<String> {
    raw.and_then(normalize_display_name)
}

pub(crate) fn parse_optional_display_name(raw: &str) -> Result<Option<String>, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Ok(None);
    }
    if name.len() > MAX_DISPLAY_NAME_LENGTH {
        return Err(format!(
            "name is too long (max {MAX_DISPLAY_NAME_LENGTH} characters)"
        ));
    }
    if name.chars().any(|ch| ch.is_control()) {
        return Err("name cannot contain control characters".to_string());
    }

    Ok(Some(name.to_string()))
}

pub(crate) fn validate_display_name_override(raw: &str) -> Result<String, String> {
    match parse_optional_display_name(raw)? {
        Some(name) => Ok(name),
        None => Err("name cannot be empty".to_string()),
    }
}

pub(crate) fn can_save_profile_configuration(
    persistent_config_access: Option<&crate::PersistentConfigAccess>,
    owns_profile_lock: bool,
) -> bool {
    if persistent_config_access.is_none() {
        return false;
    }

    owns_profile_lock
}

pub(crate) fn runtime_state_path_for_profile(profile: &str) -> Option<PathBuf> {
    ProjectDirs::from("com", APP_NAME, APP_NAME).map(|proj| {
        proj.data_local_dir()
            .join("runtime")
            .join("profiles")
            .join(format!("{}.toml", sanitize_profile_name(profile)))
    })
}

pub(crate) fn profile_lock_path_for_profile(profile: &str) -> Option<PathBuf> {
    ProjectDirs::from("com", APP_NAME, APP_NAME).map(|proj| {
        proj.data_local_dir()
            .join("runtime")
            .join("profiles")
            .join(format!("{}.lock", sanitize_profile_name(profile)))
    })
}

pub(crate) fn acquire_profile_lock(path: &Path) -> Result<ProfileLockHandle, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("failed to create lock dir: {err}"))?;
    }

    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|err| format!("failed to open profile lock file: {err}"))?;

    let status = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if status != 0 {
        let err = std::io::Error::last_os_error();
        let raw_os_error = err.raw_os_error();
        if raw_os_error == Some(libc::EWOULDBLOCK) || raw_os_error == Some(libc::EAGAIN) {
            return Err("profile lock already held by another process".to_string());
        }
        return Err(format!("failed to acquire profile lock: {err}"));
    }

    Ok(ProfileLockHandle { lock_file: file })
}

pub(crate) fn runtime_state_path_for_ephemeral() -> Option<PathBuf> {
    ProjectDirs::from("com", APP_NAME, APP_NAME)
        .map(|proj| proj.data_local_dir().join("runtime").join("ephemeral.toml"))
}

pub(crate) enum RuntimeStateLoadResult {
    Loaded(RuntimeOwnershipState),
    Missing,
    Invalid,
}

pub(crate) fn load_runtime_state_result(path: &Path) -> RuntimeStateLoadResult {
    let content = match fs::read_to_string(path) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return RuntimeStateLoadResult::Missing;
        }
        Err(err) => {
            eprintln!(
                "failed to read runtime state at {}: {}",
                path.display(),
                err
            );
            return RuntimeStateLoadResult::Invalid;
        }
    };

    match toml::from_str(&content) {
        Ok(state) => RuntimeStateLoadResult::Loaded(state),
        Err(err) => {
            eprintln!(
                "failed to parse runtime state at {}: {}",
                path.display(),
                err
            );
            RuntimeStateLoadResult::Invalid
        }
    }
}

#[cfg(test)]
pub(crate) fn load_runtime_state(path: &Path) -> Option<RuntimeOwnershipState> {
    match load_runtime_state_result(path) {
        RuntimeStateLoadResult::Loaded(state) => Some(state),
        RuntimeStateLoadResult::Missing | RuntimeStateLoadResult::Invalid => None,
    }
}

pub(crate) fn save_runtime_state(path: &Path, state: &RuntimeOwnershipState) -> Result<(), String> {
    let payload = toml::to_string_pretty(state)
        .map_err(|err| format!("failed to serialize runtime state: {err}"))?;
    atomic_write(path, &payload).map_err(|err| format!("failed to write runtime state: {err}"))
}

pub(crate) fn clear_runtime_state(path: &Path) -> Result<(), String> {
    if path.exists() {
        fs::remove_file(path).map_err(|err| format!("failed to remove runtime state: {err}"))?;
    }
    Ok(())
}

pub fn reconcile_startup_runtime_state(mut startup: crate::StartupState) -> crate::StartupState {
    let Some(runtime_path) = &startup.runtime_state_path else {
        return startup;
    };

    let Some(runtime_ownership) = &startup.runtime_ownership else {
        return startup;
    };

    let result = crate::process::reconcile_runtime_state(
        runtime_ownership,
        crate::process::is_process_group_alive,
    );

    match result {
        crate::process::RuntimeReconcileResult::RestoreRunning => {
            startup.restored_running = true;
            startup.startup_message = Some(RUNTIME_RESTORED_MESSAGE.to_string());
        }
        crate::process::RuntimeReconcileResult::ClearStale => {
            if let Err(err) = clear_runtime_state(runtime_path) {
                eprintln!("failed to clear stale runtime state: {err}");
            }
            startup.runtime_ownership = None;
            startup.restored_running = false;
            startup.startup_message = Some(RUNTIME_STALE_CLEARED_MESSAGE.to_string());
        }
        crate::process::RuntimeReconcileResult::IgnoreInvalid => {
            if let Err(err) = clear_runtime_state(runtime_path) {
                eprintln!("failed to clear invalid runtime state: {err}");
            }
            startup.runtime_ownership = None;
            startup.restored_running = false;
            startup.startup_message = Some(RUNTIME_INVALID_CLEARED_MESSAGE.to_string());
        }
    }

    startup
}
