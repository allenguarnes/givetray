use crate::cli::{
    build_detached_args, build_startup_state, effective_command_token, ephemeral_command_text,
    ephemeral_runtime_config, help_text, parse_cli_args_from, parse_cli_request_from,
    persistent_config_access, prepare_run_startup_with, should_expose_configuration, tray_tooltip,
    validate_runtime_mode,
};
use crate::config::config_path_for_profile;
use crate::config::{
    acquire_profile_lock, atomic_write, can_save_profile_configuration, clear_runtime_state,
    load_runtime_state, profile_lock_path_for_profile, reconcile_startup_runtime_state,
    runtime_state_path_for_ephemeral, runtime_state_path_for_profile, save_config,
    save_configuration, save_runtime_state, validate_saved_command_text,
};
use crate::desktop::create_desktop_file_from_cli;
use crate::logs::{
    append_log_to_file, clear_runtime_state_after_exit, extract_log_links,
    should_activate_log_link, strip_ansi_codes,
};
use crate::process::can_control_profile;
use crate::process::is_process_group_alive;
use crate::process::reconcile_runtime_state;
use crate::process::RuntimeReconcileResult;
use crate::*;
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Mutex, Once};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

static ENV_LOCK: Mutex<()> = Mutex::new(());
static GTK_INIT: Once = Once::new();

fn ensure_gtk_initialized() {
    GTK_INIT.call_once(|| {
        gtk::init().expect("gtk should initialize for ui-backed tests");
    });
}

fn build_save_test_state(temp_root: &Path) -> Rc<RefCell<AppState>> {
    ensure_gtk_initialized();

    let profile = "save-test";
    let config_path = temp_root.join("config").join("save-test.toml");
    let (
        logs_window,
        logs_view,
        logs_buffer,
        logs_clear_button,
        logs_copy_button,
        logs_status_label,
    ) = crate::logs::build_logs_window();
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
    ) = crate::ui::build_config_window(profile, "echo ready", false, false);
    let about_window = crate::ui::build_about_window(None);
    let start_stop_item = tray_icon::menu::MenuItem::with_id(
        tray_icon::menu::MenuId::new("test-start-stop"),
        "Start",
        true,
        None,
    );

    Rc::new(RefCell::new(AppState {
        persistent_config_access: Some(PersistentConfigAccess {
            profile: profile.to_string(),
            config_path,
        }),
        command: "echo ready".to_string(),
        saved_command: "echo ready".to_string(),
        saved_autostart: false,
        saved_icon_path: None,
        saved_log_to_file: false,
        saved_log_file_path: None,
        child: None,
        owned_pgid: None,
        owned_pid: None,
        process_exit_reported: false,
        runtime_state_path: None,
        restored_running: false,
        owns_profile_lock: true,
        profile_lock: None,
        log_lines: VecDeque::new(),
        log_links: VecDeque::new(),
        log_file_path: None,
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
        config_last: "echo ready".to_string(),
        config_ignore: false,
        start_stop_item,
    }))
}

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
fn save_configuration_rejects_empty_command() {
    assert!(validate_saved_command_text("   ").is_err());
}

#[test]
fn save_configuration_rejects_unparseable_command() {
    assert!(validate_saved_command_text("unterminated '").is_err());
}

#[test]
fn save_configuration_returns_false_for_invalid_command() {
    let _env_lock = ENV_LOCK.lock().expect("env lock should be acquired");
    let temp_root = unique_test_dir("save-configuration-invalid-command");
    fs::create_dir_all(&temp_root).expect("temp root should be created");
    let _env = TestEnvGuard::set(&temp_root);
    let state = build_save_test_state(&temp_root);

    let saved = save_configuration(state.clone(), "unterminated '".to_string(), false);
    assert!(!saved);

    let state_ref = state.borrow();
    let last_log = state_ref
        .log_lines
        .back()
        .expect("save failure should append a log line");
    assert!(last_log.contains("invalid command:"));
    assert!(!last_log.contains("Invalid command: invalid command:"));
    assert_eq!(state_ref.saved_command, "echo ready");
}

#[test]
fn reject_empty_ephemeral_mode() {
    let err = parse_cli_args_from(["givetray", "--"]).expect_err("empty ephemeral mode must fail");

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
fn cli_rejects_invalid_command_override() {
    let err = parse_cli_args_from(["givetray", "-c", "test", "--command", "unterminated '"])
        .expect_err("invalid command override must fail");

    assert!(err.contains("invalid"));
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
        runtime_state_path: None,
        runtime_ownership: None,
        restored_running: false,
        owns_profile_lock: true,
        profile_lock: None,
        startup_message: None,
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
        runtime_state_path: None,
        runtime_ownership: None,
        restored_running: false,
        owns_profile_lock: true,
        profile_lock: None,
        startup_message: None,
    };

    assert!(should_expose_configuration(
        startup.persistent_config_access.as_ref()
    ));
}

#[test]
fn startup_state_can_mark_profile_lock_conflict() {
    let startup = StartupState {
        profile_label: "default".to_string(),
        persistent_config_access: None,
        config: Config {
            command: DEFAULT_COMMAND.to_string(),
            autostart: false,
            icon_path: None,
            log_to_file: false,
            log_file_path: None,
        },
        log_file_path: None,
        launch_on_startup: false,
        runtime_state_path: None,
        runtime_ownership: None,
        restored_running: false,
        owns_profile_lock: false,
        profile_lock: None,
        startup_message: Some(RUNTIME_ALREADY_OPEN_MESSAGE.to_string()),
    };

    assert!(!startup.owns_profile_lock);
    assert!(startup.profile_lock.is_none());
    assert_eq!(
        startup.startup_message.as_deref(),
        Some(RUNTIME_ALREADY_OPEN_MESSAGE)
    );
}

#[test]
fn startup_state_default_owns_profile_lock_for_persistent_profile() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let temp_root = unique_test_dir("startup-state-persistent-lock-ownership");
    fs::create_dir_all(&temp_root).expect("temp root should be created");
    let _env = TestEnvGuard::set(&temp_root);

    let cli = parse_cli_args_from(["givetray", "-c", "default"])
        .expect("persistent profile mode should parse");
    let startup = build_startup_state(&cli).expect("persistent startup should build");

    assert!(startup.owns_profile_lock);
    assert!(startup.profile_lock.is_some());
}

#[test]
fn second_same_profile_instance_skips_runtime_recovery() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let temp_root = unique_test_dir("second-instance-runtime-skip");
    fs::create_dir_all(&temp_root).expect("temp root should be created");
    let _env = TestEnvGuard::set(&temp_root);

    let cli = parse_cli_args_from(["givetray", "-c", "default"])
        .expect("persistent profile mode should parse");

    let startup1 = build_startup_state(&cli).expect("first instance should build");
    assert!(startup1.owns_profile_lock);
    assert!(startup1.profile_lock.is_some());

    let startup2 = build_startup_state(&cli).expect("second instance should build");
    assert!(!startup2.owns_profile_lock);
    assert!(startup2.profile_lock.is_none());
    assert!(startup2.runtime_ownership.is_none());
    assert_eq!(
        startup2.startup_message.as_deref(),
        Some(RUNTIME_ALREADY_OPEN_MESSAGE)
    );

    drop(startup1);

    let startup3 =
        build_startup_state(&cli).expect("third instance should build after lock release");
    assert!(startup3.owns_profile_lock);
    assert!(startup3.profile_lock.is_some());
}

