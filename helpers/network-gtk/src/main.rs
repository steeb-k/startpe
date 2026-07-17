// SPDX-License-Identifier: GPL-3.0-or-later
#![windows_subsystem = "windows"]

//! GTK4/Libadwaita **network manager** for Windows PE — StartPE's networking
//! component (`Network.exe`), a thin client of the winrx-creator GTK4 runtime.
//!
//! Two surfaces in one resident process:
//! - the **wifi flyout** ("StartPE Network", undecorated, excluded from the
//!   taskbar): Win11-style network list — click a network, type the key
//!   inline, watch the status line while it connects;
//! - the **Network Settings** window (`settings_win`): per-adapter IPv4
//!   config and export/import of the `network-profile.ini` drop-file.
//!
//! Pre-warmed hidden at StartPE startup (with `--apply-profile` when a
//! drop-file exists) and driven by the taskbar's network glyph via the
//! registered `StartPE_ToggleNetworkFlyout` message (WPARAM 0 = flyout,
//! 1 = settings). See `winipc` / `winplace` / `wlan` / `ipcfg` / `profile`.

mod ipcfg;
mod profile;
mod settings_win;
// Duplicated verbatim across the GTK helpers; each uses a different subset.
#[allow(dead_code)]
mod winicon;
mod winipc;
mod winplace;
mod wlan;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gio, glib, Align, Box as GtkBox, Button, EventControllerKey, Image, Label, ListBox,
    ListBoxRow, Orientation, PasswordEntry, PolicyType, Revealer, ScrolledWindow, SelectionMode,
    Separator};

use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::CreateMutexW;

const APP_ID: &str = "org.winrx.PeNetwork";
const FLYOUT_TITLE: &str = "StartPE Network";
/// How long the connect poll waits before declaring failure.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

const CSS: &str = "
.net-list { background: transparent; }
.net-list row { border-radius: 8px; padding: 2px; }
.net-status { font-size: 0.85em; color: @warning_color; }
.net-status-ok { font-size: 0.85em; color: @success_color; }
";

/// What a flyout list row does when activated.
#[derive(Clone)]
enum RowAction {
    /// Informational (ethernet status, "no wifi hardware").
    None,
    /// A wifi network, by SSID.
    Net(String),
}

/// Outcome line shown under the network being (or just) connected.
#[derive(Clone)]
struct ConnectState {
    ssid: String,
    text: String,
    done: bool,
    ok: bool,
}

/// Everything the flyout handlers need.
#[derive(Clone)]
struct Ui {
    app: adw::Application,
    window: adw::ApplicationWindow,
    list: ListBox,
    actions: Rc<RefCell<Vec<RowAction>>>,
    shown: Rc<Cell<bool>>,
    shown_at: Rc<Cell<Instant>>,
    wlan: Rc<RefCell<Option<wlan::Wlan>>>,
    nets: Rc<RefCell<Vec<wlan::Network>>>,
    /// SSID whose row shows the inline password entry.
    expanded: Rc<RefCell<Option<String>>>,
    connect: Rc<RefCell<Option<ConnectState>>>,
}

