// SPDX-License-Identifier: GPL-3.0-or-later
#![windows_subsystem = "windows"]

//! GTK4/Libadwaita **Start menu** for Windows PE — the libadwaita counterpart to
//! StartPE's Win32/GDI start menu (`startpe/src/start_menu.rs`). Two panes: a
//! searchable app list (with folder drill-down) from the Start Menu\Programs
//! folders on the left, and system links + power on the right.
//!
//! Phase 1: the UI, shown standalone for development. The taskbar IPC (pre-warmed
//! toggle on the Win key) and the Win+X menu come next.

mod appsource;
mod icons;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{glib, Align, Box as GtkBox, Button, Image, Label, ListBox, ListBoxRow, Orientation,
    Popover, PolicyType, ScrolledWindow, SearchEntry, SelectionMode, Separator};

use appsource::{AppItem, ItemKind};

const APP_ID: &str = "org.winrx.PeStartMenu";

const CSS: &str = "
.sm-right { background-color: @sidebar_bg_color; }
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

    // --- Left pane: search + app list. ---
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

    let left = GtkBox::new(Orientation::Vertical, 0);
    left.set_hexpand(true);
    left.append(&search);
    left.append(&list_scroll);

    // Refresh the list from the current stack + query.
    let do_refresh: Rc<dyn Fn()> = {
        let (list, stack, search, actions) =
            (list.clone(), stack.clone(), search.clone(), actions.clone());
        Rc::new(move || {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            let query = search.text().to_string();
            let items = appsource::enumerate(&stack.borrow(), &query);
            let mut acts = Vec::with_capacity(items.len());
            for item in items {
                let (row, action) = make_row(item);
                list.append(&row);
                acts.push(action);
            }
            *actions.borrow_mut() = acts;
        })
    };

    {
        let do_refresh = do_refresh.clone();
        search.connect_search_changed(move |_| do_refresh());
    }

    // Row activation: Back pops, Folder drills in, Launch runs and closes.
    {
        let (stack, actions, search) = (stack.clone(), actions.clone(), search.clone());
        let (do_refresh, app) = (do_refresh.clone(), app.clone());
        list.connect_row_activated(move |_, row| {
            let idx = row.index();
            if idx < 0 {
                return;
            }
            let action = actions.borrow().get(idx as usize).cloned();
            match action {
                Some(RowAction::Back) => {
                    stack.borrow_mut().pop();
                    do_refresh();
                }
                Some(RowAction::Folder(p)) => {
                    search.set_text("");
                    stack.borrow_mut().push(p);
                    do_refresh();
                }
                Some(RowAction::Launch(p)) => {
                    appsource::launch_path(&p);
                    close_all(&app);
                }
                None => {}
            }
        });
    }

    // --- Right pane: system links + power. ---
    let right = build_right_pane(app);
    right.add_css_class("sm-right");

    // --- Assemble. ---
    let panes = GtkBox::new(Orientation::Horizontal, 0);
    panes.append(&left);
    panes.append(&Separator::new(Orientation::Vertical));
    panes.append(&right);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("StartPE Menu")
        .resizable(false)
        .default_width(560)
        .default_height(540)
        .content(&panes)
        .build();

    do_refresh();
    window.present();
    search.grab_focus();
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
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "User".into());
    vec![
        ("user-home-symbolic", user, profile.clone()),
        ("folder-download-symbolic", "Downloads".into(), format!("{profile}\\Downloads")),
        ("drive-harddisk-symbolic", "This PC".into(), "shell:MyComputerFolder".into()),
        ("applications-system-symbolic", "Control Panel".into(), "control.exe".into()),
        ("utilities-terminal-symbolic", "Command Prompt".into(), "cmd.exe".into()),
    ]
}

fn build_right_pane(app: &adw::Application) -> GtkBox {
    let right = GtkBox::new(Orientation::Vertical, 4);
    right.set_size_request(176, -1);
    right.set_margin_top(14);
    right.set_margin_bottom(10);
    right.set_margin_start(8);
    right.set_margin_end(8);

    for (icon, label, target) in right_links() {
        let b = link_button(icon, &label);
        let (target, app) = (target.clone(), app.clone());
        b.connect_clicked(move |_| {
            appsource::launch(&target, "");
            close_all(&app);
        });
        right.append(&b);
    }

    // Run… launches the sibling RunBox.exe (or startpe --run) helper.
    let run_btn = link_button("system-run-symbolic", "Run\u{2026}");
    {
        let app = app.clone();
        run_btn.connect_clicked(move |_| {
            launch_run();
            close_all(&app);
        });
    }
    right.append(&run_btn);

    // Spacer pushes power to the bottom.
    let spacer = GtkBox::new(Orientation::Vertical, 0);
    spacer.set_vexpand(true);
    right.append(&spacer);

    right.append(&Separator::new(Orientation::Horizontal));
    right.append(&power_button(app));
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

fn power_button(app: &adw::Application) -> Button {
    let popover = Popover::new();
    let menu = GtkBox::new(Orientation::Vertical, 2);
    for (label, args) in [("Restart", "/r /t 0"), ("Shut down", "/s /t 0")] {
        let item = Button::builder().label(label).build();
        item.add_css_class("flat");
        item.set_halign(Align::Fill);
        if let Some(lbl) = item.child().and_downcast::<Label>() {
            lbl.set_xalign(0.0);
        }
        let (args, pop, app) = (args.to_string(), popover.clone(), app.clone());
        item.connect_clicked(move |_| {
            pop.popdown();
            appsource::launch("shutdown.exe", &args);
            close_all(&app);
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

/// Phase 1: launching an item closes the menu (the process exits). The IPC phase
/// will hide it instead and keep the process pre-warmed.
fn close_all(app: &adw::Application) {
    for w in app.windows() {
        w.close();
    }
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}
