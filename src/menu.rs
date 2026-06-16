// SPDX-License-Identifier: GPL-3.0-or-later
//! Dark, rounded, custom-drawn popup menus.
//!
//! WinPE has no DWM, so a system `TrackPopupMenu` can't get rounded corners, and
//! its owner-drawn separators still take mouse highlight. This module draws the
//! menu itself instead: each menu level is a small `WS_POPUP` window with a
//! rounded GDI region, painted dark with documented GDI.
//!
//! Input without stealing focus (the menu is `WS_EX_NOACTIVATE` so it never
//! dismisses the window that opened it — the start menu hosts its power flyout
//! this way). Because a background window's mouse capture only sees clicks while
//! the cursor is over it, the menu instead watches input globally with transient
//! hooks for its lifetime: a `WH_KEYBOARD_LL` hook drives keyboard navigation
//! and access keys, a `WH_MOUSE_LL` hook dismisses on any click outside the
//! menu, and an `EVENT_SYSTEM_FOREGROUND` WinEvent hook dismisses when another
//! window comes up. Mouse moves/clicks *inside* the menu arrive as ordinary
//! window messages (the cursor is over our window).
//!
//! Items can be entries, separators, or submenus ([`Item`]); a submenu opens as
//! a child window to the right with a chevron. Labels may carry a `&` access-key
//! marker (the next char is underlined and activates the item, Win11-style).
//! [`track`] / [`track_items`] block until the user picks an entry (returning its
//! command id) or dismisses the menu (returning 0).

use std::cell::{Cell, RefCell};

use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::taskbar::{make_font, make_font_face, scaled, COL_BG, COL_HOVER, COL_TEXT, COL_TEXT_DIM};
use crate::util;

/// Posted to the root window from the keyboard hook: WPARAM = virtual-key code.
const MENU_KEY: u32 = WM_APP + 0x40;
/// Posted to the root window to dismiss the menu (from the mouse / WinEvent hooks).
const MENU_DISMISS: u32 = WM_APP + 0x41;

/// ChevronRight in Segoe MDL2 Assets — drawn at the right of a submenu row.
const GLYPH_CHEVRON: &str = "\u{E76C}";

// Geometry (unscaled px).
const ITEM_H: i32 = 30; // entry row height
const SEP_H: i32 = 9; // separator row height
const PAD_V: i32 = 6; // top/bottom margin inside a panel
const TEXT_L: i32 = 16; // left text inset
const TEXT_R: i32 = 16; // right inset
const CHEVRON_W: i32 = 22; // extra right column when a panel has a submenu
const RADIUS: i32 = 12; // rounded-corner ellipse size
const MIN_W: i32 = 170; // minimum panel width

thread_local! {
    static MENU_FONT: Cell<Option<HFONT>> = const { Cell::new(None) };
    static SYM_FONT: Cell<Option<HFONT>> = const { Cell::new(None) };
    /// The active menu while [`track_items`] runs (single UI thread, one at a time).
    static MENU: RefCell<Option<Menu>> = const { RefCell::new(None) };
}

