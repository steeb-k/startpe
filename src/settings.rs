// SPDX-License-Identifier: GPL-3.0-or-later
//! A small dark settings window for the on/off StartPE options.
//!
//! StartPE has a fistful of registry-backed switches (see `config.rs`). This is
//! the first slice of a real settings pane: the boolean toggles, grouped by the
//! surface they affect (Taskbar / Desktop / Menus), with a checkbox each.
//! Opened from the taskbar's right-click menu (Settings).
//!
//! It is a single owner-drawn GDI window in the same dark palette as the rest of
//! StartPE — no common controls, no DWM dependency, so it renders the same in a
//! plain PE as on a full desktop. Toggling a row writes the value to
//! `HKCU\Software\StartPE` immediately (see [`config::save_bool`]) and asks the
//! taskbar to re-read its config so layout switches apply live; the few that need
//! the windows recreated (marked with †) take effect on the next launch.

use std::cell::RefCell;

use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// Defined in the Win32_UI_Controls module; declared here (as in `taskbar.rs`) to
// avoid pulling that whole feature in for one constant.
const WM_MOUSELEAVE: u32 = 0x02A3;

use crate::config::{self, Config};
use crate::taskbar::{
    make_font, make_font_face, scaled, COL_ACCENT, COL_BG, COL_HOVER, COL_TEXT, COL_TEXT_DIM,
};
use crate::util;

/// One boolean setting exposed in the pane.
struct Toggle {
    /// Section heading this setting sits under (groups render in first-seen order).
    group: &'static str,
    label: &'static str,
    /// Registry value name under `HKCU\Software\StartPE`.
    reg: &'static str,
    /// Reads the current value out of a loaded [`Config`].
    get: fn(&Config) -> bool,
    /// True if a relaunch is needed before the change fully takes effect (the
    /// window/desktop is built once at startup). Live-applied settings are false.
    restart: bool,
}

/// The settings shown, in display order. Grouped by `group` (first-seen order).
const TOGGLES: &[Toggle] = &[
    Toggle {
        group: "Taskbar",
        label: "Show window labels",
        reg: "TaskbarLabels",
        get: |c| c.show_labels,
        restart: false,
    },
    Toggle {
        group: "Taskbar",
        label: "Combine taskbar buttons",
        reg: "TaskbarCombine",
        get: |c| c.combine,
        restart: false,
    },
    Toggle {
        group: "Taskbar",
        label: "Center taskbar",
        reg: "CenterTaskbar",
        get: |c| c.center_taskbar,
        restart: false,
    },
    Toggle {
        group: "Desktop",
        label: "Show system desktop icons",
        reg: "ShowSystemDesktopIcons",
        get: |c| c.show_system_desktop_icons,
        restart: true,
    },
    Toggle {
        group: "Menus",
        label: "Dark context menus",
        reg: "DarkMenus",
        get: |c| c.dark_menus,
        restart: true,
    },
];