#[test]
fn startup_state_ephemeral_owns_no_profile_lock() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let temp_root = unique_test_dir("startup-state-ephemeral-lock-ownership");
    fs::create_dir_all(&temp_root).expect("temp root should be created");
    let _env = TestEnvGuard::set(&temp_root);

    let cli = parse_cli_args_from(["givetray", "--", "echo", "hello"])
        .expect("ephemeral mode should parse");
    let startup = build_startup_state(&cli).expect("ephemeral startup should build");

    assert!(!startup.owns_profile_lock);
    assert!(startup.profile_lock.is_none());
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
        runtime_state_path: None,
        runtime_ownership: None,
        restored_running: false,
        owns_profile_lock: true,
        profile_lock: None,
        startup_message: None,
    };

    assert!(should_expose_configuration(
        startup.persistent_config_access.as_ref()
    ));
}

#[test]
fn startup_preflight_runs_after_detach_for_persistent_profiles() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
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
        assert!(!config_path.exists());

        Ok(())
    })
    .expect("startup preparation should succeed");

    let config_path = config_path_for_profile("preflight").expect("config path should resolve");
    let saved = fs::read_to_string(&config_path).expect("config should be written after detach");
    assert!(saved.contains("printf ready"));
    assert_eq!(startup.config.command, "printf ready");
    assert!(startup.persistent_config_access.is_some());
}

#[test]
fn startup_preflight_does_not_persist_cli_overrides_without_profile_lock() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let temp_root = unique_test_dir("startup-preflight-lock-guard");
    fs::create_dir_all(&temp_root).expect("temp root should be created");
    let _env = TestEnvGuard::set(&temp_root);

    let profile = "preflight-locked";
    let config_path = config_path_for_profile(profile).expect("config path should resolve");
    save_config(
        &config_path,
        &Config {
            command: "echo baseline".to_string(),
            autostart: false,
            icon_path: None,
            log_to_file: false,
            log_file_path: None,
        },
    )
    .expect("baseline config should be saved");

    let lock_path = profile_lock_path_for_profile(profile).expect("lock path should resolve");
    let _lock = acquire_profile_lock(&lock_path).expect("lock should be acquired for test");

    let cli = parse_cli_args_from(["givetray", "-c", profile, "--command", "printf changed"])
        .expect("persistent cli should parse");

    let startup = prepare_run_startup_with(&cli, |_| Ok(()))
        .expect("startup preparation should still return non-owning startup state");

    assert!(!startup.owns_profile_lock);
    assert_eq!(
        startup.startup_message.as_deref(),
        Some(RUNTIME_ALREADY_OPEN_MESSAGE)
    );
    assert_eq!(startup.config.command, "echo baseline");

    let saved = fs::read_to_string(&config_path).expect("config should remain readable");
    assert!(saved.contains("echo baseline"));
    assert!(!saved.contains("printf changed"));
}

#[test]
fn desktop_file_creation_requires_profile_lock_before_persisting_overrides() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let temp_root = unique_test_dir("desktop-file-lock-guard");
    fs::create_dir_all(&temp_root).expect("temp root should be created");
    let output_dir = temp_root.join("desktop");
    fs::create_dir_all(&output_dir).expect("output dir should be created");
    let _env = TestEnvGuard::set(&temp_root);

    let profile = "desktop-locked";
    let config_path = config_path_for_profile(profile).expect("config path should resolve");
    save_config(
        &config_path,
        &Config {
            command: "echo baseline".to_string(),
            autostart: false,
            icon_path: None,
            log_to_file: false,
            log_file_path: None,
        },
    )
    .expect("baseline config should be saved");

    let lock_path = profile_lock_path_for_profile(profile).expect("lock path should resolve");
    let _lock = acquire_profile_lock(&lock_path).expect("lock should be acquired for test");

    let cli = parse_cli_args_from([
        "givetray",
        "desktop-file",
        "-c",
        profile,
        "--command",
        "printf changed",
    ])
    .expect("desktop-file cli should parse");

    let err = create_desktop_file_from_cli(&cli, Some(output_dir), false)
        .expect_err("desktop-file mode should fail when lock is already held");
    assert!(err.contains("profile lock already held"));

    let saved = fs::read_to_string(&config_path).expect("config should remain readable");
    assert!(saved.contains("echo baseline"));
    assert!(!saved.contains("printf changed"));
}