fn menu_font() -> HFONT {
    MENU_FONT.with(|f| match f.get() {
        Some(h) => h,
        None => {
            let h = make_font(scaled(13), 400);
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
    /// A dark divider line (never selectable).
    Separator,
    /// A flyout submenu: (label, children). Rendered with a right-edge chevron;
    /// selecting a leaf returns its command id.
    Submenu(&'a str, &'a [Item<'a>]),
}

/// One owned, resolved menu item.
#[derive(Clone)]
enum Kind {
    Entry,
    Separator,
    Submenu(Vec<RItem>),
}

#[derive(Clone)]
struct RItem {
    cmd: u32,
    /// Label as UTF-16 without a trailing NUL (slice length = char count). Keeps
    /// the `&` access-key marker for `DrawTextW` prefix processing (underline).
    label: Vec<u16>,
    /// Lowercased access-key letter from a `&` marker, if any.
    access: Option<char>,
    kind: Kind,
}

/// Find the `&`-marked access key (the char after the first lone `&`), lowercased.
fn access_key(s: &str) -> Option<char> {
    let b = s.as_bytes();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'&' && b[i + 1] != b'&' {
            return s[i + 1..].chars().next().map(|c| c.to_ascii_lowercase());
        }
        i += 1;
    }
    None
}

/// Label UTF-16 with the first lone `&` removed (for width measurement, since
/// `DrawTextW` does not render the marker itself).
fn measure_label(label: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(label.len());
    let mut dropped = false;
    let mut i = 0;
    while i < label.len() {
        if !dropped && label[i] == b'&' as u16 && label.get(i + 1) != Some(&(b'&' as u16)) {
            dropped = true;
            i += 1;
            continue;
        }
        out.push(label[i]);
        i += 1;
    }
    out
}

fn resolve(items: &[Item]) -> Vec<RItem> {
    items
        .iter()
        .map(|it| match it {
            Item::Entry(id, t) => RItem {
                cmd: *id,
                label: t.encode_utf16().collect(),
                access: access_key(t),
                kind: Kind::Entry,
            },
            Item::Separator => RItem {
                cmd: 0,
                label: Vec::new(),
                access: None,
                kind: Kind::Separator,
            },
            Item::Submenu(t, ch) => RItem {
                cmd: 0,
                label: t.encode_utf16().collect(),
                access: access_key(t),
                kind: Kind::Submenu(resolve(ch)),
            },
        })
        .collect()
}

/// One open menu level (a window).
struct Panel {
    hwnd: HWND,
    items: Vec<RItem>,
    /// Currently highlighted item index (never a separator).
    hover: Option<usize>,
    width: i32,
    height: i32,
    /// Index, in the *parent* panel, of the item that opened this one.
    parent_item: Option<usize>,
}

struct Menu {
    /// `panels[0]` is the root; each later entry is an open submenu level.
    panels: Vec<Panel>,
    result: Option<u32>,
    done: bool,
    kb_hook: HHOOK,
    mouse_hook: HHOOK,
    winevent: HWINEVENTHOOK,
    /// Last cursor position handled, to ignore the synthetic, no-movement
    /// `WM_MOUSEMOVE` that showing a submenu window can generate (it would
    /// otherwise close the submenu we just opened by keyboard).
    last_mouse: (i32, i32),
}

/// Build a dark popup menu from `(command_id, label)` items (empty label =
/// separator) and show it; see [`track_items`].
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

/// Show a dark, rounded popup menu at screen `(x, y)` and block until the user
/// picks an entry (returns its command id) or dismisses it (returns 0).
///
/// `align` positions the menu relative to the anchor: `TPM_BOTTOMALIGN` puts the
/// anchor at the menu's *bottom* (so it grows upward), `TPM_RIGHTALIGN` at its
/// right; otherwise the anchor is the top-left. The menu is clamped on-screen.
///
/// `owner` is the window the menu belongs to (kept for API symmetry; the menu
/// does not activate it). `select_first` pre-highlights the first entry so a
/// keyboard-opened menu has a default selection.
pub fn track_items(
    owner: HWND,
    x: i32,
    y: i32,
    align: TRACK_POPUP_MENU_FLAGS,
    items: &[Item],
    select_first: bool,
) -> u32 {
    let _ = owner;
    // One menu at a time: ignore a re-entrant open (e.g. Win+X pressed again
    // while the menu is up) so we never clobber the active menu's state.
    if MENU.with_borrow(|m| m.is_some()) {
        return 0;
    }
    unsafe {
        let ritems = resolve(items);
        if ritems.is_empty() {
            return 0;
        }
        let (w, h) = measure_panel(&ritems);
        let hwnd = create_panel_window();
        if hwnd.0.is_null() {
            return 0;
        }
        let (px, py) = root_pos(x, y, align, w, h);

        let hover = if select_first {
            first_selectable(&ritems)
        } else {
            None
        };
        MENU.with_borrow_mut(|m| {
            *m = Some(Menu {
                panels: vec![Panel {
                    hwnd,
                    items: ritems,
                    hover,
                    width: w,
                    height: h,
                    parent_item: None,
                }],
                result: None,
                done: false,
                kb_hook: HHOOK::default(),
                mouse_hook: HHOOK::default(),
                winevent: HWINEVENTHOOK::default(),
                last_mouse: (i32::MIN, i32::MIN),
            });
        });
        place_panel(hwnd, px, py, w, h);
        // Watch input globally for the menu's lifetime (we never take focus).
        let kb = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kb_hook), None, 0).unwrap_or_default();
        let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), None, 0).unwrap_or_default();
        let we = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(winevent_hook),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        MENU.with_borrow_mut(|m| {
            if let Some(m) = m.as_mut() {
                m.kb_hook = kb;
                m.mouse_hook = mouse;
                m.winevent = we;
            }
        });
        let _ = InvalidateRect(hwnd, None, true);

        // Modal loop: pump the thread's queue until an item is chosen or the
        // menu is dismissed. Like TrackPopupMenu, this re-enters the owner's
        // wndproc (timers etc.) — which is fine.
        let mut msg = MSG::default();
        loop {
            let fin = MENU.with_borrow(|m| {
                m.as_ref().map(|m| m.done || m.result.is_some()).unwrap_or(true)
            });
            if fin {
                break;
            }
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 <= 0 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Teardown.
        let menu = MENU.with_borrow_mut(|m| m.take());
        let Some(menu) = menu else { return 0 };
        if !menu.kb_hook.is_invalid() {
            let _ = UnhookWindowsHookEx(menu.kb_hook);
        }
        if !menu.mouse_hook.is_invalid() {
            let _ = UnhookWindowsHookEx(menu.mouse_hook);
        }
        if !menu.winevent.is_invalid() {
            let _ = UnhookWinEvent(menu.winevent);
        }
        for p in menu.panels {
            let _ = DestroyWindow(p.hwnd);
        }
        menu.result.unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Geometry

fn ih(it: &RItem) -> i32 {
    if matches!(it.kind, Kind::Separator) {
        scaled(SEP_H)
    } else {
        scaled(ITEM_H)
    }
}

/// Client rect of item `idx` within its panel (full panel width).
fn item_rect(panel: &Panel, idx: usize) -> RECT {
    let mut y = scaled(PAD_V);
    for i in 0..idx {
        y += ih(&panel.items[i]);
    }
    RECT {
        left: 0,
        top: y,
        right: panel.width,
        bottom: y + ih(&panel.items[idx]),
    }
}

unsafe fn measure_panel(items: &[RItem]) -> (i32, i32) {
    let has_sub = items.iter().any(|it| matches!(it.kind, Kind::Submenu(_)));
    let hdc = GetDC(None);
    let old = SelectObject(hdc, HGDIOBJ(menu_font().0));
    let mut text_w = 0;
    for it in items {
        if matches!(it.kind, Kind::Separator) {
            continue;
        }
        let mut sz = SIZE::default();
        let _ = GetTextExtentPoint32W(hdc, &measure_label(&it.label), &mut sz);
        text_w = text_w.max(sz.cx);
    }
    SelectObject(hdc, old);
    ReleaseDC(None, hdc);

    let chevron = if has_sub { scaled(CHEVRON_W) } else { 0 };
    let w = (text_w + scaled(TEXT_L) + scaled(TEXT_R) + chevron).max(scaled(MIN_W));
    let mut h = scaled(PAD_V) * 2;
    for it in items {
        h += ih(it);
    }
    (w, h)
}

/// Place the root menu relative to its anchor and clamp it on-screen.
fn root_pos(x: i32, y: i32, align: TRACK_POPUP_MENU_FLAGS, w: i32, h: i32) -> (i32, i32) {
    let mut px = x;
    let mut py = y;
    if align.0 & TPM_BOTTOMALIGN.0 != 0 {
        py = y - h;
    }
    if align.0 & TPM_RIGHTALIGN.0 != 0 {
        px = x - w;
    }
    clamp_screen(px, py, w, h)
}

fn clamp_screen(x: i32, y: i32, w: i32, h: i32) -> (i32, i32) {
    unsafe {
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let px = x.clamp(scaled(2), (sw - w - scaled(2)).max(scaled(2)));
        let py = y.clamp(scaled(2), (sh - h - scaled(2)).max(scaled(2)));
        (px, py)
    }
}

fn first_selectable(items: &[RItem]) -> Option<usize> {
    items
        .iter()
        .position(|it| !matches!(it.kind, Kind::Separator))
}

/// Next non-separator index from `cur` in direction `dir` (±1), wrapping.
fn next_sel(items: &[RItem], cur: Option<usize>, dir: i32) -> Option<usize> {
    let n = items.len() as i32;
    if n == 0 {
        return None;
    }
    let mut i = match cur {
        Some(c) => c as i32,
        None => {
            if dir > 0 {
                -1
            } else {
                n
            }
        }
    };
    for _ in 0..n {
        i = (i + dir).rem_euclid(n);
        if !matches!(items[i as usize].kind, Kind::Separator) {
            return Some(i as usize);
        }
    }
    cur
}

/// Hit-test a screen point against the open panels (deepest first). Returns
/// `(panel index, item index, is_submenu, is_separator)`.
unsafe fn hit_panel_item(m: &Menu, pt: POINT) -> Option<(usize, usize, bool, bool)> {
    for pi in (0..m.panels.len()).rev() {
        let p = &m.panels[pi];
        let mut wr = RECT::default();
        let _ = GetWindowRect(p.hwnd, &mut wr);
        if pt.x < wr.left || pt.x >= wr.right || pt.y < wr.top || pt.y >= wr.bottom {
            continue;
        }
        let cx = pt.x - wr.left;
        let cy = pt.y - wr.top;
        for ii in 0..p.items.len() {
            let r = item_rect(p, ii);
            if cy >= r.top && cy < r.bottom && cx >= 0 && cx < p.width {
                let is_sub = matches!(p.items[ii].kind, Kind::Submenu(_));
                let is_sep = matches!(p.items[ii].kind, Kind::Separator);
                return Some((pi, ii, is_sub, is_sep));
            }
        }
        return None; // inside the window margin, between items
    }
    None
}

// ---------------------------------------------------------------------------
// Windows

unsafe fn create_panel_window() -> HWND {
    let hinstance: HINSTANCE = match GetModuleHandleW(None) {
        Ok(h) => h.into(),
        Err(_) => return HWND::default(),
    };
    let class = w!("StartPE_PopupMenu");
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW | CS_DROPSHADOW,
        lpfnWndProc: Some(wndproc),
        hInstance: hinstance,
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        lpszClassName: class,
        ..Default::default()
    };
    RegisterClassW(&wc); // harmless if already registered
    CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
        class,
        w!("StartPE Menu"),
        WS_POPUP,
        0,
        0,
        10,
        10,
        None,
        None,
        hinstance,
        None,
    )
    .unwrap_or_default()
}

unsafe fn place_panel(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) {
    let r = scaled(RADIUS);
    let rgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, r, r);
    SetWindowRgn(hwnd, rgn, true);
    let _ = SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_SHOWWINDOW | SWP_NOACTIVATE);
}

