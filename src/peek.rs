// SPDX-License-Identifier: GPL-3.0-or-later
//! Hover previews ("peek") for taskbar buttons, Windows 11 style.
//!
//! Hovering a task button pops a panel above the taskbar with one cell per
//! window in the group. Each cell has the window title and a ✕ that closes
//! that window. Where DWM composition is available the cells show live
//! thumbnails (`DwmRegisterThumbnail`); without DWM (typical for WinPE) the
//! panel degrades to icon + title rows with the same interactions.
//!
//! The panel never takes focus (`WS_EX_NOACTIVATE`); a poll timer dismisses
//! it once the cursor leaves both the panel and the originating button.

use std::cell::RefCell;

use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{
    DwmIsCompositionEnabled, DwmRegisterThumbnail, DwmUnregisterThumbnail,
    DwmUpdateThumbnailProperties, DWM_THUMBNAIL_PROPERTIES, DWM_TNP_RECTDESTINATION,
    DWM_TNP_VISIBLE,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::taskbar::{
    make_font, make_font_face, scaled, COL_BG, COL_HOVER, COL_TEXT, COL_TEXT_DIM,
};
use crate::util;

const TIMER_POLL: usize = 1;

// Geometry (unscaled px).
const PAD: i32 = 8;
const CELL_W: i32 = 196;
const CELL_H: i32 = 150;
const TITLE_H: i32 = 28;
const ROW_W: i32 = 300;
const ROW_H: i32 = 38;
const CLOSE_W: i32 = 24;

const COL_CLOSE_HOVER: u32 = 0x002311E8; // Windows close-button red (BGR)
const GLYPH_CLOSE: &str = "\u{E8BB}"; // Segoe MDL2 ChromeClose

pub struct PeekEntry {
    pub hwnd: HWND,
    pub title: String,
    pub icon: Option<HICON>,
}

struct PeekState {
    hwnd: HWND,
    entries: Vec<PeekEntry>,
    thumbs: Vec<Option<isize>>,
    thumbs_mode: bool,
    width: i32,
    height: i32,
    /// (entry index, cursor on the close button)
    hover: Option<(usize, bool)>,
    /// Button rect (screen) that keeps the peek alive while hovered.
    anchor: RECT,
    taskbar_top: i32,
    font: HFONT,
    font_glyph: HFONT,
}

thread_local! {
    static PEEK: RefCell<Option<PeekState>> = const { RefCell::new(None) };
}

pub fn is_visible() -> bool {
    PEEK.with_borrow(|p| {
        p.as_ref()
            .map(|p| unsafe { IsWindowVisible(p.hwnd).as_bool() })
            .unwrap_or(false)
    })
}

pub fn show(entries: Vec<PeekEntry>, anchor: RECT, taskbar_top: i32) {
    if entries.is_empty() {
        hide();
        return;
    }
    ensure_window();
    clear_thumbs();
    unsafe {
        let thumbs_mode = DwmIsCompositionEnabled()
            .map(|b| b.as_bool())
            .unwrap_or(false);
        let n = entries.len() as i32;
        let (width, height) = if thumbs_mode {
            (
                n * scaled(CELL_W) + (n + 1) * scaled(PAD),
                scaled(CELL_H) + 2 * scaled(PAD),
            )
        } else {
            (scaled(ROW_W), n * scaled(ROW_H) + 2 * scaled(PAD))
        };

        let hwnd = PEEK.with_borrow(|p| p.as_ref().unwrap().hwnd);
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let x = ((anchor.left + anchor.right) / 2 - width / 2)
            .clamp(scaled(4), (sw - width - scaled(4)).max(scaled(4)));
        let y = taskbar_top - height - scaled(8);

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

        // Register DWM thumbnails into each cell.
        let mut thumbs: Vec<Option<isize>> = Vec::with_capacity(entries.len());
        if thumbs_mode {
            for (i, e) in entries.iter().enumerate() {
                let thumb = DwmRegisterThumbnail(hwnd, e.hwnd).ok();
                if let Some(t) = thumb {
                    let cell = cell_rect(i);
                    let dest = RECT {
                        left: cell.left + scaled(6),
                        top: cell.top + scaled(TITLE_H),
                        right: cell.right - scaled(6),
                        bottom: cell.bottom - scaled(6),
                    };
                    let props = DWM_THUMBNAIL_PROPERTIES {
                        dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE,
                        rcDestination: dest,
                        fVisible: TRUE,
                        ..Default::default()
                    };
                    let _ = DwmUpdateThumbnailProperties(t, &props);
                }
                thumbs.push(thumb);
            }
        } else {
            thumbs.resize(entries.len(), None);
        }

        PEEK.with_borrow_mut(|p| {
            let p = p.as_mut().unwrap();
            p.entries = entries;
            p.thumbs = thumbs;
            p.thumbs_mode = thumbs_mode;
            p.width = width;
            p.height = height;
            p.hover = None;
            p.anchor = anchor;
            p.taskbar_top = taskbar_top;
        });

        SetTimer(hwnd, TIMER_POLL, 250, None);
        let _ = InvalidateRect(hwnd, None, true);
    }
}

pub fn hide() {
    clear_thumbs();
    PEEK.with_borrow(|p| {
        if let Some(p) = p.as_ref() {
            unsafe {
                let _ = KillTimer(p.hwnd, TIMER_POLL);
                let _ = ShowWindow(p.hwnd, SW_HIDE);
            }
        }
    });
}

fn clear_thumbs() {
    let thumbs = PEEK.with_borrow_mut(|p| {
        p.as_mut()
            .map(|p| std::mem::take(&mut p.thumbs))
            .unwrap_or_default()
    });
    unsafe {
        for t in thumbs.into_iter().flatten() {
            let _ = DwmUnregisterThumbnail(t);
        }
    }
}

fn ensure_window() {
    let exists = PEEK.with_borrow(|p| p.is_some());
    if exists {
        return;
    }
    unsafe {
        let hinstance: HINSTANCE = match GetModuleHandleW(None) {
            Ok(h) => h.into(),
            Err(_) => return,
        };
        let class = w!("StartPE_Peek");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            class,
            w!("StartPE Peek"),
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
        PEEK.with_borrow_mut(|p| {
            *p = Some(PeekState {
                hwnd,
                entries: Vec::new(),
                thumbs: Vec::new(),
                thumbs_mode: false,
                width: 0,
                height: 0,
                hover: None,
                anchor: RECT::default(),
                taskbar_top: 0,
                font: make_font(scaled(13), 400),
                font_glyph: make_font_face(scaled(11), 400, w!("Segoe MDL2 Assets")),
            });
        });
    }
}

// ---------------------------------------------------------------------------
// Layout

fn cell_rect(i: usize) -> RECT {
    let i = i as i32;
    RECT {
        left: scaled(PAD) + i * (scaled(CELL_W) + scaled(PAD)),
        top: scaled(PAD),
        right: scaled(PAD) + i * (scaled(CELL_W) + scaled(PAD)) + scaled(CELL_W),
        bottom: scaled(PAD) + scaled(CELL_H),
    }
}

fn row_rect(i: usize, width: i32) -> RECT {
    let i = i as i32;
    RECT {
        left: scaled(PAD),
        top: scaled(PAD) + i * scaled(ROW_H),
        right: width - scaled(PAD),
        bottom: scaled(PAD) + (i + 1) * scaled(ROW_H),
    }
}

fn close_rect(item: &RECT, thumbs_mode: bool) -> RECT {
    if thumbs_mode {
        RECT {
            left: item.right - scaled(CLOSE_W) - scaled(4),
            top: item.top + scaled(2),
            right: item.right - scaled(4),
            bottom: item.top + scaled(TITLE_H) - scaled(2),
        }
    } else {
        RECT {
            left: item.right - scaled(CLOSE_W) - scaled(4),
            top: item.top + (scaled(ROW_H) - scaled(CLOSE_W)) / 2,
            right: item.right - scaled(4),
            bottom: item.top + (scaled(ROW_H) + scaled(CLOSE_W)) / 2,
        }
    }
}

fn in_rect(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn hit_test(p: &PeekState, x: i32, y: i32) -> Option<(usize, bool)> {
    for i in 0..p.entries.len() {
        let item = if p.thumbs_mode {
            cell_rect(i)
        } else {
            row_rect(i, p.width)
        };
        if in_rect(&item, x, y) {
            return Some((i, in_rect(&close_rect(&item, p.thumbs_mode), x, y)));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Painting

fn fill(hdc: HDC, rect: &RECT, color: u32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color));
        FillRect(hdc, rect, brush);
        let _ = DeleteObject(brush);
    }
}

unsafe fn draw_str(hdc: HDC, s: &str, rect: &mut RECT, flags: DRAW_TEXT_FORMAT) {
    let mut t = util::wide(s);
    t.pop();
    DrawTextW(hdc, &mut t, rect, flags);
}

fn paint(p: &PeekState) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(p.hwnd, &mut ps);
        // No double buffer here: DWM composes thumbnails over our surface,
        // and the panel is small enough that flicker is not noticeable.
        let full = RECT {
            left: 0,
            top: 0,
            right: p.width,
            bottom: p.height,
        };
        fill(hdc, &full, COL_BG);
        SetBkMode(hdc, TRANSPARENT);
        let old_font = SelectObject(hdc, p.font);

        for (i, e) in p.entries.iter().enumerate() {
            let item = if p.thumbs_mode {
                cell_rect(i)
            } else {
                row_rect(i, p.width)
            };
            let hovered = p.hover.map(|(h, _)| h) == Some(i);
            if hovered {
                fill(hdc, &item, COL_HOVER);
            }

            // Icon + title (the title strip in thumbs mode, the row otherwise).
            let text_top = item.top;
            let text_h = if p.thumbs_mode { scaled(TITLE_H) } else { scaled(ROW_H) };
            let mut text_left = item.left + scaled(8);
            if let Some(icon) = e.icon {
                let sz = scaled(16);
                let _ = DrawIconEx(
                    hdc,
                    text_left,
                    text_top + (text_h - sz) / 2,
                    icon,
                    sz,
                    sz,
                    0,
                    None,
                    DI_NORMAL,
                );
            }
            text_left += scaled(22);
            SetTextColor(hdc, COLORREF(COL_TEXT));
            let mut tr = RECT {
                left: text_left,
                top: text_top,
                right: item.right - scaled(CLOSE_W) - scaled(8),
                bottom: text_top + text_h,
            };
            draw_str(
                hdc,
                &e.title,
                &mut tr,
                DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
            );

            // Close button.
            let cr = close_rect(&item, p.thumbs_mode);
            let close_hovered = p.hover == Some((i, true));
            if close_hovered {
                fill(hdc, &cr, COL_CLOSE_HOVER);
            }
            SelectObject(hdc, p.font_glyph);
            SetTextColor(
                hdc,
                COLORREF(if close_hovered { COL_TEXT } else { COL_TEXT_DIM }),
            );
            let mut gr = cr;
            draw_str(hdc, GLYPH_CLOSE, &mut gr, DT_SINGLELINE | DT_VCENTER | DT_CENTER);
            SelectObject(hdc, p.font);
        }

        SelectObject(hdc, old_font);
        let _ = EndPaint(p.hwnd, &ps);
    }
}

