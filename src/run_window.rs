// SPDX-License-Identifier: GPL-3.0-or-later
//! A from-scratch, dark Run window — StartPE's replacement for the shell Run box.
//!
//! The shell's `RunFileDlg` can't be made properly dark in a plain PE: its
//! titlebar needs DWM (absent) and its control faces need the Themes service
//! (often not running), so dark theming only reached the GDI `WM_CTLCOLOR*`
//! layer. This window sidesteps all of that the way the rest of StartPE does —
//! a borderless `WS_POPUP` we own and paint entirely with double-buffered GDI in
//! the StartPE dark palette (no system caption, no uxtheme/DWM dependency). The
//! one real child control is a single-line `EDIT` for input, colored dark via
//! `WM_CTLCOLOREDIT` (pure GDI, which *does* work in PE). Everything else — the
//! title-bar app icon, the body icon + prompt, the inline "Open:" label, the
//! OK / Cancel / Browse… buttons, and the **command-history dropdown** (its own
//! owner-drawn dark popup, since a real combobox draws a light system arrow +
//! sunken border that can't go dark in PE) — is owner-drawn and hit-tested. The
//! layout mirrors the classic Windows Run box.
//!
//! It is opened by every Run entry point StartPE controls (Win+R, the start
//! menu's Run…, the Win+X menu), which is every way the Run box appears on these
//! PE images — Explorer's own shell never comes up — so this effectively
//! *replaces* the standard Run window without injecting into other processes.

