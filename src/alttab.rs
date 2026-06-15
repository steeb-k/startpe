// SPDX-License-Identifier: GPL-3.0-or-later
//! Windows 11–style Alt+Tab window switcher.
//!
//! A low-level keyboard hook captures Alt+Tab (and the navigation that follows
//! while Alt is held) before the system's own switcher can fire, and drives a
//! centered overlay panel: one tile per top-level window, each showing the
//! app icon, title, and a screenshot of the window. Tiles flow left-to-right
//! and wrap into a grid once a row would run past ~85% of the screen width, so
//! the panel never extends past the screen edges.
//!
//! No DWM dependency: screenshots are captured with `PrintWindow`
//! (`PW_RENDERFULLCONTENT`) into plain GDI bitmaps, so it works in WinPE where
//! there is no composition for live `DwmRegisterThumbnail` previews. Everything
//! is documented Win32.
//!
//! Tab / →   next, Shift+Tab / ←   previous, ↑/↓ move by a row, Esc cancels,
//! releasing Alt (or Enter, or a mouse click) activates the selected window.

use core::ffi::c_void;
use std::cell::{Cell, RefCell};

use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::taskbar::{
    make_font, scaled, COL_ACCENT, COL_ACTIVE, COL_BG, COL_HOVER, COL_TEXT, COL_TEXT_DIM,
};
use crate::util;

// Posted from the hook to the overlay window (the hook itself must return fast,
// so the heavy work — enumeration, screenshots, painting — happens here).
const MSG_STEP: u32 = WM_APP + 1; // wparam: 1 = forward, 0 = backward
const MSG_NAV_ROW: u32 = WM_APP + 2; // wparam: 1 = down, 0 = up
const MSG_COMMIT: u32 = WM_APP + 3; // activate the selected window
const MSG_CANCEL: u32 = WM_APP + 4; // dismiss without switching

// Tile / panel geometry (unscaled px).
const TILE_W: i32 = 200;
const TILE_H: i32 = 150;
const HEADER_H: i32 = 30; // icon + title strip at the top of each tile
const TILE_PAD: i32 = 6; // inset inside a tile
const GAP: i32 = 10; // gap between tiles
const PANEL_PAD: i32 = 12; // panel border around the grid
/// The grid wraps once a row would exceed this fraction of the screen width.
const MAX_WIDTH_PCT: i32 = 85;

/// One switchable window and its captured presentation.
struct Entry {
    hwnd: HWND,
    title: String,
    icon: Option<HICON>,
    /// Screenshot pre-scaled to fit a tile's thumbnail area, with its size.
    /// `None` for minimized / un-capturable windows (icon shown instead).
    shot: Option<HBITMAP>,
    shot_w: i32,
    shot_h: i32,
}

struct AltTab {
    hwnd: HWND,
    font: HFONT,
    entries: Vec<Entry>,
    selected: usize,
    cols: usize,
    width: i32,
    height: i32,
    /// Tile under the mouse, for hover highlight.
    hover: Option<usize>,
    shown: bool,
}

thread_local! {
    static AT: RefCell<Option<AltTab>> = const { RefCell::new(None) };
    /// Overlay HWND, read by the hook without borrowing `AT`.
    static OVERLAY: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
    /// True from the opening Alt+Tab until the gesture commits/cancels. Owned by
    /// the hook so it can decide open-vs-advance synchronously (posted messages
    /// are processed later, which would otherwise race into a double-open).
    static ENGAGED: Cell<bool> = const { Cell::new(false) };
}

/// Create the (hidden) overlay window and install the Alt+Tab keyboard hook.
pub fn install() {
    ensure_window();
    unsafe {
        // Hook callbacks arrive on this (installing) thread's message loop, so
        // the thread_locals above are safe to touch from inside the hook.
        let _ = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kb_hook), None, 0);
    }
    log_install();
}

fn log_install() {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("X:\\startpe.log")
    {
        let _ = writeln!(
            f,
            "StartPE v{} alt-tab switcher installed",
            env!("CARGO_PKG_VERSION")
        );
    }
}

// ---------------------------------------------------------------------------
// Keyboard hook

