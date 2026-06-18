// SPDX-License-Identifier: GPL-3.0-or-later
#![windows_subsystem = "windows"]

//! GTK4/Libadwaita **Start menu** for Windows PE — the libadwaita counterpart to
//! StartPE's Win32/GDI start menu (`startpe/src/start_menu.rs`). Two panes: a
//! searchable app list (with folder drill-down) on the left, system links + power
//! on the right.
//!
//! Pre-warmed: launched hidden at StartPE startup and toggled by the taskbar via
//! the registered `StartPE_ToggleStartMenu` message (so it opens instantly on the
//! Win key). On toggle it resets to the root, positions itself above the taskbar,
//! comes to the front, and focuses search; launching an item / Esc / losing focus
//! hides it (the process stays resident). See `winipc` and `winplace`.

mod appsource;
mod icons;
mod winipc;
mod winplace;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{glib, Align, Box as GtkBox, Button, EventControllerKey, Image, Label, ListBox,
    ListBoxRow, Orientation, Popover, PolicyType, ScrolledWindow, SearchEntry, SelectionMode,
    Separator};

use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::CreateMutexW;

use appsource::{AppItem, ItemKind};

const APP_ID: &str = "org.winrx.PeStartMenu";

const CSS: &str = "
.sm-right {
  background-color: @sidebar_bg_color;
  border-top-right-radius: 12px;
  border-bottom-right-radius: 12px;
}
.sm-list { background: transparent; }
.sm-list row { border-radius: 8px; }
.sm-rightbtn { padding: 8px 10px; }
";

#[derive(Clone)]
enum RowAction {
    Back,
    Folder(PathBuf),
    Launch(PathBuf),
}

/// Everything the toggle handler needs to drive the menu.
#[derive(Clone)]
struct Ui {
    window: adw::ApplicationWindow,
    search: SearchEntry,
    stack: Rc<RefCell<Vec<PathBuf>>>,
    refresh: Rc<dyn Fn()>,
    shown: Rc<Cell<bool>>,
    shown_at: Rc<Cell<Instant>>,
    showing_all: Rc<Cell<bool>>,
}