use std::cell::{Cell, RefCell};

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::Dialogs::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{
    DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::taskbar::{
    make_font, make_font_face, scaled, COL_ACCENT, COL_ACTIVE, COL_BG, COL_HOVER, COL_TEXT,
    COL_TEXT_DIM,
};
use crate::util;

// Declared locally (as elsewhere in StartPE) to avoid pulling in extra features.
const WM_MOUSELEAVE: u32 = 0x02A3;
const EM_SETSEL: u32 = 0x00B1;

const RUN_PROMPT: &str = "Type the name of a program, folder, document, or Internet resource, and StartPE will open it for you.";

// Layout metrics in 96-DPI px (run through `scaled`).
const WIDTH: i32 = 400;
const TITLE_H: i32 = 30;
const PAD: i32 = 14;
const ICON: i32 = 32;
const TITLE_ICON: i32 = 16;
const PROMPT_H: i32 = 44;
const ROW_H: i32 = 24; // input field height
const LABEL_W: i32 = 44; // "Open:" label width
const ARROW_W: i32 = 18; // dropdown-arrow slot inside the field
const BTN_W: i32 = 80;
const BTN_H: i32 = 26;
const GAP: i32 = 9;
const CLOSE: i32 = 30;
const LIST_ROW_H: i32 = 22; // history popup row height
const LIST_MAX: usize = 8; // most history rows shown at once

const GLYPH_CLOSE: u16 = 0xE8BB; // Segoe MDL2 ChromeClose
const GLYPH_CHEVRON: u16 = 0xE70D; // Segoe MDL2 ChevronDown
const GLYPH_RUN: u16 = 0xE74C; // Segoe MDL2 OEM (the Run app icon)

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hover {
    None,
    Close,
    Drop,
    Ok,
    Cancel,
    Browse,
}

/// The open command-history dropdown.
struct ListState {
    hwnd: HWND,
    items: Vec<String>,
    sel: i32,   // keyboard-selected row (-1 = none)
    hover: i32, // mouse-hovered row (-1 = none)
}

struct State {
    hwnd: HWND,
    edit: HWND,
    /// Accent color for the Run glyph (matches the start menu / Start button).
    accent: u32,
    hover: Hover,
    tracking_mouse: bool,
    list: Option<ListState>,
    font: HFONT,
    font_title: HFONT,
    font_glyph: HFONT,
    /// Segoe MDL2 fonts for the Run glyph at title (16px) and body (32px) sizes.
    font_icon_sm: HFONT,
    font_icon_lg: HFONT,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    /// Commands run this session, oldest first (PE wipes the registry each boot,
    /// so persisting across reboots is pointless — session recall is enough).
    static HISTORY: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// Cached dark brush for the input field (one per process).
    static FIELD_BRUSH: Cell<HBRUSH> = const { Cell::new(HBRUSH(std::ptr::null_mut())) };
    /// True when running as the dedicated `startpe.exe --run` process, so closing
    /// the window quits the process (rather than just hiding an in-app window).
    static STANDALONE: Cell<bool> = const { Cell::new(false) };
}

/// Entry point for `startpe.exe --run`: show the Run window and pump messages
/// until it closes, then return (the process exits). Run is its own process so
/// the shell treats it like any app — taskbar/Alt+Tab listing, normal Z order,
/// accent border.
pub fn run_standalone() {
    unsafe {
        // Single instance: if a Run window is already up (this or another --run
        // process), focus it and exit instead of stacking a second one.
        if let Ok(existing) = FindWindowW(w!("StartPE_Run"), PCWSTR::null()) {
            if !existing.is_invalid() {
                let _ = SetForegroundWindow(existing);
                return;
            }
        }
    }
    STANDALONE.with(|f| f.set(true));
    // Seat the window above the taskbar using the work-area bottom as reference.
    let taskbar_top = unsafe {
        let mut wa = RECT::default();
        let ok = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut wa as *mut _ as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok();
        if ok && wa.bottom > 0 {
            wa.bottom
        } else {
            GetSystemMetrics(SM_CYSCREEN)
        }
    };
    show(taskbar_top);
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Launch the Run window as a separate `startpe.exe --run` process. Called from
/// the taskbar process for every Run entry point (Win+R, start menu, Win+X).
pub fn launch() {
    if let Ok(exe) = std::env::current_exe() {
        if let Ok(child) = std::process::Command::new(exe).arg("--run").spawn() {
            // Grant the new process the right to come to the foreground (it isn't
            // the foreground process yet, so SetForegroundWindow would be denied).
            unsafe {
                let _ = AllowSetForegroundWindow(child.id());
            }
        }
    }
}

/// One laid-out window: every rect is client-relative and already DPI-scaled.
struct Layout {
    title_icon: RECT,
    title_text_left: i32,
    icon: RECT,
    prompt: RECT,
    label: RECT,
    field: RECT, // the bordered input box (edit + arrow)
    arrow: RECT, // dropdown-arrow slot at the right of the field
    edit: RECT,  // the EDIT child, inset inside the field
    ok: RECT,
    cancel: RECT,
    browse: RECT,
    close: RECT,
    height: i32,
}

fn layout() -> Layout {
    let w = scaled(WIDTH);
    let pad = scaled(PAD);
    let title = scaled(TITLE_H);

    let title_icon = RECT {
        left: scaled(10),
        top: (title - scaled(TITLE_ICON)) / 2,
        right: scaled(10) + scaled(TITLE_ICON),
        bottom: (title - scaled(TITLE_ICON)) / 2 + scaled(TITLE_ICON),
    };
    let title_text_left = title_icon.right + scaled(8);

    let icon = RECT {
        left: pad,
        top: title + pad,
        right: pad + scaled(ICON),
        bottom: title + pad + scaled(ICON),
    };
    let prompt = RECT {
        left: icon.right + scaled(12),
        top: title + pad,
        right: w - pad,
        bottom: title + pad + scaled(PROMPT_H),
    };
    let block_bottom = prompt.bottom.max(icon.bottom);

    let row_top = block_bottom + scaled(14);
    let row_h = scaled(ROW_H);
    let label = RECT {
        left: pad,
        top: row_top,
        right: pad + scaled(LABEL_W),
        bottom: row_top + row_h,
    };
    let field = RECT {
        left: label.right + scaled(6),
        top: row_top,
        right: w - pad,
        bottom: row_top + row_h,
    };
    let arrow = RECT {
        left: field.right - scaled(ARROW_W),
        top: field.top,
        right: field.right,
        bottom: field.bottom,
    };
    let edit = RECT {
        left: field.left + scaled(5),
        top: field.top + scaled(3),
        right: arrow.left - scaled(2),
        bottom: field.bottom - scaled(3),
    };

    let btn_top = field.bottom + scaled(18);
    let btn_bottom = btn_top + scaled(BTN_H);
    let bw = scaled(BTN_W);
    let g = scaled(GAP);
    let browse = RECT {
        left: w - pad - bw,
        top: btn_top,
        right: w - pad,
        bottom: btn_bottom,
    };
    let cancel = RECT {
        left: browse.left - g - bw,
        top: btn_top,
        right: browse.left - g,
        bottom: btn_bottom,
    };
    let ok = RECT {
        left: cancel.left - g - bw,
        top: btn_top,
        right: cancel.left - g,
        bottom: btn_bottom,
    };
    let close = RECT {
        left: w - scaled(CLOSE),
        top: 0,
        right: w,
        bottom: scaled(CLOSE),
    };
    Layout {
        title_icon,
        title_text_left,
        icon,
        prompt,
        label,
        field,
        arrow,
        edit,
        ok,
        cancel,
        browse,
        close,
        height: btn_bottom + pad,
    }
}

/// Show the Run window seated bottom-left above the taskbar (`taskbar_top` is the
/// taskbar's top screen edge), or re-focus it if already open.
pub fn show(taskbar_top: i32) {
    unsafe {
        // Single instance: bring the existing window forward instead of stacking.
        let existing = STATE.with_borrow(|s| s.as_ref().map(|s| (s.hwnd, s.edit)));
        if let Some((hwnd, edit)) = existing {
            if IsWindow(hwnd).as_bool() {
                let _ = SetForegroundWindow(hwnd);
                let _ = SetFocus(edit);
                return;
            }
        }

        let Ok(hinstance) = GetModuleHandleW(None) else {
            return;
        };
        let hinstance: HINSTANCE = hinstance.into();
        let class = w!("StartPE_Run");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc); // idempotent
        register_list_class(hinstance);

        let lay = layout();
        let w = scaled(WIDTH);
        let h = lay.height;
        let margin = scaled(12);
        let x = margin;
        let y = (taskbar_top - h - margin).max(margin);

        // WS_EX_APPWINDOW so the shell lists Run in the taskbar / Alt+Tab and
        // treats it as an ordinary window (normal Z order, accent border). Not
        // WS_EX_TOPMOST — other apps can come in front of it.
        let hwnd = CreateWindowExW(
            WS_EX_APPWINDOW,
            class,
            w!("Run"),
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
            return;
        };

        // Rounded corners via a GDI region (no DWM needed in PE). Radius 8 to
        // match the accent window border (border.rs CORNER) so the frame sits
        // flush on the corners.
        let rgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, scaled(16), scaled(16));
        let _ = SetWindowRgn(hwnd, rgn, true);

        let font = make_font(scaled(12), 400);

        // The single real control: a flat dark single-line edit.
        let edit = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            lay.edit.left,
            lay.edit.top,
            lay.edit.right - lay.edit.left,
            lay.edit.bottom - lay.edit.top,
            hwnd,
            None,
            hinstance,
            None,
        )
        .unwrap_or_default();
        SendMessageW(edit, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        let _ = SetWindowSubclass(edit, Some(edit_subclass), 1, 0);
        // Preselect the most recent command, like the classic Run box.
        if let Some(last) = HISTORY.with_borrow(|h| h.last().cloned()) {
            set_text(edit, &last);
        }

        STATE.with_borrow_mut(|s| {
            *s = Some(State {
                hwnd,
                edit,
                accent: crate::taskbar::start_button_color(),
                hover: Hover::None,
                tracking_mouse: false,
                list: None,
                font,
                font_title: make_font(scaled(13), 400),
                font_glyph: make_font_face(scaled(10), 400, w!("Segoe MDL2 Assets")),
                font_icon_sm: make_font_face(scaled(14), 400, w!("Segoe MDL2 Assets")),
                font_icon_lg: make_font_face(scaled(26), 400, w!("Segoe MDL2 Assets")),
            });
        });

        let _ = ShowWindow(hwnd, SW_SHOW);
        // Raise above everything, then drop back to the normal (non-topmost) band
        // so it opens in front yet still behaves like an ordinary window. Z-order
        // changes via SetWindowPos don't need foreground rights, whereas a freshly
        // spawned process's SetForegroundWindow can be denied.
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        let _ = SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(edit);
        log_open();
    }
}