unsafe extern "system" fn kb_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let injected = kb.flags.0 & LLKHF_INJECTED.0 != 0;
        if !injected {
            let m = wparam.0 as u32;
            let down = m == WM_KEYDOWN || m == WM_SYSKEYDOWN;
            let up = m == WM_KEYUP || m == WM_SYSKEYUP;
            let vk = kb.vkCode;
            let overlay = OVERLAY.get();
            let engaged = ENGAGED.get();
            let alt_down = key_down(VK_MENU);
            let is_alt = vk == VK_LMENU.0 as u32 || vk == VK_RMENU.0 as u32;

            if !overlay.is_invalid() {
                // Releasing Alt commits the highlighted window.
                if up && is_alt && engaged {
                    ENGAGED.set(false);
                    let _ = PostMessageW(overlay, MSG_COMMIT, WPARAM(0), LPARAM(0));
                    // Let the real Alt-up through: the menu-activation it would
                    // otherwise trigger was already defused by the dummy key we
                    // injected at open (below).
                    return CallNextHookEx(None, code, wparam, lparam);
                }

                if down {
                    // Alt+Tab: open the switcher (first press) or advance it.
                    if vk == VK_TAB.0 as u32 && alt_down {
                        if !engaged {
                            ENGAGED.set(true);
                            // The foreground app already saw the Alt-down that
                            // preceded Tab; a lone Alt press/release activates
                            // its menu bar. Inject a throwaway keystroke now so
                            // the Alt no longer reads as "pressed alone" — the
                            // same defusing trick the Win-key hook uses.
                            send_dummy();
                        }
                        let dir = if key_down(VK_SHIFT) { 0 } else { 1 };
                        let _ = PostMessageW(overlay, MSG_STEP, WPARAM(dir), LPARAM(0));
                        return LRESULT(1);
                    }
                    if engaged {
                        // Navigation while the switcher is up.
                        let posted = if vk == VK_ESCAPE.0 as u32 {
                            ENGAGED.set(false);
                            Some((MSG_CANCEL, 0usize))
                        } else if vk == VK_RIGHT.0 as u32 {
                            Some((MSG_STEP, 1))
                        } else if vk == VK_LEFT.0 as u32 {
                            Some((MSG_STEP, 0))
                        } else if vk == VK_DOWN.0 as u32 {
                            Some((MSG_NAV_ROW, 1))
                        } else if vk == VK_UP.0 as u32 {
                            Some((MSG_NAV_ROW, 0))
                        } else if vk == VK_RETURN.0 as u32 {
                            ENGAGED.set(false);
                            Some((MSG_COMMIT, 0))
                        } else {
                            None
                        };
                        if let Some((msg, w)) = posted {
                            let _ = PostMessageW(overlay, msg, WPARAM(w), LPARAM(0));
                            return LRESULT(1);
                        }
                    }
                }

                // While engaged, swallow Tab repeats and Alt auto-repeat so the
                // foreground app stays quiescent under the overlay.
                if engaged && (vk == VK_TAB.0 as u32 || (is_alt && down)) {
                    return LRESULT(1);
                }
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

fn key_down(vk: VIRTUAL_KEY) -> bool {
    unsafe { (GetAsyncKeyState(vk.0 as i32) as u32 & 0x8000) != 0 }
}

/// Inject an undefined-VK keystroke (down+up) to break a "lone Alt" sequence.
unsafe fn send_dummy() {
    let mk = |up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0xFF),
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                ..Default::default()
            },
        },
    };
    let inputs = [mk(false), mk(true)];
    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
}

// ---------------------------------------------------------------------------
// Window enumeration & capture

unsafe fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = GetClassNameW(hwnd, &mut buf) as usize;
    String::from_utf16_lossy(&buf[..n])
}

unsafe fn title_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = GetWindowTextW(hwnd, &mut buf) as usize;
    String::from_utf16_lossy(&buf[..n])
}