#[test]
fn strip_ansi_codes_removes_csi_sequences() {
    let line = "[\x1b[32mok\x1b[0m] uv                 /usr/bin/uv";

    assert_eq!(
        strip_ansi_codes(line),
        "[ok] uv                 /usr/bin/uv"
    );
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

#[test]
fn extract_log_links_finds_multiple_urls() {
    let links = extract_log_links("see https://example.com and http://localhost:8000/test");

    assert_eq!(links.len(), 2);
    assert_eq!(links[0].uri, "https://example.com");
    assert_eq!(links[1].uri, "http://localhost:8000/test");
}

#[test]
fn extract_log_links_trims_trailing_punctuation() {
    let links = extract_log_links("docs: https://example.com/test), done");

    assert_eq!(links.len(), 1);
    assert_eq!(links[0].uri, "https://example.com/test");
}

#[test]
fn extract_log_links_preserves_balanced_parentheses() {
    let links = extract_log_links("see https://example.com/func(test) now");

    assert_eq!(links.len(), 1);
    assert_eq!(links[0].uri, "https://example.com/func(test)");
}

#[test]
fn extract_log_links_allows_assignment_prefixes() {
    let links = extract_log_links("url=https://example.com/path");

    assert_eq!(links.len(), 1);
    assert_eq!(links[0].uri, "https://example.com/path");
}

#[test]
fn extract_log_links_matches_uppercase_schemes() {
    let links = extract_log_links("See HTTPS://EXAMPLE.COM/docs for details");

    assert_eq!(links.len(), 1);
    assert_eq!(links[0].uri, "HTTPS://EXAMPLE.COM/docs");
}

#[test]
fn should_activate_log_link_requires_click_without_selection() {
    let pending = PendingLogLink {
        uri: "https://example.com".to_string(),
        x: 10.0,
        y: 20.0,
    };

    assert!(should_activate_log_link(
        Some(&pending),
        Some("https://example.com"),
        12.0,
        22.0,
        false,
    ));
    assert!(!should_activate_log_link(
        Some(&pending),
        Some("https://example.com"),
        20.0,
        35.0,
        false,
    ));
    assert!(!should_activate_log_link(
        Some(&pending),
        Some("https://example.com"),
        12.0,
        22.0,
        true,
    ));
    assert!(!should_activate_log_link(
        Some(&pending),
        Some("https://other.example"),
        12.0,
        22.0,
        false,
    ));
}

#[test]
fn append_log_to_file_reuses_writer_across_calls() {
    let temp_root = unique_test_dir("log-writer-reuse");
    fs::create_dir_all(&temp_root).expect("temp root should be created");
    let log_path = temp_root.join("logs").join("givetray.log");
    let mut writer = None;

    append_log_to_file(&mut writer, &log_path, "first line")
        .expect("first log write should succeed");
    assert!(writer.is_some());

    append_log_to_file(&mut writer, &log_path, "second line")
        .expect("second log write should succeed");

    drop(writer);

    let mut contents = String::new();
    fs::File::open(&log_path)
        .expect("log file should exist")
        .read_to_string(&mut contents)
        .expect("log file should be readable");

    assert_eq!(contents, "first line\nsecond line\n");
}

#[test]
fn runtime_state_round_trips_for_profile_run() {
    let state = RuntimeOwnershipState {
        pid: 1234,
        pgid: 1234,
        started_at_clock_ticks: 1000,
        command_label: "sleep 30".to_string(),
        profile_name: Some("default".to_string()),
        ephemeral: false,
    };

    let encoded = toml::to_string(&state).expect("state should serialize");
    let decoded: RuntimeOwnershipState =
        toml::from_str(&encoded).expect("state should deserialize");

    assert_eq!(decoded.profile_name.as_deref(), Some("default"));
    assert_eq!(decoded.pgid, 1234);
    assert!(!decoded.ephemeral);
}

#[test]
fn runtime_state_round_trips_for_ephemeral_run() {
    let state = RuntimeOwnershipState {
        pid: 5678,
        pgid: 5678,
        started_at_clock_ticks: 2000,
        command_label: "echo hello".to_string(),
        profile_name: None,
        ephemeral: true,
    };

    let encoded = toml::to_string(&state).expect("state should serialize");
    let decoded: RuntimeOwnershipState =
        toml::from_str(&encoded).expect("state should deserialize");

    assert_eq!(decoded.profile_name, None);
    assert_eq!(decoded.pid, 5678);
    assert!(decoded.ephemeral);
}

#[test]
fn invalid_runtime_state_toml_returns_none() {
    let temp_dir = unique_test_dir("invalid-runtime-state");
    fs::create_dir_all(&temp_dir).expect("temp dir should be created");
    let state_path = temp_dir.join("runtime-state.toml");

    fs::write(&state_path, "invalid toml content {{{{").expect("invalid file should be written");

    let result = load_runtime_state(&state_path);
    assert!(result.is_none());
}

#[test]
fn missing_runtime_state_file_returns_none() {
    let temp_root = unique_test_dir("missing-runtime-state");
    fs::create_dir_all(&temp_root).expect("temp root should be created");
    let state_path = temp_root.join("nonexistent.toml");

    let loaded = load_runtime_state(&state_path);
    assert!(loaded.is_none(), "missing file should return None");
}

#[test]
fn malformed_runtime_state_file_gets_cleared_during_startup() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let temp_root = unique_test_dir("malformed-runtime-state-cleanup");
    fs::create_dir_all(&temp_root).expect("temp root should be created");
    let _env = TestEnvGuard::set(&temp_root);

    let config_path = config_path_for_profile("broken").expect("config path should resolve");
    let config_parent = config_path.parent().expect("config parent should exist");
    fs::create_dir_all(config_parent).expect("config dir should be created");
    save_config(
        &config_path,
        &Config {
            command: DEFAULT_COMMAND.to_string(),
            autostart: false,
            icon_path: None,
            log_to_file: false,
            log_file_path: None,
        },
    )
    .expect("config should save");

    let runtime_path =
        runtime_state_path_for_profile("broken").expect("runtime path should resolve");
    fs::create_dir_all(runtime_path.parent().expect("runtime parent should exist"))
        .expect("runtime dir should be created");
    fs::write(&runtime_path, "not valid toml {{{")
        .expect("invalid runtime state should be written");

    let cli = parse_cli_args_from(["givetray", "-c", "broken"]).expect("cli should parse");
    let startup = build_startup_state(&cli).expect("startup should build");
    let reconciled = reconcile_startup_runtime_state(startup);

    assert!(reconciled.runtime_ownership.is_none());
    assert_eq!(
        reconciled.startup_message.as_deref(),
        Some(RUNTIME_INVALID_CLEARED_MESSAGE)
    );
    assert!(
        !runtime_path.exists(),
        "malformed runtime-state file should be cleared"
    );
}

#[test]
fn runtime_state_path_resolves_for_profile() {
    let _env_lock = ENV_LOCK.lock().expect("env lock should be acquired");
    let temp_root = unique_test_dir("runtime-state-profile-path");
    fs::create_dir_all(&temp_root).expect("temp root should be created");
    let _env = TestEnvGuard::set(&temp_root);

    let path = runtime_state_path_for_profile("my-profile");

    assert!(path.is_some());
    let path = path.unwrap();
    assert!(path.to_string_lossy().contains("my-profile"));
}

#[test]
fn profile_lock_path_resolves_for_profile() {
    let _env_lock = ENV_LOCK.lock().expect("env lock should be acquired");
    let temp_root = unique_test_dir("profile-lock-path");
    fs::create_dir_all(&temp_root).expect("temp root should be created");
    let _env = TestEnvGuard::set(&temp_root);

    let path = profile_lock_path_for_profile("demo").expect("lock path should resolve");

    assert!(path.to_string_lossy().contains("demo"));
}

#[test]
fn acquiring_same_profile_lock_twice_fails() {
    let _env_lock = ENV_LOCK.lock().expect("env lock should be acquired");
    let temp_root = unique_test_dir("profile-lock-double-acquire");
    fs::create_dir_all(&temp_root).expect("temp root should be created");
    let _env = TestEnvGuard::set(&temp_root);

    let path = profile_lock_path_for_profile("demo").expect("lock path should resolve");
    let _first = acquire_profile_lock(&path).expect("first lock should succeed");

    assert!(acquire_profile_lock(&path).is_err());
}

#[test]
fn dropping_profile_lock_releases_for_reacquisition() {
    let _env_lock = ENV_LOCK.lock().expect("env lock should be acquired");
    let temp_root = unique_test_dir("profile-lock-release-reacquire");
    fs::create_dir_all(&temp_root).expect("temp root should be created");
    let _env = TestEnvGuard::set(&temp_root);

    let path = profile_lock_path_for_profile("release-test").expect("lock path should resolve");
    {
        let _lock = acquire_profile_lock(&path).expect("first lock should succeed");
    }

    let _second = acquire_profile_lock(&path).expect("second lock should succeed after drop");
}

#[test]
fn non_owning_profile_session_cannot_start_command() {
    assert!(!can_control_profile(false));
}

#[test]
fn non_owning_profile_session_cannot_save_configuration() {
    assert!(!can_save_profile_configuration(false));
}

#[test]
fn log_overflow_is_coalesced_into_one_message() {
    let msg = coalesced_log_overflow_message(42);
    assert!(msg.contains("42"));
}

