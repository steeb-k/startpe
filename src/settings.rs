// SPDX-License-Identifier: GPL-3.0-or-later
//! A small dark settings window for the on/off StartPE options.
//!
//! StartPE has a fistful of registry-backed switches (see `config.rs`). This is
//! the first slice of a real settings pane: the boolean toggles, grouped by the
//! surface they affect (Taskbar / Menus), with a checkbox each, plus
//! the Start button glyph color (preset swatches + a Custom… picker). Opened
//! from the taskbar's right-click menu (Settings).
//!
//! It is a single owner-drawn GDI window in the same dark palette as the rest of
//! StartPE — no common controls, no DWM dependency, so it renders the same in a
//! plain PE as on a full desktop. Changing a row writes the value to
//! `HKCU\Software\StartPE` immediately (see [`config::save_bool`] / [`config::save_u32`])
//! and asks the taskbar to re-read its config so it applies live; the few that
//! need the windows recreated (marked with †) take effect on the next launch.

use std::cell::RefCell;

use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::Dialogs::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// Defined in the Win32_UI_Controls module; declared here (as in `taskbar.rs`) to
// avoid pulling that whole feature in for one constant.
const WM_MOUSELEAVE: u32 = 0x02A3;

use crate::config::{self, Config};
use crate::taskbar::{
    make_font, make_font_face, scaled, COL_ACCENT, COL_ACTIVE, COL_BG, COL_HOVER, COL_TEXT,
    COL_TEXT_DIM,
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
        group: "Windows",
        label: "Accent border on active window",
        reg: "WindowBorders",
        get: |c| c.window_borders,
        restart: false,
    },
    Toggle {
        group: "Menus",
        label: "Dark context menus",
        reg: "DarkMenus",
        get: |c| c.dark_menus,
        restart: true,
    },
];

/// Build a `COLORREF` (0x00BBGGRR) from sRGB components, usable in const context.
const fn rgb(r: u32, g: u32, b: u32) -> u32 {
    r | (g << 8) | (b << 16)
}

/// Preset Start button glyph colors offered as swatches. The first is the
/// default (near-white, matching the taskbar text); the rest are the usual
/// Windows accent hues. A Custom… button covers everything else.
const N_SWATCH: usize = 7;
const SWATCHES: [u32; N_SWATCH] = [
    rgb(240, 240, 240), // near-white (default)
    rgb(0, 120, 212),   // blue
    rgb(0, 178, 148),   // teal
    rgb(16, 185, 110),  // green
    rgb(255, 140, 0),   // orange
    rgb(232, 17, 35),   // red
    rgb(180, 90, 230),  // purple
];

/// A laid-out line in the pane: a section heading, a toggle row, or the Start
/// button color row (swatches + Custom… button).
enum Row {
    Header(&'static str),
    /// Index into [`TOGGLES`].
    Toggle(usize),
    /// The Start button color picker.
    Color,
}

/// What the cursor is over, for hover highlighting and click dispatch.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Hover {
    None,
    /// A toggle row, by `TOGGLES` index.
    Toggle(usize),
    /// A color swatch, by `SWATCHES` index.
    Swatch(usize),
    /// The Custom… color button.
    Custom,
}

struct State {
    hwnd: HWND,
    rows: Vec<Row>,
    /// Current on/off value per `TOGGLES` index.
    values: Vec<bool>,
    /// Current Start button glyph color (COLORREF 0x00BBGGRR).
    start_color: u32,
    /// What the cursor is currently over.
    hover: Hover,
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
const COLOR_H: i32 = 48; // height of the Start button color row
const SW: i32 = 22; // swatch side length
const SW_GAP: i32 = 9; // gap between swatches
const CUSTOM_W: i32 = 72; // Custom… button width
const CUSTOM_H: i32 = 28; // Custom… button height

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
        // The Start button color picker, in its own section.
        rows.push(Row::Header("Start button"));
        rows.push(Row::Color);

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
                start_color: cfg.start_button_color,
                hover: Hover::None,
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
            "StartPE v{} settings pane opened ({} toggles + start button color)",
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

/// Filled rounded rectangle in `color` (the pen matches the fill, so the rounded
/// edge has no hard 1px border). Plain GDI `RoundRect`, no DWM needed.
fn fill_round(hdc: HDC, rc: &RECT, color: u32, radius: i32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color));
        let pen = CreatePen(PS_SOLID, 1, COLORREF(color));
        let ob = SelectObject(hdc, HGDIOBJ(brush.0));
        let op = SelectObject(hdc, HGDIOBJ(pen.0));
        let _ = RoundRect(hdc, rc.left, rc.top, rc.right, rc.bottom, radius, radius);
        SelectObject(hdc, ob);
        SelectObject(hdc, op);
        let _ = DeleteObject(HGDIOBJ(brush.0));
        let _ = DeleteObject(HGDIOBJ(pen.0));
    }
}

