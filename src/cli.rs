use crate::config::{
    apply_cli_overrides_to_config, clear_runtime_state, config_path_for_profile,
    load_or_create_config, load_runtime_state_result, resolve_log_file_path,
    runtime_state_path_for_ephemeral, runtime_state_path_for_profile, save_config,
    RuntimeStateLoadResult,
};
use crate::{
    CliMode, CliOptions, CliRequest, CliRunTarget, Config, PersistentConfigAccess, StartupState,
    APP_NAME, BG_CHILD_ENV, MAX_COMMAND_LENGTH, MAX_PROFILE_LENGTH,
    RUNTIME_INVALID_CLEARED_MESSAGE,
};
use std::env;
use std::io;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::thread;
use std::time::Duration;

pub(crate) fn detach_to_background_if_needed(cli: &CliOptions) -> Result<(), String> {
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

pub(crate) fn build_detached_args(cli: &CliOptions) -> Vec<String> {
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

pub(crate) fn tray_tooltip(run_target: &CliRunTarget) -> String {
    format!("{APP_NAME} ({})", run_target_label(run_target))
}

pub(crate) fn run_target_label(run_target: &CliRunTarget) -> String {
    match run_target {
        CliRunTarget::PersistentProfile { profile } => profile.clone(),
        CliRunTarget::EphemeralArgv { .. } => "ephemeral".to_string(),
    }
}

pub(crate) fn persistent_config_access(
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

pub(crate) fn should_expose_configuration(access: Option<&PersistentConfigAccess>) -> bool {
    access.is_some()
}

pub(crate) fn ephemeral_command_text(argv: &[String]) -> String {
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

pub(crate) fn ephemeral_runtime_config(argv: &[String]) -> Config {
    Config {
        command: ephemeral_command_text(argv),
        autostart: false,
        icon_path: None,
        log_to_file: false,
        log_file_path: None,
    }
}

pub(crate) fn build_startup_state(cli: &CliOptions) -> Result<StartupState, String> {
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

            let runtime_state_path = runtime_state_path_for_profile(profile);
            let (runtime_ownership, startup_message) =
                load_startup_runtime_state(runtime_state_path.as_deref());

            Ok(StartupState {
                profile_label: profile.clone(),
                persistent_config_access: persistent_config_access(
                    Some(profile.clone()),
                    Some(config_path),
                ),
                log_file_path: resolve_log_file_path(profile, &config),
                launch_on_startup: config.autostart,
                config,
                runtime_state_path,
                runtime_ownership,
                restored_running: false,
                owns_profile_lock: true,
                profile_lock: None,
                startup_message,
            })
        }
        CliRunTarget::EphemeralArgv { argv } => {
            let runtime_state_path = runtime_state_path_for_ephemeral();
            let (runtime_ownership, startup_message) =
                load_startup_runtime_state(runtime_state_path.as_deref());

            Ok(StartupState {
                profile_label: run_target_label(&cli.run_target),
                persistent_config_access: None,
                config: ephemeral_runtime_config(argv),
                log_file_path: None,
                launch_on_startup: true,
                runtime_state_path,
                runtime_ownership,
                restored_running: false,
                owns_profile_lock: false,
                profile_lock: None,
                startup_message,
            })
        }
    }
}

fn load_startup_runtime_state(
    runtime_state_path: Option<&Path>,
) -> (Option<crate::RuntimeOwnershipState>, Option<String>) {
    let Some(path) = runtime_state_path else {
        return (None, None);
    };

    match load_runtime_state_result(path) {
        RuntimeStateLoadResult::Loaded(state) => (Some(state), None),
        RuntimeStateLoadResult::Missing => (None, None),
        RuntimeStateLoadResult::Invalid => {
            if let Err(err) = clear_runtime_state(path) {
                eprintln!("failed to clear invalid runtime state: {err}");
            }
            (None, Some(RUNTIME_INVALID_CLEARED_MESSAGE.to_string()))
        }
    }
}

pub(crate) fn prepare_run_startup(cli: &CliOptions) -> Result<StartupState, String> {
    prepare_run_startup_with(cli, detach_to_background_if_needed)
}

pub(crate) fn prepare_run_startup_with<F>(
    cli: &CliOptions,
    detach: F,
) -> Result<StartupState, String>
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

pub(crate) fn command_file_name(arg: &str) -> Option<&str> {
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

pub(crate) fn effective_command_token(argv: &[String]) -> Option<&str> {
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

pub(crate) fn parse_cli_args() -> Result<CliOptions, String> {
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

pub(crate) fn parse_cli_request_from<I, S>(args: I) -> Result<CliRequest, String>
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

pub(crate) fn parse_cli_args_from<I, S>(args: I) -> Result<CliOptions, String>
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

pub(crate) fn validate_runtime_mode(cli: &CliOptions) -> Result<(), String> {
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

pub(crate) fn help_text() -> String {
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
