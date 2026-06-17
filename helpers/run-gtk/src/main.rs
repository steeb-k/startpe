// SPDX-License-Identifier: GPL-3.0-or-later
#![windows_subsystem = "windows"]

//! GTK4/Libadwaita **Run** window for Windows PE — a thin client of the
//! winrx-creator GTK4 runtime, the libadwaita counterpart to StartPE's Win32/GDI
//! Run box (`startpe/src/run_window.rs`). The command/history core is reused
//! (`run_exec`); the UI is a small libadwaita dialog: prompt, an "Open:" entry
//! with a history dropdown, and Browse / Cancel / OK. History is shared with
//! StartPE via `HKCU\Software\StartPE\RunHistory`.

mod run_exec;

use adw::prelude::*;
use gtk::{gio, glib};
use gtk::{
    Align, Box as GtkBox, Button, Entry, Image, Label, ListBox, ListBoxRow, MenuButton,
    Orientation, PolicyType, Popover, ScrolledWindow, SelectionMode,
};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, SetForegroundWindow};

const APP_ID: &str = "org.winrx.PeRun";
const PROMPT: &str = "Type the name of a program, folder, document, or Internet resource, and StartPE will open it for you.";

fn build_ui(app: &adw::Application) {
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);

    let history = run_exec::load_history(); // oldest first

    // --- Header: title + close only. ---
    let header = adw::HeaderBar::new();
    header.set_decoration_layout(Some(":close"));

    // --- Prompt: app glyph + wrapping description. ---
    let icon = Image::from_icon_name("system-run-symbolic");
    icon.set_pixel_size(32);
    icon.set_valign(Align::Start);
    let prompt = Label::new(Some(PROMPT));
    prompt.set_wrap(true);
    prompt.set_xalign(0.0);
    prompt.set_hexpand(true);
    let prompt_row = GtkBox::new(Orientation::Horizontal, 12);
    prompt_row.append(&icon);
    prompt_row.append(&prompt);

    // --- Input row: "Open:" + entry + history dropdown. ---
    let entry = Entry::new();
    entry.set_hexpand(true);
    entry.set_activates_default(true); // Enter triggers the default (OK)
    if let Some(last) = history.last() {
        entry.set_text(last);
        entry.select_region(0, -1); // preselect, like the classic Run box
    }

    let history_pop = Popover::new();
    let list = ListBox::new();
    list.set_selection_mode(SelectionMode::None);
    list.add_css_class("menu");
    for item in history.iter().rev() {
        let label = Label::new(Some(item));
        label.set_xalign(0.0);
        label.set_margin_top(4);
        label.set_margin_bottom(4);
        label.set_margin_start(6);
        label.set_margin_end(6);
        let row = ListBoxRow::new();
        row.set_child(Some(&label));
        list.append(&row);
    }
    {
        let (entry, pop) = (entry.clone(), history_pop.clone());
        list.connect_row_activated(move |_, row| {
            if let Some(label) = row.child().and_downcast::<Label>() {
                entry.set_text(&label.text());
                entry.set_position(-1);
            }
            pop.popdown();
            entry.grab_focus();
        });
    }
    let scroller = ScrolledWindow::new();
    scroller.set_hscrollbar_policy(PolicyType::Never);
    scroller.set_propagate_natural_height(true);
    scroller.set_max_content_height(220);
    scroller.set_child(Some(&list));
    history_pop.set_child(Some(&scroller));

    let dropdown = MenuButton::new();
    dropdown.set_icon_name("pan-down-symbolic");
    dropdown.set_tooltip_text(Some("Recent commands"));
    dropdown.set_popover(Some(&history_pop));
    dropdown.set_sensitive(!history.is_empty());

    let input_row = GtkBox::new(Orientation::Horizontal, 6);
    let open_label = Label::new(Some("Open:"));
    input_row.append(&open_label);
    input_row.append(&entry);
    input_row.append(&dropdown);

    // --- Buttons: Browse… / Cancel / OK. ---
    let browse = Button::with_label("Browse\u{2026}");
    let cancel = Button::with_label("Cancel");
    let ok = Button::with_label("OK");
    ok.add_css_class("suggested-action");
    let button_row = GtkBox::new(Orientation::Horizontal, 8);
    button_row.set_halign(Align::End);
    button_row.append(&browse);
    button_row.append(&cancel);
    button_row.append(&ok);

    let body = GtkBox::new(Orientation::Vertical, 16);
    body.set_margin_top(18);
    body.set_margin_bottom(18);
    body.set_margin_start(18);
    body.set_margin_end(18);
    body.append(&prompt_row);
    body.append(&input_row);
    body.append(&button_row);

    let content = GtkBox::new(Orientation::Vertical, 0);
    content.append(&header);
    content.append(&body);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Run")
        .resizable(false)
        .default_width(460)
        .content(&content)
        .build();
    window.set_default_widget(Some(&ok));

    // --- Handlers. ---
    {
        let (entry, window) = (entry.clone(), window.clone());
        ok.connect_clicked(move |_| do_run(&entry, &window));
    }
    {
        let window = window.clone();
        cancel.connect_clicked(move |_| window.close());
    }
    {
        let (entry, window) = (entry.clone(), window.clone());
        browse.connect_clicked(move |_| browse_file(&window, &entry));
    }

    window.present();
    entry.grab_focus();
}