/// A laid-out line in the pane: either a section heading or a toggle row.
enum Row {
    Header(&'static str),
    /// Index into [`TOGGLES`].
    Toggle(usize),
}

struct State {
    hwnd: HWND,
    rows: Vec<Row>,
    /// Current on/off value per `TOGGLES` index.
    values: Vec<bool>,
    /// Index (into `TOGGLES`) of the toggle row under the cursor, or `None`.
    hover: Option<usize>,
    tracking_mouse: bool,
    font: HFONT,
    font_header: HFONT,
    font_title: HFONT,
    font_glyph: HFONT,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

// Layout metrics, in 96-DPI px (run through `scaled`).
const PAD: i32 = 18;
const WIDTH: i32 = 340;
const TITLE_H: i32 = 46;
const HEADER_H: i32 = 30;
const ROW_H: i32 = 36;
const FOOTER_H: i32 = 30;
const BOX: i32 = 18; // checkbox side length
const CLOSE: i32 = 34; // close-button hit square in the title bar

// Segoe MDL2 Assets glyphs.
const GLYPH_CHECK: u16 = 0xE73E; // CheckMark
const GLYPH_CLOSE: u16 = 0xE8BB; // ChromeClose

/// Open the settings window (or bring it to the front if already open).
pub fn open() {
    unsafe {
        // Single instance: re-focus the existing window instead of stacking.
        let existing = STATE.with_borrow(|s| s.as_ref().map(|s| s.hwnd));
        if let Some(hwnd) = existing {
            if IsWindow(hwnd).as_bool() {
                let _ = SetForegroundWindow(hwnd);
                return;
            }
        }

        let Ok(hinstance) = GetModuleHandleW(None) else {
            return;
        };
        let hinstance: HINSTANCE = hinstance.into();
        let class = w!("StartPE_Settings");
        // Register once; a second RegisterClassW just fails harmlessly.
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);

        // Build the row layout and snapshot the current values.
        let cfg = Config::load();
        let mut rows = Vec::new();
        let mut values = Vec::new();
        let mut last = "";
        for (i, t) in TOGGLES.iter().enumerate() {
            if t.group != last {
                rows.push(Row::Header(t.group));
                last = t.group;
            }
            rows.push(Row::Toggle(i));
            values.push((t.get)(&cfg));
        }

        let height = content_height(&rows);
        let w = scaled(WIDTH);
        let h = height;
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        let x = (sw - w) / 2;
        let y = (sh - h) / 2;

        STATE.with_borrow_mut(|s| {
            *s = Some(State {
                hwnd: HWND::default(),
                rows,
                values,
                hover: None,
                tracking_mouse: false,
                font: make_font(scaled(14), 400),
                font_header: make_font(scaled(12), 600),
                font_title: make_font(scaled(16), 600),
                font_glyph: make_font_face(scaled(12), 400, w!("Segoe MDL2 Assets")),
            });
        });

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class,
            w!("StartPE Settings"),
            WS_POPUP,
            x,
            y,
            w,
            h,
            None,
            None,
            hinstance,
            None,
        );
        let Ok(hwnd) = hwnd else {
            STATE.with_borrow_mut(|s| *s = None);
            return;
        };

        // Rounded corners via a GDI region (no DWM needed in PE).
        let rgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, scaled(10), scaled(10));
        let _ = SetWindowRgn(hwnd, rgn, true);

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        log_open();
    }
}

/// Version-stamped record that the settings pane opened (PE has no Event Viewer;
/// this tells which binary the user is configuring). Best-effort.
fn log_open() {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("X:\\startpe.log")
    {
        let _ = writeln!(
            f,
            "StartPE v{} settings pane opened ({} toggles)",
            env!("CARGO_PKG_VERSION"),
            TOGGLES.len()
        );
    }
}

/// UTF-16 buffer *without* a NUL terminator, for `DrawTextW` (which takes the
/// slice length as the character count).
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Total window height for a given row layout.
fn content_height(rows: &[Row]) -> i32 {
    let mut y = scaled(TITLE_H);
    for r in rows {
        y += match r {
            Row::Header(_) => scaled(HEADER_H),
            Row::Toggle(_) => scaled(ROW_H),
        };
    }
    y + scaled(FOOTER_H)
}

/// Top edge (client-relative) of the row at `index` in `rows`.
fn row_top(rows: &[Row], index: usize) -> i32 {
    let mut y = scaled(TITLE_H);
    for r in &rows[..index] {
        y += match r {
            Row::Header(_) => scaled(HEADER_H),
            Row::Toggle(_) => scaled(ROW_H),
        };
    }
    y
}

/// The toggle index (into `TOGGLES`) whose row contains client point `(x, y)`,
/// or `None`. Only toggle rows are hit-testable.
fn hit_toggle(state: &State, x: i32, y: i32) -> Option<usize> {
    if x < 0 || x > scaled(WIDTH) {
        return None;
    }
    for (i, r) in state.rows.iter().enumerate() {
        if let Row::Toggle(t) = r {
            let top = row_top(&state.rows, i);
            if y >= top && y < top + scaled(ROW_H) {
                return Some(*t);
            }
        }
    }
    None
}

/// Whether client point `(x, y)` is on the title-bar close button.
fn hit_close(x: i32, y: i32) -> bool {
    let w = scaled(WIDTH);
    x >= w - scaled(CLOSE) && y < scaled(CLOSE)
}

