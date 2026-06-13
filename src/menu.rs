// SPDX-License-Identifier: GPL-3.0-or-later
//! Dark owner-drawn popup menus.
//!
//! Plain `HMENU` popup menus render in the system's light menu colors, and the
//! only way to recolor them is the undocumented uxtheme dark-mode ordinals —
//! which the project forbids in `startpe.exe`. So instead we own-draw: items are
//! `MF_OWNERDRAW`, the owner window forwards `WM_MEASUREITEM` / `WM_DRAWITEM`
//! here, and we paint them dark with documented GDI. `MIM_BACKGROUND` keeps the
//! surrounding menu margin dark too. Works with or without DWM.
//!
//! Items can be entries, separators, or submenus ([`Item`]); submenus are real
//! `MF_POPUP` flyouts (so they open on hover, native-style) drawn with a
//! right-edge chevron. Usage: a window builds and shows a menu with [`track`] /
//! [`track_items`], and forwards the two owner-draw messages from its wndproc to
//! [`on_measure`] / [`on_draw`].

use std::cell::Cell;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Controls::{
    DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_DISABLED, ODS_GRAYED, ODS_SELECTED, ODT_MENU,
};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_DOWN;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::taskbar::{make_font, make_font_face, scaled, COL_ACTIVE, COL_BG, COL_TEXT, COL_TEXT_DIM};

/// ChevronRight in Segoe MDL2 Assets — drawn at the right of a submenu row.
const GLYPH_CHEVRON: u16 = 0xE76C;

thread_local! {
    /// Shared menu font, created on first use (single UI thread).
    static MENU_FONT: Cell<Option<HFONT>> = const { Cell::new(None) };
    /// Shared symbol font for the submenu chevron.
    static SYM_FONT: Cell<Option<HFONT>> = const { Cell::new(None) };
}

fn menu_font() -> HFONT {
    MENU_FONT.with(|f| match f.get() {
        Some(h) => h,
        None => {
            let h = make_font(scaled(12), 400);
            f.set(Some(h));
            h
        }
    })
}

fn sym_font() -> HFONT {
    SYM_FONT.with(|f| match f.get() {
        Some(h) => h,
        None => {
            let h = make_font_face(scaled(10), 400, w!("Segoe MDL2 Assets"));
            f.set(Some(h));
            h
        }
    })
}

