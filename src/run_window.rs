// SPDX-License-Identifier: GPL-3.0-or-later
//! A from-scratch, dark Run window — StartPE's replacement for the shell Run box.
//!
//! The shell's `RunFileDlg` can't be made properly dark in a plain PE: its
//! titlebar needs DWM (absent) and its control faces need the Themes service
//! (often not running), so dark theming only reached the GDI `WM_CTLCOLOR*`
//! layer. This window sidesteps all of that the way the rest of StartPE does —
//! a borderless `WS_POPUP` we own and paint entirely with double-buffered GDI in
//! the StartPE dark palette (no system caption, no uxtheme/DWM dependency). The
//! one real child control is a single-line `EDIT` for the input, colored dark via
//! `WM_CTLCOLOREDIT` (pure GDI, which *does* work in PE). The icon + prompt + the
//! OK / Cancel / Browse… buttons are owner-drawn and hit-tested in the wndproc.
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
    SHGSI_ICON, SHSTOCKICONINFO, SIID_DESKTOPPC,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::taskbar::{make_font, make_font_face, scaled, COL_ACCENT, COL_ACTIVE, COL_BG, COL_HOVER,
    COL_TEXT, COL_TEXT_DIM};
use crate::util;

// Declared locally (as elsewhere in StartPE) to avoid pulling in extra features.
const WM_MOUSELEAVE: u32 = 0x02A3;
const EM_SETSEL: u32 = 0x00B1;

const RUN_PROMPT: &str = "Type the name of a program, folder, document, or Internet resource, and StartPE will open it for you.";

// Layout metrics in 96-DPI px (run through `scaled`).
const WIDTH: i32 = 390;
const TITLE_H: i32 = 34;
const PAD: i32 = 16;
const ICON: i32 = 32;
const PROMPT_H: i32 = 48;
const LABEL_H: i32 = 20;
const EDIT_H: i32 = 26;
const BTN_W: i32 = 84;
const BTN_H: i32 = 30;
const GAP: i32 = 10;
const CLOSE: i32 = 34;

const GLYPH_CLOSE: u16 = 0xE8BB; // Segoe MDL2 ChromeClose

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
    edit: HWND,
    icon: HICON,
    hover: Hover,
    tracking_mouse: bool,
    /// Cursor into [`HISTORY`] for Up/Down recall (`== len` means "past the end",
    /// i.e. a fresh empty entry).
    hist_pos: i32,
    font: HFONT,
    font_title: HFONT,
    font_glyph: HFONT,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    /// Commands run this session, oldest first (PE wipes the registry each boot,
    /// so persisting across reboots is pointless — session recall is enough).
    static HISTORY: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// Cached dark brush for the input field (one per process, never freed).
    static FIELD_BRUSH: Cell<HBRUSH> = const { Cell::new(HBRUSH(std::ptr::null_mut())) };
}

/// One laid-out window: every rect is client-relative and already DPI-scaled.
struct Layout {
    icon: RECT,
    prompt: RECT,
    label: RECT,
    edit: RECT,
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
    let label = RECT {
        left: pad,
        top: block_bottom + scaled(GAP),
        right: w - pad,
        bottom: block_bottom + scaled(GAP) + scaled(LABEL_H),
    };
    let edit = RECT {
        left: pad,
        top: label.bottom + scaled(2),
        right: w - pad,
        bottom: label.bottom + scaled(2) + scaled(EDIT_H),
    };
    let btn_top = edit.bottom + scaled(GAP) * 2;
    let btn_bottom = btn_top + scaled(BTN_H);
    let bw = scaled(BTN_W);
    let g = scaled(GAP);
    // Right-aligned block; left→right reads OK, Cancel, Browse… (Browse rightmost).
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
        icon,
        prompt,
        label,
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

        let font = make_font(scaled(14), 400);

        // The single real control: a dark single-line edit for the input.
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

        let hist_len = HISTORY.with_borrow(|h| h.len() as i32);
        STATE.with_borrow_mut(|s| {
            *s = Some(State {
                hwnd,
                edit,
                icon: load_icon(),
                hover: Hover::None,
                tracking_mouse: false,
                hist_pos: hist_len,
                font,
                font_title: make_font(scaled(15), 600),
                font_glyph: make_font_face(scaled(11), 400, w!("Segoe MDL2 Assets")),
            });
        });

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(edit);
        log_open();
    }
}