// ---------------------------------------------------------------------------
// Window procedure

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEMOVE => {
            let x = util::loword(lparam.0);
            let y = util::hiword(lparam.0);
            let changed = PEEK.with_borrow_mut(|p| {
                let Some(p) = p.as_mut() else { return false };
                let hit = hit_test(p, x, y);
                let changed = hit != p.hover;
                p.hover = hit;
                changed
            });
            if changed {
                let _ = InvalidateRect(hwnd, None, true);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == TIMER_POLL {
                // Dismiss when the cursor leaves both the panel and the
                // taskbar button it belongs to.
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let keep = PEEK.with_borrow(|p| {
                    let Some(p) = p.as_ref() else { return false };
                    let mut wr = RECT::default();
                    let _ = GetWindowRect(p.hwnd, &mut wr);
                    // Bridge the gap between panel and taskbar.
                    wr.bottom = p.taskbar_top + scaled(2);
                    in_rect(&wr, pt.x, pt.y) || in_rect(&p.anchor, pt.x, pt.y)
                });
                if !keep {
                    hide();
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let x = util::loword(lparam.0);
            let y = util::hiword(lparam.0);
            enum Action {
                None,
                Activate(HWND),
                Close { target: HWND, relayout: Option<(Vec<PeekEntry>, RECT, i32)> },
            }
            let action = PEEK.with_borrow_mut(|p| {
                let Some(p) = p.as_mut() else { return Action::None };
                match hit_test(p, x, y) {
                    Some((i, true)) => {
                        let target = p.entries[i].hwnd;
                        p.entries.remove(i);
                        let relayout = if p.entries.is_empty() {
                            None
                        } else {
                            Some((std::mem::take(&mut p.entries), p.anchor, p.taskbar_top))
                        };
                        Action::Close { target, relayout }
                    }
                    Some((i, false)) => Action::Activate(p.entries[i].hwnd),
                    None => Action::None,
                }
            });
            match action {
                Action::Activate(target) => {
                    hide();
                    if IsIconic(target).as_bool() {
                        let _ = ShowWindow(target, SW_RESTORE);
                    }
                    let _ = SetForegroundWindow(target);
                }
                Action::Close { target, relayout } => {
                    let _ = PostMessageW(target, WM_CLOSE, WPARAM(0), LPARAM(0));
                    match relayout {
                        Some((entries, anchor, top)) => show(entries, anchor, top),
                        None => hide(),
                    }
                }
                Action::None => {}
            }
            LRESULT(0)
        }
        WM_PAINT => {
            PEEK.with_borrow(|p| {
                if let Some(p) = p.as_ref() {
                    paint(p);
                }
            });
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
