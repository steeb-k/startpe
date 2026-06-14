// SPDX-License-Identifier: GPL-3.0-or-later
//! A from-scratch, dark Run window — StartPE's replacement for the shell Run box.
//!
//! The shell's `RunFileDlg` can't be made properly dark in a plain PE: its
//! titlebar needs DWM (absent) and its control faces need the Themes service
//! (often not running), so dark theming only reached the GDI `WM_CTLCOLOR*`
//! layer. This window sidesteps all of that the way the rest of StartPE does —
//! a borderless `WS_POPUP` we own and paint entirely with double-buffered GDI in
//! the StartPE dark palette (no system caption, no uxtheme/DWM dependency). The
//! one real child control is a `COMBOBOX` for the input (a dropdown of this
//! session's command history), colored dark via `WM_CTLCOLOR*` (pure GDI, which
//! *does* work in PE). The title-bar app icon, the body icon + prompt, the
//! inline "Open:" label, and the OK / Cancel / Browse… buttons are owner-drawn
//! and hit-tested in the wndproc. The layout mirrors the classic Windows Run box.
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
    DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass, ShellExecuteW, SHGetStockIconInfo,
    SHGSI_ICON, SHSTOCKICONINFO, SIID_APPLICATION,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::taskbar::{
    make_font, make_font_face, scaled, COL_ACCENT, COL_ACTIVE, COL_BG, COL_HOVER, COL_TEXT,
    COL_TEXT_DIM,
};
use crate::util;

// Declared locally (as elsewhere in StartPE) to avoid pulling in extra features.
const WM_MOUSELEAVE: u32 = 0x02A3;

const RUN_PROMPT: &str = "Type the name of a program, folder, document, or Internet resource, and StartPE will open it for you.";

// Layout metrics in 96-DPI px (run through `scaled`).
const WIDTH: i32 = 400;
const TITLE_H: i32 = 30;
const PAD: i32 = 14;
const ICON: i32 = 32;
const TITLE_ICON: i32 = 16;
const PROMPT_H: i32 = 44;
const ROW_H: i32 = 24; // input row (combo closed height)
const LABEL_W: i32 = 44; // "Open:" label width
const COMBO_DROP: i32 = 150; // total combo height incl. drop-down area
const BTN_W: i32 = 80;
const BTN_H: i32 = 26;
const GAP: i32 = 9;
const CLOSE: i32 = 30;

const GLYPH_CLOSE: u16 = 0xE8BB; // Segoe MDL2 ChromeClose

// Combo / edit messages not always surfaced by name.
const CB_GETDROPPEDSTATE_: u32 = 0x0157;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hover {
    None,
    Close,
    Ok,
    Cancel,
    Browse,
}

struct State {
    hwnd: HWND,
    /// The input combobox (history dropdown). Holds the typed text. Its child
    /// edit is subclassed for Enter/Esc at creation time.
    combo: HWND,
    icon: HICON,
    hover: Hover,
    tracking_mouse: bool,
    font: HFONT,
    font_title: HFONT,
    font_glyph: HFONT,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    /// Commands run this session, oldest first (PE wipes the registry each boot,
    /// so persisting across reboots is pointless — session recall is enough).
    static HISTORY: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// Cached dark brush for the input field / list (one per process).
    static FIELD_BRUSH: Cell<HBRUSH> = const { Cell::new(HBRUSH(std::ptr::null_mut())) };
}

/// One laid-out window: every rect is client-relative and already DPI-scaled.
struct Layout {
    title_icon: RECT,
    title_text_left: i32,
    icon: RECT,
    prompt: RECT,
    label: RECT,
    combo: RECT,
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
    let combo = RECT {
        left: label.right + scaled(6),
        top: row_top,
        right: w - pad,
        bottom: row_top + row_h,
    };

    let btn_top = combo.bottom + scaled(18);
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
        combo,
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
        let existing = STATE.with_borrow(|s| s.as_ref().map(|s| (s.hwnd, s.combo)));
        if let Some((hwnd, combo)) = existing {
            if IsWindow(hwnd).as_bool() {
                let _ = SetForegroundWindow(hwnd);
                let _ = SetFocus(combo);
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

        let lay = layout();
        let w = scaled(WIDTH);
        let h = lay.height;
        let margin = scaled(12);
        let x = margin;
        let y = (taskbar_top - h - margin).max(margin);

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
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

        // Rounded corners via a GDI region (no DWM needed in PE).
        let rgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, scaled(10), scaled(10));
        let _ = SetWindowRgn(hwnd, rgn, true);

        let font = make_font(scaled(12), 400);

        // The single real control: a dark editable combobox with history.
        let combo = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("COMBOBOX"),
            PCWSTR::null(),
            WS_CHILD
                | WS_VISIBLE
                | WS_VSCROLL
                | WINDOW_STYLE((CBS_DROPDOWN | CBS_AUTOHSCROLL) as u32),
            lay.combo.left,
            lay.combo.top,
            lay.combo.right - lay.combo.left,
            scaled(COMBO_DROP),
            hwnd,
            None,
            hinstance,
            None,
        )
        .unwrap_or_default();
        SendMessageW(combo, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));

        // Populate the dropdown with this session's history, newest first, and
        // preselect the most recent (like the classic Run box).
        HISTORY.with_borrow(|hist| {
            for item in hist.iter().rev() {
                let s = util::WideStr::new(item);
                SendMessageW(combo, CB_ADDSTRING, WPARAM(0), LPARAM(s.pcwstr().0 as isize));
            }
            if let Some(last) = hist.last() {
                set_text(combo, last);
            }
        });

        // Subclass the combobox's child edit for Enter / Esc.
        let edit = FindWindowExW(combo, None, w!("EDIT"), None).unwrap_or_default();
        if !edit.is_invalid() {
            let _ = SetWindowSubclass(edit, Some(edit_subclass), 1, 0);
        }

        STATE.with_borrow_mut(|s| {
            *s = Some(State {
                hwnd,
                combo,
                icon: load_icon(),
                hover: Hover::None,
                tracking_mouse: false,
                font,
                font_title: make_font(scaled(13), 400),
                font_glyph: make_font_face(scaled(10), 400, w!("Segoe MDL2 Assets")),
            });
        });

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(combo);
        log_open();
    }
}