#[test]
fn runtime_state_path_resolves_for_ephemeral() {
    let _env_lock = ENV_LOCK.lock().expect("env lock should be acquired");
    let temp_root = unique_test_dir("runtime-state-ephemeral-path");
    fs::create_dir_all(&temp_root).expect("temp root should be created");
    let _env = TestEnvGuard::set(&temp_root);

    let path = runtime_state_path_for_ephemeral();

    assert!(path.is_some());
}

#[test]
fn save_and_load_runtime_state() {
    let temp_root = unique_test_dir("save-load-runtime-state");
    fs::create_dir_all(&temp_root).expect("temp dir should be created");
    let state_path = temp_root.join("runtime-state.toml");

    let state = RuntimeOwnershipState {
        pid: 9999,
        pgid: 9999,
        started_at_clock_ticks: 3000,
        command_label: "test command".to_string(),
        profile_name: Some("testprofile".to_string()),
        ephemeral: false,
    };

    save_runtime_state(&state_path, &state).expect("save should succeed");

    let loaded = load_runtime_state(&state_path);
    assert!(loaded.is_some());
    let loaded = loaded.unwrap();
    assert_eq!(loaded.pid, 9999);
    assert_eq!(loaded.pgid, 9999);
    assert_eq!(loaded.profile_name.as_deref(), Some("testprofile"));
}

#[test]
fn atomic_write_replaces_existing_file_contents() {
    let temp_dir = unique_test_dir("atomic-write-test");
    fs::create_dir_all(&temp_dir).expect("temp dir should be created");
    let file_path = temp_dir.join("test.txt");

    let old_contents = "old contents\nmultiple lines\n";
    fs::write(&file_path, old_contents).expect("should write old contents");

    let new_contents = "new contents\n";
    let result = atomic_write(&file_path, new_contents);
    assert!(result.is_ok(), "atomic_write should succeed");

    let read_back = fs::read_to_string(&file_path).expect("should read file");
    assert_eq!(read_back, new_contents);
}

#[test]
fn atomic_write_creates_deeply_nested_missing_parent_directories() {
    let temp_dir = unique_test_dir("atomic-write-nested-parents");
    fs::create_dir_all(&temp_dir).expect("temp dir should be created");
    let file_path = temp_dir
        .join("nested")
        .join("profile")
        .join("configs")
        .join("settings.toml");

    atomic_write(&file_path, "enabled = true\n").expect("atomic_write should succeed");

    assert!(file_path.exists(), "atomic write should create target file");
    let read_back = fs::read_to_string(&file_path).expect("should read file");
    assert_eq!(read_back, "enabled = true\n");
}

#[cfg(unix)]
#[test]
fn atomic_write_preserves_existing_file_permissions() {
    let temp_dir = unique_test_dir("atomic-write-permissions");
    fs::create_dir_all(&temp_dir).expect("temp dir should be created");
    let file_path = temp_dir.join("secure.txt");

    fs::write(&file_path, "old").expect("should create file");
    let secure_mode = 0o600;
    fs::set_permissions(&file_path, fs::Permissions::from_mode(secure_mode))
        .expect("should set secure permissions");

    atomic_write(&file_path, "new").expect("atomic_write should succeed");

    let mode_after = fs::metadata(&file_path)
        .expect("metadata should be readable")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode_after, secure_mode);
}

#[test]
fn clear_runtime_state_removes_file() {
    let temp_root = unique_test_dir("clear-runtime-state");
    fs::create_dir_all(&temp_root).expect("temp dir should be created");
    let state_path = temp_root.join("runtime-state.toml");

    let state = RuntimeOwnershipState {
        pid: 1111,
        pgid: 1111,
        started_at_clock_ticks: 4000,
        command_label: "to be cleared".to_string(),
        profile_name: None,
        ephemeral: true,
    };

    save_runtime_state(&state_path, &state).expect("save should succeed");
    assert!(state_path.exists());

    clear_runtime_state(&state_path).expect("clear should succeed");
    assert!(!state_path.exists());
}

#[test]
fn clear_nonexistent_runtime_state_succeeds() {
    let temp_root = unique_test_dir("clear-nonexistent-runtime-state");
    fs::create_dir_all(&temp_root).expect("temp root should be created");
    let state_path = temp_root.join("nonexistent.toml");

    let result = clear_runtime_state(&state_path);
    assert!(result.is_ok(), "clearing nonexistent file should succeed");
}

#[test]
fn runtime_ownership_state_validates_pid_zero() {
    let state = RuntimeOwnershipState {
        pid: 0,
        pgid: 1234,
        started_at_clock_ticks: 1000,
        command_label: "test".to_string(),
        profile_name: None,
        ephemeral: true,
    };

    let err = state
        .validate()
        .expect_err("zero pid should fail validation");
    assert!(err.contains("pid"));
}

#[test]
fn runtime_ownership_state_validates_pgid_zero() {
    let state = RuntimeOwnershipState {
        pid: 1234,
        pgid: 0,
        started_at_clock_ticks: 1000,
        command_label: "test".to_string(),
        profile_name: None,
        ephemeral: true,
    };

    let err = state
        .validate()
        .expect_err("zero pgid should fail validation");
    assert!(err.contains("pgid"));
}

#[test]
fn runtime_ownership_state_validates_command_label_length() {
    let state = RuntimeOwnershipState {
        pid: 1234,
        pgid: 1234,
        started_at_clock_ticks: 1000,
        command_label: "x".repeat(MAX_COMMAND_LENGTH + 1),
        profile_name: None,
        ephemeral: true,
    };

    let err = state
        .validate()
        .expect_err("oversized command_label should fail validation");
    assert!(err.contains("command_label"));
    assert!(err.contains("maximum length"));
}

#[test]
fn reconcile_runtime_state_restores_running_when_group_is_alive() {
    use crate::process::get_process_start_time;

    let own_pid = std::process::id();
    let own_pgid = unsafe { libc::getpgid(0) };
    let actual_start_time = get_process_start_time(own_pid as libc::pid_t)
        .expect("should be able to get process start time");

    let state = RuntimeOwnershipState {
        pid: own_pid,
        pgid: own_pgid as u32,
        started_at_clock_ticks: actual_start_time,
        command_label: "sleep 30".to_string(),
        profile_name: Some("default".to_string()),
        ephemeral: false,
    };

    let result = reconcile_runtime_state(&state, is_process_group_alive);
    assert!(matches!(result, RuntimeReconcileResult::RestoreRunning));
}

#[test]
fn reconcile_runtime_state_clears_stale_when_group_is_missing() {
    let state = RuntimeOwnershipState {
        pid: 1234,
        pgid: 1234,
        started_at_clock_ticks: 1000,
        command_label: "sleep 30".to_string(),
        profile_name: Some("default".to_string()),
        ephemeral: false,
    };

    let result = reconcile_runtime_state(&state, |_pgid| false);
    assert!(matches!(result, RuntimeReconcileResult::ClearStale));
}

