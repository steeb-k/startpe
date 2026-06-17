// SPDX-License-Identifier: GPL-3.0-or-later
#![windows_subsystem = "windows"]

//! GTK4/Libadwaita **System Information** for Windows PE — a thin client of the
//! winrx-creator GTK4 runtime. It is the libadwaita counterpart to StartPE's
//! Win32/GDI System Information window (`startpe/src/sysinfo.rs`): the data layer
//! is reused (`sysinfo_data`), the UI is an `AdwNavigationSplitView` with a
//! section sidebar and `.property` rows in `AdwPreferencesGroup`s.
//!
//! Hardware is gathered on a worker thread (WMI is slow in PE); the window opens
//! immediately showing a spinner, then repaints when the data arrives.

mod sysinfo_data;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use gtk::{
    Align, Box as GtkBox, Image, Label, ListBox, ListBoxRow, Orientation, PolicyType,
    ScrolledWindow, SelectionMode, Spinner,
};

use sysinfo_data::{section_groups, SysInfo, SECTIONS};

const APP_ID: &str = "org.winrx.PeSysInfo";

const CSS: &str = "
.si-content { padding: 18px; }
";

fn build_ui(app: &adw::Application) {
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);

    let provider = gtk::CssProvider::new();
    provider.load_from_data(CSS);
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().expect("display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // --- Sidebar: the four sections. ---
    let list = ListBox::new();
    list.add_css_class("navigation-sidebar");
    list.set_selection_mode(SelectionMode::Single);
    for (label, icon) in SECTIONS {
        let row_box = GtkBox::new(Orientation::Horizontal, 12);
        row_box.set_margin_top(7);
        row_box.set_margin_bottom(7);
        row_box.set_margin_start(6);
        row_box.set_margin_end(6);
        row_box.append(&Image::from_icon_name(icon));
        let lbl = Label::new(Some(label));
        lbl.set_xalign(0.0);
        row_box.append(&lbl);
        let row = ListBoxRow::new();
        row.set_child(Some(&row_box));
        list.append(&row);
    }

    let sidebar_scroll = ScrolledWindow::new();
    sidebar_scroll.set_hscrollbar_policy(PolicyType::Never);
    sidebar_scroll.set_vexpand(true);
    sidebar_scroll.set_child(Some(&list));

    let sidebar_tv = adw::ToolbarView::new();
    sidebar_tv.add_top_bar(&adw::HeaderBar::new());
    sidebar_tv.set_content(Some(&sidebar_scroll));
    let sidebar_page = adw::NavigationPage::new(&sidebar_tv, "System Info");
    sidebar_page.set_tag(Some("sidebar"));

    // --- Content: groups/rows for the selected section. ---
    let content_title = adw::WindowTitle::new(SECTIONS[0].0, "");
    let content_header = adw::HeaderBar::new();
    content_header.set_title_widget(Some(&content_title));

    let content_box = GtkBox::new(Orientation::Vertical, 18);
    content_box.add_css_class("si-content");
    content_box.set_valign(Align::Start);

    let clamp = adw::Clamp::new();
    clamp.set_maximum_size(620);
    clamp.set_child(Some(&content_box));

    let content_scroll = ScrolledWindow::new();
    content_scroll.set_hscrollbar_policy(PolicyType::Never);
    content_scroll.set_vexpand(true);
    content_scroll.set_child(Some(&clamp));

    let content_tv = adw::ToolbarView::new();
    content_tv.add_top_bar(&content_header);
    content_tv.set_content(Some(&content_scroll));
    let content_page = adw::NavigationPage::new(&content_tv, "Details");
    content_page.set_tag(Some("content"));

    let split = adw::NavigationSplitView::new();
    split.set_min_sidebar_width(200.0);
    split.set_max_sidebar_width(240.0);
    split.set_sidebar(Some(&sidebar_page));
    split.set_content(Some(&content_page));

    // --- State; start on the spinner view. ---
    let info_store: Rc<RefCell<Option<SysInfo>>> = Rc::new(RefCell::new(None));
    let current = Rc::new(Cell::new(0usize));
    render(&content_box, &content_title, &info_store.borrow(), current.get());

    // Selecting a section re-renders the content pane.
    {
        let (content_box, content_title) = (content_box.clone(), content_title.clone());
        let (info_store, current, split) =
            (info_store.clone(), current.clone(), split.clone());
        list.connect_row_selected(move |_, row| {
            if let Some(row) = row {
                let idx = row.index().max(0) as usize;
                current.set(idx);
                render(&content_box, &content_title, &info_store.borrow(), idx);
                split.set_show_content(true); // navigate on a collapsed/narrow layout
            }
        });
    }
    if let Some(first) = list.row_at_index(0) {
        list.select_row(Some(&first));
    }

    // --- Gather hardware off the main thread, then repaint (gtk4-rs book pattern). ---
    let (sender, receiver) = async_channel::bounded(1);
    gio::spawn_blocking(move || {
        let _ = sender.send_blocking(sysinfo_data::gather());
    });
    {
        let (content_box, content_title) = (content_box.clone(), content_title.clone());
        let (info_store, current) = (info_store.clone(), current.clone());
        glib::spawn_future_local(async move {
            if let Ok(info) = receiver.recv().await {
                *info_store.borrow_mut() = Some(info);
                render(&content_box, &content_title, &info_store.borrow(), current.get());
            }
        });
    }

    adw::ApplicationWindow::builder()
        .application(app)
        .title("System Information")
        .default_width(760)
        .default_height(560)
        .width_request(360)
        .height_request(400)
        .content(&split)
        .build()
        .present();
}

/// Clear and rebuild the content pane for `section`. With `info == None`
/// (still gathering) it shows a centered spinner.
fn render(content_box: &GtkBox, title: &adw::WindowTitle, info: &Option<SysInfo>, section: usize) {
    title.set_title(SECTIONS[section].0);
    while let Some(child) = content_box.first_child() {
        content_box.remove(&child);
    }
    match info {
        None => {
            let busy = GtkBox::new(Orientation::Horizontal, 12);
            busy.set_halign(Align::Center);
            busy.set_valign(Align::Center);
            busy.set_vexpand(true);
            busy.set_margin_top(48);
            let spinner = Spinner::new();
            spinner.start();
            busy.append(&spinner);
            let lbl = Label::new(Some("Gathering system information\u{2026}"));
            lbl.add_css_class("dim-label");
            busy.append(&lbl);
            content_box.append(&busy);
        }
        Some(info) => {
            for g in section_groups(section, info) {
                let group = adw::PreferencesGroup::new();
                group.set_title(&g.title);
                for (k, v) in g.rows {
                    // `.property` styles the row as a caption (title) + value
                    // (subtitle) pair — the libadwaita idiom for displaying info.
                    let row = adw::ActionRow::new();
                    row.add_css_class("property");
                    row.set_title(&glib::markup_escape_text(&k));
                    row.set_subtitle(&glib::markup_escape_text(&v));
                    row.set_subtitle_selectable(true);
                    group.add(&row);
                }
                content_box.append(&group);
            }
        }
    }
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}
