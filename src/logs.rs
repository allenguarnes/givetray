use crate::{
    config::clear_runtime_state, AppState, LogLink, PendingLogLink, UiEvent, LOG_LINK_CLICK_SLOP,
    LOG_LINK_TAG_NAME, MAX_LOG_LINES,
};
use async_channel::Receiver;
use glib::{MainContext, Propagation};
use gtk::gdk;
use gtk::prelude::*;
use std::fs::{self, File};
use std::io::{LineWriter, Write};
use std::path::Path;
use std::{cell::RefCell, rc::Rc};

pub(crate) const PROFILE_LOCK_ACTION_BLOCKED_MESSAGE: &str =
    "profile already open in another session; start/stop and configuration save are disabled";

pub(crate) fn profile_lock_action_blocked_message() -> String {
    PROFILE_LOCK_ACTION_BLOCKED_MESSAGE.to_string()
}

pub(crate) fn clear_runtime_state_after_exit(runtime_state_path: Option<&Path>) {
    if let Some(path) = runtime_state_path {
        if let Err(err) = clear_runtime_state(path) {
            eprintln!("failed to clear runtime state after exit: {err}");
        }
    }
}

pub(crate) fn build_logs_window() -> (
    gtk::Window,
    gtk::TextView,
    gtk::TextBuffer,
    gtk::Button,
    gtk::Button,
    gtk::Label,
) {
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("Logs");
    window.set_default_size(820, 520);

    let tag_table = gtk::TextTagTable::new();
    let link_tag = gtk::TextTag::new(Some(LOG_LINK_TAG_NAME));
    link_tag.set_foreground(Some("#1c71d8"));
    link_tag.set_underline(gtk::pango::Underline::Single);
    tag_table.add(&link_tag);

    let buffer = gtk::TextBuffer::new(Some(&tag_table));
    let text_view = gtk::TextView::with_buffer(&buffer);
    text_view.set_widget_name("logs-view");
    text_view.set_editable(false);
    text_view.set_monospace(true);
    text_view.set_cursor_visible(false);
    text_view.add_events(
        gdk::EventMask::BUTTON_PRESS_MASK
            | gdk::EventMask::BUTTON_RELEASE_MASK
            | gdk::EventMask::POINTER_MOTION_MASK
            | gdk::EventMask::LEAVE_NOTIFY_MASK,
    );

    text_view.set_left_margin(8);
    text_view.set_right_margin(8);
    text_view.set_top_margin(8);
    text_view.set_bottom_margin(8);

    let clear_button = gtk::Button::new();
    let clear_icon = gtk::Image::from_icon_name(Some("edit-clear"), gtk::IconSize::Button);
    let clear_label = gtk::Label::new(Some("Clear"));
    let clear_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    clear_box.pack_start(&clear_icon, false, false, 0);
    clear_box.pack_start(&clear_label, false, false, 0);
    clear_button.add(&clear_box);

    let copy_button = gtk::Button::new();
    let copy_icon = gtk::Image::from_icon_name(Some("edit-copy"), gtk::IconSize::Button);
    let copy_label = gtk::Label::new(Some("Copy All"));
    let copy_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    copy_box.pack_start(&copy_icon, false, false, 0);
    copy_box.pack_start(&copy_label, false, false, 0);
    copy_button.add(&copy_box);

    let status_label = gtk::Label::new(Some("0 lines"));
    status_label.set_halign(gtk::Align::Start);
    status_label.set_xalign(0.0);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    actions.set_halign(gtk::Align::Fill);
    actions.set_margin_start(8);
    actions.set_margin_end(8);
    actions.set_margin_top(8);
    actions.set_margin_bottom(4);
    actions.pack_start(&status_label, true, true, 0);
    actions.pack_start(&copy_button, false, false, 0);
    actions.pack_start(&clear_button, false, false, 0);

    let scroller = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    scroller.set_hexpand(true);
    scroller.set_vexpand(true);
    scroller.add(&text_view);

    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    container.set_hexpand(true);
    container.set_vexpand(true);
    container.pack_start(&actions, false, false, 0);
    container.pack_start(&scroller, true, true, 0);

    window.add(&container);
    window.connect_delete_event(|window, _| {
        window.hide();
        Propagation::Stop
    });

    window.show_all();
    window.hide();

    (
        window,
        text_view,
        buffer,
        clear_button,
        copy_button,
        status_label,
    )
}