/// Standard Alt+Tab eligibility: a visible, titled, un-owned, non-tool,
/// non-cloaked top-level window that isn't one of StartPE's own surfaces or the
/// shell's desktop windows.
unsafe fn eligible(hwnd: HWND) -> bool {
    if !IsWindowVisible(hwnd).as_bool() || GetWindowTextLengthW(hwnd) == 0 {
        return false;
    }
    let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
    if ex & WS_EX_TOOLWINDOW.0 != 0 {
        return false;
    }
    let mut cloaked = 0u32;
    let _ = DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut _ as *mut c_void,
        std::mem::size_of::<u32>() as u32,
    );
    if cloaked != 0 {
        return false;
    }
    if let Ok(owner) = GetWindow(hwnd, GW_OWNER) {
        if !owner.is_invalid() {
            return false;
        }
    }
    !matches!(
        class_of(hwnd).as_str(),
        "Progman"
            | "WorkerW"
            | "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "StartPE_Taskbar"
            | "StartPE_Menu"
            | "StartPE_Peek"
            | "StartPE_AltTab"
            | "StartPE_Desktop"
    )
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let v = &mut *(lparam.0 as *mut Vec<HWND>);
    if eligible(hwnd) {
        v.push(hwnd);
    }
    TRUE
}

unsafe fn icon_of(hwnd: HWND) -> Option<HICON> {
    for wparam in [ICON_SMALL2 as usize, ICON_SMALL as usize, ICON_BIG as usize] {
        let mut result: usize = 0;
        let _ = SendMessageTimeoutW(
            hwnd,
            WM_GETICON,
            WPARAM(wparam),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            100,
            Some(&mut result),
        );
        if result != 0 {
            return Some(HICON(result as *mut _));
        }
    }
    let h = GetClassLongPtrW(hwnd, GCLP_HICONSM);
    if h != 0 {
        return Some(HICON(h as *mut _));
    }
    let h = GetClassLongPtrW(hwnd, GCLP_HICON);
    if h != 0 {
        return Some(HICON(h as *mut _));
    }
    None
}

/// Capture `hwnd` and pre-scale it to fit a tile thumbnail, returning the
/// scaled bitmap and its size. `None` for minimized or un-capturable windows.
unsafe fn capture(hwnd: HWND) -> Option<(HBITMAP, i32, i32)> {
    if IsIconic(hwnd).as_bool() {
        return None;
    }
    let mut rc = RECT::default();
    if GetWindowRect(hwnd, &mut rc).is_err() {
        return None;
    }
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    if w <= 0 || h <= 0 {
        return None;
    }

    let screen = GetDC(None);
    let src = CreateCompatibleDC(screen);
    let full = CreateCompatibleBitmap(screen, w, h);
    let old_src = SelectObject(src, HGDIOBJ(full.0));
    let ok = PrintWindow(hwnd, src, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)).as_bool();

    let result = if ok {
        // Fit (w,h) into the tile's thumbnail box, preserving aspect.
        let maxw = scaled(TILE_W) - 2 * scaled(TILE_PAD);
        let maxh = scaled(TILE_H - HEADER_H) - 2 * scaled(TILE_PAD);
        let (sw, sh) = if (w as i64) * (maxh as i64) <= (h as i64) * (maxw as i64) {
            ((w * maxh / h).max(1), maxh) // height-bound
        } else {
            (maxw, (h * maxw / w).max(1)) // width-bound
        };
        let dst = CreateCompatibleDC(screen);
        let thumb = CreateCompatibleBitmap(screen, sw, sh);
        let old_dst = SelectObject(dst, HGDIOBJ(thumb.0));
        SetStretchBltMode(dst, HALFTONE);
        let _ = StretchBlt(dst, 0, 0, sw, sh, src, 0, 0, w, h, SRCCOPY);
        SelectObject(dst, old_dst);
        let _ = DeleteDC(dst);
        Some((thumb, sw, sh))
    } else {
        None
    };

    SelectObject(src, old_src);
    let _ = DeleteObject(HGDIOBJ(full.0));
    let _ = DeleteDC(src);
    ReleaseDC(None, screen);
    result
}

