use crate::cli::should_expose_configuration;
use crate::config::save_configuration;
use crate::desktop::{applications_desktop_path, apply_desktop_actions, autostart_desktop_path};
use crate::logs::{append_log, buffer_text};
use crate::process::{start_command, stop_command, stop_command_blocking};
use crate::{AppState, ConfigCloseAction, UiEvent, MAX_UNDO};
use async_channel::Sender;
use glib::{ControlFlow, LogLevels, Propagation};
use gtk::gdk;
use gtk::gdk_pixbuf::{InterpType, Pixbuf};
use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use tray_icon::menu::MenuEvent;

pub(crate) fn build_config_window(
    profile: &str,
    command: &str,
    autostart: bool,
    log_to_file: bool,
) -> (
    gtk::Window,
    gtk::TextView,
    gtk::TextBuffer,
    gtk::CheckButton,
    gtk::CheckButton,
    gtk::CheckButton,
    gtk::CheckButton,
    gtk::Button,
    gtk::Label,
) {
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title(&format!("Configuration ({profile})"));
    window.set_default_size(860, 300);

    let buffer = gtk::TextBuffer::new(None::<&gtk::TextTagTable>);
    buffer.set_text(command);
    let text_view = gtk::TextView::with_buffer(&buffer);
    text_view.set_monospace(true);
    text_view.set_wrap_mode(gtk::WrapMode::WordChar);
    text_view.set_hexpand(true);
    text_view.set_vexpand(true);
    text_view.set_left_margin(8);
    text_view.set_right_margin(8);
    text_view.set_top_margin(8);
    text_view.set_bottom_margin(8);

    let label = gtk::Label::new(Some("Command or script"));
    label.set_halign(gtk::Align::Start);
    label.set_margin_start(8);
    label.set_margin_end(8);
    label.set_margin_top(12);
    label.set_margin_bottom(4);

    let scroller = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    scroller.set_hexpand(true);
    scroller.set_vexpand(true);
    scroller.add(&text_view);

    let hint = gtk::Label::new(Some("Enter the terminal command to run for this profile."));
    hint.set_halign(gtk::Align::Start);
    hint.set_xalign(0.0);
    hint.set_margin_start(8);
    hint.set_margin_end(8);
    hint.set_margin_bottom(4);

    let autostart_toggle =
        gtk::CheckButton::with_label("Run command automatically when givetray launches");
    autostart_toggle.set_active(autostart);
    autostart_toggle.set_halign(gtk::Align::Start);
    autostart_toggle.set_tooltip_text(Some(
        "Runs this profile command when the givetray instance starts.",
    ));

    let log_to_file_toggle = gtk::CheckButton::with_label("Write logs to file");
    log_to_file_toggle.set_active(log_to_file);
    log_to_file_toggle.set_halign(gtk::Align::Start);
    log_to_file_toggle.set_tooltip_text(Some(
        "When enabled, command logs are appended to a profile log file.",
    ));

    let apps_toggle = gtk::CheckButton::with_label("Create Applications entry (.desktop)");
    apps_toggle.set_halign(gtk::Align::Start);
    apps_toggle.set_tooltip_text(Some(
        "Creates or removes ~/.local/share/applications desktop entry for this profile.",
    ));

    let autostart_desktop_toggle =
        gtk::CheckButton::with_label("Enable desktop session autostart (~/.config/autostart)");
    autostart_desktop_toggle.set_halign(gtk::Align::Start);
    autostart_desktop_toggle.set_tooltip_text(Some(
        "Creates or removes ~/.config/autostart desktop entry for this profile.",
    ));

    let save_button = gtk::Button::new();
    let save_icon = gtk::Image::from_icon_name(Some("media-floppy"), gtk::IconSize::Button);
    let save_label = gtk::Label::new(Some("Save"));
    let save_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    save_box.pack_start(&save_icon, false, false, 0);
    save_box.pack_start(&save_label, false, false, 0);
    save_button.add(&save_box);

    let options = gtk::Box::new(gtk::Orientation::Vertical, 4);
    options.pack_start(&autostart_toggle, false, false, 0);
    options.pack_start(&log_to_file_toggle, false, false, 0);
    options.pack_start(&apps_toggle, false, false, 0);
    options.pack_start(&autostart_desktop_toggle, false, false, 0);

    let status_label = gtk::Label::new(Some("Saved"));
    status_label.set_halign(gtk::Align::End);
    status_label.set_xalign(1.0);

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    footer.set_halign(gtk::Align::Fill);
    footer.set_valign(gtk::Align::Start);
    footer.set_margin_start(8);
    footer.set_margin_end(8);
    footer.set_margin_top(6);
    footer.set_margin_bottom(8);
    footer.pack_start(&options, true, true, 0);
    footer.pack_start(&status_label, false, false, 0);
    footer.pack_start(&save_button, false, false, 0);
    save_button.set_valign(gtk::Align::Center);
    save_button.set_halign(gtk::Align::End);
    save_button.set_vexpand(false);
    save_button.set_hexpand(false);

    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    container.set_hexpand(true);
    container.set_vexpand(true);
    container.pack_start(&label, false, false, 0);
    container.pack_start(&hint, false, false, 0);
    container.pack_start(&scroller, true, true, 0);
    container.pack_start(&footer, false, false, 0);

    window.add(&container);

    window.show_all();
    window.hide();

    (
        window,
        text_view,
        buffer,
        autostart_toggle,
        log_to_file_toggle,
        apps_toggle,
        autostart_desktop_toggle,
        save_button,
        status_label,
    )
}