pub(crate) fn setup_logs_handlers(state: Rc<RefCell<AppState>>) {
    let clear_button = state.borrow().logs_clear_button.clone();
    let copy_button = state.borrow().logs_copy_button.clone();
    let buffer = state.borrow().logs_buffer.clone();
    let text_view = state.borrow().logs_view.clone();
    let status_label = state.borrow().logs_status_label.clone();
    let pending_link = Rc::new(RefCell::new(None::<PendingLogLink>));

    let state_clear = state.clone();
    let buffer_clear = buffer.clone();
    let status_clear = status_label.clone();
    clear_button.connect_clicked(move |_| {
        let mut state = state_clear.borrow_mut();
        state.log_lines.clear();
        state.log_links.clear();
        buffer_clear.set_text("");
        set_logs_status(&status_clear, 0, Some("cleared"));
    });

    let buffer_copy = buffer.clone();
    let state_copy = state.clone();
    let status_copy = status_label.clone();
    copy_button.connect_clicked(move |_| {
        let text = buffer_text(&buffer_copy);
        let clipboard = gtk::Clipboard::get(&gdk::SELECTION_CLIPBOARD);
        clipboard.set_text(&text);
        let line_count = state_copy.borrow().log_lines.len();
        set_logs_status(&status_copy, line_count, Some("copied"));
    });

    let pending_press = pending_link.clone();
    let state_press = state.clone();
    text_view.connect_button_press_event(move |text_view, event| {
        if event.button() != 1 {
            pending_press.borrow_mut().take();
            return Propagation::Proceed;
        }

        let candidate = event.window().and_then(|event_window| {
            let (x, y) = event.position();
            iter_at_pointer(text_view, &event_window, x, y).and_then(|iter| {
                let state = state_press.borrow();
                log_link_at_iter(&state, &iter).map(|uri| PendingLogLink { uri, x, y })
            })
        });

        *pending_press.borrow_mut() = candidate;
        Propagation::Proceed
    });

    let state_click = state.clone();
    let status_click = status_label.clone();
    let pending_release = pending_link.clone();
    text_view.connect_button_release_event(move |text_view, event| {
        if event.button() != 1 {
            pending_release.borrow_mut().take();
            return Propagation::Proceed;
        }

        let Some(event_window) = event.window() else {
            pending_release.borrow_mut().take();
            return Propagation::Proceed;
        };
        let (x, y) = event.position();
        let Some(iter) = iter_at_pointer(text_view, &event_window, x, y) else {
            pending_release.borrow_mut().take();
            return Propagation::Proceed;
        };

        let (uri, has_selection) = {
            let state = state_click.borrow();
            (
                log_link_at_iter(&state, &iter),
                state.logs_buffer.has_selection(),
            )
        };
        let activate = {
            let pending = pending_release.borrow();
            should_activate_log_link(pending.as_ref(), uri.as_deref(), x, y, has_selection)
        };
        pending_release.borrow_mut().take();
        if !activate {
            return Propagation::Proceed;
        }
        let Some(uri) = uri else {
            return Propagation::Proceed;
        };

        let (window, line_count) = {
            let state = state_click.borrow();
            (state.logs_window.clone(), state.log_lines.len())
        };

        match gtk::show_uri_on_window(Some(&window), &uri, event.time()) {
            Ok(()) => {
                set_logs_status(&status_click, line_count, Some("opened link"));
            }
            Err(err) => {
                append_log(
                    &mut state_click.borrow_mut(),
                    format!("failed to open URL {uri}: {err}"),
                );
            }
        }

        Propagation::Stop
    });

    let state_motion = state.clone();
    text_view.connect_motion_notify_event(move |text_view, event| {
        let active = event.window().and_then(|event_window| {
            let (x, y) = event.position();
            iter_at_pointer(text_view, &event_window, x, y).and_then(|iter| {
                let state = state_motion.borrow();
                log_link_at_iter(&state, &iter)
            })
        });

        set_logs_link_cursor(text_view, active.is_some());
        Propagation::Proceed
    });

    let pending_leave = pending_link.clone();
    text_view.connect_leave_notify_event(move |text_view, _| {
        pending_leave.borrow_mut().take();
        set_logs_link_cursor(text_view, false);
        Propagation::Proceed
    });
}

pub(crate) fn set_logs_status(label: &gtk::Label, line_count: usize, detail: Option<&str>) {
    let text = match detail {
        Some(detail) => format!("{line_count} lines | {detail}"),
        None => format!("{line_count} lines"),
    };
    label.set_text(&text);
}

pub(crate) fn apply_process_exited(state: &mut AppState, code: Option<i32>) {
    if state.process_exit_reported {
        return;
    }

    let owned_was_cleared = state.owned_pgid.is_none() && state.child.is_none();

    if owned_was_cleared {
        clear_runtime_state_after_exit(state.runtime_state_path.as_deref());
        state.restored_running = false;
        state.start_stop_item.set_text("Start");
    }

    state.child = None;
    state.process_exit_reported = true;

    let msg = match code {
        Some(code) => format!("command exited with code {code}"),
        None => "command exited".to_string(),
    };
    append_log(state, msg);
}