fn build_ui(app: &adw::Application) {
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    let provider = gtk::CssProvider::new();
    provider.load_from_data(CSS);
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().expect("display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let stack: Rc<RefCell<Vec<PathBuf>>> = Rc::new(RefCell::new(Vec::new()));
    let actions: Rc<RefCell<Vec<RowAction>>> = Rc::new(RefCell::new(Vec::new()));
    let shown = Rc::new(Cell::new(false));
    let shown_at = Rc::new(Cell::new(Instant::now()));
    let showing_all = Rc::new(Cell::new(false));

    // --- Left pane: app list, an All apps / Pinned toggle, then search (bottom). ---
    let search = SearchEntry::new();
    search.set_placeholder_text(Some("Search programs"));
    search.set_margin_top(8);
    search.set_margin_bottom(8);
    search.set_margin_start(8);
    search.set_margin_end(8);

    let list = ListBox::new();
    list.set_selection_mode(SelectionMode::None);
    list.add_css_class("sm-list");
    list.set_margin_start(6);
    list.set_margin_end(6);

    let list_scroll = ScrolledWindow::new();
    list_scroll.set_hscrollbar_policy(PolicyType::Never);
    list_scroll.set_vexpand(true);
    list_scroll.set_child(Some(&list));

    // "All apps" / "Pinned apps" toggle — only shown when start-menu pins exist.
    let has_pins = appsource::has_pins();
    let all_apps = Button::with_label("All apps");
    all_apps.add_css_class("flat");
    all_apps.set_margin_start(8);
    all_apps.set_margin_end(8);
    if let Some(lbl) = all_apps.child().and_downcast::<Label>() {
        lbl.set_xalign(0.0);
    }

    let refresh: Rc<dyn Fn()> = {
        let (list, stack, search, actions, showing_all, all_apps) = (
            list.clone(),
            stack.clone(),
            search.clone(),
            actions.clone(),
            showing_all.clone(),
            all_apps.clone(),
        );
        Rc::new(move || {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            let items = appsource::enumerate(&stack.borrow(), &search.text(), showing_all.get());
            let mut acts = Vec::with_capacity(items.len());
            for item in items {
                let (row, action) = make_row(item);
                list.append(&row);
                acts.push(action);
            }
            *actions.borrow_mut() = acts;
            all_apps.set_label(if showing_all.get() {
                "Pinned apps"
            } else {
                "All apps"
            });
        })
    };
    {
        let (showing_all, refresh) = (showing_all.clone(), refresh.clone());
        all_apps.connect_clicked(move |_| {
            showing_all.set(!showing_all.get());
            refresh();
        });
    }

    let left = GtkBox::new(Orientation::Vertical, 0);
    left.set_hexpand(true);
    left.append(&list_scroll);
    if has_pins {
        left.append(&Separator::new(Orientation::Horizontal));
        left.append(&all_apps);
    }
    left.append(&search);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("StartPE Menu")
        .resizable(false)
        .default_width(560)
        .default_height(540)
        .build();
    window.set_decorated(false);

    let ui = Ui {
        window: window.clone(),
        search: search.clone(),
        stack: stack.clone(),
        refresh: refresh.clone(),
        shown: shown.clone(),
        shown_at: shown_at.clone(),
        showing_all: showing_all.clone(),
    };

    let hide: Rc<dyn Fn()> = {
        let ui = ui.clone();
        Rc::new(move || hide_menu(&ui))
    };

    // Right pane uses `hide` so launching an item dismisses (not closes) the menu.
    let right = build_right_pane(hide.clone());
    right.add_css_class("sm-right");

    let panes = GtkBox::new(Orientation::Horizontal, 0);
    panes.append(&left);
    panes.append(&Separator::new(Orientation::Vertical));
    panes.append(&right);
    window.set_content(Some(&panes));

    // Row activation.
    {
        let (stack, actions, search, refresh, hide) =
            (stack.clone(), actions.clone(), search.clone(), refresh.clone(), hide.clone());
        list.connect_row_activated(move |_, row| {
            let idx = row.index();
            if idx < 0 {
                return;
            }
            let action = actions.borrow().get(idx as usize).cloned();
            match action {
                Some(RowAction::Back) => {
                    stack.borrow_mut().pop();
                    refresh();
                }
                Some(RowAction::Folder(p)) => {
                    search.set_text("");
                    stack.borrow_mut().push(p);
                    refresh();
                }
                Some(RowAction::Launch(p)) => {
                    appsource::launch_path(&p);
                    hide();
                }
                None => {}
            }
        });
    }
    {
        let refresh = refresh.clone();
        search.connect_search_changed(move |_| refresh());
    }

    // Esc hides; so does losing focus (click-away), but not in the moment right
    // after showing (the foreground handoff can briefly report inactive).
    let keys = EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let hide = hide.clone();
        keys.connect_key_pressed(move |_, keyval, _, _| {
            if keyval == gtk::gdk::Key::Escape {
                hide();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }
    window.add_controller(keys);
    // Position + foreground each time the window maps (HWND exists then).
    window.connect_map(|_| winplace::place_and_show());
    {
        let (hide, shown, shown_at) = (hide.clone(), shown.clone(), shown_at.clone());
        window.connect_is_active_notify(move |w| {
            if !w.is_active()
                && shown.get()
                && shown_at.get().elapsed() > Duration::from_millis(200)
            {
                hide();
            }
        });
    }

    // Build the content once so the first open is instant, but stay hidden.
    refresh();

    // Toggle channel from the IPC thread.
    let (tx, rx) = async_channel::unbounded::<()>();
    winipc::start(tx);
    {
        let ui = ui.clone();
        glib::spawn_future_local(async move {
            while rx.recv().await.is_ok() {
                toggle_menu(&ui);
            }
        });
    }
}

fn toggle_menu(ui: &Ui) {
    if ui.shown.get() {
        hide_menu(ui);
    } else {
        // Open at the root (pinned view) with an empty search. `present()` maps the
        // window; positioning + foreground happen on its `map` signal.
        ui.stack.borrow_mut().clear();
        ui.search.set_text("");
        ui.showing_all.set(false);
        (ui.refresh)();
        ui.shown.set(true);
        ui.shown_at.set(Instant::now());
        ui.window.present();
        ui.search.grab_focus();
    }
}

fn hide_menu(ui: &Ui) {
    ui.window.set_visible(false);
    ui.shown.set(false);
}

/// Build a list row + its action from an [`AppItem`].
fn make_row(item: AppItem) -> (ListBoxRow, RowAction) {
    let row_box = GtkBox::new(Orientation::Horizontal, 10);
    row_box.set_margin_top(4);
    row_box.set_margin_bottom(4);
    row_box.set_margin_start(8);
    row_box.set_margin_end(8);

    let image = match item.icon.and_then(icons::texture_from_hicon) {
        Some(tex) => Image::from_paintable(Some(&tex)),
        None => match item.kind {
            ItemKind::Back => Image::from_icon_name("go-previous-symbolic"),
            ItemKind::Folder(_) => Image::from_icon_name("folder-symbolic"),
            ItemKind::Launch(_) => Image::from_icon_name("application-x-executable-symbolic"),
        },
    };
    image.set_pixel_size(24);
    row_box.append(&image);

    let label = Label::new(Some(&item.name));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    row_box.append(&label);

    if matches!(item.kind, ItemKind::Folder(_)) {
        row_box.append(&Image::from_icon_name("go-next-symbolic"));
    }

    let row = ListBoxRow::new();
    row.set_child(Some(&row_box));

    let action = match item.kind {
        ItemKind::Back => RowAction::Back,
        ItemKind::Folder(p) => RowAction::Folder(p),
        ItemKind::Launch(p) => RowAction::Launch(p),
    };
    (row, action)
}

/// (symbolic icon, label, ShellExecute target). Folder paths open in Explorer.
fn right_links() -> Vec<(&'static str, String, String)> {
    let profile = std::env::var("USERPROFILE").unwrap_or_else(|_| "X:\\Users\\Default".into());
    vec![
        ("folder-download-symbolic", "Downloads".into(), format!("{profile}\\Downloads")),
        ("drive-harddisk-symbolic", "This PC".into(), "shell:MyComputerFolder".into()),
        ("applications-system-symbolic", "Control Panel".into(), "control.exe".into()),
        ("utilities-terminal-symbolic", "Command Prompt".into(), "cmd.exe".into()),
    ]
}

/// The configured user picture (`UserPicture` in the registry) as a texture, if set.
fn user_picture() -> Option<gtk::gdk::Texture> {
    let mut path: Option<String> = None;
    for hive in [
        winreg::enums::HKEY_LOCAL_MACHINE,
        winreg::enums::HKEY_CURRENT_USER,
    ] {
        if let Ok(k) = winreg::RegKey::predef(hive).open_subkey("Software\\StartPE") {
            if let Ok(v) = k.get_value::<String, _>("UserPicture") {
                if !v.is_empty() {
                    path = Some(v);
                }
            }
        }
    }
    gtk::gdk::Texture::from_filename(path?).ok()
}

fn build_right_pane(hide: Rc<dyn Fn()>) -> GtkBox {
    let right = GtkBox::new(Orientation::Vertical, 4);
    right.set_size_request(176, -1);
    right.set_margin_top(14);
    right.set_margin_bottom(10);
    right.set_margin_start(8);
    right.set_margin_end(8);

    // User avatar + name (opens the profile folder).
    let username = std::env::var("USERNAME").unwrap_or_else(|_| "User".into());
    let profile = std::env::var("USERPROFILE").unwrap_or_else(|_| "X:\\Users\\Default".into());
    let avatar = adw::Avatar::new(44, Some(&username), true);
    if let Some(tex) = user_picture() {
        avatar.set_custom_image(Some(&tex));
    }
    let name = Label::new(Some(&username));
    name.set_xalign(0.0);
    name.set_hexpand(true);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    name.add_css_class("title-4");
    let header_row = GtkBox::new(Orientation::Horizontal, 10);
    header_row.append(&avatar);
    header_row.append(&name);
    let header = Button::builder().child(&header_row).build();
    header.add_css_class("flat");
    header.add_css_class("sm-rightbtn");
    {
        let hide = hide.clone();
        header.connect_clicked(move |_| {
            appsource::launch(&profile, "");
            hide();
        });
    }
    right.append(&header);
    right.append(&Separator::new(Orientation::Horizontal));

    for (icon, label, target) in right_links() {
        let b = link_button(icon, &label);
        let (target, hide) = (target.clone(), hide.clone());
        b.connect_clicked(move |_| {
            appsource::launch(&target, "");
            hide();
        });
        right.append(&b);
    }

    let run_btn = link_button("system-run-symbolic", "Run\u{2026}");
    {
        let hide = hide.clone();
        run_btn.connect_clicked(move |_| {
            launch_run();
            hide();
        });
    }
    right.append(&run_btn);

    let spacer = GtkBox::new(Orientation::Vertical, 0);
    spacer.set_vexpand(true);
    right.append(&spacer);

    right.append(&Separator::new(Orientation::Horizontal));
    right.append(&power_button(hide));
    right
}

fn link_button(icon: &str, label: &str) -> Button {
    let row = GtkBox::new(Orientation::Horizontal, 10);
    let img = Image::from_icon_name(icon);
    img.set_pixel_size(18);
    row.append(&img);
    let lbl = Label::new(Some(label));
    lbl.set_xalign(0.0);
    lbl.set_hexpand(true);
    lbl.set_ellipsize(gtk::pango::EllipsizeMode::End);
    row.append(&lbl);
    let b = Button::builder().child(&row).build();
    b.add_css_class("flat");
    b.add_css_class("sm-rightbtn");
    b
}

fn power_button(hide: Rc<dyn Fn()>) -> Button {
    let popover = Popover::new();
    let menu = GtkBox::new(Orientation::Vertical, 2);
    for (label, args) in [("Restart", "/r /t 0"), ("Shut down", "/s /t 0")] {
        let item = Button::builder().label(label).build();
        item.add_css_class("flat");
        item.set_halign(Align::Fill);
        if let Some(lbl) = item.child().and_downcast::<Label>() {
            lbl.set_xalign(0.0);
        }
        let (args, pop, hide) = (args.to_string(), popover.clone(), hide.clone());
        item.connect_clicked(move |_| {
            pop.popdown();
            appsource::launch("shutdown.exe", &args);
            hide();
        });
        menu.append(&item);
    }
    popover.set_child(Some(&menu));

    let row = GtkBox::new(Orientation::Horizontal, 10);
    let img = Image::from_icon_name("system-shutdown-symbolic");
    img.set_pixel_size(18);
    row.append(&img);
    let lbl = Label::new(Some("Power"));
    lbl.set_xalign(0.0);
    lbl.set_hexpand(true);
    row.append(&lbl);
    let btn = Button::builder().child(&row).build();
    btn.add_css_class("flat");
    btn.add_css_class("sm-rightbtn");
    {
        let popover = popover.clone();
        btn.connect_clicked(move |b| {
            popover.set_parent(b);
            popover.popup();
        });
    }
    btn
}

/// Launch the Run helper: a sibling `RunBox.exe`, else `startpe.exe --run`.
fn launch_run() {
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name("RunBox.exe");
        if sibling.is_file() {
            let _ = std::process::Command::new(sibling).spawn();
            return;
        }
        let startpe = exe.with_file_name("startpe.exe");
        if startpe.is_file() {
            let _ = std::process::Command::new(startpe).arg("--run").spawn();
        }
    }
}

/// Single instance: only one pre-warmed menu process. Returns true if another is
/// already running (this one should exit).
fn already_running() -> bool {
    unsafe {
        let h = CreateMutexW(None, true, w!("StartPE.StartMenu.SingleInstance"));
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

fn main() -> glib::ExitCode {
    if already_running() {
        return glib::ExitCode::SUCCESS;
    }
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(build_ui);
    app.run()
}