pub(crate) fn build_about_window(window_icon: Option<&Pixbuf>) -> gtk::Window {
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("About");
    window.set_default_size(460, 300);
    window.set_resizable(true);

    let title = gtk::Label::new(None);
    title.set_markup("<b>givetray</b>");
    title.set_halign(gtk::Align::Start);
    title.set_xalign(0.0);

    let subtitle = gtk::Label::new(Some("Run terminal commands from the Linux system tray"));
    subtitle.set_halign(gtk::Align::Start);
    subtitle.set_xalign(0.0);
    subtitle.set_margin_bottom(6);

    let version = gtk::Label::new(Some(&format!("Version: {}", env!("CARGO_PKG_VERSION"))));
    version.set_halign(gtk::Align::Start);
    version.set_xalign(0.0);

    let author = gtk::Label::new(Some("Author: Allen Guarnes"));
    author.set_halign(gtk::Align::Start);
    author.set_xalign(0.0);

    let github = gtk::LinkButton::with_label("https://github.com/allenguarnes/givetray", "GitHub");
    github.set_halign(gtk::Align::Start);
    github.set_margin_top(2);

    let coffee =
        gtk::LinkButton::with_label("https://buymeacoffee.com/allenguarnes", "Buy Me a Coffee");
    coffee.set_halign(gtk::Align::Start);

    let description = gtk::Label::new(Some(
        "Run terminal commands from a tray icon with profile-based settings and logs.",
    ));
    description.set_halign(gtk::Align::Start);
    description.set_xalign(0.0);
    description.set_line_wrap(true);

    let licenses = gtk::Label::new(Some("License: MIT OR Apache-2.0"));
    licenses.set_halign(gtk::Align::Start);
    licenses.set_xalign(0.0);
    licenses.set_line_wrap(true);

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    header.set_halign(gtk::Align::Start);

    if let Some(icon) = window_icon {
        let about_icon = icon
            .scale_simple(56, 56, InterpType::Bilinear)
            .unwrap_or_else(|| icon.clone());
        let icon_image = gtk::Image::from_pixbuf(Some(&about_icon));
        icon_image.set_halign(gtk::Align::Start);
        header.pack_start(&icon_image, false, false, 0);
    }

    let title_block = gtk::Box::new(gtk::Orientation::Vertical, 2);
    title_block.pack_start(&title, false, false, 0);
    title_block.pack_start(&subtitle, false, false, 0);
    header.pack_start(&title_block, false, false, 0);

    let links = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    links.pack_start(&github, false, false, 0);
    links.pack_start(&coffee, false, false, 0);

    let divider = gtk::Separator::new(gtk::Orientation::Horizontal);

    let container = gtk::Box::new(gtk::Orientation::Vertical, 6);
    container.set_margin_start(12);
    container.set_margin_end(12);
    container.set_margin_top(12);
    container.set_margin_bottom(12);
    container.pack_start(&header, false, false, 0);
    container.pack_start(&description, false, false, 0);
    container.pack_start(&divider, false, false, 4);
    container.pack_start(&version, false, false, 0);
    container.pack_start(&author, false, false, 0);
    container.pack_start(&links, false, false, 0);
    container.pack_start(&licenses, false, false, 0);

    window.add(&container);
    window.connect_delete_event(|window, _| {
        window.hide();
        Propagation::Stop
    });

    window.show_all();
    window.hide();

    window
}