pub(crate) fn apply_clear_runtime_state(state: &mut AppState) {
    let owned_was_cleared = state.owned_pgid.is_none() && state.child.is_none();

    if owned_was_cleared {
        clear_runtime_state_after_exit(state.runtime_state_path.as_deref());
        state.restored_running = false;
    }

    state.child = None;
    state.owned_pgid = None;
    state.owned_pid = None;
}

pub(crate) fn setup_log_receiver(state: Rc<RefCell<AppState>>, receiver: Receiver<UiEvent>) {
    MainContext::default().spawn_local(async move {
        while let Ok(event) = receiver.recv().await {
            let mut state = state.borrow_mut();
            match event {
                UiEvent::AppendLog(line) => append_log(&mut state, line),
                UiEvent::ProcessExited(code) => apply_process_exited(&mut state, code),
                UiEvent::SetRunning(running) => {
                    state
                        .start_stop_item
                        .set_text(if running { "Stop" } else { "Start" });
                }
                UiEvent::ClearRuntimeState => apply_clear_runtime_state(&mut state),
            }
        }
    });
}

pub(crate) fn buffer_text(buffer: &gtk::TextBuffer) -> String {
    let start = buffer.start_iter();
    let end = buffer.end_iter();
    buffer
        .text(&start, &end, true)
        .unwrap_or_default()
        .to_string()
}

fn is_log_link_boundary(ch: char) -> bool {
    !matches!(
        ch,
        'a'..='z'
            | 'A'..='Z'
            | '0'..='9'
            | '/'
            | '_'
            | '-'
            | '.'
            | '?'
            | '&'
            | '%'
            | '#'
            | '@'
            | '~'
            | '+'
    )
}

fn log_link_scheme_len(tail: &str) -> Option<usize> {
    const LOG_LINK_SCHEMES: [&str; 4] = ["https://", "http://", "file://", "mailto:"];

    LOG_LINK_SCHEMES.iter().find_map(|scheme| {
        tail.get(..scheme.len())
            .filter(|prefix| prefix.eq_ignore_ascii_case(scheme))
            .map(|_| scheme.len())
    })
}

fn trim_log_link_end(line: &str, start_byte: usize, mut end_byte: usize) -> usize {
    while end_byte > start_byte {
        let Some(ch) = line[..end_byte].chars().next_back() else {
            break;
        };

        let trim = match ch {
            '.' | ',' | ';' | '!' | '?' => true,
            ')' => {
                let candidate = &line[start_byte..end_byte];
                candidate.matches(')').count() > candidate.matches('(').count()
            }
            ']' => {
                let candidate = &line[start_byte..end_byte];
                candidate.matches(']').count() > candidate.matches('[').count()
            }
            '}' => {
                let candidate = &line[start_byte..end_byte];
                candidate.matches('}').count() > candidate.matches('{').count()
            }
            _ => false,
        };

        if !trim {
            break;
        }

        end_byte -= ch.len_utf8();
    }

    end_byte
}

pub(crate) fn extract_log_links(line: &str) -> Vec<LogLink> {
    let mut links = Vec::new();
    let mut byte_idx = 0usize;

    while byte_idx < line.len() {
        let tail = &line[byte_idx..];
        let Some(scheme_len) = log_link_scheme_len(tail) else {
            let Some(ch) = tail.chars().next() else {
                break;
            };
            byte_idx += ch.len_utf8();
            continue;
        };

        if byte_idx > 0 {
            let prev = line[..byte_idx].chars().next_back().unwrap_or(' ');
            if !is_log_link_boundary(prev) {
                byte_idx += scheme_len;
                continue;
            }
        }

        let mut end_byte = byte_idx + scheme_len;
        while end_byte < line.len() {
            let Some(ch) = line[end_byte..].chars().next() else {
                break;
            };
            if ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '\'') {
                break;
            }
            end_byte += ch.len_utf8();
        }

        end_byte = trim_log_link_end(line, byte_idx, end_byte);
        if end_byte <= byte_idx + scheme_len {
            byte_idx += scheme_len;
            continue;
        }

        links.push(LogLink {
            start_char: line[..byte_idx].chars().count() as i32,
            end_char: line[..end_byte].chars().count() as i32,
            uri: line[byte_idx..end_byte].to_string(),
        });

        byte_idx = end_byte;
    }

    links
}

fn append_log_line_to_buffer(buffer: &gtk::TextBuffer, line: &str, links: &[LogLink]) {
    let mut start_iter = buffer.end_iter();
    let line_start_offset = start_iter.offset();
    buffer.insert(&mut start_iter, line);
    let mut end_iter = buffer.end_iter();
    buffer.insert(&mut end_iter, "\n");

    for link in links {
        let start = buffer.iter_at_offset(line_start_offset + link.start_char);
        let end = buffer.iter_at_offset(line_start_offset + link.end_char);
        buffer.apply_tag_by_name(LOG_LINK_TAG_NAME, &start, &end);
    }
}

