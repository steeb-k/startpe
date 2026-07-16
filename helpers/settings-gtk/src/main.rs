// SPDX-License-Identifier: GPL-3.0-or-later
#![windows_subsystem = "windows"]

//! GTK4/Libadwaita **Settings** for Windows PE — a thin client of the
//! winrx-creator GTK4 runtime, the libadwaita counterpart to StartPE's Win32/GDI
//! settings pane (`startpe/src/settings.rs`). Toggles are `AdwSwitchRow`s grouped
//! by surface; the Start-button color is a `GtkColorDialogButton`. Each change
//! writes `HKCU\Software\StartPE` and posts `StartPE_ReloadConfig` so StartPE
//! applies it live (`settings_io`).

mod settings_io;
// Duplicated verbatim across the GTK helpers; each uses a different subset.
#[allow(dead_code)]
mod winicon;

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

/// View-switcher sections (label, symbolic icon), in display order. Each becomes
/// its own `AdwPreferencesPage`; the boolean toggles are filtered into them by
/// matching `Toggle::group`.
const SECTIONS: &[(&str, &str)] = &[
    ("Taskbar", "view-list-symbolic"),
    ("Windows", "video-display-symbolic"),
    ("Menus", "open-menu-symbolic"),
];

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

    // A view per section, switched by an AdwViewSwitcher in the header (the
    // Adwaita-demo pattern) instead of one long scrolling page.
    let stack = adw::ViewStack::new();

    for (section, icon) in SECTIONS {
        let group = adw::PreferencesGroup::new();
        for t in TOGGLES.iter().filter(|t| t.group == *section) {
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
        let page = stack.add_titled(&section_clamp(&group), Some(section), section);
        page.set_icon_name(Some(icon));
    }

    // Start-button glyph color, its own view.
    let start_group = adw::PreferencesGroup::new();
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
    start_group.add(&color_row);
    let start = stack.add_titled(&section_clamp(&start_group), Some("start"), "Start button");
    start.set_icon_name(Some("start-here-symbolic"));

    // Header bar carrying the view switcher; content is the stack.
    let switcher = adw::ViewSwitcher::builder()
        .stack(&stack)
        .policy(adw::ViewSwitcherPolicy::Wide)
        .build();
    let header = adw::HeaderBar::new();
    header.set_decoration_layout(Some(":close")); // drop minimize/maximize
    header.set_title_widget(Some(&switcher));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("StartPE Settings")
        .resizable(false) // fixed dialog; avoids maximizing behind StartPE's taskbar
        .default_width(600)
        .default_height(300)
        .content(&toolbar)
        .build();

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

    // Swap GTK's default icon for the accent Settings gear (taskbar / Alt+Tab)
    // once the native window exists.
    window.connect_map(|_| winicon::apply_to_own_window("StartPE Settings", '\u{E713}'));

    window.present();
}

/// Wrap a section's group in a clamp with comfortable margins for a stack page.
fn section_clamp(group: &adw::PreferencesGroup) -> adw::Clamp {
    let clamp = adw::Clamp::new();
    clamp.set_maximum_size(440);
    clamp.set_margin_top(18);
    clamp.set_margin_bottom(18);
    clamp.set_margin_start(12);
    clamp.set_margin_end(12);
    clamp.set_child(Some(group));
    clamp
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