/// A menu entry for [`track_items`].
pub enum Item<'a> {
    /// Clickable entry: (command id, label).
    Entry(u32, &'a str),
    /// A dark divider line.
    Separator,
    /// A flyout submenu: (label, children). Rendered with a right-edge chevron;
    /// the submenu opens on hover (native `MF_POPUP` behavior). Selecting a leaf
    /// returns its command id from [`track_items`].
    Submenu(&'a str, &'a [Item<'a>]),
}

/// Per-item data handed to the owner-draw messages via `dwItemData`. Boxed and
/// kept alive (in a `Vec`) for the duration of the synchronous `TrackPopupMenu`,
/// which reads it back while measuring/painting.
struct ItemData {
    /// Label as UTF-16 *without* a NUL terminator (so the slice length is the
    /// text length for `DrawTextW`/`GetTextExtentPoint32W`). Empty = separator.
    text: Vec<u16>,
    /// True for a submenu row (draw the chevron, reserve width for it).
    submenu: bool,
}

/// Build a dark owner-drawn popup menu from `(command_id, label)` items, show it
/// at screen `(x, y)`, and return the chosen command id (0 if dismissed).
///
/// An item with an empty label is rendered as a (dark, owner-drawn) separator.
/// This is the flat convenience wrapper over [`track_items`]; see it for the
/// `align` / `select_first` semantics.
pub fn track(
    owner: HWND,
    x: i32,
    y: i32,
    align: TRACK_POPUP_MENU_FLAGS,
    items: &[(u32, &str)],
    select_first: bool,
) -> u32 {
    let items: Vec<Item> = items
        .iter()
        .map(|(id, t)| {
            if t.is_empty() {
                Item::Separator
            } else {
                Item::Entry(*id, t)
            }
        })
        .collect();
    track_items(owner, x, y, align, &items, select_first)
}

/// Show a dark owner-drawn popup menu (with optional submenus) at screen
/// `(x, y)` and return the chosen command id (0 if dismissed).
///
/// `align` adds placement flags (e.g. `TPM_BOTTOMALIGN`); selection/return-mode
/// flags are supplied internally. Blocks until the menu is dismissed.
///
/// When `select_first` is set, the first item is pre-highlighted (so a menu
/// opened by keyboard has a default selection that Enter activates) by feeding
/// the menu's modal loop a Down-arrow.
pub fn track_items(
    owner: HWND,
    x: i32,
    y: i32,
    align: TRACK_POPUP_MENU_FLAGS,
    items: &[Item],
    select_first: bool,
) -> u32 {
    unsafe {
        // Dark margin/gutter behind the (owner-drawn) items. One brush is shared
        // by every (sub)menu and must outlive TrackPopupMenu, which reads it
        // while painting; deleted once after the call.
        let bg = CreateSolidBrush(COLORREF(COL_BG));
        // Label buffers (referenced by dwItemData) must outlive TrackPopupMenu.
        let mut datas: Vec<Box<ItemData>> = Vec::new();
        let menu = build(items, bg, &mut datas);
        if menu.0.is_null() {
            let _ = DeleteObject(HGDIOBJ(bg.0));
            return 0;
        }

        // Required so the menu dismisses on an outside click even when the owner
        // is a WS_EX_NOACTIVATE appbar.
        let _ = SetForegroundWindow(owner);
        if select_first {
            // Queue a Down-arrow so the menu's modal loop highlights the first
            // item on open (menu mode consumes the key for navigation).
            let _ = PostMessageW(owner, WM_KEYDOWN, WPARAM(VK_DOWN.0 as usize), LPARAM(0));
        }
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | align,
            x,
            y,
            0,
            owner,
            None,
        );
        // Destroying the root menu recursively frees its attached submenus.
        let _ = DestroyMenu(menu);
        let _ = DeleteObject(HGDIOBJ(bg.0));
        // `datas` drop here, now that painting is done.
        cmd.0 as u32
    }
}

/// Recursively build a dark owner-drawn (sub)menu, pushing each item's boxed
/// `ItemData` into `datas` so the pointers handed to `dwItemData` stay valid.
/// Returns a null `HMENU` on allocation failure.
unsafe fn build(items: &[Item], bg: HBRUSH, datas: &mut Vec<Box<ItemData>>) -> HMENU {
    let Ok(menu) = CreatePopupMenu() else {
        return HMENU::default();
    };
    let info = MENUINFO {
        cbSize: std::mem::size_of::<MENUINFO>() as u32,
        fMask: MIM_BACKGROUND,
        hbrBack: bg,
        ..Default::default()
    };
    let _ = SetMenuInfo(menu, &info);

    for it in items {
        match it {
            Item::Entry(id, text) => {
                append(menu, MF_OWNERDRAW, *id as usize, wide_no_nul(text), false, datas);
            }
            // Separators are still owner-drawn (so they stay dark) but disabled
            // so they can't be hovered or returned.
            Item::Separator => {
                append(menu, MF_OWNERDRAW | MF_DISABLED, 0, Vec::new(), false, datas);
            }
            Item::Submenu(text, children) => {
                let sub = build(children, bg, datas);
                append(menu, MF_OWNERDRAW | MF_POPUP, sub.0 as usize, wide_no_nul(text), true, datas);
            }
        }
    }
    menu
}

/// Append one owner-drawn item, recording its boxed `ItemData` pointer as the
/// item's `dwItemData` (so the owner-draw messages can read its label + flags).
unsafe fn append(
    menu: HMENU,
    flags: MENU_ITEM_FLAGS,
    id: usize,
    text: Vec<u16>,
    submenu: bool,
    datas: &mut Vec<Box<ItemData>>,
) {
    let d = Box::new(ItemData { text, submenu });
    let ptr = (&*d as *const ItemData) as *const u16;
    datas.push(d);
    let _ = AppendMenuW(menu, flags, id, PCWSTR(ptr));
}

/// UTF-16 without a trailing NUL (slice length = character count for the GDI
/// text calls).
fn wide_no_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Handle `WM_MEASUREITEM` for a dark menu item. Returns false (so the caller
/// defers to `DefWindowProc`) if the message isn't for one of our menu items.
pub fn on_measure(lparam: LPARAM) -> bool {
    unsafe {
        let mis = &mut *(lparam.0 as *mut MEASUREITEMSTRUCT);
        if mis.CtlType != ODT_MENU {
            return false;
        }
        let data = &*(mis.itemData as *const ItemData);
        if data.text.is_empty() {
            // Separator: a short, narrow row (the divider line is drawn centered).
            mis.itemWidth = scaled(40) as u32;
            mis.itemHeight = scaled(7) as u32;
            return true;
        }
        let mut sz = SIZE::default();
        let hdc = GetDC(None);
        let old = SelectObject(hdc, HGDIOBJ(menu_font().0));
        let _ = GetTextExtentPoint32W(hdc, &data.text, &mut sz);
        SelectObject(hdc, old);
        ReleaseDC(None, hdc);
        // Leave room for the (empty) check-mark gutter on the left + padding,
        // plus a chevron column on the right for submenu rows.
        let chevron = if data.submenu { scaled(22) } else { 0 };
        mis.itemWidth = (sz.cx + scaled(44) + chevron) as u32;
        mis.itemHeight = (sz.cy.max(scaled(16)) + scaled(10)) as u32;
        true
    }
}

/// Handle `WM_DRAWITEM` for a dark menu item. Returns false if it isn't ours.
pub fn on_draw(lparam: LPARAM) -> bool {
    unsafe {
        let dis = &*(lparam.0 as *const DRAWITEMSTRUCT);
        if dis.CtlType != ODT_MENU {
            return false;
        }
        let selected = dis.itemState.0 & ODS_SELECTED.0 != 0;
        let disabled = dis.itemState.0 & (ODS_GRAYED.0 | ODS_DISABLED.0) != 0;
        let hdc = dis.hDC;
        let rc = dis.rcItem;
        let data = &*(dis.itemData as *const ItemData);

        let bg = CreateSolidBrush(COLORREF(if selected { COL_ACTIVE } else { COL_BG }));
        FillRect(hdc, &rc, bg);
        let _ = DeleteObject(HGDIOBJ(bg.0));

        if data.text.is_empty() {
            // Separator: a single hairline across the middle, inset from the edges.
            let line = CreateSolidBrush(COLORREF(COL_TEXT_DIM));
            let y = (rc.top + rc.bottom) / 2;
            let bar = RECT {
                left: rc.left + scaled(12),
                top: y,
                right: rc.right - scaled(12),
                bottom: y + 1,
            };
            FillRect(hdc, &bar, line);
            let _ = DeleteObject(HGDIOBJ(line.0));
            return true;
        }

        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(if disabled { COL_TEXT_DIM } else { COL_TEXT }));
        let mut text = data.text.clone();
        let old = SelectObject(hdc, HGDIOBJ(menu_font().0));
        let mut tr = RECT {
            left: rc.left + scaled(28),
            top: rc.top,
            right: rc.right - scaled(12),
            bottom: rc.bottom,
        };
        DrawTextW(
            hdc,
            &mut text,
            &mut tr,
            DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX,
        );
        // Submenu rows get a right-aligned chevron in the symbol font.
        if data.submenu {
            SelectObject(hdc, HGDIOBJ(sym_font().0));
            let mut chev = [GLYPH_CHEVRON];
            let mut cr = RECT {
                left: rc.right - scaled(24),
                top: rc.top,
                right: rc.right - scaled(8),
                bottom: rc.bottom,
            };
            DrawTextW(
                hdc,
                &mut chev,
                &mut cr,
                DT_SINGLELINE | DT_VCENTER | DT_RIGHT | DT_NOPREFIX,
            );
        }
        SelectObject(hdc, old);
        true
    }
}
