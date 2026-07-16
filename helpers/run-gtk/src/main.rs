// SPDX-License-Identifier: GPL-3.0-or-later
#![windows_subsystem = "windows"]

//! GTK4/Libadwaita **Run** window for Windows PE — a thin client of the
//! winrx-creator GTK4 runtime, the libadwaita counterpart to StartPE's Win32/GDI
//! Run box (`startpe/src/run_window.rs`). The command/history core is reused
//! (`run_exec`); the UI is a small libadwaita dialog: prompt, an "Open:" entry
//! with inline history autocomplete, and Browse / Cancel / OK. History is shared
//! with StartPE via `HKCU\Software\StartPE\RunHistory`.
//!
//! Like the GDI Run box it seats itself bottom-left above StartPE's taskbar.
//! GTK4 doesn't let an app position its own window, so we move the native HWND
//! with `SetWindowPos` once it maps (the one place this helper reaches past GTK).

mod run_exec;
mod winicon;

use adw::prelude::*;
use gtk::{gio, glib};
use gtk::{Align, Box as GtkBox, Button, Entry, EventControllerKey, Image, Label, Orientation};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, BOOL, ERROR_ALREADY_EXISTS, HWND, LPARAM, RECT,
};
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcessId};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowW, GetSystemMetrics, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, SetForegroundWindow, SetWindowPos, SystemParametersInfoW,
    HWND_NOTOPMOST, HWND_TOPMOST, SM_CYSCREEN, SPI_GETWORKAREA, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};

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

    // --- Input row: "Open:" + entry (inline history autocomplete, no dropdown). ---
    let entry = Entry::new();
    entry.set_hexpand(true);
    entry.set_activates_default(true); // Enter triggers the default (OK)
    attach_completion(&entry, &history);
    if let Some(last) = history.last() {
        entry.set_text(last);
        entry.select_region(0, -1); // preselect, like the classic Run box
    }
    let input_row = GtkBox::new(Orientation::Horizontal, 6);
    input_row.append(&Label::new(Some("Open:")));
    input_row.append(&entry);

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

    // Escape closes the window (capture phase so the entry doesn't swallow it).
    let keys = EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let window = window.clone();
        keys.connect_key_pressed(move |_, keyval, _, _| {
            if keyval == gtk::gdk::Key::Escape {
                window.close();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }
    window.add_controller(keys);

    // Seat it bottom-left above the taskbar once the native window exists.
    window.connect_map(|_| position_bottom_left());

    window.present();
    entry.grab_focus();
}

/// Inline autocomplete from history with no visible dropdown — typing completes
/// to the most recent matching command (suffix selected), which is the "auto-fill"
/// behavior of the classic Run box. (`EntryCompletion` is deprecated in GTK 4.10
/// but is the simplest correct inline completion; confined here behind `allow`.)
#[allow(deprecated)]
fn attach_completion(entry: &Entry, history: &[String]) {
    let store = gtk::ListStore::new(&[glib::Type::STRING]);
    for item in history.iter().rev() {
        // newest first, so the freshest match wins
        let iter = store.append();
        store.set_value(&iter, 0, &item.to_value());
    }
    let completion = gtk::EntryCompletion::new();
    completion.set_model(Some(&store));
    completion.set_text_column(0);
    completion.set_inline_completion(true);
    completion.set_popup_completion(false); // no dropdown — inline only
    completion.set_minimum_key_length(1);
    entry.set_completion(Some(&completion));
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

// ---- native window placement ----------------------------------------------

/// Move our window to the bottom-left, just above StartPE's taskbar (or the work
/// area if the taskbar isn't found). GTK4 can't position its own windows, so we
/// nudge the native HWND directly.
fn position_bottom_left() {
    unsafe {
        let Some(hwnd) = own_window() else { return };
        // Swap GTK's default icon for the accent Run glyph (taskbar / Alt+Tab).
        winicon::apply(hwnd);
        let mut wr = RECT::default();
        if GetWindowRect(hwnd, &mut wr).is_err() {
            return;
        }
        let height = wr.bottom - wr.top;
        let bottom = startpe_taskbar_top().unwrap_or_else(work_area_bottom);
        let margin = 12;
        let y = (bottom - height - margin).max(margin);
        let _ = SetWindowPos(
            hwnd,
            None,
            margin,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        // Come to the front reliably. Spawned from StartPE, GTK's present() alone
        // can leave us behind the desktop ("under the wallpaper") — the window is
        // up (taskbar/peek shows it) but not foreground. Raise above everything,
        // drop back to the normal band, then activate. SetWindowPos z-order
        // changes don't need foreground rights; a spawned SetForegroundWindow can
        // be denied, so it's a best-effort finish.
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        let _ = SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        let _ = SetForegroundWindow(hwnd);
    }
}

/// The top edge of StartPE's taskbar, so we sit just above it (matches the GDI
/// Run box, which doesn't reserve work area so `SPI_GETWORKAREA` is the full screen).
fn startpe_taskbar_top() -> Option<i32> {
    unsafe {
        let bar = FindWindowW(w!("StartPE_Taskbar"), PCWSTR::null()).ok()?;
        if bar.is_invalid() {
            return None;
        }
        let mut rc = RECT::default();
        (GetWindowRect(bar, &mut rc).is_ok() && rc.top > 0).then_some(rc.top)
    }
}

fn work_area_bottom() -> i32 {
    unsafe {
        let mut wa = RECT::default();
        let ok = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut wa as *mut _ as *mut std::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok();
        if ok && wa.bottom > 0 {
            wa.bottom
        } else {
            GetSystemMetrics(SM_CYSCREEN)
        }
    }
}

/// Find this process's "Run" window (the one we just mapped).
fn own_window() -> Option<HWND> {
    unsafe {
        let mut data = (GetCurrentProcessId(), HWND::default());
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut data as *mut _ as isize));
        (!data.1.is_invalid()).then_some(data.1)
    }
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let data = &mut *(lparam.0 as *mut (u32, HWND));
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == data.0 && window_title(hwnd) == "Run" {
        data.1 = hwnd;
        return BOOL(0); // found it; stop enumerating
    }
    BOOL(1)
}

unsafe fn window_title(hwnd: HWND) -> String {
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let n = GetWindowTextW(hwnd, &mut buf);
    String::from_utf16_lossy(&buf[..n as usize])
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