pub(crate) fn setup_config_handlers(state: Rc<RefCell<AppState>>) {
    let view = state.borrow().config_view.clone();
    let buffer = state.borrow().config_buffer.clone();
    let window = state.borrow().config_window.clone();
    let autostart_toggle = state.borrow().config_autostart.clone();
    let log_to_file_toggle = state.borrow().config_log_to_file.clone();
    let save_button = state.borrow().config_save_button.clone();
    let apps_toggle = state.borrow().config_applications.clone();
    let system_autostart_toggle = state.borrow().config_system_autostart.clone();

    let state_close = state.clone();
    let buffer_close = buffer.clone();
    let autostart_toggle_close = autostart_toggle.clone();
    let log_to_file_toggle_close = log_to_file_toggle.clone();
    let apps_toggle_close = apps_toggle.clone();
    let system_autostart_toggle_close = system_autostart_toggle.clone();
    window.connect_delete_event(move |window, _| {
        let current_text = buffer_text(&buffer_close);
        let has_unsaved = {
            let app = state_close.borrow();
            config_has_unsaved_changes(
                &app,
                &current_text,
                autostart_toggle_close.is_active(),
                log_to_file_toggle_close.is_active(),
                apps_toggle_close.is_active(),
                system_autostart_toggle_close.is_active(),
            )
        };

        if !has_unsaved {
            window.hide();
            return Propagation::Stop;
        }

        match show_config_close_dialog(window) {
            ConfigCloseAction::Save => {
                save_from_config_widgets(
                    state_close.clone(),
                    &buffer_close,
                    &log_to_file_toggle_close,
                    &apps_toggle_close,
                    &system_autostart_toggle_close,
                );
                window.hide();
            }
            ConfigCloseAction::Discard => {
                refresh_config_dirty_status(state_close.clone());
                window.hide();
            }
            ConfigCloseAction::Cancel => {}
        }

        Propagation::Stop
    });

    let state_save = state.clone();
    let buffer_save = buffer.clone();
    let log_to_file_toggle_save = log_to_file_toggle.clone();
    let apps_toggle_save = apps_toggle.clone();
    let system_autostart_save = system_autostart_toggle.clone();
    save_button.connect_clicked(move |_| {
        save_from_config_widgets(
            state_save.clone(),
            &buffer_save,
            &log_to_file_toggle_save,
            &apps_toggle_save,
            &system_autostart_save,
        );
    });

    let state_changed = state.clone();
    let buffer_changed = buffer.clone();
    buffer.connect_changed(move |_| {
        let text = buffer_text(&buffer_changed);
        let mut state = state_changed.borrow_mut();
        if state.config_ignore || text == state.config_last {
            return;
        }
        let last = state.config_last.clone();
        state.config_undo.push(last);
        if state.config_undo.len() > MAX_UNDO {
            state.config_undo.remove(0);
        }
        state.config_last = text;
        state.config_redo.clear();
        drop(state);
        refresh_config_dirty_status(state_changed.clone());
    });

    let state_autostart_toggled = state.clone();
    autostart_toggle.connect_toggled(move |_| {
        refresh_config_dirty_status(state_autostart_toggled.clone());
    });

    let state_logfile_toggled = state.clone();
    log_to_file_toggle.connect_toggled(move |_| {
        refresh_config_dirty_status(state_logfile_toggled.clone());
    });

    let state_apps_toggled = state.clone();
    apps_toggle.connect_toggled(move |_| {
        refresh_config_dirty_status(state_apps_toggled.clone());
    });

    let state_system_toggled = state.clone();
    system_autostart_toggle.connect_toggled(move |_| {
        refresh_config_dirty_status(state_system_toggled.clone());
    });

    let state_keys = state.clone();
    let buffer_keys = buffer.clone();
    view.connect_key_press_event(move |_, event| {
        let key = event.keyval();
        let modifiers = event.state();
        let ctrl = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
        let shift = modifiers.contains(gdk::ModifierType::SHIFT_MASK);

        if ctrl && shift && (key == gdk::keys::constants::Z || key == gdk::keys::constants::z) {
            if let Some(next) = {
                let mut state = state_keys.borrow_mut();
                let next = state.config_redo.pop();
                if let Some(ref value) = next {
                    let last = state.config_last.clone();
                    state.config_undo.push(last);
                    state.config_last = value.clone();
                    state.config_ignore = true;
                }
                next
            } {
                buffer_keys.set_text(&next);
                state_keys.borrow_mut().config_ignore = false;
                refresh_config_dirty_status(state_keys.clone());
                return Propagation::Stop;
            }
            return Propagation::Proceed;
        }

        if ctrl && !shift && (key == gdk::keys::constants::z || key == gdk::keys::constants::Z) {
            if let Some(prev) = {
                let mut state = state_keys.borrow_mut();
                let prev = state.config_undo.pop();
                if let Some(ref value) = prev {
                    let last = state.config_last.clone();
                    state.config_redo.push(last);
                    state.config_last = value.clone();
                    state.config_ignore = true;
                }
                prev
            } {
                buffer_keys.set_text(&prev);
                state_keys.borrow_mut().config_ignore = false;
                refresh_config_dirty_status(state_keys.clone());
                return Propagation::Stop;
            }
            return Propagation::Proceed;
        }

        if ctrl && (key == gdk::keys::constants::y || key == gdk::keys::constants::Y) {
            if let Some(next) = {
                let mut state = state_keys.borrow_mut();
                let next = state.config_redo.pop();
                if let Some(ref value) = next {
                    let last = state.config_last.clone();
                    state.config_undo.push(last);
                    state.config_last = value.clone();
                    state.config_ignore = true;
                }
                next
            } {
                buffer_keys.set_text(&next);
                state_keys.borrow_mut().config_ignore = false;
                refresh_config_dirty_status(state_keys.clone());
                return Propagation::Stop;
            }
            return Propagation::Proceed;
        }

        Propagation::Proceed
    });
}