fn build_ui(app: &adw::Application, open_flyout: bool, open_settings: bool) {
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    let provider = gtk::CssProvider::new();
    provider.load_from_data(CSS);
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().expect("display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let list = ListBox::new();
    list.set_selection_mode(SelectionMode::None);
    list.add_css_class("net-list");
    list.set_margin_top(8);
    list.set_margin_start(8);
    list.set_margin_end(8);

    let scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();

    // Bottom bar: "Network settings…" (the link into the full window).
    let settings_btn = Button::with_label("Network settings…");
    settings_btn.add_css_class("flat");
    settings_btn.set_halign(Align::Start);
    settings_btn.set_margin_top(4);
    settings_btn.set_margin_bottom(6);
    settings_btn.set_margin_start(8);

    let root = GtkBox::new(Orientation::Vertical, 0);
    root.append(&scroll);
    root.append(&Separator::new(Orientation::Horizontal));
    root.append(&settings_btn);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(FLYOUT_TITLE)
        .resizable(false)
        .default_width(360)
        .default_height(480)
        .build();
    window.set_decorated(false);
    window.set_content(Some(&root));

    let ui = Ui {
        app: app.clone(),
        window: window.clone(),
        list: list.clone(),
        actions: Rc::new(RefCell::new(Vec::new())),
        shown: Rc::new(Cell::new(false)),
        shown_at: Rc::new(Cell::new(Instant::now())),
        wlan: Rc::new(RefCell::new(None)),
        nets: Rc::new(RefCell::new(Vec::new())),
        expanded: Rc::new(RefCell::new(None)),
        connect: Rc::new(RefCell::new(None)),
    };

    {
        let ui = ui.clone();
        settings_btn.connect_clicked(move |_| {
            hide_flyout(&ui);
            settings_win::open(&ui.app);
        });
    }
    {
        let ui = ui.clone();
        list.connect_row_activated(move |_, row| {
            let idx = row.index();
            if idx < 0 {
                return;
            }
            let action = ui.actions.borrow().get(idx as usize).cloned();
            if let Some(RowAction::Net(ssid)) = action {
                activate_network(&ui, &ssid);
            }
        });
    }

    // Esc hides; so does losing focus (click-away), with a short grace period
    // right after showing (the foreground handoff can briefly report inactive).
    let keys = EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let ui = ui.clone();
        keys.connect_key_pressed(move |_, keyval, _, _| {
            if keyval == gtk::gdk::Key::Escape {
                hide_flyout(&ui);
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }
    window.add_controller(keys);
    window.connect_map(|_| winplace::place_and_show());
    {
        let ui = ui.clone();
        window.connect_is_active_notify(move |w| {
            if !w.is_active()
                && ui.shown.get()
                && ui.shown_at.get().elapsed() > Duration::from_millis(200)
            {
                hide_flyout(&ui);
            }
        });
    }

    // Command channel from the IPC thread (taskbar glyph clicks).
    let (tx, rx) = async_channel::unbounded::<usize>();
    winipc::start(tx);
    {
        let ui = ui.clone();
        glib::spawn_future_local(async move {
            while let Ok(cmd) = rx.recv().await {
                match cmd {
                    winipc::CMD_SETTINGS => settings_win::open(&ui.app),
                    _ => toggle_flyout(&ui),
                }
            }
        });
    }

    if open_settings {
        settings_win::open(app);
    }
    if open_flyout {
        toggle_flyout(&ui);
    }
}

fn toggle_flyout(ui: &Ui) {
    if ui.shown.get() {
        hide_flyout(ui);
        return;
    }
    // (Re)open the wlan session each show — cheap, and recovers from a wlansvc
    // restart. Kick a scan; the periodic refresh below picks up its results.
    if ui.wlan.borrow().is_none() {
        *ui.wlan.borrow_mut() = wlan::Wlan::open();
    }
    if let Some(w) = ui.wlan.borrow().as_ref() {
        w.scan();
    }
    ui.expanded.borrow_mut().take();
    refresh_list(ui);
    ui.shown.set(true);
    ui.shown_at.set(Instant::now());
    ui.window.present();

    // While shown: re-list every 2.5 s (picks up scan results), but never
    // while the password entry is open — rebuilding would eat the typing.
    let ui2 = ui.clone();
    glib::timeout_add_local(Duration::from_millis(2500), move || {
        if !ui2.shown.get() {
            return glib::ControlFlow::Break;
        }
        let connecting = ui2.connect.borrow().as_ref().map(|c| !c.done).unwrap_or(false);
        if ui2.expanded.borrow().is_none() && !connecting {
            refresh_list(&ui2);
        }
        glib::ControlFlow::Continue
    });
}

fn hide_flyout(ui: &Ui) {
    ui.window.set_visible(false);
    ui.shown.set(false);
    ui.expanded.borrow_mut().take();
    // Drop the wlan session while hidden; reopened on next show.
    ui.wlan.borrow_mut().take();
}

/// Click on a network row: connected → nothing; open or saved profile →
/// connect right away; secured without a profile → expand the password entry.
fn activate_network(ui: &Ui, ssid: &str) {
    let net = ui.nets.borrow().iter().find(|n| n.ssid == ssid).cloned();
    let Some(net) = net else { return };
    if net.connected {
        return;
    }
    if !net.secured || net.has_profile {
        start_connect(ui, &net, None);
        return;
    }
    let already = ui.expanded.borrow().as_deref() == Some(ssid);
    *ui.expanded.borrow_mut() = if already { None } else { Some(ssid.to_string()) };
    refresh_list(ui);
}

/// Save the profile (when a key was entered), start the connection, and poll
/// the association state into the row's status line — the Win11 flow.
fn start_connect(ui: &Ui, net: &wlan::Network, key: Option<&str>) {
    ui.expanded.borrow_mut().take();
    let result = match ui.wlan.borrow().as_ref() {
        Some(w) => w.connect(net, key),
        None => Err("Wireless is unavailable".to_string()),
    };
    *ui.connect.borrow_mut() = Some(match result {
        Ok(()) => ConnectState {
            ssid: net.ssid.clone(),
            text: "Verifying and connecting…".into(),
            done: false,
            ok: false,
        },
        Err(e) => ConnectState {
            ssid: net.ssid.clone(),
            text: e,
            done: true,
            ok: false,
        },
    });
    refresh_list(ui);
    if ui.connect.borrow().as_ref().map(|c| c.done).unwrap_or(true) {
        return;
    }

    let ui2 = ui.clone();
    let ssid = net.ssid.clone();
    let started = Instant::now();
    glib::timeout_add_local(Duration::from_millis(500), move || {
        let Some(state) = ui2.connect.borrow().as_ref().cloned() else {
            return glib::ControlFlow::Break;
        };
        if state.done || state.ssid != ssid {
            return glib::ControlFlow::Break;
        }
        let connected = ui2
            .wlan
            .borrow()
            .as_ref()
            .and_then(|w| w.current_ssid())
            .as_deref()
            == Some(ssid.as_str());
        if connected {
            *ui2.connect.borrow_mut() = Some(ConnectState {
                ssid: ssid.clone(),
                text: "Connected".into(),
                done: true,
                ok: true,
            });
            refresh_list(&ui2);
            return glib::ControlFlow::Break;
        }
        if started.elapsed() > CONNECT_TIMEOUT {
            *ui2.connect.borrow_mut() = Some(ConnectState {
                ssid: ssid.clone(),
                text: "Can't connect to this network".into(),
                done: true,
                ok: false,
            });
            refresh_list(&ui2);
            return glib::ControlFlow::Break;
        }
        if ui2.shown.get() {
            refresh_list(&ui2);
        }
        glib::ControlFlow::Continue
    });
}

/// Rebuild the list: ethernet status row, then wifi networks (or an
/// explanatory row when wireless is unavailable).
fn refresh_list(ui: &Ui) {
    if let Some(w) = ui.wlan.borrow().as_ref() {
        *ui.nets.borrow_mut() = w.networks();
    } else {
        ui.nets.borrow_mut().clear();
    }

    while let Some(child) = ui.list.first_child() {
        ui.list.remove(&child);
    }
    let mut actions = Vec::new();

    // Ethernet first, mirroring the taskbar glyph's priority.
    let eth = ipcfg::adapters()
        .into_iter()
        .find(|a| a.kind == ipcfg::Kind::Ethernet && a.up);
    if let Some(eth) = eth {
        let subtitle = if eth.ip.is_empty() {
            "Connected".to_string()
        } else {
            format!("Connected — {}", eth.ip)
        };
        ui.list.append(&info_row("network-wired-symbolic", &eth.name, &subtitle));
        actions.push(RowAction::None);
    }

    let have_wlan = ui.wlan.borrow().is_some();
    if !have_wlan {
        ui.list.append(&info_row(
            "network-wireless-offline-symbolic",
            "Wi-Fi unavailable",
            "No wireless hardware or the WLAN service isn't running",
        ));
        actions.push(RowAction::None);
    } else {
        let nets = ui.nets.borrow().clone();
        let expanded = ui.expanded.borrow().clone();
        let connect = ui.connect.borrow().clone();
        for net in &nets {
            let row = net_row(
                ui,
                net,
                expanded.as_deref() == Some(net.ssid.as_str()),
                connect.as_ref().filter(|c| c.ssid == net.ssid),
            );
            ui.list.append(&row);
            actions.push(RowAction::Net(net.ssid.clone()));
        }
        if nets.is_empty() {
            ui.list.append(&info_row(
                "network-wireless-symbolic",
                "Searching for networks…",
                "",
            ));
            actions.push(RowAction::None);
        }
    }
    *ui.actions.borrow_mut() = actions;
}

/// A non-interactive icon + title(+subtitle) row.
fn info_row(icon: &str, title: &str, subtitle: &str) -> ListBoxRow {
    let hbox = GtkBox::new(Orientation::Horizontal, 10);
    hbox.set_margin_top(8);
    hbox.set_margin_bottom(8);
    hbox.set_margin_start(8);
    hbox.set_margin_end(8);
    let img = Image::from_icon_name(icon);
    img.set_pixel_size(20);
    hbox.append(&img);
    let vbox = GtkBox::new(Orientation::Vertical, 2);
    let t = Label::new(Some(title));
    t.set_xalign(0.0);
    vbox.append(&t);
    if !subtitle.is_empty() {
        let s = Label::new(Some(subtitle));
        s.set_xalign(0.0);
        s.add_css_class("dim-label");
        s.add_css_class("caption");
        vbox.append(&s);
    }
    hbox.append(&vbox);
    let row = ListBoxRow::new();
    row.set_child(Some(&hbox));
    row.set_activatable(false);
    row
}

fn signal_icon(quality: u32) -> &'static str {
    match quality {
        75.. => "network-wireless-signal-excellent-symbolic",
        50.. => "network-wireless-signal-good-symbolic",
        25.. => "network-wireless-signal-ok-symbolic",
        _ => "network-wireless-signal-weak-symbolic",
    }
}

/// One wifi network row: signal icon, SSID, state/status line, a lock for
/// secured networks, and (when `expanded`) the inline key entry + Connect.
fn net_row(
    ui: &Ui,
    net: &wlan::Network,
    expanded: bool,
    connect: Option<&ConnectState>,
) -> ListBoxRow {
    let hbox = GtkBox::new(Orientation::Horizontal, 10);
    hbox.set_margin_top(8);
    hbox.set_margin_bottom(8);
    hbox.set_margin_start(8);
    hbox.set_margin_end(8);
    let img = Image::from_icon_name(signal_icon(net.signal));
    img.set_pixel_size(20);
    hbox.append(&img);

    let vbox = GtkBox::new(Orientation::Vertical, 2);
    vbox.set_hexpand(true);
    let title = Label::new(Some(&net.ssid));
    title.set_xalign(0.0);
    vbox.append(&title);
    // One line under the SSID: live connect status wins, else the state.
    if let Some(c) = connect {
        let s = Label::new(Some(&c.text));
        s.set_xalign(0.0);
        s.add_css_class(if c.done && !c.ok { "net-status" } else { "net-status-ok" });
        vbox.append(&s);
    } else if net.connected {
        let s = Label::new(Some("Connected"));
        s.set_xalign(0.0);
        s.add_css_class("dim-label");
        s.add_css_class("caption");
        vbox.append(&s);
    } else if net.has_profile {
        let s = Label::new(Some("Saved"));
        s.set_xalign(0.0);
        s.add_css_class("dim-label");
        s.add_css_class("caption");
        vbox.append(&s);
    }
    hbox.append(&vbox);

    if net.secured {
        let lock = Image::from_icon_name("system-lock-screen-symbolic");
        lock.set_pixel_size(14);
        lock.add_css_class("dim-label");
        hbox.append(&lock);
    }

    let outer = GtkBox::new(Orientation::Vertical, 0);
    outer.append(&hbox);

    // Inline password entry, revealed under the clicked network (Win11 style).
    let reveal = Revealer::new();
    reveal.set_reveal_child(expanded);
    if expanded {
        let entry_box = GtkBox::new(Orientation::Horizontal, 6);
        entry_box.set_margin_start(38);
        entry_box.set_margin_end(8);
        entry_box.set_margin_bottom(8);
        let entry = PasswordEntry::new();
        entry.set_show_peek_icon(true);
        entry.set_hexpand(true);
        entry.set_property("placeholder-text", "Enter the network security key");
        let go = Button::with_label("Connect");
        go.add_css_class("suggested-action");
        entry_box.append(&entry);
        entry_box.append(&go);
        reveal.set_child(Some(&entry_box));

        let do_connect = {
            let ui = ui.clone();
            let net = net.clone();
            let entry = entry.clone();
            move || {
                let key = entry.text().to_string();
                if !key.is_empty() {
                    start_connect(&ui, &net, Some(&key));
                }
            }
        };
        {
            let do_connect = do_connect.clone();
            go.connect_clicked(move |_| do_connect());
        }
        entry.connect_activate(move |_| do_connect());
        // Focus the key entry once the row lands in the widget tree.
        let entry2 = entry.clone();
        glib::idle_add_local_once(move || {
            entry2.grab_focus();
        });
    }
    outer.append(&reveal);

    let row = ListBoxRow::new();
    row.set_child(Some(&outer));
    row
}

// ---- single instance ------------------------------------------------------

fn already_running() -> bool {
    unsafe {
        let h = CreateMutexW(None, true, w!("StartPE.Network.SingleInstance"));
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

/// Best-effort line into StartPE's PE log, so profile import is diagnosable.
fn log(text: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("X:\\startpe.log")
    {
        let _ = writeln!(f, "StartPE Network: {text}");
    }
}

fn main() -> glib::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let has = |f: &str| args.iter().any(|a| a == f);

    if already_running() {
        // Forward the request to the resident instance instead.
        if has("--settings") {
            let _ = winipc::post_to_running(winipc::CMD_SETTINGS);
        } else if has("--flyout") {
            let _ = winipc::post_to_running(winipc::CMD_FLYOUT);
        }
        return glib::ExitCode::SUCCESS;
    }

    if has("--apply-profile") {
        // Startup import of the drop-file, off the UI thread; netsh and
        // wlansvc don't need GTK, and the flyout stays instant meanwhile.
        std::thread::spawn(|| {
            if let Some(summary) = profile::apply() {
                log(&summary);
            }
        });
    }

    let open_flyout = has("--flyout");
    let open_settings = has("--settings");
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| build_ui(app, open_flyout, open_settings));
    app.run_with_args::<&str>(&[])
}
