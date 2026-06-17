// SPDX-License-Identifier: GPL-3.0-or-later
#![windows_subsystem = "windows"]

//! GTK4/Libadwaita **Settings** for Windows PE — a thin client of the
//! winrx-creator GTK4 runtime, the libadwaita counterpart to StartPE's Win32/GDI
//! settings pane (`startpe/src/settings.rs`). Toggles are `AdwSwitchRow`s grouped
//! by surface; the Start-button color is a `GtkColorDialogButton`. Each change
//! writes `HKCU\Software\StartPE` and posts `StartPE_ReloadConfig` so StartPE
//! applies it live (`settings_io`).

mod settings_io;

use adw::prelude::*;
use gtk::{gio, glib};
use gtk::{Align, EventControllerKey};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, SetForegroundWindow};

const APP_ID: &str = "org.winrx.PeSettings";

/// (group, label, registry value, current-getter, needs-relaunch).
struct Toggle {
    group: &'static str,
    label: &'static str,
    reg: &'static str,
    get: fn(&settings_io::Settings) -> bool,
    restart: bool,
}

const TOGGLES: &[Toggle] = &[
    Toggle { group: "Taskbar", label: "Show window labels", reg: "TaskbarLabels", get: |s| s.show_labels, restart: false },
    Toggle { group: "Taskbar", label: "Combine taskbar buttons", reg: "TaskbarCombine", get: |s| s.combine, restart: false },
    Toggle { group: "Taskbar", label: "Center taskbar", reg: "CenterTaskbar", get: |s| s.center_taskbar, restart: false },
    Toggle { group: "Windows", label: "Accent border on active window", reg: "WindowBorders", get: |s| s.window_borders, restart: false },
    Toggle { group: "Menus", label: "Dark context menus", reg: "DarkMenus", get: |s| s.dark_menus, restart: true },
];

fn build_ui(app: &adw::Application) {
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);

    let settings = settings_io::load();
    let page = adw::PreferencesPage::new();

    // Toggle groups, in first-seen group order.
    let mut current_group: Option<(&str, adw::PreferencesGroup)> = None;
    for t in TOGGLES {
        let group = match &current_group {
            Some((g, grp)) if *g == t.group => grp.clone(),
            _ => {
                let grp = adw::PreferencesGroup::builder().title(t.group).build();
                page.add(&grp);
                current_group = Some((t.group, grp.clone()));
                grp
            }
        };
        let row = adw::SwitchRow::builder()
            .title(t.label)
            .active((t.get)(&settings))
            .build();
        if t.restart {
            row.set_subtitle("Applies after StartPE restarts");
        }
        let reg = t.reg;
        row.connect_active_notify(move |r| settings_io::save_bool(reg, r.is_active()));
        group.add(&row);
    }

    // Start-button glyph color.
    let color_group = adw::PreferencesGroup::builder().title("Start button").build();
    let dialog = gtk::ColorDialog::builder().with_alpha(false).build();
    let color_button = gtk::ColorDialogButton::new(Some(dialog));
    color_button.set_rgba(&colorref_to_rgba(settings.start_color));
    color_button.set_valign(Align::Center);
    color_button.connect_rgba_notify(|b| {
        settings_io::save_u32("StartButtonColor", rgba_to_colorref(&b.rgba()));
    });
    let color_row = adw::ActionRow::builder().title("Glyph color").build();
    color_row.add_suffix(&color_button);
    color_row.set_activatable_widget(Some(&color_button));
    color_group.add(&color_row);
    page.add(&color_group);

    let window = adw::PreferencesWindow::builder()
        .application(app)
        .title("StartPE Settings")
        .search_enabled(false)
        .resizable(false) // fixed dialog; avoids maximizing behind StartPE's taskbar
        .default_width(420)
        .default_height(520)
        .build();
    window.add(&page);

    // Escape closes the window.
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

    window.present();
}

/// COLORREF 0x00BBGGRR -> RGBA (opaque).
fn colorref_to_rgba(c: u32) -> gtk::gdk::RGBA {
    gtk::gdk::RGBA::new(
        (c & 0xFF) as f32 / 255.0,
        ((c >> 8) & 0xFF) as f32 / 255.0,
        ((c >> 16) & 0xFF) as f32 / 255.0,
        1.0,
    )
}

/// RGBA -> COLORREF 0x00BBGGRR.
fn rgba_to_colorref(c: &gtk::gdk::RGBA) -> u32 {
    let to8 = |f: f32| (f.clamp(0.0, 1.0) * 255.0).round() as u32 & 0xFF;
    to8(c.red()) | (to8(c.green()) << 8) | (to8(c.blue()) << 16)
}

// ---- single instance ------------------------------------------------------

fn already_running() -> bool {
    unsafe {
        let h = CreateMutexW(None, true, w!("StartPE.Settings.SingleInstance"));
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

fn focus_existing() {
    unsafe {
        if let Ok(h) = FindWindowW(PCWSTR::null(), w!("StartPE Settings")) {
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
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(build_ui);
    app.run()
}