fn collect_entries() -> Vec<Entry> {
    unsafe {
        let mut hwnds: Vec<HWND> = Vec::new();
        // EnumWindows yields top-to-bottom Z order, so the current window is
        // first and the most-recently-used one is next — the order Alt+Tab wants.
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut hwnds as *mut _ as isize));
        hwnds
            .into_iter()
            .map(|hwnd| {
                let (shot, shot_w, shot_h) = match capture(hwnd) {
                    Some((b, w, h)) => (Some(b), w, h),
                    None => (None, 0, 0),
                };
                Entry {
                    hwnd,
                    title: title_of(hwnd),
                    icon: icon_of(hwnd),
                    shot,
                    shot_w,
                    shot_h,
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Layout

/// Grid rect (in panel client coords) of tile `i`. The final, partial row is
/// centered, matching the Windows switcher.
fn tile_rect(cols: usize, n: usize, width: i32, i: usize) -> RECT {
    let tile_w = scaled(TILE_W);
    let tile_h = scaled(TILE_H);
    let gap = scaled(GAP);
    let pad = scaled(PANEL_PAD);
    let cols = cols.max(1);
    let row = i / cols;
    let col = i % cols;
    let rows = n.div_ceil(cols);
    // How many tiles are on this (possibly last) row.
    let in_row = if row + 1 == rows {
        n - cols * row
    } else {
        cols
    } as i32;
    let row_w = in_row * tile_w + (in_row - 1).max(0) * gap;
    let x0 = (width - row_w) / 2;
    let x = x0 + col as i32 * (tile_w + gap);
    let y = pad + row as i32 * (tile_h + gap);
    RECT {
        left: x,
        top: y,
        right: x + tile_w,
        bottom: y + tile_h,
    }
}

fn tile_at(a: &AltTab, x: i32, y: i32) -> Option<usize> {
    let n = a.entries.len();
    (0..n).find(|&i| {
        let r = tile_rect(a.cols, n, a.width, i);
        x >= r.left && x < r.right && y >= r.top && y < r.bottom
    })
}

// ---------------------------------------------------------------------------
// Open / step / commit / close

fn ensure_window() {
    if AT.with_borrow(|a| a.is_some()) {
        return;
    }
    unsafe {
        let hinstance: HINSTANCE = match GetModuleHandleW(None) {
            Ok(h) => h.into(),
            Err(_) => return,
        };
        let class = w!("StartPE_AltTab");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        // WS_EX_NOACTIVATE: the switcher never steals focus — the keyboard hook
        // drives it, and we want the previously-foreground app to stay foreground
        // until we explicitly activate the chosen one.
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            class,
            w!("StartPE Alt+Tab"),
            WS_POPUP,
            0,
            0,
            10,
            10,
            None,
            None,
            hinstance,
            None,
        ) else {
            return;
        };
        OVERLAY.set(hwnd);
        AT.with_borrow_mut(|a| {
            *a = Some(AltTab {
                hwnd,
                font: make_font(scaled(13), 400),
                entries: Vec::new(),
                selected: 0,
                cols: 1,
                width: 0,
                height: 0,
                hover: None,
                shown: false,
            });
        });
    }
}

/// Build entries, lay out the grid, and show the panel. Returns false (and frees
/// nothing) if there are no switchable windows.
fn open() -> bool {
    ensure_window();
    let entries = collect_entries();
    if entries.is_empty() {
        return false;
    }

    let n = entries.len();
    let (sw, _sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let tile_w = scaled(TILE_W);
    let tile_h = scaled(TILE_H);
    let gap = scaled(GAP);
    let pad = scaled(PANEL_PAD);

    // Columns: as many tiles as fit within MAX_WIDTH_PCT of the screen, then wrap.
    let max_panel_w = sw * MAX_WIDTH_PCT / 100;
    let fit = ((max_panel_w - 2 * pad + gap) / (tile_w + gap)).max(1) as usize;
    let cols = fit.min(n);
    let rows = n.div_ceil(cols);

    let width = 2 * pad + cols as i32 * tile_w + (cols as i32 - 1) * gap;
    let height = 2 * pad + rows as i32 * tile_h + (rows as i32 - 1) * gap;

    let hwnd = OVERLAY.get();
    unsafe {
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let x = (sw - width) / 2;
        let y = (sh - height) / 2;

        let corner = scaled(16);
        let rgn = CreateRoundRectRgn(0, 0, width + 1, height + 1, corner, corner);
        SetWindowRgn(hwnd, rgn, true);

        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            x,
            y,
            width,
            height,
            SWP_SHOWWINDOW | SWP_NOACTIVATE,
        );
    }

    AT.with_borrow_mut(|a| {
        if let Some(a) = a.as_mut() {
            free_shots(&mut a.entries);
            a.entries = entries;
            a.selected = 0; // the current window; first step moves off it
            a.cols = cols;
            a.width = width;
            a.height = height;
            a.hover = None;
            a.shown = true;
        }
    });
    unsafe {
        let _ = InvalidateRect(hwnd, None, true);
    }
    true
}

fn step(forward: bool) {
    let hwnd = AT.with_borrow_mut(|a| {
        let a = a.as_mut()?;
        let n = a.entries.len();
        if n == 0 {
            return None;
        }
        a.selected = if forward {
            (a.selected + 1) % n
        } else {
            (a.selected + n - 1) % n
        };
        Some(a.hwnd)
    });
    if let Some(hwnd) = hwnd {
        unsafe {
            let _ = InvalidateRect(hwnd, None, true);
        }
    }
}

fn nav_row(down: bool) {
    let hwnd = AT.with_borrow_mut(|a| {
        let a = a.as_mut()?;
        let n = a.entries.len();
        if n == 0 {
            return None;
        }
        // Move by one row, wrapping through the flat list. `cols <= n` always,
        // so `cols % n` is the row stride (and 0 for a single full row → no-op).
        let stride = a.cols.max(1) % n;
        a.selected = if down {
            (a.selected + stride) % n
        } else {
            (a.selected + n - stride) % n
        };
        Some(a.hwnd)
    });
    if let Some(hwnd) = hwnd {
        unsafe {
            let _ = InvalidateRect(hwnd, None, true);
        }
    }
}

fn commit() {
    let target = AT.with_borrow(|a| {
        a.as_ref()
            .and_then(|a| a.entries.get(a.selected).map(|e| e.hwnd))
    });
    close();
    if let Some(h) = target {
        unsafe {
            if IsIconic(h).as_bool() {
                let _ = ShowWindow(h, SW_RESTORE);
            }
            let _ = SetForegroundWindow(h);
        }
    }
}

fn close() {
    ENGAGED.set(false);
    AT.with_borrow_mut(|a| {
        if let Some(a) = a.as_mut() {
            a.shown = false;
            free_shots(&mut a.entries);
            unsafe {
                let _ = ShowWindow(a.hwnd, SW_HIDE);
            }
        }
    });
}

fn free_shots(entries: &mut Vec<Entry>) {
    for e in entries.drain(..) {
        if let Some(b) = e.shot {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(b.0));
            }
        }
    }
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

fn fill_rounded(hdc: HDC, rect: &RECT, color: u32, radius: i32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color));
        let pen = CreatePen(PS_SOLID, 1, COLORREF(color));
        let old_brush = SelectObject(hdc, HGDIOBJ(brush.0));
        let old_pen = SelectObject(hdc, HGDIOBJ(pen.0));
        let _ = RoundRect(hdc, rect.left, rect.top, rect.right, rect.bottom, radius, radius);
        SelectObject(hdc, old_brush);
        SelectObject(hdc, old_pen);
        let _ = DeleteObject(HGDIOBJ(brush.0));
        let _ = DeleteObject(HGDIOBJ(pen.0));
    }
}