fn show_config_close_dialog(parent: &gtk::Window) -> ConfigCloseAction {
    let dialog = gtk::MessageDialog::new(
        Some(parent),
        gtk::DialogFlags::MODAL,
        gtk::MessageType::Question,
        gtk::ButtonsType::None,
        "You have unsaved configuration changes.",
    );
    dialog.set_secondary_text(Some("Save changes before closing this window?"));
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    dialog.add_button("Discard", gtk::ResponseType::No);
    dialog.add_button("Save", gtk::ResponseType::Yes);
    dialog.set_default_response(gtk::ResponseType::Yes);

    let response = dialog.run();
    dialog.close();

    match response {
        gtk::ResponseType::Yes => ConfigCloseAction::Save,
        gtk::ResponseType::No => ConfigCloseAction::Discard,
        _ => ConfigCloseAction::Cancel,
    }
}

fn save_from_config_widgets(
    state: Rc<RefCell<AppState>>,
    buffer: &gtk::TextBuffer,
    log_to_file_toggle: &gtk::CheckButton,
    apps_toggle: &gtk::CheckButton,
    system_autostart_toggle: &gtk::CheckButton,
) {
    let text = buffer_text(buffer);
    let saved = save_configuration(state.clone(), text, log_to_file_toggle.is_active());
    if saved {
        apply_desktop_actions(
            state.clone(),
            apps_toggle.is_active(),
            system_autostart_toggle.is_active(),
        );
        refresh_desktop_toggles(state.clone(), apps_toggle, system_autostart_toggle);
    }
    refresh_config_dirty_status(state);
}

fn config_has_unsaved_changes(
    state: &AppState,
    current_command: &str,
    current_autostart: bool,
    current_log_to_file: bool,
    current_applications: bool,
    current_system_autostart: bool,
) -> bool {
    current_command != state.saved_command
        || current_autostart != state.saved_autostart
        || current_log_to_file != state.saved_log_to_file
        || current_applications != state.config_saved_applications
        || current_system_autostart != state.config_saved_system_autostart
}

pub(crate) fn refresh_config_dirty_status(state: Rc<RefCell<AppState>>) {
    let (status_label, status_text) = {
        let app = state.borrow();
        if app.config_ignore {
            return;
        }

        let command = buffer_text(&app.config_buffer);
        let unsaved = config_has_unsaved_changes(
            &app,
            &command,
            app.config_autostart.is_active(),
            app.config_log_to_file.is_active(),
            app.config_applications.is_active(),
            app.config_system_autostart.is_active(),
        );
        (
            app.config_status_label.clone(),
            if unsaved { "Unsaved changes" } else { "Saved" },
        )
    };

    status_label.set_text(status_text);
}