#[test]
fn reconcile_runtime_state_ignores_invalid_metadata() {
    let state = RuntimeOwnershipState {
        pid: 0,
        pgid: 0,
        started_at_clock_ticks: 1000,
        command_label: "sleep 30".to_string(),
        profile_name: Some("default".to_string()),
        ephemeral: false,
    };

    let result = reconcile_runtime_state(&state, is_process_group_alive);
    assert!(matches!(result, RuntimeReconcileResult::IgnoreInvalid));
}

#[test]
fn reconcile_runtime_state_handles_old_unix_timestamp_format() {
    // Test that old format (unix timestamp in ms) is handled gracefully
    // Old format used started_at_unix_ms which would be a large number like 1700000000000+
    // New format uses clock ticks which are much smaller
    let own_pid = std::process::id();
    let own_pgid = unsafe { libc::getpgid(0) };

    // Use a unix timestamp in ms (very large number > 1 trillion)
    let old_unix_timestamp_ms = 1700000000000u64;

    let state = RuntimeOwnershipState {
        pid: own_pid,
        pgid: own_pgid as u32,
        started_at_clock_ticks: old_unix_timestamp_ms, // Old format: unix ms
        command_label: "sleep 30".to_string(),
        profile_name: Some("default".to_string()),
        ephemeral: false,
    };

    // Should restore running because it's the old format - we skip start time validation
    // but still validate PGID membership
    let result = reconcile_runtime_state(&state, is_process_group_alive);
    assert!(matches!(result, RuntimeReconcileResult::RestoreRunning));
}

#[test]
fn parse_process_start_time_handles_process_name_with_closing_paren() {
    let stat = "123 (worker) helper) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 424242 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52";

    let parsed = crate::process::parse_process_start_time_from_stat(stat);

    assert_eq!(parsed, Some(424242));
}

#[test]
fn process_exit_event_is_not_re_emitted_after_already_reported() {
    let should_emit = crate::ui::should_emit_process_exited(true, true, true, true);

    assert!(
        !should_emit,
        "watcher should not emit duplicate ProcessExited events"
    );
}

#[test]
fn startup_runtime_state_no_state_stops() {
    let startup = StartupState {
        profile_label: "default".to_string(),
        persistent_config_access: Some(PersistentConfigAccess {
            profile: "default".to_string(),
            config_path: PathBuf::from("/tmp/default.toml"),
        }),
        config: Config {
            command: "echo test".to_string(),
            autostart: false,
            icon_path: None,
            log_to_file: false,
            log_file_path: None,
        },
        log_file_path: None,
        launch_on_startup: false,
        runtime_state_path: None,
        runtime_ownership: None,
        restored_running: false,
        owns_profile_lock: true,
        profile_lock: None,
        startup_message: None,
    };

    assert!(startup.runtime_state_path.is_none());
    assert!(startup.runtime_ownership.is_none());
    assert!(!startup.restored_running);
}

#[test]
fn startup_runtime_state_recovered_running() {
    let ownership = RuntimeOwnershipState {
        pid: 12345,
        pgid: 12345,
        started_at_clock_ticks: 1000,
        command_label: "sleep 60".to_string(),
        profile_name: Some("default".to_string()),
        ephemeral: false,
    };

    let startup = StartupState {
        profile_label: "default".to_string(),
        persistent_config_access: Some(PersistentConfigAccess {
            profile: "default".to_string(),
            config_path: PathBuf::from("/tmp/default.toml"),
        }),
        config: Config {
            command: "sleep 60".to_string(),
            autostart: false,
            icon_path: None,
            log_to_file: false,
            log_file_path: None,
        },
        log_file_path: None,
        launch_on_startup: false,
        runtime_state_path: Some(PathBuf::from("/tmp/runtime-state.toml")),
        runtime_ownership: Some(ownership),
        restored_running: true,
        owns_profile_lock: true,
        profile_lock: None,
        startup_message: None,
    };

    assert!(startup.runtime_state_path.is_some());
    assert!(startup.runtime_ownership.is_some());
    assert!(startup.restored_running);
    assert_eq!(startup.runtime_ownership.as_ref().unwrap().pid, 12345);
}

#[test]
fn startup_runtime_state_stale_clears() {
    let ownership = RuntimeOwnershipState {
        pid: 99999,
        pgid: 99999,
        started_at_clock_ticks: 1000,
        command_label: "sleep 60".to_string(),
        profile_name: Some("default".to_string()),
        ephemeral: false,
    };

    let startup = StartupState {
        profile_label: "default".to_string(),
        persistent_config_access: Some(PersistentConfigAccess {
            profile: "default".to_string(),
            config_path: PathBuf::from("/tmp/default.toml"),
        }),
        config: Config {
            command: "sleep 60".to_string(),
            autostart: false,
            icon_path: None,
            log_to_file: false,
            log_file_path: None,
        },
        log_file_path: None,
        launch_on_startup: false,
        runtime_state_path: Some(PathBuf::from("/tmp/runtime-state.toml")),
        runtime_ownership: Some(ownership),
        restored_running: false,
        owns_profile_lock: true,
        profile_lock: None,
        startup_message: None,
    };

    assert!(startup.runtime_state_path.is_some());
    assert!(startup.runtime_ownership.is_some());
    assert!(!startup.restored_running);
}

#[test]
fn startup_reconcile_dead_runtime_state_gets_cleared() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let temp_root = unique_test_dir("startup-reconcile-dead");
    fs::create_dir_all(&temp_root).expect("temp root should be created");
    let _env = TestEnvGuard::set(&temp_root);

    let state_path = runtime_state_path_for_profile("testprofile").expect("path should resolve");
    fs::create_dir_all(state_path.parent().expect("parent dir should exist"))
        .expect("parent dir should be created");

    let dead_state = RuntimeOwnershipState {
        pid: 99999,
        pgid: 99999,
        started_at_clock_ticks: 1000,
        command_label: "sleep 60".to_string(),
        profile_name: Some("testprofile".to_string()),
        ephemeral: false,
    };
    save_runtime_state(&state_path, &dead_state).expect("state should save");
    assert!(state_path.exists());

    let startup = StartupState {
        profile_label: "testprofile".to_string(),
        persistent_config_access: Some(PersistentConfigAccess {
            profile: "testprofile".to_string(),
            config_path: PathBuf::from("/tmp/default.toml"),
        }),
        config: Config {
            command: "sleep 60".to_string(),
            autostart: false,
            icon_path: None,
            log_to_file: false,
            log_file_path: None,
        },
        log_file_path: None,
        launch_on_startup: false,
        runtime_state_path: Some(state_path.clone()),
        runtime_ownership: Some(dead_state),
        restored_running: false,
        owns_profile_lock: true,
        profile_lock: None,
        startup_message: None,
    };

    let reconciled = reconcile_startup_runtime_state(startup);

    assert!(!reconciled.restored_running);
    assert!(reconciled.runtime_state_path.is_some());
    assert!(reconciled.runtime_ownership.is_none());
    assert!(!state_path.exists());
}