fn frame_rounded(hdc: HDC, rect: &RECT, color: u32, radius: i32, thick: i32) {
    unsafe {
        let pen = CreatePen(PS_SOLID, thick, COLORREF(color));
        let old_pen = SelectObject(hdc, HGDIOBJ(pen.0));
        let old_brush = SelectObject(hdc, GetStockObject(HOLLOW_BRUSH));
        let _ = RoundRect(hdc, rect.left, rect.top, rect.right, rect.bottom, radius, radius);
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(HGDIOBJ(pen.0));
    }
}

fn paint(a: &AltTab) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(a.hwnd, &mut ps);

        // Double buffer.
        let mem = CreateCompatibleDC(hdc);
        let bmp = CreateCompatibleBitmap(hdc, a.width, a.height);
        let old_bmp = SelectObject(mem, HGDIOBJ(bmp.0));

        let full = RECT {
            left: 0,
            top: 0,
            right: a.width,
            bottom: a.height,
        };
        fill(mem, &full, COL_BG);
        SetBkMode(mem, TRANSPARENT);
        let old_font = SelectObject(mem, HGDIOBJ(a.font.0));

        let n = a.entries.len();
        let radius = scaled(8);
        for (i, e) in a.entries.iter().enumerate() {
            let r = tile_rect(a.cols, n, a.width, i);
            let selected = i == a.selected;
            if selected {
                fill_rounded(mem, &r, COL_ACTIVE, radius);
            } else if a.hover == Some(i) {
                fill_rounded(mem, &r, COL_HOVER, radius);
            }

            // Header: app icon + title.
            let pad = scaled(TILE_PAD);
            let header_h = scaled(HEADER_H);
            let mut text_left = r.left + pad + scaled(2);
            if let Some(icon) = e.icon {
                let sz = scaled(16);
                let _ = DrawIconEx(
                    mem,
                    text_left,
                    r.top + (header_h - sz) / 2,
                    icon,
                    sz,
                    sz,
                    0,
                    None,
                    DI_NORMAL,
                );
                text_left += sz + scaled(6);
            }
            SetTextColor(mem, COLORREF(if selected { COL_TEXT } else { COL_TEXT_DIM }));
            let mut tr = RECT {
                left: text_left,
                top: r.top,
                right: r.right - pad,
                bottom: r.top + header_h,
            };
            let mut title = util::wide(&e.title);
            title.pop();
            DrawTextW(
                mem,
                &mut title,
                &mut tr,
                DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
            );

            // Thumbnail area: the screenshot centered, or the app icon enlarged.
            let area = RECT {
                left: r.left + pad,
                top: r.top + header_h,
                right: r.right - pad,
                bottom: r.bottom - pad,
            };
            if let Some(shot) = e.shot {
                let dx = area.left + ((area.right - area.left) - e.shot_w) / 2;
                let dy = area.top + ((area.bottom - area.top) - e.shot_h) / 2;
                let sdc = CreateCompatibleDC(mem);
                let old = SelectObject(sdc, HGDIOBJ(shot.0));
                let _ = BitBlt(mem, dx, dy, e.shot_w, e.shot_h, sdc, 0, 0, SRCCOPY);
                SelectObject(sdc, old);
                let _ = DeleteDC(sdc);
            } else if let Some(icon) = e.icon {
                let sz = scaled(48);
                let _ = DrawIconEx(
                    mem,
                    (area.left + area.right - sz) / 2,
                    (area.top + area.bottom - sz) / 2,
                    icon,
                    sz,
                    sz,
                    0,
                    None,
                    DI_NORMAL,
                );
            }

            // Accent outline on the selected tile.
            if selected {
                frame_rounded(mem, &r, COL_ACCENT, radius, scaled(2));
            }
        }

        SelectObject(mem, old_font);
        // 1px ring (borderless window: accent when focused, gray otherwise).
        let ring = RECT { left: 0, top: 0, right: a.width, bottom: a.height };
        crate::taskbar::accent_ring(mem, a.hwnd, &ring, scaled(16));
        let _ = BitBlt(hdc, 0, 0, a.width, a.height, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old_bmp);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        let _ = EndPaint(a.hwnd, &ps);
    }
}