/// An application icon for the window, matching the classic Run box.
unsafe fn load_icon() -> HICON {
    let mut info = SHSTOCKICONINFO {
        cbSize: std::mem::size_of::<SHSTOCKICONINFO>() as u32,
        ..Default::default()
    };
    if SHGetStockIconInfo(SIID_APPLICATION, SHGSI_ICON, &mut info).is_ok()
        && !info.hIcon.is_invalid()
    {
        return info.hIcon;
    }
    HICON::default()
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
        WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
            let hdc = HDC(wp.0 as *mut core::ffi::c_void);
            SetTextColor(hdc, COLORREF(COL_TEXT));
            SetBkColor(hdc, COLORREF(COL_HOVER));
            LRESULT(field_brush().0 as isize)
        }
        WM_SETFOCUS => {
            let combo = STATE.with_borrow(|s| s.as_ref().map(|s| s.combo));
            if let Some(combo) = combo {
                let _ = SetFocus(combo);
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
            let y = util::hiword(lp.0);
            let x = util::loword(lp.0);
            // Drag from the title bar (anywhere but the close button).
            if y < scaled(TITLE_H) && !point_in(&layout().close, x, y) {
                let _ = ReleaseCapture();
                SendMessageW(hwnd, WM_NCLBUTTONDOWN, WPARAM(HTCAPTION as usize), LPARAM(0));
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let act = STATE.with_borrow(|s| s.as_ref().map(|s| (s.hover, s.hwnd, s.combo)));
            if let Some((hover, hw, combo)) = act {
                match hover {
                    Hover::Close | Hover::Cancel => {
                        let _ = DestroyWindow(hw);
                    }
                    Hover::Ok => do_run(hw, combo),
                    Hover::Browse => browse(hw, combo),
                    Hover::None => {}
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            STATE.with_borrow_mut(|s| {
                if let Some(s) = s.take() {
                    let _ = DeleteObject(HGDIOBJ(s.font.0));
                    let _ = DeleteObject(HGDIOBJ(s.font_title.0));
                    let _ = DeleteObject(HGDIOBJ(s.font_glyph.0));
                }
            });
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

        // Title-bar app icon + "Run".
        if !state.icon.is_invalid() {
            let _ = DrawIconEx(
                mem,
                lay.title_icon.left,
                lay.title_icon.top,
                state.icon,
                scaled(TITLE_ICON),
                scaled(TITLE_ICON),
                0,
                None,
                DI_NORMAL,
            );
        }
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

        // Body icon + prompt.
        if !state.icon.is_invalid() {
            let _ = DrawIconEx(
                mem,
                lay.icon.left,
                lay.icon.top,
                state.icon,
                scaled(ICON),
                scaled(ICON),
                0,
                None,
                DI_NORMAL,
            );
        }
        SelectObject(mem, HGDIOBJ(state.font.0));
        SetTextColor(mem, COLORREF(COL_TEXT));
        let mut prompt = wide(RUN_PROMPT);
        let mut pr = lay.prompt;
        DrawTextW(mem, &mut prompt, &mut pr, DT_LEFT | DT_WORDBREAK | DT_NOPREFIX);

        // Inline "Open:" label, vertically centered with the combo.
        let mut label = wide("Open:");
        let mut lr = lay.label;
        DrawTextW(
            mem,
            &mut label,
            &mut lr,
            DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX,
        );

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

/// Subclass on the combobox's edit: Enter runs, Esc cancels (or just closes an
/// open dropdown). Up/Down fall through to the combo for native history cycling.
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
                if let Ok(combo) = GetParent(hwnd) {
                    if let Ok(win) = GetParent(combo) {
                        do_run(win, combo);
                    }
                }
                return LRESULT(0);
            }
            if key == VK_ESCAPE.0 as usize {
                if let Ok(combo) = GetParent(hwnd) {
                    let dropped =
                        SendMessageW(combo, CB_GETDROPPEDSTATE_, WPARAM(0), LPARAM(0)).0 != 0;
                    if dropped {
                        return DefSubclassProc(hwnd, msg, wp, lp);
                    }
                    if let Ok(win) = GetParent(combo) {
                        let _ = DestroyWindow(win);
                    }
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
fn do_run(hwnd: HWND, combo: HWND) {
    let cmd = unsafe { get_text(combo) };
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

/// Open a file picker and drop the chosen (quoted) path into the combo.
fn browse(hwnd: HWND, combo: HWND) {
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
            set_text(combo, &format!("\"{path}\""));
        }
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(combo);
    }
}

unsafe fn get_text(combo: HWND) -> String {
    let len = GetWindowTextLengthW(combo);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let n = GetWindowTextW(combo, &mut buf);
    String::from_utf16_lossy(&buf[..n as usize])
}

unsafe fn set_text(combo: HWND, s: &str) {
    let w = util::WideStr::new(s);
    let _ = SetWindowTextW(combo, w.pcwstr());
    // Select all the edit text (LOWORD=start 0, HIWORD=end -1).
    SendMessageW(combo, CB_SETEDITSEL, WPARAM(0), LPARAM(0xFFFF_0000u32 as i32 as isize));
}