/// Destroy every open panel at depth `>= depth` (closing submenu levels).
unsafe fn close_below(depth: usize) {
    let closed: Vec<HWND> = MENU.with_borrow_mut(|m| {
        m.as_mut()
            .map(|m| {
                if depth < m.panels.len() {
                    m.panels.drain(depth..).map(|p| p.hwnd).collect()
                } else {
                    Vec::new()
                }
            })
            .unwrap_or_default()
    });
    for h in closed {
        let _ = DestroyWindow(h);
    }
}

/// Open the submenu of item `item_idx` in panel `parent_idx` (no-op if already
/// open for that item). `select_first` pre-highlights the first child entry.
unsafe fn open_submenu(parent_idx: usize, item_idx: usize, select_first: bool) {
    // Gather what we need under a short borrow, then act without it.
    let info = MENU.with_borrow(|m| {
        let m = m.as_ref()?;
        if parent_idx + 1 < m.panels.len()
            && m.panels[parent_idx + 1].parent_item == Some(item_idx)
        {
            return None; // already open for this item
        }
        let parent = m.panels.get(parent_idx)?;
        let Kind::Submenu(children) = &parent.items.get(item_idx)?.kind else {
            return None;
        };
        let top = item_rect(parent, item_idx).top;
        Some((parent.hwnd, children.clone(), top))
    });
    let Some((parent_hwnd, children, item_top)) = info else {
        return;
    };
    close_below(parent_idx + 1);

    let (w, h) = measure_panel(&children);
    let mut pr = RECT::default();
    let _ = GetWindowRect(parent_hwnd, &mut pr);
    let sw = GetSystemMetrics(SM_CXSCREEN);
    let mut cx = pr.right - scaled(4);
    if cx + w > sw - scaled(2) {
        cx = pr.left - w + scaled(4); // flip to the left if it would overflow
    }
    let (cx, cy) = clamp_screen(cx, pr.top + item_top - scaled(PAD_V), w, h);

    let child = create_panel_window();
    if child.0.is_null() {
        return;
    }
    let hover = if select_first {
        first_selectable(&children)
    } else {
        None
    };
    MENU.with_borrow_mut(|m| {
        if let Some(m) = m.as_mut() {
            // Keep the parent's submenu row highlighted while its flyout is open
            // (so a keyboard-opened submenu reads as "this row is active").
            if let Some(parent) = m.panels.get_mut(parent_idx) {
                parent.hover = Some(item_idx);
            }
            m.panels.push(Panel {
                hwnd: child,
                items: children,
                hover,
                width: w,
                height: h,
                parent_item: Some(item_idx),
            });
        }
    });
    place_panel(child, cx, cy, w, h);
    invalidate_all();
}