fn log_open() {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("X:\\startpe.log")
    {
        let _ = writeln!(
            f,
            "StartPE v{} native Run window opened",
            env!("CARGO_PKG_VERSION")
        );
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1), // painted in WM_PAINT (double-buffered)
        WM_PAINT => {
            STATE.with_borrow(|s| {
                if let Some(s) = s.as_ref() {
                    paint(s);
                }
            });
            LRESULT(0)
        }
        WM_CTLCOLOREDIT => {
            let hdc = HDC(wp.0 as *mut core::ffi::c_void);
            SetTextColor(hdc, COLORREF(COL_TEXT));
            SetBkColor(hdc, COLORREF(COL_HOVER));
            LRESULT(field_brush().0 as isize)
        }
        WM_SETFOCUS => {
            let edit = STATE.with_borrow(|s| s.as_ref().map(|s| s.edit));
            if let Some(edit) = edit {
                let _ = SetFocus(edit);
            }
            LRESULT(0)
        }
        WM_ACTIVATE => {
            // Clicking away closes the dropdown (the window itself stays up).
            // LOWORD(wParam) == WA_INACTIVE (0).
            if util::loword(wp.0 as isize) == 0 {
                close_list();
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = util::loword(lp.0);
            let y = util::hiword(lp.0);
            let now = hit(x, y);
            STATE.with_borrow_mut(|s| {
                if let Some(s) = s.as_mut() {
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
                    if s.hover != now {
                        s.hover = now;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
            });
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            STATE.with_borrow_mut(|s| {
                if let Some(s) = s.as_mut() {
                    s.tracking_mouse = false;
                    if s.hover != Hover::None {
                        s.hover = Hover::None;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
            });
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = util::loword(lp.0);
            let y = util::hiword(lp.0);
            // Drag from the title bar (anywhere but the close button).
            if y < scaled(TITLE_H) && !point_in(&layout().close, x, y) {
                let _ = ReleaseCapture();
                SendMessageW(hwnd, WM_NCLBUTTONDOWN, WPARAM(HTCAPTION as usize), LPARAM(0));
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let act = STATE.with_borrow(|s| s.as_ref().map(|s| (s.hover, s.hwnd, s.edit)));
            if let Some((hover, hw, edit)) = act {
                match hover {
                    Hover::Close | Hover::Cancel => {
                        let _ = DestroyWindow(hw);
                    }
                    Hover::Ok => do_run(hw, edit),
                    Hover::Browse => browse(hw, edit),
                    Hover::Drop => {
                        let open = STATE.with_borrow(|s| {
                            s.as_ref().map(|s| s.list.is_some()).unwrap_or(false)
                        });
                        if open {
                            close_list();
                        } else {
                            open_list(hw);
                        }
                        let _ = SetFocus(edit);
                    }
                    Hover::None => {}
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            close_list();
            STATE.with_borrow_mut(|s| {
                if let Some(s) = s.take() {
                    let _ = DeleteObject(HGDIOBJ(s.font.0));
                    let _ = DeleteObject(HGDIOBJ(s.font_title.0));
                    let _ = DeleteObject(HGDIOBJ(s.font_glyph.0));
                    let _ = DeleteObject(HGDIOBJ(s.font_icon_sm.0));
                    let _ = DeleteObject(HGDIOBJ(s.font_icon_lg.0));
                }
            });
            // As its own process, closing the Run window ends the process.
            if STANDALONE.with(|f| f.get()) {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// The process-lifetime input-field brush, created on first use.
fn field_brush() -> HBRUSH {
    let cur = FIELD_BRUSH.get();
    if !cur.0.is_null() {
        return cur;
    }
    let b = unsafe { CreateSolidBrush(COLORREF(COL_HOVER)) };
    FIELD_BRUSH.set(b);
    b
}

fn point_in(rc: &RECT, x: i32, y: i32) -> bool {
    x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom
}

fn hit(x: i32, y: i32) -> Hover {
    let lay = layout();
    if point_in(&lay.close, x, y) {
        Hover::Close
    } else if point_in(&lay.arrow, x, y) {
        Hover::Drop
    } else if point_in(&lay.ok, x, y) {
        Hover::Ok
    } else if point_in(&lay.cancel, x, y) {
        Hover::Cancel
    } else if point_in(&lay.browse, x, y) {
        Hover::Browse
    } else {
        Hover::None
    }
}

/// UTF-16 buffer without a NUL terminator, for `DrawTextW`.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

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

fn frame(hdc: HDC, rc: &RECT, color: u32) {
    unsafe {
        let b = CreateSolidBrush(COLORREF(color));
        FrameRect(hdc, rc, b);
        let _ = DeleteObject(HGDIOBJ(b.0));
    }
}

fn paint(state: &State) {
    unsafe {
        let hwnd = state.hwnd;
        let lay = layout();
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let (width, height) = (rc.right, rc.bottom);

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

        // Title-bar Run glyph (accent-tinted) + "Run".
        SelectObject(mem, HGDIOBJ(state.font_icon_sm.0));
        SetTextColor(mem, COLORREF(state.accent));
        let mut ticon = [GLYPH_RUN, 0u16];
        let mut tir = lay.title_icon;
        DrawTextW(
            mem,
            &mut ticon[..1],
            &mut tir,
            DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
        );
        SetTextColor(mem, COLORREF(COL_TEXT));
        SelectObject(mem, HGDIOBJ(state.font_title.0));
        let mut title = wide("Run");
        let mut tr = RECT {
            left: lay.title_text_left,
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

        // Close glyph (brighter on hover).
        SelectObject(mem, HGDIOBJ(state.font_glyph.0));
        SetTextColor(
            mem,
            COLORREF(if state.hover == Hover::Close {
                COL_TEXT
            } else {
                COL_TEXT_DIM
            }),
        );
        let mut close = [GLYPH_CLOSE, 0u16];
        let mut cr = lay.close;
        DrawTextW(
            mem,
            &mut close[..1],
            &mut cr,
            DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
        );

        // Body Run glyph (accent-tinted) + prompt.
        SelectObject(mem, HGDIOBJ(state.font_icon_lg.0));
        SetTextColor(mem, COLORREF(state.accent));
        let mut bicon = [GLYPH_RUN, 0u16];
        let mut bir = lay.icon;
        DrawTextW(
            mem,
            &mut bicon[..1],
            &mut bir,
            DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
        );
        SelectObject(mem, HGDIOBJ(state.font.0));
        SetTextColor(mem, COLORREF(COL_TEXT));
        let mut prompt = wide(RUN_PROMPT);
        let mut pr = lay.prompt;
        DrawTextW(mem, &mut prompt, &mut pr, DT_LEFT | DT_WORDBREAK | DT_NOPREFIX);

        // Inline "Open:" label, vertically centered with the field.
        let mut label = wide("Open:");
        let mut lr = lay.label;
        DrawTextW(
            mem,
            &mut label,
            &mut lr,
            DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX,
        );

        // Input field: flat dark fill, owner-drawn dropdown arrow, 1px border.
        // (The EDIT child paints its own interior over the left part.)
        let field_fill = CreateSolidBrush(COLORREF(COL_HOVER));
        FillRect(mem, &lay.field, field_fill);
        let _ = DeleteObject(HGDIOBJ(field_fill.0));
        if state.hover == Hover::Drop {
            let hov = CreateSolidBrush(COLORREF(COL_ACTIVE));
            FillRect(mem, &lay.arrow, hov);
            let _ = DeleteObject(HGDIOBJ(hov.0));
        }
        SelectObject(mem, HGDIOBJ(state.font_glyph.0));
        SetTextColor(mem, COLORREF(COL_TEXT));
        let mut chev = [GLYPH_CHEVRON, 0u16];
        let mut ar = lay.arrow;
        DrawTextW(
            mem,
            &mut chev[..1],
            &mut ar,
            DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
        );
        frame(mem, &lay.field, COL_TEXT_DIM);

        // Buttons.
        draw_button(mem, state, &lay.ok, "OK", Hover::Ok, true);
        draw_button(mem, state, &lay.cancel, "Cancel", Hover::Cancel, false);
        draw_button(mem, state, &lay.browse, "Browse\u{2026}", Hover::Browse, false);

        let _ = BitBlt(hdc, 0, 0, width, height, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old_bmp);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        let _ = EndPaint(hwnd, &ps);
    }
}

fn draw_button(hdc: HDC, state: &State, rc: &RECT, label: &str, which: Hover, default: bool) {
    let hovered = state.hover == which;
    let bg = if default {
        if hovered {
            lighten(COL_ACCENT)
        } else {
            COL_ACCENT
        }
    } else if hovered {
        COL_ACTIVE
    } else {
        COL_HOVER
    };
    fill_round(hdc, rc, bg, scaled(4));
    unsafe {
        SetTextColor(hdc, COLORREF(COL_TEXT));
        SelectObject(hdc, HGDIOBJ(state.font.0));
        let mut text = wide(label);
        let mut tr = *rc;
        DrawTextW(
            hdc,
            &mut text,
            &mut tr,
            DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
        );
    }
}

/// Nudge a COLORREF lighter for a hovered accent button.
fn lighten(c: u32) -> u32 {
    let r = ((c & 0xFF) + 24).min(255);
    let g = (((c >> 8) & 0xFF) + 24).min(255);
    let b = (((c >> 16) & 0xFF) + 24).min(255);
    r | (g << 8) | (b << 16)
}

// ---- command-history dropdown (owner-drawn dark popup) ----------------------

fn register_list_class(hinstance: HINSTANCE) {
    unsafe {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(list_wndproc),
            hInstance: hinstance,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: w!("StartPE_RunList"),
            ..Default::default()
        };
        RegisterClassW(&wc);
    }
}

/// Open the history dropdown below the field (no-op if there's no history).
fn open_list(main: HWND) {
    let items: Vec<String> = HISTORY.with_borrow(|h| h.iter().rev().cloned().collect());
    if items.is_empty() {
        return;
    }
    unsafe {
        let lay = layout();
        let mut p = POINT {
            x: lay.field.left,
            y: lay.field.bottom + scaled(1),
        };
        let _ = ClientToScreen(main, &mut p);
        let width = lay.field.right - lay.field.left;
        let shown = items.len().min(LIST_MAX) as i32;
        let height = shown * scaled(LIST_ROW_H) + 2;
        let hinstance: HINSTANCE = GetModuleHandleW(None).map(Into::into).unwrap_or_default();
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            w!("StartPE_RunList"),
            PCWSTR::null(),
            WS_POPUP,
            p.x,
            p.y,
            width,
            height,
            main,
            None,
            hinstance,
            None,
        )
        .unwrap_or_default();
        if hwnd.is_invalid() {
            return;
        }
        STATE.with_borrow_mut(|s| {
            if let Some(s) = s.as_mut() {
                s.list = Some(ListState {
                    hwnd,
                    items,
                    sel: -1,
                    hover: -1,
                });
            }
        });
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
}

fn close_list() {
    let hwnd = STATE.with_borrow_mut(|s| s.as_mut().and_then(|s| s.list.take()).map(|l| l.hwnd));
    if let Some(hwnd) = hwnd {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}

/// Move the dropdown selection by `delta` (opening it first if closed), and
/// preview the selected entry in the edit.
fn list_nav(main: HWND, edit: HWND, delta: i32) {
    let open = STATE.with_borrow(|s| s.as_ref().map(|s| s.list.is_some()).unwrap_or(false));
    if !open {
        open_list(main);
    }
    let item = STATE.with_borrow_mut(|s| {
        let s = s.as_mut()?;
        let l = s.list.as_mut()?;
        let len = l.items.len() as i32;
        if len == 0 {
            return None;
        }
        // First nav after opening lands on the natural end.
        let base = if l.sel < 0 {
            if delta >= 0 {
                0
            } else {
                len - 1
            }
        } else {
            (l.sel + delta).clamp(0, len - 1)
        };
        l.sel = base;
        l.hover = -1;
        unsafe {
            let _ = InvalidateRect(l.hwnd, None, false);
        }
        Some(l.items[base as usize].clone())
    });
    if let Some(item) = item {
        unsafe { set_text(edit, &item) };
    }
}

unsafe extern "system" fn list_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint_list(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let y = util::hiword(lp.0);
            let idx = y / scaled(LIST_ROW_H);
            STATE.with_borrow_mut(|s| {
                if let Some(l) = s.as_mut().and_then(|s| s.list.as_mut()) {
                    let idx = if idx >= 0 && (idx as usize) < l.items.len() {
                        idx
                    } else {
                        -1
                    };
                    if l.hover != idx {
                        l.hover = idx;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
            });
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let y = util::hiword(lp.0);
            let idx = y / scaled(LIST_ROW_H);
            let pick = STATE.with_borrow(|s| {
                s.as_ref()
                    .and_then(|s| s.list.as_ref())
                    .and_then(|l| l.items.get(idx as usize).cloned())
            });
            let edit = STATE.with_borrow(|s| s.as_ref().map(|s| s.edit));
            if let (Some(item), Some(edit)) = (pick, edit) {
                set_text(edit, &item);
                close_list();
                let _ = SetFocus(edit);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn paint_list(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let (width, height) = (rc.right, rc.bottom);

        let mem = CreateCompatibleDC(hdc);
        let bmp = CreateCompatibleBitmap(hdc, width, height);
        let old_bmp = SelectObject(mem, bmp);

        let bg = CreateSolidBrush(COLORREF(COL_HOVER));
        FillRect(mem, &rc, bg);
        let _ = DeleteObject(HGDIOBJ(bg.0));
        SetBkMode(mem, TRANSPARENT);

        STATE.with_borrow(|s| {
            if let Some((font, l)) = s.as_ref().and_then(|s| Some((s.font, s.list.as_ref()?))) {
                SelectObject(mem, HGDIOBJ(font.0));
                let row = scaled(LIST_ROW_H);
                for (i, item) in l.items.iter().take(LIST_MAX).enumerate() {
                    let top = i as i32 * row;
                    let rr = RECT {
                        left: 0,
                        top,
                        right: width,
                        bottom: top + row,
                    };
                    if i as i32 == l.hover || (l.hover < 0 && i as i32 == l.sel) {
                        let hb = CreateSolidBrush(COLORREF(COL_ACTIVE));
                        FillRect(mem, &rr, hb);
                        let _ = DeleteObject(HGDIOBJ(hb.0));
                    }
                    SetTextColor(mem, COLORREF(COL_TEXT));
                    let mut text = wide(item);
                    let mut tr = RECT {
                        left: scaled(8),
                        top,
                        right: width - scaled(8),
                        bottom: top + row,
                    };
                    DrawTextW(
                        mem,
                        &mut text,
                        &mut tr,
                        DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX | DT_END_ELLIPSIS,
                    );
                }
            }
        });
        frame(mem, &rc, COL_TEXT_DIM);

        let _ = BitBlt(hdc, 0, 0, width, height, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old_bmp);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        let _ = EndPaint(hwnd, &ps);
    }
}

// ---- input edit subclass ----------------------------------------------------

/// Subclass on the input edit: Enter runs, Esc cancels (or closes an open
/// dropdown), Up/Down drive the history dropdown.
unsafe extern "system" fn edit_subclass(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _ref: usize,
) -> LRESULT {
    match msg {
        WM_KEYDOWN => {
            let key = wp.0;
            if key == VK_RETURN.0 as usize {
                if let Ok(win) = GetParent(hwnd) {
                    close_list();
                    do_run(win, hwnd);
                }
                return LRESULT(0);
            }
            if key == VK_ESCAPE.0 as usize {
                let open = STATE.with_borrow(|s| s.as_ref().map(|s| s.list.is_some()).unwrap_or(false));
                if open {
                    close_list();
                } else if let Ok(win) = GetParent(hwnd) {
                    let _ = DestroyWindow(win);
                }
                return LRESULT(0);
            }
            if key == VK_DOWN.0 as usize || key == VK_UP.0 as usize {
                if let Ok(win) = GetParent(hwnd) {
                    let delta = if key == VK_DOWN.0 as usize { 1 } else { -1 };
                    list_nav(win, hwnd, delta);
                }
                return LRESULT(0);
            }
            DefSubclassProc(hwnd, msg, wp, lp)
        }
        WM_CHAR => {
            // Swallow Enter/Esc so the edit doesn't beep.
            if wp.0 == 0x0D || wp.0 == 0x1B {
                return LRESULT(0);
            }
            DefSubclassProc(hwnd, msg, wp, lp)
        }
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(hwnd, Some(edit_subclass), 1);
            DefSubclassProc(hwnd, msg, wp, lp)
        }
        _ => DefSubclassProc(hwnd, msg, wp, lp),
    }
}

/// Read the command, record it, and run it. Closes on success; on failure shows
/// the familiar "cannot find" message and leaves the window open to fix.
fn do_run(hwnd: HWND, edit: HWND) {
    let cmd = unsafe { get_text(edit) };
    let trimmed = cmd.trim().to_string();
    if trimmed.is_empty() {
        return;
    }
    HISTORY.with_borrow_mut(|h| {
        h.retain(|e| e != &trimmed);
        h.push(trimmed.clone());
    });
    if unsafe { execute(&trimmed) } {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    } else {
        unsafe {
            let caption = util::WideStr::new("Run");
            let body = util::WideStr::new(&format!(
                "StartPE cannot find '{trimmed}'. Make sure you typed the name correctly, and then try again."
            ));
            let _ = MessageBoxW(
                hwnd,
                body.pcwstr(),
                caption.pcwstr(),
                MB_OK | MB_ICONEXCLAMATION,
            );
        }
    }
}

/// Run a command the way the Run box does: expand env vars, split program from
/// args, and `ShellExecute` it. Returns whether the launch succeeded.
unsafe fn execute(raw: &str) -> bool {
    let expanded = expand_env(raw);
    let (program, args) = split_command(&expanded);
    let p = util::WideStr::new(&program);
    let a = util::WideStr::new(&args);
    let params = if args.is_empty() {
        PCWSTR::null()
    } else {
        a.pcwstr()
    };
    let r = ShellExecuteW(None, w!("open"), p.pcwstr(), params, PCWSTR::null(), SW_SHOWNORMAL);
    r.0 as usize > 32
}

unsafe fn expand_env(s: &str) -> String {
    let src = util::WideStr::new(s);
    let n = ExpandEnvironmentStringsW(src.pcwstr(), None);
    if n == 0 {
        return s.to_string();
    }
    let mut buf = vec![0u16; n as usize];
    let n2 = ExpandEnvironmentStringsW(src.pcwstr(), Some(&mut buf));
    if n2 == 0 {
        return s.to_string();
    }
    String::from_utf16_lossy(&buf[..(n2 as usize).saturating_sub(1)])
}

/// Split an entered command into (program, args). If the whole string is an
/// existing path it runs as-is (handles paths with spaces and no args); a quoted
/// program is honored; otherwise it splits on the first whitespace (the shell
/// resolves bare names like `notepad` via PATH/App Paths).
fn split_command(s: &str) -> (String, String) {
    let s = s.trim();
    if path_exists(s) {
        return (s.to_string(), String::new());
    }
    if let Some(rest) = s.strip_prefix('"') {
        if let Some(end) = rest.find('"') {
            return (
                rest[..end].to_string(),
                rest[end + 1..].trim_start().to_string(),
            );
        }
    }
    match s.find(char::is_whitespace) {
        Some(i) => (s[..i].to_string(), s[i + 1..].trim_start().to_string()),
        None => (s.to_string(), String::new()),
    }
}

fn path_exists(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let w = util::WideStr::new(s);
    unsafe { GetFileAttributesW(w.pcwstr()) != INVALID_FILE_ATTRIBUTES }
}

/// Open a file picker and drop the chosen (quoted) path into the edit.
fn browse(hwnd: HWND, edit: HWND) {
    unsafe {
        let mut buf = [0u16; 1040];
        let filter: Vec<u16> = "Programs\0*.exe;*.bat;*.cmd;*.msc\0All Files\0*.*\0\0"
            .encode_utf16()
            .collect();
        let title: Vec<u16> = "Browse\0".encode_utf16().collect();
        let mut ofn = OPENFILENAMEW {
            lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
            hwndOwner: hwnd,
            lpstrFilter: PCWSTR(filter.as_ptr()),
            lpstrFile: PWSTR(buf.as_mut_ptr()),
            nMaxFile: buf.len() as u32,
            lpstrTitle: PCWSTR(title.as_ptr()),
            Flags: OFN_FILEMUSTEXIST | OFN_HIDEREADONLY,
            ..Default::default()
        };
        if GetOpenFileNameW(&mut ofn).as_bool() {
            let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            let path = String::from_utf16_lossy(&buf[..n]);
            set_text(edit, &format!("\"{path}\""));
        }
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(edit);
    }
}

unsafe fn get_text(edit: HWND) -> String {
    let len = GetWindowTextLengthW(edit);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let n = GetWindowTextW(edit, &mut buf);
    String::from_utf16_lossy(&buf[..n as usize])
}

unsafe fn set_text(edit: HWND, s: &str) {
    let w = util::WideStr::new(s);
    let _ = SetWindowTextW(edit, w.pcwstr());
    // Move the caret to the end and select all.
    SendMessageW(edit, EM_SETSEL, WPARAM(0), LPARAM(-1));
}