pub(crate) fn refresh_desktop_toggles(
    state: Rc<RefCell<AppState>>,
    apps_toggle: &gtk::CheckButton,
    system_autostart_toggle: &gtk::CheckButton,
) {
    let access = state.borrow().persistent_config_access.clone();
    let enabled = access.is_some();
    let (apps_exists, autostart_exists) = if let Some(access) = access {
        (
            applications_desktop_path(&access.profile).is_some_and(|path| path.exists()),
            autostart_desktop_path(&access.profile).is_some_and(|path| path.exists()),
        )
    } else {
        (false, false)
    };

    {
        let mut app = state.borrow_mut();
        app.config_ignore = true;
        app.config_saved_applications = apps_exists;
        app.config_saved_system_autostart = autostart_exists;
    }

    apps_toggle.set_active(apps_exists);
    system_autostart_toggle.set_active(autostart_exists);
    apps_toggle.set_sensitive(enabled);
    system_autostart_toggle.set_sensitive(enabled);

    {
        let mut app = state.borrow_mut();
        app.config_ignore = false;
    }

    refresh_config_dirty_status(state);
}

pub(crate) fn setup_menu_polling(state: Rc<RefCell<AppState>>, ui_tx: Sender<UiEvent>) {
    glib::timeout_add_local(Duration::from_millis(150), move || {
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            let id = event.id;
            if id == "start-stop" {
                let running = state.borrow().child.is_some();
                if running {
                    stop_command(state.clone(), ui_tx.clone());
                } else {
                    start_command(state.clone(), ui_tx.clone());
                }
            } else if id == "logs" {
                let window = state.borrow().logs_window.clone();
                window.show_all();
                window.resize(820, 520);
            } else if id == "configure" {
                let can_configure = {
                    let state = state.borrow();
                    should_expose_configuration(state.persistent_config_access.as_ref())
                };
                if !can_configure {
                    append_log(
                        &mut state.borrow_mut(),
                        "configuration window is unavailable in ephemeral mode".to_string(),
                    );
                    continue;
                }
                let (
                    window,
                    view,
                    buffer,
                    autostart_toggle,
                    log_to_file_toggle,
                    command,
                    autostart,
                    log_to_file,
                ) = {
                    let state = state.borrow();
                    (
                        state.config_window.clone(),
                        state.config_view.clone(),
                        state.config_buffer.clone(),
                        state.config_autostart.clone(),
                        state.config_log_to_file.clone(),
                        state.saved_command.clone(),
                        state.saved_autostart,
                        state.saved_log_to_file,
                    )
                };
                let (apps_toggle, system_autostart_toggle) = {
                    let state = state.borrow();
                    (
                        state.config_applications.clone(),
                        state.config_system_autostart.clone(),
                    )
                };
                {
                    let mut state = state.borrow_mut();
                    state.config_ignore = true;
                    state.config_last = command.clone();
                    state.config_undo.clear();
                    state.config_redo.clear();
                }
                buffer.set_text(&command);
                autostart_toggle.set_active(autostart);
                log_to_file_toggle.set_active(log_to_file);
                refresh_desktop_toggles(state.clone(), &apps_toggle, &system_autostart_toggle);
                refresh_config_dirty_status(state.clone());
                window.show_all();
                view.grab_focus();
            } else if id == "about" {
                let window = state.borrow().about_window.clone();
                window.show_all();
            } else if id == "exit" {
                stop_command_blocking(state.clone());
                gtk::main_quit();
            }
        }

        ControlFlow::Continue
    });
}

pub(crate) fn install_log_filters() {
    glib::log_set_handler(
        Some("libayatana-appindicator"),
        LogLevels::LEVEL_WARNING,
        false,
        false,
        |_domain, _level, _message| {},
    );
}

pub(crate) fn install_css() {
    let provider = gtk::CssProvider::new();
    let css = b"
        textview,
        textview text {
            font-family: monospace;
            font-size: 11pt;
        }
    ";
    provider.load_from_data(css).expect("failed to load CSS");

    if let Some(screen) = gdk::Screen::default() {
        gtk::StyleContext::add_provider_for_screen(
            &screen,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

pub(crate) fn setup_process_watcher(state: Rc<RefCell<AppState>>, ui_tx: Sender<UiEvent>) {
    glib::timeout_add_local(Duration::from_millis(500), move || {
        let mut should_emit = None;
        {
            let mut state = state.borrow_mut();
            if let Some(child) = state.child.as_mut() {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        should_emit = Some(status.code());
                        state.child = None;
                    }
                    Ok(None) => {}
                    Err(err) => {
                        append_log(&mut state, format!("failed to check command status: {err}"));
                    }
                }
            }
        }

        if let Some(code) = should_emit {
            let _ = ui_tx.send_blocking(UiEvent::ProcessExited(code));
        }

        ControlFlow::Continue
    });
}