fn invalidate_all() {
    MENU.with_borrow(|m| {
        if let Some(m) = m.as_ref() {
            for p in &m.panels {
                unsafe {
                    let _ = InvalidateRect(p.hwnd, None, true);
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Painting

fn fill(hdc: HDC, rect: &RECT, color: u32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color));
        FillRect(hdc, rect, brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));
    }
}

unsafe fn draw_str(hdc: HDC, s: &[u16], rect: &mut RECT, flags: DRAW_TEXT_FORMAT) {
    let mut t = s.to_vec();
    if !t.is_empty() {
        DrawTextW(hdc, &mut t, rect, flags);
    }
}

unsafe fn paint(panel: &Panel) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(panel.hwnd, &mut ps);

    // Double-buffer to avoid flicker on hover repaints.
    let mem = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, panel.width, panel.height);
    let oldbmp = SelectObject(mem, HGDIOBJ(bmp.0));

    let full = RECT {
        left: 0,
        top: 0,
        right: panel.width,
        bottom: panel.height,
    };
    fill(mem, &full, COL_BG);
    SetBkMode(mem, TRANSPARENT);
    let oldfont = SelectObject(mem, HGDIOBJ(menu_font().0));

    for (i, it) in panel.items.iter().enumerate() {
        let r = item_rect(panel, i);
        match &it.kind {
            Kind::Separator => {
                let y = (r.top + r.bottom) / 2;
                let bar = RECT {
                    left: r.left + scaled(12),
                    top: y,
                    right: r.right - scaled(12),
                    bottom: y + 1,
                };
                fill(mem, &bar, COL_TEXT_DIM);
            }
            Kind::Entry | Kind::Submenu(_) => {
                if panel.hover == Some(i) {
                    // Rounded highlight, inset slightly from the panel edges.
                    let hl = RECT {
                        left: r.left + scaled(4),
                        top: r.top + scaled(1),
                        right: r.right - scaled(4),
                        bottom: r.bottom - scaled(1),
                    };
                    let brush = CreateSolidBrush(COLORREF(COL_HOVER));
                    let oldbr = SelectObject(mem, HGDIOBJ(brush.0));
                    let oldpen = SelectObject(mem, GetStockObject(NULL_PEN));
                    let rad = scaled(7);
                    let _ = RoundRect(mem, hl.left, hl.top, hl.right, hl.bottom, rad, rad);
                    SelectObject(mem, oldpen);
                    SelectObject(mem, oldbr);
                    let _ = DeleteObject(HGDIOBJ(brush.0));
                }
                SetTextColor(mem, COLORREF(COL_TEXT));
                let mut tr = RECT {
                    left: r.left + scaled(TEXT_L),
                    top: r.top,
                    right: r.right - scaled(TEXT_R),
                    bottom: r.bottom,
                };
                // No DT_NOPREFIX: the `&` marker underlines the access-key letter.
                draw_str(mem, &it.label, &mut tr, DT_SINGLELINE | DT_VCENTER | DT_LEFT);
                if matches!(it.kind, Kind::Submenu(_)) {
                    SelectObject(mem, HGDIOBJ(sym_font().0));
                    SetTextColor(mem, COLORREF(COL_TEXT_DIM));
                    let mut cr = RECT {
                        left: r.right - scaled(24),
                        top: r.top,
                        right: r.right - scaled(8),
                        bottom: r.bottom,
                    };
                    let chev: Vec<u16> = GLYPH_CHEVRON.encode_utf16().collect();
                    draw_str(mem, &chev, &mut cr, DT_SINGLELINE | DT_VCENTER | DT_RIGHT | DT_NOPREFIX);
                    SelectObject(mem, HGDIOBJ(menu_font().0));
                }
            }
        }
    }

    // 1px accent ring around the panel, matching the Start menu's purple trim.
    // The menu never takes focus (WS_EX_NOACTIVATE), so we draw the live accent
    // color directly rather than the focus-aware ring. Same corner radius as the
    // window region so the stroke hugs the rounded edge.
    let ring = RECT {
        left: 0,
        top: 0,
        right: panel.width,
        bottom: panel.height,
    };
    let pen = CreatePen(PS_SOLID, 1, COLORREF(crate::taskbar::start_button_color()));
    let old_pen = SelectObject(mem, HGDIOBJ(pen.0));
    let old_brush = SelectObject(mem, GetStockObject(NULL_BRUSH));
    let rad = scaled(RADIUS);
    let _ = RoundRect(mem, ring.left, ring.top, ring.right, ring.bottom, rad, rad);
    SelectObject(mem, old_pen);
    SelectObject(mem, old_brush);
    let _ = DeleteObject(HGDIOBJ(pen.0));

    let _ = BitBlt(hdc, 0, 0, panel.width, panel.height, mem, 0, 0, SRCCOPY);
    SelectObject(mem, oldfont);
    SelectObject(mem, oldbmp);
    let _ = DeleteObject(HGDIOBJ(bmp.0));
    let _ = DeleteDC(mem);
    let _ = EndPaint(panel.hwnd, &ps);
}

