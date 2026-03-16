use crate::{config::save_runtime_state, AppState, RuntimeOwnershipState, UiEvent, BG_CHILD_ENV};
use async_channel::Sender;
use gtk::prelude::*;
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zeroize::Zeroizing;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeReconcileResult {
    RestoreRunning,
    ClearStale,
    IgnoreInvalid,
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

    if is_group_alive(state.pgid as i32) {
        RuntimeReconcileResult::RestoreRunning
    } else {
        RuntimeReconcileResult::ClearStale
    }
}

pub fn is_process_group_alive(pgid: i32) -> bool {
    unsafe { libc::kill(pgid, 0) == 0 }
}

pub fn stop_process_group(pgid: i32, timeout: Duration) -> bool {
    if pgid == 0 {
        return false;
    }

    if !is_process_group_alive(pgid) {
        return false;
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
    let pids = get_process_group_pids(pgid);
    if pids.is_empty() {
        return unsafe { libc::kill(-pgid, signal) == 0 };
    }

    for pid in pids {
        unsafe {
            libc::kill(pid, signal);
        }
    }

    true
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

    let mut child = cmd
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
    let runtime_state_path = runtime_state_path
        .as_ref()
        .ok_or("runtime state path not set")?;

    let started_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system time before epoch")?
        .as_millis() as u64;

    let state = RuntimeOwnershipState {
        pid: owned_pid,
        pgid: owned_pgid as u32,
        started_at_unix_ms,
        command_label: command.to_string(),
        profile_name: profile_name.map(String::from),
        ephemeral,
    };

    save_runtime_state(runtime_state_path, &state)
}

pub(crate) fn start_command(state: Rc<RefCell<AppState>>, ui_tx: Sender<UiEvent>) {
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

    let mut spawn_result =
        match spawn_command_in_new_process_group(&args[0], &args[1..], sudo_password.is_some()) {
            Ok(result) => result,
            Err(err) => {
                let _ = ui_tx.send_blocking(UiEvent::AppendLog(err));
                return;
            }
        };

    if let Some(password) = sudo_password {
        if let Some(mut stdin) = spawn_result.child.stdin.take() {
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
        let _ = ui_tx.send_blocking(UiEvent::AppendLog(format!(
            "failed to persist runtime state: {err}"
        )));
    }

    state.borrow_mut().child = Some(spawn_result.child);
    state.borrow_mut().owned_pid = Some(spawn_result.owned_pid);
    state.borrow_mut().owned_pgid = Some(spawn_result.owned_pgid);

    let _ = ui_tx.send_blocking(UiEvent::SetRunning(true));
    let _ = ui_tx.send_blocking(UiEvent::AppendLog("command started".to_string()));
}

pub(crate) fn stop_command(state: Rc<RefCell<AppState>>, ui_tx: Sender<UiEvent>) {
    let owned_pgid = state.borrow_mut().owned_pgid.take();
    let child = state.borrow_mut().child.take();

    let pgid = owned_pgid.or_else(|| child.as_ref().map(|c| c.id() as i32));

    if let Some(pgid) = pgid {
        thread::spawn(move || {
            let stopped = stop_process_group(pgid, Duration::from_secs(2));

            if let Some(mut child) = child {
                let _ = child.wait();
            }

            if stopped {
                let _ = ui_tx.send_blocking(UiEvent::ProcessExited(None));
            } else {
                let _ = ui_tx.send_blocking(UiEvent::AppendLog(
                    "failed to stop process group".to_string(),
                ));
                let _ = ui_tx.send_blocking(UiEvent::ProcessExited(None));
            }
        });
    }
}

pub(crate) fn stop_command_blocking(state: Rc<RefCell<AppState>>) {
    let owned_pgid = state.borrow_mut().owned_pgid.take();
    let child = state.borrow_mut().child.take();

    let pgid = owned_pgid.or_else(|| child.as_ref().map(|c| c.id() as i32));

    if let Some(pgid) = pgid {
        let stopped = stop_process_group(pgid, Duration::from_secs(2));

        if let Some(mut child) = child {
            let _ = child.wait();
        }

        if !stopped {
            eprintln!("failed to stop process group");
        }
    }
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
        .and_then(|arg| Path::new(arg).file_name())
        .and_then(|name| name.to_str())
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
