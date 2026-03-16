use crate::cli::{
    build_detached_args, build_startup_state, effective_command_token, ephemeral_command_text,
    ephemeral_runtime_config, help_text, parse_cli_args_from, parse_cli_request_from,
    persistent_config_access, prepare_run_startup_with, should_expose_configuration, tray_tooltip,
    validate_runtime_mode,
};
use crate::config::config_path_for_profile;
use crate::config::{
    clear_runtime_state, load_runtime_state, runtime_state_path_for_ephemeral,
    runtime_state_path_for_profile, save_runtime_state,
};
use crate::logs::{
    append_log_to_file, extract_log_links, should_activate_log_link, strip_ansi_codes,
};
use crate::process::is_process_group_alive;
use crate::process::reconcile_runtime_state;
use crate::process::RuntimeReconcileResult;
use crate::*;
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process;
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
        started_at_unix_ms: 1000,
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
        started_at_unix_ms: 2000,
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
        started_at_unix_ms: 3000,
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
fn clear_runtime_state_removes_file() {
    let temp_root = unique_test_dir("clear-runtime-state");
    fs::create_dir_all(&temp_root).expect("temp dir should be created");
    let state_path = temp_root.join("runtime-state.toml");

    let state = RuntimeOwnershipState {
        pid: 1111,
        pgid: 1111,
        started_at_unix_ms: 4000,
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
fn runtime_ownership_state_validates_pid_zero() {
    let state = RuntimeOwnershipState {
        pid: 0,
        pgid: 1234,
        started_at_unix_ms: 1000,
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
        started_at_unix_ms: 1000,
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
        started_at_unix_ms: 1000,
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
    let state = RuntimeOwnershipState {
        pid: std::process::id(),
        pgid: std::process::id(),
        started_at_unix_ms: 1000,
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
        started_at_unix_ms: 1000,
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
        started_at_unix_ms: 1000,
        command_label: "sleep 30".to_string(),
        profile_name: Some("default".to_string()),
        ephemeral: false,
    };

    let result = reconcile_runtime_state(&state, is_process_group_alive);
    assert!(matches!(result, RuntimeReconcileResult::IgnoreInvalid));
}
