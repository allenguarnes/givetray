use crate::{AppState, UiEvent, BG_CHILD_ENV};
use async_channel::Sender;
use gtk::prelude::*;
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

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

    let mut cmd = Command::new(&args[0]);
    cmd.env_remove(BG_CHILD_ENV);
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

pub(crate) fn stop_command(state: Rc<RefCell<AppState>>, ui_tx: Sender<UiEvent>) {
    let child = state.borrow_mut().child.take();
    if let Some(mut child) = child {
        thread::spawn(move || {
            terminate_child(&mut child, Duration::from_secs(2));
            let code = child.wait().ok().and_then(|status| status.code());
            let _ = ui_tx.send_blocking(UiEvent::ProcessExited(code));
        });
    }
}

pub(crate) fn stop_command_blocking(state: Rc<RefCell<AppState>>) {
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
