use crate::logs::profile_lock_action_blocked_message;
use crate::{
    coalesced_log_overflow_message, config::clear_runtime_state, config::save_runtime_state,
    AppState, RuntimeOwnershipState, UiEvent, BG_CHILD_ENV, RUNTIME_STOP_FAILED_MESSAGE,
};
use async_channel::Sender;
use gtk::prelude::*;
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeReconcileResult {
    RestoreRunning,
    ClearStale,
    IgnoreInvalid,
}

pub(crate) fn parse_process_start_time_from_stat(stat: &str) -> Option<u64> {
    let after_comm = stat.rsplit(") ").next()?;
    after_comm
        .split_whitespace()
        .nth(19)
        .and_then(|s| s.parse::<u64>().ok())
}

pub fn get_process_start_time(pid: libc::pid_t) -> Option<u64> {
    let stat_path = format!("/proc/{}/stat", pid);
    let content = std::fs::read_to_string(&stat_path).ok()?;
    parse_process_start_time_from_stat(&content)
}

pub fn reconcile_runtime_state<F>(
    state: &RuntimeOwnershipState,
    is_group_alive: F,
) -> RuntimeReconcileResult
where
    F: FnOnce(i32) -> bool,
{
    if state.pid == 0 || state.pgid == 0 {
        return RuntimeReconcileResult::IgnoreInvalid;
    }

    let pid = state.pid as libc::pid_t;
    let saved_pgid = state.pgid as i32;
    let saved_start_time = state.started_at_clock_ticks;

    // Validate that the specific leader process still exists and belongs to the saved PGID
    if unsafe { libc::getpgid(pid) } != saved_pgid {
        return RuntimeReconcileResult::ClearStale;
    }

    // Validate the start time to protect against PID reuse.
    // Old runtime-state files used unix milliseconds (very large numbers like 1700000000000+).
    // New format uses clock ticks (much smaller numbers, typically millions).
    // If the value is > 1 trillion, it's likely an old unix timestamp format - skip start time
    // validation but still validate PGID membership above.
    const UNIX_MS_THRESHOLD: u64 = 1_000_000_000_000; // 1 trillion
    if saved_start_time > UNIX_MS_THRESHOLD {
        // Old format: skip start time validation, rely on PGID check above
    } else if let Some(current_start_time) = get_process_start_time(pid) {
        if current_start_time != saved_start_time {
            return RuntimeReconcileResult::ClearStale;
        }
    } else {
        return RuntimeReconcileResult::ClearStale;
    }

    if is_group_alive(state.pgid as i32) {
        RuntimeReconcileResult::RestoreRunning
    } else {
        RuntimeReconcileResult::ClearStale
    }
}

pub fn is_process_group_alive(pgid: i32) -> bool {
    if pgid <= 0 {
        return false;
    }
    unsafe {
        let result = libc::kill(-pgid, 0);
        result == 0 || (result == -1 && *libc::__errno_location() == libc::EPERM)
    }
}

pub fn stop_process_group(pgid: i32, timeout: Duration) -> bool {
    if pgid == 0 {
        return false;
    }

    // If the process group is already gone, that's success - nothing to stop
    if !is_process_group_alive(pgid) {
        return true;
    }

    if !kill_process_group_members(pgid, libc::SIGTERM) {
        return false;
    }

    let start = Instant::now();
    while start.elapsed() < timeout {
        if !is_process_group_alive(pgid) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }

    if !is_process_group_alive(pgid) {
        return true;
    }

    if !kill_process_group_members(pgid, libc::SIGKILL) {
        return false;
    }

    thread::sleep(Duration::from_millis(200));

    !is_process_group_alive(pgid)
}

fn kill_process_group_members(pgid: i32, signal: libc::c_int) -> bool {
    // Primary: use kill(-pgid, signal) to signal the entire process group atomically
    let result = unsafe { libc::kill(-pgid, signal) };

    if result == 0 {
        return true;
    }

    // Check errno - if ESRCH (no such process), the group already exited, treat as success
    let err = unsafe { *libc::__errno_location() };
    if err == libc::ESRCH {
        // Group already gone - either it exited between our check and kill,
        // or was never valid. Either way, stop "succeeded".
        return true;
    }

    // If kill to negative PGID failed with other error, fall back to per-PID iteration
    // This handles edge cases where the group signaling might not work
    let pids = get_process_group_pids(pgid);
    if pids.is_empty() {
        return false;
    }

    let mut any_succeeded = false;
    for pid in pids {
        let pid_result = unsafe { libc::kill(pid, signal) };
        if pid_result == 0 {
            any_succeeded = true;
        }
    }

    any_succeeded
}