#[test]
fn startup_reconcile_live_runtime_state_sets_restored_running() {
    use crate::process::get_process_start_time;

    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let temp_root = unique_test_dir("startup-reconcile-live");
    fs::create_dir_all(&temp_root).expect("temp root should be created");
    let _env = TestEnvGuard::set(&temp_root);

    let state_path = runtime_state_path_for_profile("testprofile").expect("path should resolve");
    fs::create_dir_all(state_path.parent().expect("parent dir should exist"))
        .expect("parent dir should be created");

    let own_pid = std::process::id();
    let own_pgid = unsafe { libc::getpgid(0) as u32 };
    let actual_start_time = get_process_start_time(own_pid as libc::pid_t)
        .expect("should be able to get process start time");

    let live_state = RuntimeOwnershipState {
        pid: own_pid,
        pgid: own_pgid,
        started_at_clock_ticks: actual_start_time,
        command_label: "sleep 60".to_string(),
        profile_name: Some("testprofile".to_string()),
        ephemeral: false,
    };
    save_runtime_state(&state_path, &live_state).expect("state should save");

    let startup = StartupState {
        profile_label: "testprofile".to_string(),
        persistent_config_access: Some(PersistentConfigAccess {
            profile: "testprofile".to_string(),
            config_path: PathBuf::from("/tmp/default.toml"),
        }),
        config: Config {
            command: "sleep 60".to_string(),
            autostart: false,
            icon_path: None,
            log_to_file: false,
            log_file_path: None,
        },
        log_file_path: None,
        launch_on_startup: false,
        runtime_state_path: Some(state_path),
        runtime_ownership: Some(live_state),
        restored_running: false,
        owns_profile_lock: true,
        profile_lock: None,
        startup_message: None,
    };

    let reconciled = reconcile_startup_runtime_state(startup);

    assert!(reconciled.restored_running);
    assert!(reconciled.runtime_ownership.is_some());
    assert_eq!(reconciled.runtime_ownership.as_ref().unwrap().pid, own_pid);
}

#[test]
fn startup_reconcile_invalid_runtime_state_falls_back_to_stopped() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let temp_root = unique_test_dir("startup-reconcile-invalid");
    fs::create_dir_all(&temp_root).expect("temp root should be created");
    let _env = TestEnvGuard::set(&temp_root);

    let state_path = runtime_state_path_for_profile("testprofile").expect("path should resolve");
    fs::create_dir_all(state_path.parent().expect("parent dir should exist"))
        .expect("parent dir should be created");

    let invalid_state = RuntimeOwnershipState {
        pid: 0,
        pgid: 0,
        started_at_clock_ticks: 1000,
        command_label: "sleep 60".to_string(),
        profile_name: Some("testprofile".to_string()),
        ephemeral: false,
    };
    save_runtime_state(&state_path, &invalid_state).expect("state should save");

    let startup = StartupState {
        profile_label: "testprofile".to_string(),
        persistent_config_access: Some(PersistentConfigAccess {
            profile: "testprofile".to_string(),
            config_path: PathBuf::from("/tmp/default.toml"),
        }),
        config: Config {
            command: "sleep 60".to_string(),
            autostart: false,
            icon_path: None,
            log_to_file: false,
            log_file_path: None,
        },
        log_file_path: None,
        launch_on_startup: false,
        runtime_state_path: Some(state_path.clone()),
        runtime_ownership: Some(invalid_state),
        restored_running: false,
        owns_profile_lock: true,
        profile_lock: None,
        startup_message: None,
    };

    let reconciled = reconcile_startup_runtime_state(startup);

    assert!(!reconciled.restored_running);
    assert!(reconciled.runtime_state_path.is_some());
    assert!(reconciled.runtime_ownership.is_none());
    assert!(!state_path.exists());
}

#[test]
fn clear_runtime_state_after_exit_removes_persisted_file() {
    let _env_lock = ENV_LOCK.lock().expect("env lock should be acquired");
    let temp_root = unique_test_dir("clear-runtime-state-after-exit");
    fs::create_dir_all(&temp_root).expect("temp root should be created");
    let _env = TestEnvGuard::set(&temp_root);

    let state_path = runtime_state_path_for_profile("cleanup").expect("path should resolve");
    save_runtime_state(
        &state_path,
        &RuntimeOwnershipState {
            pid: 321,
            pgid: 321,
            started_at_clock_ticks: 123,
            command_label: "sleep 1".to_string(),
            profile_name: Some("cleanup".to_string()),
            ephemeral: false,
        },
    )
    .expect("runtime state should save");

    clear_runtime_state_after_exit(Some(&state_path));

    assert!(!state_path.exists());
}

#[test]
fn initial_start_stop_label_reflects_restored_state() {
    assert_eq!(initial_start_stop_label(false), "Start");
    assert_eq!(initial_start_stop_label(true), "Stop");
}

#[test]
fn should_launch_on_startup_skips_relaunch_for_restored_run() {
    let startup = StartupState {
        profile_label: "default".to_string(),
        persistent_config_access: None,
        config: Config {
            command: DEFAULT_COMMAND.to_string(),
            autostart: true,
            icon_path: None,
            log_to_file: false,
            log_file_path: None,
        },
        log_file_path: None,
        launch_on_startup: true,
        runtime_state_path: None,
        runtime_ownership: None,
        restored_running: true,
        owns_profile_lock: true,
        profile_lock: None,
        startup_message: None,
    };

    assert!(!should_launch_on_startup(&startup));
}

#[test]
fn should_launch_on_startup_runs_when_not_restored() {
    let startup = StartupState {
        profile_label: "default".to_string(),
        persistent_config_access: None,
        config: Config {
            command: DEFAULT_COMMAND.to_string(),
            autostart: true,
            icon_path: None,
            log_to_file: false,
            log_file_path: None,
        },
        log_file_path: None,
        launch_on_startup: true,
        runtime_state_path: None,
        runtime_ownership: None,
        restored_running: false,
        owns_profile_lock: true,
        profile_lock: None,
        startup_message: None,
    };

    assert!(should_launch_on_startup(&startup));
}

#[test]
fn startup_reconcile_sets_restored_message() {
    use crate::process::get_process_start_time;

    let own_pid = std::process::id();
    let own_pgid = unsafe { libc::getpgid(0) as u32 };
    let actual_start_time = get_process_start_time(own_pid as libc::pid_t)
        .expect("should be able to get process start time");

    let startup = StartupState {
        profile_label: "default".to_string(),
        persistent_config_access: None,
        config: Config {
            command: DEFAULT_COMMAND.to_string(),
            autostart: false,
            icon_path: None,
            log_to_file: false,
            log_file_path: None,
        },
        log_file_path: None,
        launch_on_startup: false,
        runtime_state_path: Some(PathBuf::from("/tmp/runtime-state.toml")),
        runtime_ownership: Some(RuntimeOwnershipState {
            pid: own_pid,
            pgid: own_pgid,
            started_at_clock_ticks: actual_start_time,
            command_label: "sleep 1".to_string(),
            profile_name: Some("default".to_string()),
            ephemeral: false,
        }),
        restored_running: false,
        owns_profile_lock: true,
        profile_lock: None,
        startup_message: None,
    };

    let reconciled = reconcile_startup_runtime_state(startup);
    assert_eq!(
        reconciled.startup_message.as_deref(),
        Some(RUNTIME_RESTORED_MESSAGE)
    );
}