fn drop_oldest_log_line_from_buffer(buffer: &gtk::TextBuffer) {
    let mut start = buffer.start_iter();
    let mut end = start;
    if end.forward_line() {
        buffer.delete(&mut start, &mut end);
    } else {
        buffer.set_text("");
    }
}

pub(crate) fn log_link_at_iter(state: &AppState, iter: &gtk::TextIter) -> Option<String> {
    let line_idx = usize::try_from(iter.line()).ok()?;
    let char_offset = iter.line_offset();

    state
        .log_links
        .get(line_idx)?
        .iter()
        .find(|link| char_offset >= link.start_char && char_offset < link.end_char)
        .map(|link| link.uri.clone())
}

pub(crate) fn should_activate_log_link(
    pending: Option<&PendingLogLink>,
    released_uri: Option<&str>,
    release_x: f64,
    release_y: f64,
    has_selection: bool,
) -> bool {
    let Some(pending) = pending else {
        return false;
    };
    let Some(released_uri) = released_uri else {
        return false;
    };
    if has_selection || pending.uri != released_uri {
        return false;
    }

    let dx = release_x - pending.x;
    let dy = release_y - pending.y;
    (dx * dx) + (dy * dy) <= LOG_LINK_CLICK_SLOP * LOG_LINK_CLICK_SLOP
}

fn iter_at_pointer(
    text_view: &gtk::TextView,
    event_window: &gdk::Window,
    x: f64,
    y: f64,
) -> Option<gtk::TextIter> {
    let window_type = text_view.window_type(event_window);
    let (buffer_x, buffer_y) = text_view.window_to_buffer_coords(window_type, x as i32, y as i32);
    text_view
        .iter_at_position(buffer_x, buffer_y)
        .map(|(iter, _trailing)| iter)
}

fn set_logs_link_cursor(text_view: &gtk::TextView, active: bool) {
    let Some(window) = gtk::prelude::TextViewExt::window(text_view, gtk::TextWindowType::Text)
    else {
        return;
    };

    let cursor = if active {
        gdk::Display::default().and_then(|display| gdk::Cursor::from_name(&display, "pointer"))
    } else {
        None
    };

    window.set_cursor(cursor.as_ref());
}

fn open_log_file_writer(path: &Path) -> Result<LineWriter<File>, std::io::Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    Ok(LineWriter::new(file))
}

pub(crate) fn append_log_to_file(
    writer: &mut Option<LineWriter<File>>,
    path: &Path,
    line: &str,
) -> Result<(), std::io::Error> {
    if writer.is_none() {
        *writer = Some(open_log_file_writer(path)?);
    }

    if let Some(writer) = writer.as_mut() {
        writeln!(writer, "{line}")?;
    }

    Ok(())
}

pub(crate) fn strip_ansi_codes(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut result = String::with_capacity(line.len());
    let mut chunk_start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }

        if chunk_start < i {
            result.push_str(&line[chunk_start..i]);
        }

        i += 1;
        if i >= bytes.len() {
            chunk_start = i;
            break;
        }

        match bytes[i] {
            b'[' => {
                i += 1;
                while i < bytes.len() {
                    if (0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b']' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            b'P' | b'X' | b'^' | b'_' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            _ => {
                i += 1;
            }
        }

        chunk_start = i;
    }

    if chunk_start < line.len() {
        result.push_str(&line[chunk_start..]);
    }

    result
}

pub(crate) fn append_log(state: &mut AppState, line: String) {
    let clean_line = strip_ansi_codes(&line);
    let links = extract_log_links(&clean_line);
    if state.log_lines.len() >= MAX_LOG_LINES {
        state.log_lines.pop_front();
        state.log_links.pop_front();
        drop_oldest_log_line_from_buffer(&state.logs_buffer);
    }

    append_log_line_to_buffer(&state.logs_buffer, &clean_line, &links);

    if let Some(path) = state.log_file_path.as_deref() {
        if let Err(err) = append_log_to_file(&mut state.log_file_writer, path, &clean_line) {
            eprintln!("failed to write log file at {}: {err}", path.display());
            state.log_file_writer = None;
        }
    }

    state.log_lines.push_back(clean_line);
    state.log_links.push_back(links);

    let mut end_iter = state.logs_buffer.end_iter();
    state
        .logs_view
        .scroll_to_iter(&mut end_iter, 0.0, false, 0.0, 0.0);

    set_logs_status(&state.logs_status_label, state.log_lines.len(), None);
}