fn get_process_group_pids(pgid: i32) -> Vec<libc::pid_t> {
    let mut pids = Vec::new();

    let proc_path = std::path::Path::new("/proc");
    if let Ok(entries) = std::fs::read_dir(proc_path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name() {
                if let Ok(pid) = name.to_string_lossy().parse::<libc::pid_t>() {
                    if unsafe { libc::getpgid(pid) } == pgid {
                        pids.push(pid);
                    }
                }
            }
        }
    }

    pids
}

pub struct SpawnResult {
    pub child: Child,
    pub owned_pid: u32,
    pub owned_pgid: i32,
}

pub fn spawn_command_in_new_process_group(
    program: &str,
    args: &[String],
    stdinpiped: bool,
) -> Result<SpawnResult, String> {
    let mut cmd = Command::new(program);
    cmd.env_remove(BG_CHILD_ENV);
    if !args.is_empty() {
        cmd.args(args);
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    if stdinpiped {
        cmd.stdin(Stdio::piped());
    }

    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd
        .spawn()
        .map_err(|err| format!("failed to start command: {err}"))?;

    let pid = child.id();
    let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
    if pgid == -1 {
        return Err("failed to get process group id".to_string());
    }

    Ok(SpawnResult {
        child,
        owned_pid: pid,
        owned_pgid: pgid as i32,
    })
}

pub fn persist_launch_metadata(
    runtime_state_path: &Option<PathBuf>,
    command: &str,
    owned_pid: u32,
    owned_pgid: i32,
    profile_name: Option<&str>,
    ephemeral: bool,
) -> Result<(), String> {
    persist_launch_metadata_with_start_time(
        runtime_state_path,
        command,
        owned_pid,
        owned_pgid,
        profile_name,
        ephemeral,
        None,
    )
}

pub fn persist_launch_metadata_with_start_time(
    runtime_state_path: &Option<PathBuf>,
    command: &str,
    owned_pid: u32,
    owned_pgid: i32,
    profile_name: Option<&str>,
    ephemeral: bool,
    start_time_override: Option<u64>,
) -> Result<(), String> {
    let runtime_state_path = runtime_state_path
        .as_ref()
        .ok_or("runtime state path not set")?;

    let started_at_clock_ticks = if let Some(st) = start_time_override {
        st
    } else {
        get_process_start_time(owned_pid as libc::pid_t)
            .ok_or("failed to get process start time")?
    };

    let state = RuntimeOwnershipState {
        pid: owned_pid,
        pgid: owned_pgid as u32,
        started_at_clock_ticks,
        command_label: command.to_string(),
        profile_name: profile_name.map(String::from),
        ephemeral,
    };

    save_runtime_state(runtime_state_path, &state)
}

pub(crate) fn can_control_profile(owns_profile_lock: bool) -> bool {
    owns_profile_lock
}

fn try_send_main_thread_event(ui_tx: &Sender<UiEvent>, event: UiEvent) -> bool {
    match ui_tx.try_send(event) {
        Ok(()) => true,
        Err(async_channel::TrySendError::Full(_)) => false,
        Err(async_channel::TrySendError::Closed(_)) => false,
    }
}

fn flush_dropped_lines_blocking(ui_tx: &Sender<UiEvent>, dropped_lines: &mut usize) {
    if *dropped_lines == 0 {
        return;
    }

    if ui_tx
        .send_blocking(UiEvent::AppendLog(coalesced_log_overflow_message(
            *dropped_lines,
        )))
        .is_ok()
    {
        *dropped_lines = 0;
    }
}

pub(crate) fn start_command(state: Rc<RefCell<AppState>>, ui_tx: Sender<UiEvent>) {
    if !can_control_profile(state.borrow().owns_profile_lock) {
        let _ = try_send_main_thread_event(
            &ui_tx,
            UiEvent::AppendLog(profile_lock_action_blocked_message()),
        );
        return;
    }

    let has_active_process = state.borrow().child.is_some() || state.borrow().owned_pgid.is_some();
    if has_active_process {
        let _ = try_send_main_thread_event(
            &ui_tx,
            UiEvent::AppendLog("command is already running".to_string()),
        );
        return;
    }

    let command = state.borrow().command.clone();
    let mut args = match shell_words::split(&command) {
        Ok(parts) if !parts.is_empty() => parts,
        Ok(_) => {
            let _ = try_send_main_thread_event(
                &ui_tx,
                UiEvent::AppendLog("command is empty".to_string()),
            );
            return;
        }
        Err(err) => {
            let _ = try_send_main_thread_event(
                &ui_tx,
                UiEvent::AppendLog(format!("command parse error: {err}")),
            );
            return;
        }
    };

    let sudo_password = if is_sudo_command(&args) {
        match detect_sudo_mode(&args) {
            Some(SudoMode::Plain) => {
                ensure_sudo_stdin_flag(&mut args);
                match prompt_sudo_password() {
                    Some(password) => Some(password),
                    None => {
                        let _ = try_send_main_thread_event(
                            &ui_tx,
                            UiEvent::AppendLog("sudo password prompt cancelled".to_string()),
                        );
                        return;
                    }
                }
            }
            Some(SudoMode::Stdin) | Some(SudoMode::Askpass) | Some(SudoMode::NonInteractive) => {
                None
            }
            None => None,
        }
    } else {
        None
    };

    let mut spawn_result =
        match spawn_command_in_new_process_group(&args[0], &args[1..], sudo_password.is_some()) {
            Ok(result) => result,
            Err(err) => {
                let _ = try_send_main_thread_event(&ui_tx, UiEvent::AppendLog(err));
                return;
            }
        };

    if let Some(password) = sudo_password {
        if let Some(mut stdin) = spawn_result.child.stdin.take() {
            if let Err(err) = stdin
                .write_all(password.as_bytes())
                .and_then(|_| stdin.write_all(b"\n"))
            {
                let _ = try_send_main_thread_event(
                    &ui_tx,
                    UiEvent::AppendLog(format!("failed to send sudo password to process: {err}")),
                );
            }
        } else {
            let _ = try_send_main_thread_event(
                &ui_tx,
                UiEvent::AppendLog("unable to access sudo stdin pipe".to_string()),
            );
        }
    }

    if let Some(stdout) = spawn_result.child.stdout.take() {
        spawn_reader(stdout, ui_tx.clone());
    }
    if let Some(stderr) = spawn_result.child.stderr.take() {
        spawn_reader(stderr, ui_tx.clone());
    }

    let runtime_state_path = state.borrow().runtime_state_path.clone();
    let profile_name = state
        .borrow()
        .persistent_config_access
        .as_ref()
        .map(|a| a.profile.as_str())
        .map(String::from);
    let ephemeral = profile_name.is_none();
    let profile_name_owned = profile_name.as_deref();

    if let Err(err) = persist_launch_metadata(
        &runtime_state_path,
        &command,
        spawn_result.owned_pid,
        spawn_result.owned_pgid,
        profile_name_owned,
        ephemeral,
    ) {
        let _ = try_send_main_thread_event(
            &ui_tx,
            UiEvent::AppendLog(format!("failed to persist runtime state: {err}")),
        );
    }

    state.borrow_mut().child = Some(spawn_result.child);
    state.borrow_mut().owned_pid = Some(spawn_result.owned_pid);
    state.borrow_mut().owned_pgid = Some(spawn_result.owned_pgid);
    state.borrow_mut().process_exit_reported = false;

    if !try_send_main_thread_event(&ui_tx, UiEvent::SetRunning(true)) {
        state.borrow().start_stop_item.set_text("Stop");
    }
    let _ = try_send_main_thread_event(&ui_tx, UiEvent::AppendLog("command started".to_string()));
}

pub(crate) fn stop_command(state: Rc<RefCell<AppState>>, ui_tx: Sender<UiEvent>) {
    if !can_control_profile(state.borrow().owns_profile_lock) {
        let _ = try_send_main_thread_event(
            &ui_tx,
            UiEvent::AppendLog(profile_lock_action_blocked_message()),
        );
        return;
    }

    let pgid = state.borrow().owned_pgid;

    if let Some(pgid) = pgid {
        thread::spawn(move || {
            let stopped = stop_process_group(pgid, Duration::from_secs(2));

            if stopped {
                let _ = ui_tx.send_blocking(UiEvent::ClearRuntimeState);
                let _ = ui_tx.send_blocking(UiEvent::ProcessExited(None));
            } else {
                let _ = ui_tx
                    .send_blocking(UiEvent::AppendLog(RUNTIME_STOP_FAILED_MESSAGE.to_string()));
            }
        });
    } else if state.borrow().child.is_some() {
        let child = state.borrow_mut().child.take();
        thread::spawn(move || {
            if let Some(mut child) = child {
                let _ = child.wait();
            }
            let _ = ui_tx.send_blocking(UiEvent::ProcessExited(None));
        });
    }
}

pub(crate) fn stop_command_blocking(state: Rc<RefCell<AppState>>) {
    let pgid = state.borrow().owned_pgid;
    let runtime_state_path = state.borrow().runtime_state_path.clone();

    if let Some(pgid) = pgid {
        let stopped = stop_process_group(pgid, Duration::from_secs(2));

        if stopped {
            if let Some(ref path) = runtime_state_path {
                let _ = clear_runtime_state(path);
            }
            state.borrow_mut().owned_pgid = None;
            state.borrow_mut().owned_pid = None;
            state.borrow_mut().child = None;
        } else {
            eprintln!("{RUNTIME_STOP_FAILED_MESSAGE}");
        }
    } else if state.borrow().child.is_some() {
        let mut child = state.borrow_mut().child.take();
        if let Some(child) = child.as_mut() {
            let _ = child.wait();
        }
    }
}

fn spawn_reader<R: std::io::Read + Send + 'static>(reader: R, ui_tx: Sender<UiEvent>) {
    thread::spawn(move || {
        let buf = BufReader::new(reader);
        let mut dropped_lines = 0usize;

        for line in buf.lines() {
            match line {
                Ok(line) => {
                    if dropped_lines > 0 {
                        match ui_tx.try_send(UiEvent::AppendLog(coalesced_log_overflow_message(
                            dropped_lines,
                        ))) {
                            Ok(()) => dropped_lines = 0,
                            Err(async_channel::TrySendError::Full(_)) => {
                                dropped_lines += 1;
                                continue;
                            }
                            Err(async_channel::TrySendError::Closed(_)) => break,
                        }
                    }

                    match ui_tx.try_send(UiEvent::AppendLog(line)) {
                        Ok(()) => {}
                        Err(async_channel::TrySendError::Full(_)) => dropped_lines += 1,
                        Err(async_channel::TrySendError::Closed(_)) => break,
                    }
                }
                Err(err) => {
                    flush_dropped_lines_blocking(&ui_tx, &mut dropped_lines);

                    let _ = ui_tx.try_send(UiEvent::AppendLog(format!("log read error: {err}")));
                    break;
                }
            }
        }

        flush_dropped_lines_blocking(&ui_tx, &mut dropped_lines);
    });
}