// ---------------------------------------------------------------------------
// Window procedure

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        MSG_STEP => {
            let forward = wparam.0 == 1;
            let shown = AT.with_borrow(|a| a.as_ref().is_some_and(|a| a.shown));
            if !shown && !open() {
                ENGAGED.set(false);
                return LRESULT(0);
            }
            step(forward);
            LRESULT(0)
        }
        MSG_NAV_ROW => {
            nav_row(wparam.0 == 1);
            LRESULT(0)
        }
        MSG_COMMIT => {
            commit();
            LRESULT(0)
        }
        MSG_CANCEL => {
            close();
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = util::loword(lparam.0);
            let y = util::hiword(lparam.0);
            let changed = AT.with_borrow_mut(|a| {
                let Some(a) = a.as_mut() else { return false };
                let hit = tile_at(a, x, y);
                let changed = hit != a.hover;
                a.hover = hit;
                changed
            });
            if changed {
                let _ = InvalidateRect(hwnd, None, true);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let x = util::loword(lparam.0);
            let y = util::hiword(lparam.0);
            let hit = AT.with_borrow_mut(|a| {
                let a = a.as_mut()?;
                let i = tile_at(a, x, y)?;
                a.selected = i;
                Some(())
            });
            if hit.is_some() {
                commit();
            }
            LRESULT(0)
        }
        WM_PAINT => {
            AT.with_borrow(|a| {
                if let Some(a) = a.as_ref() {
                    paint(a);
                }
            });
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
