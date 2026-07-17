// SPDX-License-Identifier: GPL-3.0-or-later
//! The full **Network Settings** window: per-adapter IPv4 configuration
//! (DHCP/static + DNS) and export/import of the `network-profile.ini`
//! drop-file. Unlike the flyout this is a normal decorated window — it gets a
//! taskbar button, so it carries a native MDL2 network icon (`winicon`).

use adw::prelude::*;
use gtk::glib;
use gtk::{Align, Button, PolicyType, ScrolledWindow};

use crate::{ipcfg, profile, winicon};

const TITLE: &str = "Network Settings";

/// Present the settings window, building it fresh so it reflects the current
/// adapter state. A second open while one exists just raises it.
pub fn open(app: &adw::Application) {
    for w in app.windows() {
        if w.title().map(|t| t == TITLE).unwrap_or(false) {
            w.present();
            return;
        }
    }

    let toasts = adw::ToastOverlay::new();
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(TITLE)
        .default_width(560)
        .default_height(640)
        .resizable(false)
        .build();

    fill(&window, &toasts);
    window.connect_map(|_| winicon::apply_to_own_window(TITLE, '\u{E968}'));
    window.present();
}

/// (Re)build the window content from a fresh adapter snapshot.
fn fill(window: &adw::ApplicationWindow, toasts: &adw::ToastOverlay) {
    let page = adw::PreferencesPage::new();

    for a in ipcfg::adapters() {
        page.add(&adapter_group(&a, toasts));
    }
    page.add(&profile_group(toasts));

    let scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vexpand(true)
        .child(&page)
        .build();

    let header = adw::HeaderBar::new();
    header.set_decoration_layout(Some(":close"));
    let refresh = Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some("Refresh adapters"));
    {
        let window = window.clone();
        let toasts = toasts.clone();
        refresh.connect_clicked(move |_| fill(&window, &toasts));
    }
    header.pack_start(&refresh);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scroll));
    toasts.set_child(Some(&toolbar));
    window.set_content(Some(toasts));
}

/// One adapter's preferences group: status, DHCP switch, static fields, Apply.
fn adapter_group(a: &ipcfg::Adapter, toasts: &adw::ToastOverlay) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    group.set_title(&a.name);
    let kind = match a.kind {
        ipcfg::Kind::Ethernet => "Ethernet",
        ipcfg::Kind::Wifi => "Wi-Fi",
    };
    let status = if a.up {
        if a.ip.is_empty() {
            "Connected".to_string()
        } else {
            format!("Connected — {}", a.ip)
        }
    } else {
        "Disconnected".to_string()
    };
    group.set_description(Some(&format!("{kind} · {} · {status}", a.desc)));

    let dhcp = adw::SwitchRow::builder()
        .title("Automatic (DHCP)")
        .active(a.dhcp)
        .build();
    group.add(&dhcp);

    let entry = |title: &str, text: &str| {
        let row = adw::EntryRow::builder().title(title).build();
        row.set_text(text);
        row.set_sensitive(!a.dhcp);
        row
    };
    let ip = entry("IP address", &a.ip);
    let mask = entry("Subnet mask", &a.mask);
    let gateway = entry("Gateway", &a.gateway);
    let dns = entry("DNS servers (comma-separated)", &a.dns.join(","));
    for r in [&ip, &mask, &gateway, &dns] {
        group.add(r);
    }
    {
        let rows = [ip.clone(), mask.clone(), gateway.clone(), dns.clone()];
        dhcp.connect_active_notify(move |s| {
            for r in &rows {
                r.set_sensitive(!s.is_active());
            }
        });
    }

    let apply = Button::with_label("Apply");
    apply.add_css_class("suggested-action");
    apply.set_valign(Align::Center);
    let apply_row = adw::ActionRow::new();
    apply_row.add_suffix(&apply);
    group.add(&apply_row);

    let name = a.name.clone();
    let toasts = toasts.clone();
    apply.connect_clicked(move |btn| {
        let cfg = ipcfg::ApplyV4 {
            dhcp: dhcp.is_active(),
            ip: ip.text().trim().to_string(),
            mask: mask.text().trim().to_string(),
            gateway: gateway.text().trim().to_string(),
            dns: dns
                .text()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
        };
        if !cfg.dhcp && (cfg.ip.is_empty() || cfg.mask.is_empty()) {
            toasts.add_toast(adw::Toast::new("A static setup needs an IP address and mask"));
            return;
        }
        btn.set_sensitive(false);
        let name = name.clone();
        let (tx, rx) = async_channel::bounded::<Result<(), String>>(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(ipcfg::apply(&name, &cfg));
        });
        let toasts = toasts.clone();
        let btn = btn.clone();
        glib::spawn_future_local(async move {
            if let Ok(result) = rx.recv().await {
                btn.set_sensitive(true);
                toasts.add_toast(adw::Toast::new(&match result {
                    Ok(()) => "Applied".to_string(),
                    Err(e) => e,
                }));
            }
        });
    });

    group
}

/// Export / import of the `network-profile.ini` drop-file.
fn profile_group(toasts: &adw::ToastOverlay) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    group.set_title("Profile");
    let path_text = profile::path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "network-profile.ini".into());
    group.set_description(Some(
        "Save the current setup — adapter configuration and saved wireless networks \
         (including keys) — as a file dropped next to startpe.exe. StartPE re-applies \
         it automatically at shell startup.",
    ));

    let export = Button::with_label("Export");
    export.set_valign(Align::Center);
    let export_row = adw::ActionRow::builder()
        .title("Export current setup")
        .subtitle(&path_text)
        .build();
    export_row.add_suffix(&export);
    group.add(&export_row);
    {
        let toasts = toasts.clone();
        export.connect_clicked(move |_| {
            toasts.add_toast(adw::Toast::new(&match profile::export() {
                Ok(p) => format!("Saved to {}", p.display()),
                Err(e) => e,
            }));
        });
    }

    let import = Button::with_label("Import");
    import.set_valign(Align::Center);
    import.set_sensitive(profile::path().map(|p| p.is_file()).unwrap_or(false));
    let import_row = adw::ActionRow::builder()
        .title("Apply profile file now")
        .subtitle("Re-applies the saved setup from the file above")
        .build();
    import_row.add_suffix(&import);
    group.add(&import_row);
    {
        let toasts = toasts.clone();
        import.connect_clicked(move |btn| {
            btn.set_sensitive(false);
            let (tx, rx) = async_channel::bounded::<Option<String>>(1);
            std::thread::spawn(move || {
                let _ = tx.send_blocking(profile::apply());
            });
            let toasts = toasts.clone();
            let btn = btn.clone();
            glib::spawn_future_local(async move {
                if let Ok(result) = rx.recv().await {
                    btn.set_sensitive(true);
                    toasts.add_toast(adw::Toast::new(
                        result.as_deref().unwrap_or("No profile file found"),
                    ));
                }
            });
        });
    }

    group
}