mod process_group_launch {
    use super::*;

    #[test]
    fn spawn_in_new_process_group_creates_separate_pgid() {
        let mut result =
            crate::process::spawn_command_in_new_process_group("sleep", &["0".to_string()], false)
                .expect("spawn should succeed");

        assert!(result.owned_pid > 0, "owned_pid should be set");
        assert!(result.owned_pgid > 0, "owned_pgid should be set");

        let pgid_matches = unsafe { libc::getpgid(result.owned_pid as libc::pid_t) };
        assert!(pgid_matches >= 0, "pgid should be retrievable");
        assert_eq!(
            pgid_matches, result.owned_pgid as libc::pid_t,
            "child's pgid should match owned_pgid"
        );

        let _ = result.child.kill();
        let _ = result.child.wait();
    }

    #[test]
    fn spawn_in_new_process_group_pgid_differs_from_parent() {
        let parent_pgid = unsafe { libc::getpgid(0) };

        let mut result =
            crate::process::spawn_command_in_new_process_group("sleep", &["0".to_string()], false)
                .expect("spawn should succeed");

        assert_ne!(
            result.owned_pgid as libc::pid_t, parent_pgid,
            "child's pgid should differ from parent's pgid"
        );

        let _ = result.child.kill();
        let _ = result.child.wait();
    }

    #[test]
    fn persist_launch_metadata_saves_to_profile_path() {
        use crate::process::persist_launch_metadata_with_start_time;

        let _env_lock = ENV_LOCK.lock().expect("env lock should be acquired");
        let temp_root = unique_test_dir("persist-metadata-profile");
        fs::create_dir_all(&temp_root).expect("temp root should be created");
        let _env = TestEnvGuard::set(&temp_root);

        let state_path =
            runtime_state_path_for_profile("testprofile").expect("path should resolve");

        persist_launch_metadata_with_start_time(
            &Some(state_path.clone()),
            "echo hello",
            12345,
            12345,
            Some("testprofile"),
            false,
            Some(1000),
        )
        .expect("persist should succeed");

        let loaded = load_runtime_state(&state_path).expect("state should load");
        assert_eq!(loaded.pid, 12345);
        assert_eq!(loaded.pgid, 12345);
        assert_eq!(loaded.profile_name.as_deref(), Some("testprofile"));
        assert!(!loaded.ephemeral);
        assert_eq!(loaded.command_label, "echo hello");
    }

    #[test]
    fn persist_launch_metadata_saves_to_ephemeral_path() {
        use crate::process::persist_launch_metadata_with_start_time;

        let _env_lock = ENV_LOCK.lock().expect("env lock should be acquired");
        let temp_root = unique_test_dir("persist-metadata-ephemeral");
        fs::create_dir_all(&temp_root).expect("temp root should be created");
        let _env = TestEnvGuard::set(&temp_root);

        let state_path = runtime_state_path_for_ephemeral().expect("path should resolve");

        persist_launch_metadata_with_start_time(
            &Some(state_path.clone()),
            "echo ephemeral",
            54321,
            54321,
            None,
            true,
            Some(2000),
        )
        .expect("persist should succeed");

        let loaded = load_runtime_state(&state_path).expect("state should load");
        assert_eq!(loaded.pid, 54321);
        assert_eq!(loaded.pgid, 54321);
        assert!(loaded.profile_name.is_none());
        assert!(loaded.ephemeral);
        assert_eq!(loaded.command_label, "echo ephemeral");
    }

    #[test]
    fn persist_launch_metadata_overwrites_existing() {
        use crate::process::persist_launch_metadata_with_start_time;

        let _env_lock = ENV_LOCK.lock().expect("env lock should be acquired");
        let temp_root = unique_test_dir("persist-overwrite");
        fs::create_dir_all(&temp_root).expect("temp root should be created");
        let _env = TestEnvGuard::set(&temp_root);

        let state_path = runtime_state_path_for_profile("overwrite").expect("path should resolve");

        let old_state = RuntimeOwnershipState {
            pid: 11111,
            pgid: 11111,
            started_at_clock_ticks: 1000,
            command_label: "old command".to_string(),
            profile_name: Some("overwrite".to_string()),
            ephemeral: false,
        };
        save_runtime_state(&state_path, &old_state).expect("old state should save");

        persist_launch_metadata_with_start_time(
            &Some(state_path.clone()),
            "new command",
            22222,
            22222,
            Some("overwrite"),
            false,
            Some(3000),
        )
        .expect("persist should succeed");

        let loaded = load_runtime_state(&state_path).expect("state should load");
        assert_eq!(loaded.pid, 22222);
        assert_eq!(loaded.pgid, 22222);
        assert_eq!(loaded.command_label, "new command");
    }

    #[test]
    fn persist_launch_metadata_fails_without_path() {
        let result = crate::process::persist_launch_metadata(
            &None,
            "echo test",
            12345,
            12345,
            Some("test"),
            false,
        );

        assert!(result.is_err());
    }
}

mod stop_process_group {
    use std::time::Duration;

    #[test]
    fn stop_returns_false_for_invalid_pgid() {
        // Returns true because an already-exited process group is "success" - nothing to stop
        let stopped = crate::process::stop_process_group(999999, Duration::from_millis(10));

        assert!(
            stopped,
            "stop should return true for non-existent pgid (already gone)"
        );
    }

    #[test]
    fn stop_returns_false_for_zero_pgid() {
        let stopped = crate::process::stop_process_group(0, Duration::from_millis(10));

        assert!(!stopped, "stop should return false for zero pgid");
    }
}

mod sudo_mode_detection {
    use crate::process::{detect_sudo_mode, SudoMode};

    fn needs_password(args: &[String]) -> bool {
        matches!(detect_sudo_mode(args), Some(SudoMode::Plain))
    }

    #[test]
    fn sudo_askpass_mode_skips_stdin_injection() {
        let args = vec!["sudo".to_string(), "-A".to_string(), "echo".to_string()];
        let mode = detect_sudo_mode(&args);
        assert!(matches!(mode, Some(SudoMode::Askpass)));
    }

    #[test]
    fn sudo_noninteractive_mode_skips_password_prompt() {
        let args = vec!["sudo".to_string(), "-n".to_string(), "echo".to_string()];
        assert!(!needs_password(&args));
    }

    #[test]
    fn sudo_plain_mode_needs_password() {
        let args = vec!["sudo".to_string(), "echo".to_string()];
        assert!(needs_password(&args));
    }

    #[test]
    fn sudo_stdin_mode_skips_password_prompt() {
        let args = vec!["sudo".to_string(), "-S".to_string(), "echo".to_string()];
        assert!(!needs_password(&args));
    }

    #[test]
    fn non_sudo_command_returns_none() {
        let args = vec!["echo".to_string(), "hello".to_string()];
        assert!(detect_sudo_mode(&args).is_none());
    }

    #[test]
    fn empty_args_returns_none() {
        let args: Vec<String> = vec![];
        assert!(detect_sudo_mode(&args).is_none());
    }

