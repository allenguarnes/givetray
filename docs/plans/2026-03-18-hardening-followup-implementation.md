# Hardening Follow-Up Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Prevent same-profile instance races and harden logging, file persistence, config validation, and sudo-mode handling without changing normal single-instance behavior.

**Architecture:** Persistent profile mode becomes single-instance-per-profile by acquiring and holding a profile-scoped lock for the app lifetime. The lock owner is the only instance allowed to reconcile and mutate runtime-state for that profile. In the same pass, logging moves to bounded/coalesced delivery, file writes become atomic, config-window saves reuse command validation, and sudo prompting becomes mode-aware.

**Tech Stack:** Rust, GTK, `async_channel`, `fs2` or equivalent file locking API if already available/acceptable, TOML persistence, Linux process groups.

---

### Task 1: Add profile lock model to startup state

**Files:**
- Modify: `src/main.rs`
- Modify: `src/config.rs`
- Test: `src/tests.rs`

**Step 1: Write the failing test**

Add a test in `src/tests.rs` asserting a persistent startup state can represent an owning vs non-owning profile session, including a startup message for the non-owner path.

```rust
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
        startup_message: Some("profile already open".to_string()),
    };

    assert_eq!(startup.startup_message.as_deref(), Some("profile already open"));
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test startup_state_can_mark_profile_lock_conflict`
Expected: FAIL because the test or state shape is not fully supported yet.

**Step 3: Write minimal implementation**

- Add the lock-related startup/app-state fields needed to represent whether this instance owns the profile.
- Keep the state minimal: ownership flag, optional lock handle, and startup message support.

**Step 4: Run test to verify it passes**

Run: `cargo test startup_state_can_mark_profile_lock_conflict`
Expected: PASS

**Step 5: Commit**

```bash
git add src/main.rs src/config.rs src/tests.rs
git commit -m "refactor: track profile lock ownership in startup state"
```

### Task 2: Add profile lock path and acquisition helper

**Files:**
- Modify: `src/config.rs`
- Test: `src/tests.rs`

**Step 1: Write the failing tests**

Add tests for lock path resolution and same-process double acquisition failure.

```rust
#[test]
fn profile_lock_path_resolves_for_profile() {
    let path = profile_lock_path_for_profile("demo").expect("lock path should resolve");
    assert!(path.to_string_lossy().contains("demo"));
}

#[test]
fn acquiring_same_profile_lock_twice_fails() {
    let path = profile_lock_path_for_profile("demo").expect("lock path should resolve");
    let _first = acquire_profile_lock(&path).expect("first lock should succeed");
    assert!(acquire_profile_lock(&path).is_err());
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test profile_lock_path_resolves_for_profile acquiring_same_profile_lock_twice_fails`
Expected: FAIL because the helpers do not exist yet.

**Step 3: Write minimal implementation**

- Add `profile_lock_path_for_profile()` beside runtime-state path helpers.
- Add a small lock acquisition helper that creates the parent directory, opens the lock file, and takes an exclusive non-blocking lock.
- Return a concrete lock handle that stays alive while held.

**Step 4: Run tests to verify they pass**

Run: `cargo test profile_lock_path_resolves_for_profile acquiring_same_profile_lock_twice_fails`
Expected: PASS

**Step 5: Commit**

```bash
git add src/config.rs src/tests.rs Cargo.toml Cargo.lock
git commit -m "feat: add per-profile lock acquisition"
```

### Task 3: Enforce single-instance-per-profile during startup

**Files:**
- Modify: `src/cli.rs`
- Modify: `src/main.rs`
- Modify: `src/config.rs`
- Test: `src/tests.rs`

**Step 1: Write the failing tests**

Add tests that a lock owner loads runtime-state normally and a second instance becomes non-owning, skips runtime-state reconciliation, and gets an "already open" message.

```rust
#[test]
fn second_same_profile_instance_skips_runtime_recovery() {
    // Arrange one held profile lock and an existing runtime-state file.
    // Build startup state for the same profile.
    // Assert: runtime_ownership is None and startup_message reports lock conflict.
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test second_same_profile_instance_skips_runtime_recovery`
Expected: FAIL because startup still treats every instance as owner.