/// A monitor icon for the window, matching the old shell Run box.
unsafe fn load_icon() -> HICON {
    let mut info = SHSTOCKICONINFO {
        cbSize: std::mem::size_of::<SHSTOCKICONINFO>() as u32,
        ..Default::default()
    };
    if SHGetStockIconInfo(SIID_DESKTOPPC, SHGSI_ICON, &mut info).is_ok() && !info.hIcon.is_invalid() {
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
            let act = STATE.with_borrow(|s| s.as_ref().map(|s| (s.hover, s.hwnd, s.edit)));
            if let Some((hover, hw, edit)) = act {
                match hover {
                    Hover::Close | Hover::Cancel => {
                        let _ = DestroyWindow(hw);
                    }
                    Hover::Ok => do_run(hw, edit),
                    Hover::Browse => browse(hw, edit),
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

        SetTextColor(mem, COLORREF(COL_TEXT));
        SelectObject(mem, HGDIOBJ(state.font_title.0));
        let mut title = wide("Run");
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

        // Icon + prompt.
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
        SetTextColor(mem, COLORREF(COL_TEXT_DIM));
        let mut prompt = wide(RUN_PROMPT);
        let mut pr = lay.prompt;
        DrawTextW(mem, &mut prompt, &mut pr, DT_LEFT | DT_WORDBREAK | DT_NOPREFIX);

        // "Open:" label.
        SetTextColor(mem, COLORREF(COL_TEXT));
        let mut label = wide("Open:");
        let mut lr = lay.label;
        DrawTextW(
            mem,
            &mut label,
            &mut lr,
            DT_SINGLELINE | DT_BOTTOM | DT_LEFT | DT_NOPREFIX,
        );

        // Border around the input field (the edit control paints its own dark
        // interior via WM_CTLCOLOREDIT).
        let border = CreateSolidBrush(COLORREF(COL_TEXT_DIM));
        let brc = RECT {
            left: lay.edit.left - 1,
            top: lay.edit.top - 1,
            right: lay.edit.right + 1,
            bottom: lay.edit.bottom + 1,
        };
        FrameRect(mem, &brc, border);
        let _ = DeleteObject(HGDIOBJ(border.0));

        // Buttons.
        draw_button(mem, state, &lay.ok, "OK", true);
        draw_button(mem, state, &lay.cancel, "Cancel", false);
        draw_button(mem, state, &lay.browse, "Browse\u{2026}", false);

        let _ = BitBlt(hdc, 0, 0, width, height, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old_bmp);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        let _ = EndPaint(hwnd, &ps);
    }
}

fn draw_button(hdc: HDC, state: &State, rc: &RECT, label: &str, default: bool) {
    let hovered = match label {
        "OK" => state.hover == Hover::Ok,
        "Cancel" => state.hover == Hover::Cancel,
        _ => state.hover == Hover::Browse,
    };
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
    fill_round(hdc, rc, bg, scaled(5));
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

/// Subclass on the input edit: Enter runs, Esc cancels, Up/Down recall history.
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
                if let Ok(parent) = GetParent(hwnd) {
                    do_run(parent, hwnd);
                }
                return LRESULT(0);
            }
            if key == VK_ESCAPE.0 as usize {
                if let Ok(parent) = GetParent(hwnd) {
                    let _ = DestroyWindow(parent);
                }
                return LRESULT(0);
            }
            if key == VK_UP.0 as usize {
                history_move(hwnd, -1);
                return LRESULT(0);
            }
            if key == VK_DOWN.0 as usize {
                history_move(hwnd, 1);
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

/// Recall the previous/next history entry into the edit (`delta` -1/+1).
fn history_move(edit: HWND, delta: i32) {
    let text = STATE.with_borrow_mut(|s| {
        let s = s.as_mut()?;
        HISTORY.with_borrow(|h| {
            if h.is_empty() {
                return None;
            }
            let len = h.len() as i32;
            let pos = (s.hist_pos + delta).clamp(0, len);
            s.hist_pos = pos;
            Some(if pos >= len {
                String::new()
            } else {
                h[pos as usize].clone()
            })
        })
    });
    if let Some(t) = text {
        unsafe { set_text(edit, &t) };
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
    STATE.with_borrow_mut(|s| {
        if let Some(s) = s.as_mut() {
            s.hist_pos = HISTORY.with_borrow(|h| h.len() as i32);
        }
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
    // Move the caret to the end.
    SendMessageW(edit, EM_SETSEL, WPARAM(0x7fff_ffff), LPARAM(0x7fff_ffff));
}