pub(crate) enum SudoMode {
    Plain,
    Stdin,
    Askpass,
    NonInteractive,
}

pub(crate) fn detect_sudo_mode(args: &[String]) -> Option<SudoMode> {
    if args.is_empty() {
        return None;
    }

    let first_arg = Path::new(&args[0]);
    let file_name = first_arg.file_name().and_then(|n| n.to_str());
    if file_name != Some("sudo") {
        return None;
    }

    let mut skip_next = false;
    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];

        if skip_next {
            skip_next = false;
            i += 1;
            continue;
        }

        if !arg.starts_with('-') {
            break;
        }

        match arg.as_str() {
            "-n" | "--non-interactive" => return Some(SudoMode::NonInteractive),
            "-A" | "--askpass" => return Some(SudoMode::Askpass),
            "-S" | "--stdin" => return Some(SudoMode::Stdin),
            "--" => break,
            _ => {
                if let Some(mode) = parse_sudo_short_options(arg) {
                    return Some(mode);
                }
                if arg.starts_with("--") {
                    if let Some((flag, _)) = arg.split_once('=') {
                        if sudo_option_takes_value(flag) {
                            skip_next = true;
                            i += 1;
                            continue;
                        }
                    }
                    if sudo_option_takes_value(arg) {
                        skip_next = true;
                        i += 1;
                        continue;
                    }
                } else if (arg.len() == 2 && sudo_option_takes_value(arg))
                    || (arg.len() > 2 && sudo_option_takes_value(&arg[..2]))
                {
                    skip_next = true;
                    i += 1;
                    continue;
                }
            }
        }
        i += 1;
    }

    Some(SudoMode::Plain)
}

fn parse_sudo_short_options(arg: &str) -> Option<SudoMode> {
    if !arg.starts_with('-') || arg.len() < 2 || arg.starts_with("--") {
        return None;
    }
    for ch in arg.chars().skip(1) {
        match ch {
            'n' => return Some(SudoMode::NonInteractive),
            'A' => return Some(SudoMode::Askpass),
            'S' => return Some(SudoMode::Stdin),
            _ => {}
        }
    }
    None
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

fn is_sudo_command(args: &[String]) -> bool {
    args.first()
        .and_then(|arg| Path::new(arg).file_name())
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "sudo")
}

pub(crate) fn ensure_sudo_stdin_flag(args: &mut Vec<String>) {
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