**Step 3: Write minimal implementation**

- Acquire the profile lock during persistent startup preparation.
- On lock failure, mark the startup state as non-owning, skip runtime-state load/reconcile, and set the startup message.
- Preserve existing behavior for ephemeral mode.

**Step 4: Run tests to verify they pass**

Run: `cargo test second_same_profile_instance_skips_runtime_recovery`
Expected: PASS

**Step 5: Commit**

```bash
git add src/cli.rs src/main.rs src/config.rs src/tests.rs
git commit -m "fix: block concurrent same-profile instances"
```

### Task 4: Disable Start/Stop and config save for non-owning sessions

**Files:**
- Modify: `src/process.rs`
- Modify: `src/ui.rs`
- Modify: `src/config.rs`
- Modify: `src/logs.rs`
- Test: `src/tests.rs`

**Step 1: Write the failing tests**

Add tests for the non-owning path so Start/Stop is blocked and config save returns false with a clear log/status message.

```rust
#[test]
fn non_owning_profile_session_cannot_start_command() {
    assert!(!can_control_profile(true));
}

#[test]
fn non_owning_profile_session_cannot_save_configuration() {
    assert!(!can_save_profile_configuration(true));
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test non_owning_profile_session_cannot_start_command non_owning_profile_session_cannot_save_configuration`
Expected: FAIL because ownership gating does not exist yet.

**Step 3: Write minimal implementation**

- Add small helpers for ownership-gated control/save checks.
- Prevent non-owning sessions from starting/stopping commands or saving config.
- Emit one clear message instead of silently doing nothing.

**Step 4: Run tests to verify they pass**

Run: `cargo test non_owning_profile_session_cannot_start_command non_owning_profile_session_cannot_save_configuration`
Expected: PASS

**Step 5: Commit**

```bash
git add src/process.rs src/ui.rs src/config.rs src/logs.rs src/tests.rs
git commit -m "fix: gate profile actions behind lock ownership"
```

### Task 5: Bound UI log transport and add overflow coalescing

**Files:**
- Modify: `src/main.rs`
- Modify: `src/process.rs`
- Modify: `src/logs.rs`
- Test: `src/tests.rs`

**Step 1: Write the failing test**

Add a unit test for the overflow/coalescing helper.

```rust
#[test]
fn log_overflow_is_coalesced_into_one_message() {
    let msg = coalesced_log_overflow_message(42);
    assert!(msg.contains("42"));
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test log_overflow_is_coalesced_into_one_message`
Expected: FAIL because the helper and bounded path do not exist.

**Step 3: Write minimal implementation**

- Switch from `async_channel::unbounded()` to `async_channel::bounded()`.
- Update log readers to use `try_send`/batched send logic.
- Add a coalesced overflow message path when the queue is saturated.

**Step 4: Run test to verify it passes**

Run: `cargo test log_overflow_is_coalesced_into_one_message`
Expected: PASS

**Step 5: Run focused regression tests**

Run: `cargo test process_exit_event_is_not_re_emitted_after_already_reported malformed_runtime_state_file_gets_cleared_during_startup`
Expected: PASS

**Step 6: Commit**

```bash
git add src/main.rs src/process.rs src/logs.rs src/tests.rs
git commit -m "fix: bound ui log transport"
```

### Task 6: Add atomic write helper and migrate persistent writes

**Files:**
- Modify: `src/config.rs`
- Modify: `src/desktop.rs`
- Test: `src/tests.rs`

**Step 1: Write the failing tests**

Add helper-level tests for atomic overwrite semantics.

```rust
#[test]
fn atomic_write_replaces_existing_file_contents() {
    // write old contents, call helper, verify full new contents exist
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test atomic_write_replaces_existing_file_contents`
Expected: FAIL because the helper does not exist.

**Step 3: Write minimal implementation**

- Add one shared atomic-write helper in `src/config.rs` or a small dedicated module.
- Use it from config save, runtime-state save, and desktop-file write paths.
- Keep caller-facing return types unchanged.