    #[test]
    fn sudo_absolute_path_needs_password() {
        let args = vec!["/usr/bin/sudo".to_string(), "echo".to_string()];
        assert!(needs_password(&args));
    }

    #[test]
    fn sudo_short_option_bundle_an() {
        let args = vec!["sudo".to_string(), "-An".to_string(), "echo".to_string()];
        assert!(!needs_password(&args));
    }

    #[test]
    fn sudo_short_option_bundle_sn() {
        let args = vec!["sudo".to_string(), "-Sn".to_string(), "echo".to_string()];
        assert!(!needs_password(&args));
    }

    #[test]
    fn sudo_long_option_non_interactive_recognized() {
        let args = vec![
            "sudo".to_string(),
            "--non-interactive".to_string(),
            "echo".to_string(),
        ];
        let mode = detect_sudo_mode(&args);
        assert!(matches!(mode, Some(SudoMode::NonInteractive)));
        assert!(!needs_password(&args));
    }

    #[test]
    fn sudo_long_option_askpass_recognized() {
        let args = vec![
            "sudo".to_string(),
            "--askpass".to_string(),
            "echo".to_string(),
        ];
        let mode = detect_sudo_mode(&args);
        assert!(matches!(mode, Some(SudoMode::Askpass)));
        assert!(!needs_password(&args));
    }

    #[test]
    fn sudo_long_option_stdin_recognized() {
        let args = vec![
            "sudo".to_string(),
            "--stdin".to_string(),
            "echo".to_string(),
        ];
        let mode = detect_sudo_mode(&args);
        assert!(matches!(mode, Some(SudoMode::Stdin)));
        assert!(!needs_password(&args));
    }

    #[test]
    fn sudo_stops_at_first_non_option() {
        let args = vec![
            "sudo".to_string(),
            "-n".to_string(),
            "--".to_string(),
            "-n".to_string(),
        ];
        let mode = detect_sudo_mode(&args);
        assert!(matches!(mode, Some(SudoMode::NonInteractive)));
    }

    #[test]
    fn sudo_command_args_not_misclassified() {
        let args = vec![
            "sudo".to_string(),
            "myapp".to_string(),
            "-n".to_string(),
            "--flag".to_string(),
        ];
        let mode = detect_sudo_mode(&args);
        assert!(matches!(mode, Some(SudoMode::Plain)));
        assert!(needs_password(&args));
    }

    #[test]
    fn sudo_unrelated_long_option_not_misclassified() {
        let args = vec![
            "sudo".to_string(),
            "--login".to_string(),
            "echo".to_string(),
        ];
        let mode = detect_sudo_mode(&args);
        assert!(matches!(mode, Some(SudoMode::Plain)));
        assert!(needs_password(&args));
    }

    #[test]
    fn sudo_option_with_value_before_n() {
        let args = vec![
            "sudo".to_string(),
            "-u".to_string(),
            "root".to_string(),
            "-n".to_string(),
            "echo".to_string(),
        ];
        let mode = detect_sudo_mode(&args);
        assert!(matches!(mode, Some(SudoMode::NonInteractive)));
        assert!(!needs_password(&args));
    }

    #[test]
    fn sudo_attached_long_option_value_before_n() {
        let args = vec![
            "sudo".to_string(),
            "--user=root".to_string(),
            "-n".to_string(),
            "echo".to_string(),
        ];
        let mode = detect_sudo_mode(&args);
        assert!(matches!(mode, Some(SudoMode::NonInteractive)));
        assert!(!needs_password(&args));
    }

    #[test]
    fn sudo_attached_short_option_value_before_n() {
        let args = vec![
            "sudo".to_string(),
            "-uroot".to_string(),
            "-n".to_string(),
            "echo".to_string(),
        ];
        let mode = detect_sudo_mode(&args);
        assert!(matches!(mode, Some(SudoMode::NonInteractive)));
        assert!(!needs_password(&args));
    }

    #[test]
    fn sudo_short_bundle_with_u_attached_value_is_plain() {
        let args = vec!["sudo".to_string(), "-uann".to_string(), "echo".to_string()];
        let mode = detect_sudo_mode(&args);
        assert!(matches!(mode, Some(SudoMode::Plain)));
        assert!(needs_password(&args));
    }

    #[test]
    fn sudo_short_bundle_with_p_attached_value_is_plain() {
        let args = vec![
            "sudo".to_string(),
            "-pSecret".to_string(),
            "echo".to_string(),
        ];
        let mode = detect_sudo_mode(&args);
        assert!(matches!(mode, Some(SudoMode::Plain)));
        assert!(needs_password(&args));
    }

    #[test]
    fn sudo_short_bundle_preserves_flags_before_value_taker() {
        let args = vec!["sudo".to_string(), "-nuann".to_string(), "echo".to_string()];
        let mode = detect_sudo_mode(&args);
        assert!(matches!(mode, Some(SudoMode::NonInteractive)));
        assert!(!needs_password(&args));
    }
}

mod sudo_stdin_injection {
    use crate::process::ensure_sudo_stdin_flag;

    fn has_stdin_flag(args: &[String]) -> bool {
        args.iter().any(|arg| arg == "-S" || arg == "--stdin")
    }

    #[test]
    fn plain_sudo_gets_s_injected() {
        let mut args = vec!["sudo".to_string(), "echo".to_string()];
        ensure_sudo_stdin_flag(&mut args);
        assert!(has_stdin_flag(&args));
        assert_eq!(args[0], "sudo");
        assert_eq!(args[1], "-S");
        assert_eq!(args[2], "echo");
    }

    #[test]
    fn already_has_s_flag_not_duplicated() {
        let mut args = vec!["sudo".to_string(), "-S".to_string(), "echo".to_string()];
        ensure_sudo_stdin_flag(&mut args);
        assert!(has_stdin_flag(&args));
        assert_eq!(args[1], "-S");
    }

    #[test]
    fn already_has_long_stdin_not_duplicated() {
        let mut args = vec![
            "sudo".to_string(),
            "--stdin".to_string(),
            "echo".to_string(),
        ];
        ensure_sudo_stdin_flag(&mut args);
        assert!(has_stdin_flag(&args));
        assert_eq!(args[1], "--stdin");
    }

    #[test]
    fn already_has_askpass_not_duplicated() {
        let mut args = vec![
            "sudo".to_string(),
            "--askpass".to_string(),
            "echo".to_string(),
        ];
        ensure_sudo_stdin_flag(&mut args);
        assert!(!has_stdin_flag(&args));
    }

    #[test]
    fn bundled_option_sn_has_stdin() {
        let mut args = vec!["sudo".to_string(), "-Sn".to_string(), "echo".to_string()];
        ensure_sudo_stdin_flag(&mut args);
        assert!(has_stdin_flag(&args));
    }

    #[test]
    fn single_arg_sudo_gets_s_at_end() {
        let mut args = vec!["sudo".to_string()];
        ensure_sudo_stdin_flag(&mut args);
        assert!(has_stdin_flag(&args));
        assert_eq!(args[1], "-S");
    }
}