/// Total window height for a given row layout.
fn content_height(rows: &[Row]) -> i32 {
    let mut y = scaled(TITLE_H);
    for r in rows {
        y += row_height(r);
    }
    y + scaled(FOOTER_H)
}

/// Laid-out height of a single row.
fn row_height(row: &Row) -> i32 {
    match row {
        Row::Header(_) => scaled(HEADER_H),
        Row::Toggle(_) => scaled(ROW_H),
        Row::Color => scaled(COLOR_H),
    }
}

/// Top edge (client-relative) of the row at `index` in `rows`.
fn row_top(rows: &[Row], index: usize) -> i32 {
    let mut y = scaled(TITLE_H);
    for r in &rows[..index] {
        y += row_height(r);
    }
    y
}

/// Swatch rects and the Custom… button rect for a color row at client `top`.
/// Shared by paint and hit-testing so they can't drift apart.
fn color_layout(top: i32) -> ([RECT; N_SWATCH], RECT) {
    let sw = scaled(SW);
    let gap = scaled(SW_GAP);
    let sy = top + (scaled(COLOR_H) - sw) / 2;
    let mut swatches = [RECT::default(); N_SWATCH];
    let mut x = scaled(PAD);
    for r in swatches.iter_mut() {
        *r = RECT {
            left: x,
            top: sy,
            right: x + sw,
            bottom: sy + sw,
        };
        x += sw + gap;
    }
    // Custom… button: right-aligned, vertically centered in the row.
    let bw = scaled(CUSTOM_W);
    let bh = scaled(CUSTOM_H);
    let by = top + (scaled(COLOR_H) - bh) / 2;
    let custom = RECT {
        left: scaled(WIDTH) - scaled(PAD) - bw,
        top: by,
        right: scaled(WIDTH) - scaled(PAD),
        bottom: by + bh,
    };
    (swatches, custom)
}

