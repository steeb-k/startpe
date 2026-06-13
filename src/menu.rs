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
//! Usage: a window builds and shows a menu with [`track`], and forwards the two
//! owner-draw messages from its wndproc to [`on_measure`] / [`on_draw`].

use std::cell::Cell;

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Controls::{
    DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_DISABLED, ODS_GRAYED, ODS_SELECTED, ODT_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::taskbar::{make_font, scaled, COL_ACTIVE, COL_BG, COL_TEXT, COL_TEXT_DIM};
use crate::util;

thread_local! {
    /// Shared menu font, created on first use (single UI thread).
    static MENU_FONT: Cell<Option<HFONT>> = const { Cell::new(None) };
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

/// Build a dark owner-drawn popup menu of `(command_id, label)` items, show it at
/// screen `(x, y)`, and return the chosen command id (0 if dismissed).
///
/// `align` adds placement flags (e.g. `TPM_BOTTOMALIGN`); selection/return-mode
/// flags are supplied internally. Blocks until the menu is dismissed.
pub fn track(
    owner: HWND,
    x: i32,
    y: i32,
    align: TRACK_POPUP_MENU_FLAGS,
    items: &[(u32, &str)],
) -> u32 {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return 0;
        };
        // Label buffers must outlive TrackPopupMenu (which reads them back as
        // item data while painting). The call is synchronous, so locals suffice.
        let labels: Vec<Vec<u16>> = items.iter().map(|(_, t)| util::wide(t)).collect();
        for ((id, _), label) in items.iter().zip(labels.iter()) {
            let _ = AppendMenuW(menu, MF_OWNERDRAW, *id as usize, PCWSTR(label.as_ptr()));
        }

        // Dark margin/gutter behind the (owner-drawn) items.
        let bg = CreateSolidBrush(COLORREF(COL_BG));
        let info = MENUINFO {
            cbSize: std::mem::size_of::<MENUINFO>() as u32,
            fMask: MIM_BACKGROUND | MIM_APPLYTOSUBMENUS,
            hbrBack: bg,
            ..Default::default()
        };
        let _ = SetMenuInfo(menu, &info);

        // Required so the menu dismisses on an outside click even when the owner
        // is a WS_EX_NOACTIVATE appbar.
        let _ = SetForegroundWindow(owner);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | align,
            x,
            y,
            0,
            owner,
            None,
        );
        let _ = DestroyMenu(menu);
        let _ = DeleteObject(HGDIOBJ(bg.0));
        cmd.0 as u32
    }
}

/// Read a NUL-terminated wide string (the menu item's `dwItemData`) into a Vec,
/// dropping the terminator.
unsafe fn item_text(ptr: *const u16) -> Vec<u16> {
    if ptr.is_null() {
        return Vec::new();
    }
    let mut len = 0usize;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    std::slice::from_raw_parts(ptr, len).to_vec()
}

/// Handle `WM_MEASUREITEM` for a dark menu item. Returns false (so the caller
/// defers to `DefWindowProc`) if the message isn't for one of our menu items.
pub fn on_measure(lparam: LPARAM) -> bool {
    unsafe {
        let mis = &mut *(lparam.0 as *mut MEASUREITEMSTRUCT);
        if mis.CtlType != ODT_MENU {
            return false;
        }
        let text = item_text(mis.itemData as *const u16);
        let mut sz = SIZE::default();
        let hdc = GetDC(None);
        let old = SelectObject(hdc, HGDIOBJ(menu_font().0));
        if !text.is_empty() {
            let _ = GetTextExtentPoint32W(hdc, &text, &mut sz);
        }
        SelectObject(hdc, old);
        ReleaseDC(None, hdc);
        // Leave room for the (empty) check-mark gutter on the left + padding.
        mis.itemWidth = (sz.cx + scaled(44)) as u32;
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

        let bg = CreateSolidBrush(COLORREF(if selected { COL_ACTIVE } else { COL_BG }));
        FillRect(hdc, &rc, bg);
        let _ = DeleteObject(HGDIOBJ(bg.0));

        let mut text = item_text(dis.itemData as *const u16);
        if !text.is_empty() {
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, COLORREF(if disabled { COL_TEXT_DIM } else { COL_TEXT }));
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
            SelectObject(hdc, old);
        }
        true
    }
}