/// Read the command, record it, and run it. Closes on success; on failure shows
/// the familiar "cannot find" message and leaves the window open to fix.
fn do_run(entry: &Entry, window: &adw::ApplicationWindow) {
    let cmd = entry.text().trim().to_string();
    if cmd.is_empty() {
        return;
    }
    run_exec::record(&cmd);
    if run_exec::execute(&cmd) {
        window.close();
    } else {
        let alert = gtk::AlertDialog::builder()
            .message("Run")
            .detail(format!(
                "StartPE cannot find '{cmd}'. Make sure you typed the name correctly, and then try again."
            ))
            .modal(true)
            .build();
        alert.show(Some(window));
    }
}

/// Open a file picker and drop the chosen (quoted) path into the entry.
fn browse_file(window: &adw::ApplicationWindow, entry: &Entry) {
    let dialog = gtk::FileDialog::builder().title("Browse").modal(true).build();
    let entry = entry.clone();
    dialog.open(Some(window), gio::Cancellable::NONE, move |res| {
        if let Ok(file) = res {
            if let Some(path) = file.path() {
                entry.set_text(&format!("\"{}\"", path.display()));
                entry.set_position(-1);
            }
        }
    });
}

// ---- single instance ------------------------------------------------------

/// Acquire a process-wide named mutex. Returns true if another Run window is
/// already running (this process should bow out). The handle is intentionally
/// leaked so the mutex is held for the whole process lifetime. ("Run" is too
/// generic a window title to dedupe on, so the Run helper guards itself.)
fn already_running() -> bool {
    unsafe {
        let h = CreateMutexW(None, true, w!("StartPE.RunBox.SingleInstance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(h) = h {
                let _ = CloseHandle(h);
            }
            true
        } else {
            std::mem::forget(h);
            false
        }
    }
}

/// Best-effort: bring an already-open Run window to the foreground.
fn focus_existing() {
    unsafe {
        if let Ok(h) = FindWindowW(PCWSTR::null(), w!("Run")) {
            if !h.is_invalid() {
                let _ = SetForegroundWindow(h);
            }
        }
    }
}

fn main() -> glib::ExitCode {
    if already_running() {
        focus_existing();
        return glib::ExitCode::SUCCESS;
    }
    // We do our own single-instance via the mutex above; don't also use
    // GApplication uniqueness (unreliable on Windows without a session bus).
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(build_ui);
    app.run()
}