// ---------------------------------------------------------------------------
// Input

fn is_nav(vk: u32) -> bool {
    matches!(
        VIRTUAL_KEY(vk as u16),
        VK_UP | VK_DOWN | VK_LEFT | VK_RIGHT | VK_RETURN | VK_ESCAPE
    )
}

/// Keys the open menu consumes: navigation plus letters/digits (access keys).
/// Everything else (modifiers, function keys, …) passes through.
fn is_menu_key(vk: u32) -> bool {
    is_nav(vk) || (0x30..=0x5A).contains(&vk)
}

fn root_hwnd() -> Option<HWND> {
    MENU.with(|c| {
        c.try_borrow()
            .ok()
            .and_then(|m| m.as_ref().and_then(|m| m.panels.first().map(|p| p.hwnd)))
    })
}

/// Low-level keyboard hook: forwards the keys the menu handles to its root
/// window while it is up (and swallows them so they don't reach other windows).
unsafe extern "system" fn kb_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && (wparam.0 as u32 == WM_KEYDOWN || wparam.0 as u32 == WM_SYSKEYDOWN) {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        if let Some(root) = root_hwnd() {
            if is_menu_key(kb.vkCode) {
                let _ = PostMessageW(root, MENU_KEY, WPARAM(kb.vkCode as usize), LPARAM(0));
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// Low-level mouse hook: a button press anywhere outside the open menu dismisses
/// it (clicks inside arrive as ordinary window messages and are left alone). The
/// click is not swallowed, so it still reaches whatever the user clicked.
unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let down = matches!(
            wparam.0 as u32,
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_NCLBUTTONDOWN | WM_NCRBUTTONDOWN
        );
        if down {
            let ms = &*(lparam.0 as *const MSLLHOOKSTRUCT);
            let inside = MENU.with(|c| {
                c.try_borrow()
                    .ok()
                    .and_then(|m| m.as_ref().map(|m| hit_panel_item(m, ms.pt).is_some()))
                    .unwrap_or(false)
            });
            if !inside {
                if let Some(root) = root_hwnd() {
                    let _ = PostMessageW(root, MENU_DISMISS, WPARAM(0), LPARAM(0));
                }
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// WinEvent hook: another window coming to the foreground (a new window opening,
/// or one activated by a click we let through) dismisses the menu.
unsafe extern "system" fn winevent_hook(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if event != EVENT_SYSTEM_FOREGROUND {
        return;
    }
    let ours = MENU.with(|c| {
        c.try_borrow()
            .ok()
            .and_then(|m| m.as_ref().map(|m| m.panels.iter().any(|p| p.hwnd == hwnd)))
            .unwrap_or(false)
    });
    if !ours {
        if let Some(root) = root_hwnd() {
            let _ = PostMessageW(root, MENU_DISMISS, WPARAM(0), LPARAM(0));
        }
    }
}

fn set_done() {
    MENU.with_borrow_mut(|m| {
        if let Some(m) = m.as_mut() {
            m.done = true;
        }
    });
}

fn set_result(cmd: u32) {
    MENU.with_borrow_mut(|m| {
        if let Some(m) = m.as_mut() {
            m.result = Some(cmd);
        }
    });
}

/// Update hover for a screen point and open/close submenus to match. Returns
/// true if anything visible changed.
unsafe fn on_mouse_move(pt: POINT) {
    // Ignore moves that don't actually move the cursor — Windows can post a
    // synthetic WM_MOUSEMOVE when a window appears under/over the pointer, and
    // acting on it would close a submenu the keyboard just opened.
    let moved = MENU.with_borrow_mut(|m| match m.as_mut() {
        Some(m) if m.last_mouse != (pt.x, pt.y) => {
            m.last_mouse = (pt.x, pt.y);
            true
        }
        _ => false,
    });
    if !moved {
        return;
    }
    let Some((pi, ii, is_sub, is_sep)) = MENU.with_borrow(|m| m.as_ref().and_then(|m| hit_panel_item(m, pt)))
    else {
        return;
    };

    let (changed, close_to, open) = MENU.with_borrow_mut(|m| {
        let Some(m) = m.as_mut() else {
            return (false, None, false);
        };
        let newhover = if is_sep { None } else { Some(ii) };
        let already_open =
            m.panels.len() > pi + 1 && m.panels[pi + 1].parent_item == Some(ii) && is_sub;
        let changed = m.panels[pi].hover != newhover || (m.panels.len() > pi + 1 && !already_open);
        m.panels[pi].hover = newhover;
        let close_to = if already_open {
            None
        } else if m.panels.len() > pi + 1 {
            Some(pi + 1)
        } else {
            None
        };
        (changed, close_to, is_sub && !already_open)
    });

    if let Some(n) = close_to {
        close_below(n);
    }
    if open {
        open_submenu(pi, ii, false);
    }
    if changed {
        invalidate_all();
    }
}

unsafe fn on_key(vk: u32) {
    enum Act {
        None,
        Repaint,
        Done,
        OpenSub(usize, usize),
        CloseTop(usize),
        Select(u32),
    }
    let act = MENU.with_borrow_mut(|m| {
        let Some(m) = m.as_mut() else { return Act::None };
        let depth = m.panels.len() - 1;
        let panel = &mut m.panels[depth];
        match VIRTUAL_KEY(vk as u16) {
            VK_DOWN => {
                panel.hover = next_sel(&panel.items, panel.hover, 1);
                Act::Repaint
            }
            VK_UP => {
                panel.hover = next_sel(&panel.items, panel.hover, -1);
                Act::Repaint
            }
            VK_RIGHT => match panel.hover {
                Some(i) if matches!(panel.items[i].kind, Kind::Submenu(_)) => Act::OpenSub(depth, i),
                _ => Act::None,
            },
            VK_LEFT => {
                if depth > 0 {
                    Act::CloseTop(depth)
                } else {
                    Act::None
                }
            }
            VK_RETURN => match panel.hover {
                Some(i) => match &panel.items[i].kind {
                    Kind::Entry => Act::Select(panel.items[i].cmd),
                    Kind::Submenu(_) => Act::OpenSub(depth, i),
                    Kind::Separator => Act::None,
                },
                None => Act::None,
            },
            VK_ESCAPE => {
                if depth > 0 {
                    Act::CloseTop(depth)
                } else {
                    Act::Done
                }
            }
            // Access key: a letter/digit that matches an item's `&` mnemonic
            // activates it (Win11-style — no Alt needed once the menu is open).
            _ => {
                let ch = (vk as u8 as char).to_ascii_lowercase();
                match panel
                    .items
                    .iter()
                    .position(|it| it.access == Some(ch) && !matches!(it.kind, Kind::Separator))
                {
                    Some(i) => match &panel.items[i].kind {
                        Kind::Submenu(_) => Act::OpenSub(depth, i),
                        _ => Act::Select(panel.items[i].cmd),
                    },
                    None => Act::None,
                }
            }
        }
    });
    match act {
        Act::None => {}
        Act::Repaint => invalidate_all(),
        Act::Done => set_done(),
        Act::Select(cmd) => set_result(cmd),
        Act::OpenSub(p, i) => open_submenu(p, i, true),
        Act::CloseTop(depth) => {
            close_below(depth);
            invalidate_all();
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEMOVE => {
            let mut pt = POINT {
                x: util::loword(lparam.0),
                y: util::hiword(lparam.0),
            };
            let _ = ClientToScreen(hwnd, &mut pt);
            on_mouse_move(pt);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let mut pt = POINT {
                x: util::loword(lparam.0),
                y: util::hiword(lparam.0),
            };
            let _ = ClientToScreen(hwnd, &mut pt);
            enum Up {
                None,
                Select(u32),
                Open(usize, usize),
            }
            let up = MENU.with_borrow(|m| {
                let Some(m) = m.as_ref() else { return Up::None };
                match hit_panel_item(m, pt) {
                    Some((pi, ii, true, _)) => Up::Open(pi, ii),
                    Some((pi, ii, false, false)) => Up::Select(m.panels[pi].items[ii].cmd),
                    _ => Up::None,
                }
            });
            match up {
                Up::Select(cmd) => set_result(cmd),
                Up::Open(pi, ii) => open_submenu(pi, ii, false),
                Up::None => {}
            }
            LRESULT(0)
        }
        MENU_KEY => {
            on_key(wparam.0 as u32);
            LRESULT(0)
        }
        MENU_DISMISS => {
            set_done();
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let painted = MENU.with(|c| {
                if let Ok(m) = c.try_borrow() {
                    if let Some(m) = m.as_ref() {
                        if let Some(p) = m.panels.iter().find(|p| p.hwnd == hwnd) {
                            paint(p);
                            return true;
                        }
                    }
                }
                false
            });
            if !painted {
                // Panel not registered yet (mid-create): just validate it dark.
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                fill(hdc, &rc, COL_BG);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