/// True if client point `(x, y)` lies inside `r`.
fn in_rect(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// What the cursor is over at client point `(x, y)`.
fn hit(state: &State, x: i32, y: i32) -> Hover {
    if x < 0 || x > scaled(WIDTH) {
        return Hover::None;
    }
    for (i, r) in state.rows.iter().enumerate() {
        let top = row_top(&state.rows, i);
        match r {
            Row::Toggle(t) => {
                if y >= top && y < top + scaled(ROW_H) {
                    return Hover::Toggle(*t);
                }
            }
            Row::Color => {
                if y >= top && y < top + scaled(COLOR_H) {
                    let (swatches, custom) = color_layout(top);
                    if in_rect(&custom, x, y) {
                        return Hover::Custom;
                    }
                    for (i, sr) in swatches.iter().enumerate() {
                        if in_rect(sr, x, y) {
                            return Hover::Swatch(i);
                        }
                    }
                }
            }
            Row::Header(_) => {}
        }
    }
    Hover::None
}

/// Whether client point `(x, y)` is on the title-bar close button.
fn hit_close(x: i32, y: i32) -> bool {
    let w = scaled(WIDTH);
    x >= w - scaled(CLOSE) && y < scaled(CLOSE)
}

/// Persist the new Start button color and re-read it into the live taskbar
/// (`draw_start_button` reads `cfg.start_button_color`, so it repaints recolored).
fn apply_start_color(color: u32) {
    config::save_u32("StartButtonColor", color);
    crate::taskbar::reload_config();
}

thread_local! {
    /// The user's custom colors for the picker, kept across openings.
    static CUSTOM_COLORS: RefCell<[COLORREF; 16]> =
        const { RefCell::new([COLORREF(0x00FF_FFFF); 16]) };
}

/// Open the standard Windows color dialog seeded with `current`; returns the
/// chosen COLORREF (0x00BBGGRR) or `None` if cancelled. `ChooseColorW` is a
/// documented comdlg32 dialog and runs its own modal loop (it disables `owner`
/// while up), so callers must not hold the `STATE` borrow across it.
fn pick_color(owner: HWND, current: u32) -> Option<u32> {
    CUSTOM_COLORS.with_borrow_mut(|custom| unsafe {
        let mut cc = CHOOSECOLORW {
            lStructSize: std::mem::size_of::<CHOOSECOLORW>() as u32,
            hwndOwner: owner,
            rgbResult: COLORREF(current),
            lpCustColors: custom.as_mut_ptr(),
            Flags: CC_RGBINIT | CC_FULLOPEN | CC_ANYCOLOR,
            ..Default::default()
        };
        if ChooseColorW(&mut cc).as_bool() {
            Some(cc.rgbResult.0)
        } else {
            None
        }
    })
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
                    if state.hover == Hover::Toggle(*t) {
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
                Row::Color => {
                    let (swatches, custom) = color_layout(top);
                    for (si, sr) in swatches.iter().enumerate() {
                        let color = SWATCHES[si];
                        let selected = state.start_color == color;
                        let hovered = state.hover == Hover::Swatch(si);
                        // Selection ring (accent) or hover ring (dim) behind the swatch.
                        if selected || hovered {
                            let rr = RECT {
                                left: sr.left - scaled(3),
                                top: sr.top - scaled(3),
                                right: sr.right + scaled(3),
                                bottom: sr.bottom + scaled(3),
                            };
                            let ring = if selected { COL_ACCENT } else { COL_TEXT_DIM };
                            fill_round(mem, &rr, ring, scaled(6));
                        }
                        fill_round(mem, sr, color, scaled(5));
                    }

                    // Custom… button (pill).
                    let cust_bg = if state.hover == Hover::Custom {
                        COL_ACTIVE
                    } else {
                        COL_HOVER
                    };
                    fill_round(mem, &custom, cust_bg, scaled(8));
                    SelectObject(mem, HGDIOBJ(state.font.0));
                    SetTextColor(mem, COLORREF(COL_TEXT));
                    let mut label = wide("Custom\u{2026}");
                    let mut cr = custom;
                    DrawTextW(
                        mem,
                        &mut label,
                        &mut cr,
                        DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
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
                let hov = hit(s, x, y);
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
                    let had = s.hover != Hover::None;
                    s.hover = Hover::None;
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
            // Resolve what was clicked under the borrow; perform the side effects
            // (registry write, taskbar reload, the modal color dialog) after
            // dropping it, since they pump messages and re-enter this wndproc.
            enum Act {
                Toggle(&'static str, bool),
                SetColor(u32),
                /// Open the Custom… picker seeded with this color.
                PickColor(u32),
                None,
            }
            let act = STATE.with_borrow_mut(|s| {
                let Some(s) = s.as_mut() else {
                    return Act::None;
                };
                match hit(s, x, y) {
                    Hover::Toggle(t) => {
                        let now = !s.values[t];
                        s.values[t] = now;
                        Act::Toggle(TOGGLES[t].reg, now)
                    }
                    Hover::Swatch(i) => {
                        s.start_color = SWATCHES[i];
                        Act::SetColor(SWATCHES[i])
                    }
                    Hover::Custom => Act::PickColor(s.start_color),
                    Hover::None => Act::None,
                }
            });
            match act {
                Act::Toggle(reg, now) => {
                    config::save_bool(reg, now);
                    crate::taskbar::reload_config();
                    let _ = InvalidateRect(hwnd, None, false);
                }
                Act::SetColor(color) => {
                    apply_start_color(color);
                    let _ = InvalidateRect(hwnd, None, false);
                }
                Act::PickColor(current) => {
                    if let Some(color) = pick_color(hwnd, current) {
                        STATE.with_borrow_mut(|s| {
                            if let Some(s) = s.as_mut() {
                                s.start_color = color;
                            }
                        });
                        apply_start_color(color);
                    }
                    let _ = InvalidateRect(hwnd, None, false);
                }
                Act::None => {}
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