**Step 4: Run tests to verify they pass**

Run: `cargo test atomic_write_replaces_existing_file_contents save_and_load_runtime_state`
Expected: PASS

**Step 5: Commit**

```bash
git add src/config.rs src/desktop.rs src/tests.rs
git commit -m "fix: write persistent files atomically"
```

### Task 7: Reuse command validation for configuration saves

**Files:**
- Modify: `src/cli.rs`
- Modify: `src/config.rs`
- Test: `src/tests.rs`

**Step 1: Write the failing tests**

Add tests for empty and malformed command text in the config-save path.

```rust
#[test]
fn save_configuration_rejects_empty_command() {
    assert!(!validate_saved_command_text("   ").is_ok());
}

#[test]
fn save_configuration_rejects_unparseable_command() {
    assert!(validate_saved_command_text("unterminated '").is_err());
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test save_configuration_rejects_empty_command save_configuration_rejects_unparseable_command`
Expected: FAIL because save validation is not reused yet.

**Step 3: Write minimal implementation**

- Extract or reuse existing command validation logic from CLI code.
- Call it before `save_config()` in the config-window save path.
- Surface the validation error through the existing log/status behavior.

**Step 4: Run tests to verify they pass**

Run: `cargo test save_configuration_rejects_empty_command save_configuration_rejects_unparseable_command`
Expected: PASS

**Step 5: Commit**

```bash
git add src/cli.rs src/config.rs src/tests.rs
git commit -m "fix: validate commands before saving config"
```

### Task 8: Make sudo prompting mode-aware

**Files:**
- Modify: `src/process.rs`
- Test: `src/tests.rs`

**Step 1: Write the failing tests**

Add tests covering stdin, askpass, and non-interactive sudo modes.

```rust
#[test]
fn sudo_askpass_mode_skips_stdin_injection() {
    let mut args = vec!["sudo".to_string(), "-A".to_string(), "echo".to_string()];
    let mode = detect_sudo_mode(&args);
    assert!(matches!(mode, SudoMode::Askpass));
}

#[test]
fn sudo_noninteractive_mode_skips_password_prompt() {
    let args = vec!["sudo".to_string(), "-n".to_string(), "echo".to_string()];
    assert!(!sudo_mode_needs_prompt(&args));
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test sudo_askpass_mode_skips_stdin_injection sudo_noninteractive_mode_skips_password_prompt`
Expected: FAIL because sudo mode detection is incomplete.

**Step 3: Write minimal implementation**

- Add one parser/helper for sudo launch mode.
- Only prompt and inject `-S` for interactive stdin-password cases.
- Leave `-A`, `--askpass`, and `-n` untouched.

**Step 4: Run tests to verify they pass**

Run: `cargo test sudo_askpass_mode_skips_stdin_injection sudo_noninteractive_mode_skips_password_prompt`
Expected: PASS

**Step 5: Commit**

```bash
git add src/process.rs src/tests.rs
git commit -m "fix: respect non-interactive sudo modes"
```

### Task 9: Update docs and run full verification

**Files:**
- Modify: `README.md`
- Test: `src/tests.rs`

**Step 1: Update docs**

- Document that persistent profiles are single-instance.
- Document bounded log behavior at a high level.
- Note any sudo prompt behavior changes that users may notice.

**Step 2: Run full verification**

Run: `cargo test`
Expected: PASS with all tests green.

Run: `cargo build --release`
Expected: PASS with no new warnings.

**Step 3: Manual verification checklist**

Run and confirm:

```bash
cargo run -- -c default
cargo run -- -c default
cargo run -- -- yes
```

Expected:
- second same-profile instance reports that the profile is already open and cannot control it
- noisy command does not cause unbounded memory growth during a short observation window

**Step 4: Commit**

```bash
git add README.md src/cli.rs src/config.rs src/desktop.rs src/logs.rs src/main.rs src/process.rs src/tests.rs src/ui.rs
git commit -m "fix: harden profile ownership and persistence flows"
```