fn paint(state: &State) {
    unsafe {
        let hwnd = state.hwnd;
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let (width, height) = (rc.right, rc.bottom);

        // Double buffer.
        let mem = CreateCompatibleDC(hdc);
        let bmp = CreateCompatibleBitmap(hdc, width, height);
        let old_bmp = SelectObject(mem, bmp);

        let bg = CreateSolidBrush(COLORREF(COL_BG));
        FillRect(mem, &rc, bg);
        let _ = DeleteObject(HGDIOBJ(bg.0));
        SetBkMode(mem, TRANSPARENT);

        // Title bar.
        let title_bar = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: scaled(TITLE_H),
        };
        let title_bg = CreateSolidBrush(COLORREF(COL_HOVER));
        FillRect(mem, &title_bar, title_bg);
        let _ = DeleteObject(HGDIOBJ(title_bg.0));

        SetTextColor(mem, COLORREF(COL_TEXT));
        let old = SelectObject(mem, HGDIOBJ(state.font_title.0));
        let mut title = wide("StartPE Settings");
        let mut tr = RECT {
            left: scaled(PAD),
            top: 0,
            right: width - scaled(CLOSE),
            bottom: scaled(TITLE_H),
        };
        DrawTextW(
            mem,
            &mut title,
            &mut tr,
            DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX,
        );

        // Close glyph (Segoe MDL2), top-right.
        SelectObject(mem, HGDIOBJ(state.font_glyph.0));
        SetTextColor(mem, COLORREF(COL_TEXT_DIM));
        let mut close = [GLYPH_CLOSE, 0u16];
        let mut cr = RECT {
            left: width - scaled(CLOSE),
            top: 0,
            right: width,
            bottom: scaled(CLOSE),
        };
        DrawTextW(
            mem,
            &mut close[..1],
            &mut cr,
            DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
        );

        // Rows.
        let mut any_restart = false;
        for (i, r) in state.rows.iter().enumerate() {
            let top = row_top(&state.rows, i);
            match r {
                Row::Header(g) => {
                    SelectObject(mem, HGDIOBJ(state.font_header.0));
                    SetTextColor(mem, COLORREF(COL_ACCENT));
                    let mut text = wide(g);
                    let mut hr = RECT {
                        left: scaled(PAD),
                        top,
                        right: width - scaled(PAD),
                        bottom: top + scaled(HEADER_H),
                    };
                    DrawTextW(
                        mem,
                        &mut text,
                        &mut hr,
                        DT_SINGLELINE | DT_BOTTOM | DT_LEFT | DT_NOPREFIX,
                    );
                }
                Row::Toggle(t) => {
                    let toggle = &TOGGLES[*t];
                    let on = state.values[*t];
                    any_restart |= toggle.restart;
                    let row = RECT {
                        left: 0,
                        top,
                        right: width,
                        bottom: top + scaled(ROW_H),
                    };
                    if state.hover == Some(*t) {
                        let hov = CreateSolidBrush(COLORREF(COL_HOVER));
                        FillRect(mem, &row, hov);
                        let _ = DeleteObject(HGDIOBJ(hov.0));
                    }

                    // Checkbox.
                    let box_top = top + (scaled(ROW_H) - scaled(BOX)) / 2;
                    let box_rc = RECT {
                        left: scaled(PAD),
                        top: box_top,
                        right: scaled(PAD) + scaled(BOX),
                        bottom: box_top + scaled(BOX),
                    };
                    let fill = CreateSolidBrush(COLORREF(if on { COL_ACCENT } else { COL_BG }));
                    let pen = CreatePen(
                        PS_SOLID,
                        1,
                        COLORREF(if on { COL_ACCENT } else { COL_TEXT_DIM }),
                    );
                    let op = SelectObject(mem, HGDIOBJ(pen.0));
                    let ob = SelectObject(mem, HGDIOBJ(fill.0));
                    let _ = Rectangle(
                        mem,
                        box_rc.left,
                        box_rc.top,
                        box_rc.right,
                        box_rc.bottom,
                    );
                    SelectObject(mem, op);
                    SelectObject(mem, ob);
                    let _ = DeleteObject(HGDIOBJ(pen.0));
                    let _ = DeleteObject(HGDIOBJ(fill.0));
                    if on {
                        SelectObject(mem, HGDIOBJ(state.font_glyph.0));
                        SetTextColor(mem, COLORREF(COL_TEXT));
                        let mut chk = [GLYPH_CHECK, 0u16];
                        let mut chkr = box_rc;
                        DrawTextW(
                            mem,
                            &mut chk[..1],
                            &mut chkr,
                            DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
                        );
                    }

                    // Label (a dagger marks settings that need a relaunch).
                    SelectObject(mem, HGDIOBJ(state.font.0));
                    SetTextColor(mem, COLORREF(COL_TEXT));
                    let label = if toggle.restart {
                        format!("{} \u{2020}", toggle.label)
                    } else {
                        toggle.label.to_string()
                    };
                    let mut text = wide(&label);
                    let mut lr = RECT {
                        left: scaled(PAD) + scaled(BOX) + scaled(12),
                        top,
                        right: width - scaled(PAD),
                        bottom: top + scaled(ROW_H),
                    };
                    DrawTextW(
                        mem,
                        &mut text,
                        &mut lr,
                        DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX,
                    );
                }
            }
        }

        // Footer note for relaunch-only settings.
        if any_restart {
            SelectObject(mem, HGDIOBJ(state.font_header.0));
            SetTextColor(mem, COLORREF(COL_TEXT_DIM));
            let mut note = wide("\u{2020} applies after StartPE restarts");
            let mut fr = RECT {
                left: scaled(PAD),
                top: height - scaled(FOOTER_H),
                right: width - scaled(PAD),
                bottom: height,
            };
            DrawTextW(
                mem,
                &mut note,
                &mut fr,
                DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX,
            );
        }

        SelectObject(mem, old);
        let _ = BitBlt(hdc, 0, 0, width, height, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old_bmp);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        let _ = EndPaint(hwnd, &ps);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            STATE.with_borrow_mut(|s| {
                if let Some(s) = s.as_mut() {
                    s.hwnd = hwnd;
                }
            });
            LRESULT(0)
        }
        WM_PAINT => {
            STATE.with_borrow(|s| {
                if let Some(s) = s.as_ref() {
                    paint(s);
                }
            });
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = util::loword(lparam.0);
            let y = util::hiword(lparam.0);
            let changed = STATE.with_borrow_mut(|s| {
                let Some(s) = s.as_mut() else {
                    return false;
                };
                if !s.tracking_mouse {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut tme);
                    s.tracking_mouse = true;
                }
                let hov = hit_toggle(s, x, y);
                if hov != s.hover {
                    s.hover = hov;
                    true
                } else {
                    false
                }
            });
            if changed {
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            let changed = STATE.with_borrow_mut(|s| {
                s.as_mut().is_some_and(|s| {
                    s.tracking_mouse = false;
                    let had = s.hover.is_some();
                    s.hover = None;
                    had
                })
            });
            if changed {
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = util::loword(lparam.0);
            let y = util::hiword(lparam.0);
            // Drag by the title bar (but not the close button): the classic
            // borderless-window move via a synthetic non-client click.
            if y < scaled(TITLE_H) && !hit_close(x, y) {
                let _ = ReleaseCapture();
                let _ = SendMessageW(hwnd, WM_NCLBUTTONDOWN, WPARAM(HTCAPTION as usize), LPARAM(0));
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let x = util::loword(lparam.0);
            let y = util::hiword(lparam.0);
            if hit_close(x, y) {
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            // Resolve which setting to flip under the borrow, then act (the
            // registry write + taskbar reload) after dropping it.
            let flipped = STATE.with_borrow_mut(|s| {
                let s = s.as_mut()?;
                let t = hit_toggle(s, x, y)?;
                let now = !s.values[t];
                s.values[t] = now;
                Some((TOGGLES[t].reg, now))
            });
            if let Some((reg, now)) = flipped {
                config::save_bool(reg, now);
                crate::taskbar::reload_config();
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 as u32 == VK_ESCAPE.0 as u32 => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            STATE.with_borrow_mut(|s| {
                if let Some(s) = s.take() {
                    let _ = DeleteObject(HGDIOBJ(s.font.0));
                    let _ = DeleteObject(HGDIOBJ(s.font_header.0));
                    let _ = DeleteObject(HGDIOBJ(s.font_title.0));
                    let _ = DeleteObject(HGDIOBJ(s.font_glyph.0));
                }
            });
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
