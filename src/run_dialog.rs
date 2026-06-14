// SPDX-License-Identifier: GPL-3.0-or-later
//! The shell Run dialog, invoked properly.
//!
//! `rundll32 shell32.dll,#61` (the usual way to pop the Run box) calls the
//! shell's `RunFileDlg` with *no* arguments, so it shows the wrong prompt
//! ("RunDLL"), no icon, and a default position. We instead call ordinal 61
//! directly with a real icon and the standard prompt, and nudge the dialog to
//! the bottom-left above the taskbar with a one-shot thread-local `WH_CBT` hook.
//!
//! The dialog is owned by a throwaway *hidden* window, not the taskbar: a modal
//! dialog disables its owner, so owning it with the taskbar would freeze the
//! shell (the original bug). The hidden owner also keeps the dialog off our
//! taskbar (owned windows aren't task buttons). `RunFileDlg` runs its own modal
//! loop which pumps messages, so the taskbar/start menu stay live underneath.
//!
//! `RunFileDlg` (shell32 ordinal 61) is undocumented — a confined exception like
//! `darkmode.rs`. The icon (`SHGetStockIconInfo`) and positioning (`WH_CBT`) are
//! documented.

use std::cell::Cell;

use windows::core::{w, PCSTR, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, RedrawWindow, SetBkColor, SetTextColor, HBRUSH, HDC, HRGN, RDW_ALLCHILDREN,
    RDW_ERASE, RDW_INVALIDATE, RDW_UPDATENOW,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::{
    DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass, SHGetStockIconInfo, SHGSI_ICON,
    SHSTOCKICONINFO, SIID_APPLICATION, SIID_DESKTOPPC,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::taskbar::scaled;
use crate::util;

/// shell32!RunFileDlg, ordinal 61: (owner, icon, working dir, title, prompt, flags).
type RunFileDlg = unsafe extern "system" fn(HWND, HICON, PCWSTR, PCWSTR, PCWSTR, u32);

const RUN_PROMPT: &str = "Type the name of a program, folder, document, or Internet resource, and Windows will open it for you.";

thread_local! {
    static CBT: Cell<HHOOK> = const { Cell::new(HHOOK(std::ptr::null_mut())) };
    /// Top edge (screen y) of the taskbar, so the dialog can sit just above it.
    static ANCHOR_TOP: Cell<i32> = const { Cell::new(0) };
    /// Cached dark background brush for the themed Run dialog (one per process,
    /// never freed — a single GDI object for the app's lifetime).
    static DARK_BRUSH: Cell<HBRUSH> = const { Cell::new(HBRUSH(std::ptr::null_mut())) };
}

/// Show the shell Run dialog with a proper icon + prompt, positioned bottom-left
/// above the taskbar (whose top edge is `taskbar_top`). Blocks until dismissed:
/// `RunFileDlg` is modal but pumps messages and is owned by a hidden window, so
/// the taskbar and start menu stay usable underneath it.
pub fn show(taskbar_top: i32) {
    unsafe {
        let Ok(shell32) = GetModuleHandleW(w!("shell32.dll")) else {
            return;
        };
        // Ordinal 61 = RunFileDlg (MAKEINTRESOURCEA(61)).
        let Some(proc) = GetProcAddress(shell32, PCSTR(61usize as *const u8)) else {
            return;
        };
        let run_file_dlg: RunFileDlg = std::mem::transmute(proc);

        let owner = create_owner();
        let icon = run_icon();
        ANCHOR_TOP.set(taskbar_top);
        install_cbt();

        let title = util::WideStr::new("Run");
        let prompt = util::WideStr::new(RUN_PROMPT);
        run_file_dlg(owner, icon, PCWSTR::null(), title.pcwstr(), prompt.pcwstr(), 0);

        // The hook normally removes itself on first activation; this covers the
        // case where the dialog never showed.
        remove_cbt();
        if !owner.is_invalid() {
            let _ = DestroyWindow(owner);
        }
    }
}

/// A hidden, ownerless tool window to own the Run dialog (see module docs).
unsafe fn create_owner() -> HWND {
    let hinstance: HINSTANCE = GetModuleHandleW(None).map(Into::into).unwrap_or_default();
    let class = w!("StartPE_RunOwner");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(owner_proc),
        hInstance: hinstance,
        lpszClassName: class,
        ..Default::default()
    };
    RegisterClassW(&wc); // idempotent; returns 0 if already registered
    CreateWindowExW(
        WS_EX_TOOLWINDOW,
        class,
        w!("StartPE Run"),
        WS_POPUP,
        0,
        0,
        0,
        0,
        None,
        None,
        hinstance,
        None,
    )
    .unwrap_or_default()
}

unsafe extern "system" fn owner_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    DefWindowProcW(h, m, w, l)
}

/// A monitor icon for the Run box, like Task Manager's "Create New Task" dialog.
/// Documented and version-stable, unlike a guessed shell32 icon index.
unsafe fn run_icon() -> HICON {
    for siid in [SIID_DESKTOPPC, SIID_APPLICATION] {
        let mut info = SHSTOCKICONINFO {
            cbSize: std::mem::size_of::<SHSTOCKICONINFO>() as u32,
            ..Default::default()
        };
        if SHGetStockIconInfo(siid, SHGSI_ICON, &mut info).is_ok() && !info.hIcon.is_invalid() {
            return info.hIcon;
        }
    }
    HICON::default()
}

fn install_cbt() {
    unsafe {
        if let Ok(h) = SetWindowsHookExW(WH_CBT, Some(cbt_proc), None, GetCurrentThreadId()) {
            CBT.set(h);
        }
    }
}

fn remove_cbt() {
    let h = CBT.replace(HHOOK(std::ptr::null_mut()));
    if !h.is_invalid() {
        unsafe {
            let _ = UnhookWindowsHookEx(h);
        }
    }
}

/// Move the Run dialog (a standard `#32770`) to the bottom-left, just above the
/// taskbar, then unhook (one-shot).
unsafe extern "system" fn cbt_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HCBT_ACTIVATE as i32 {
        let hwnd = HWND(wparam.0 as *mut core::ffi::c_void);
        if class_of(hwnd) == "#32770" {
            reposition(hwnd);
            // Dark-mode the dialog in place (it runs in our process). dark_dialog
            // themes the controls; the subclass paints the dialog's own
            // background/text via WM_CTLCOLOR*. Fails closed to a light dialog.
            if crate::darkmode::dark_dialog(hwnd) {
                let _ = SetWindowSubclass(hwnd, Some(dlg_subclass), 1, 0);
                let _ = RedrawWindow(
                    hwnd,
                    None,
                    HRGN::default(),
                    RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW,
                );
            }
            remove_cbt();
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

unsafe fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 32];
    let n = GetClassNameW(hwnd, &mut buf) as usize;
    String::from_utf16_lossy(&buf[..n])
}

/// The process-lifetime dark background brush, created on first use.
unsafe fn dark_brush() -> HBRUSH {
    let cur = DARK_BRUSH.get();
    if !cur.0.is_null() {
        return cur;
    }
    let b = CreateSolidBrush(COLORREF(crate::taskbar::COL_BG));
    DARK_BRUSH.set(b);
    b
}

/// Subclass on the Run dialog (`#32770`) that paints its background and control
/// text dark. A dialog draws its own surface, so dark theming the controls isn't
/// enough — we must answer the `WM_CTLCOLOR*` queries with dark colors and a dark
/// brush. Removed on `WM_NCDESTROY`.
unsafe extern "system" fn dlg_subclass(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _ref: usize,
) -> LRESULT {
    match msg {
        WM_CTLCOLORDLG | WM_CTLCOLORSTATIC | WM_CTLCOLORBTN | WM_CTLCOLOREDIT
        | WM_CTLCOLORLISTBOX => {
            let hdc = HDC(wp.0 as *mut core::ffi::c_void);
            SetTextColor(hdc, COLORREF(crate::taskbar::COL_TEXT));
            SetBkColor(hdc, COLORREF(crate::taskbar::COL_BG));
            LRESULT(dark_brush().0 as isize)
        }
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(hwnd, Some(dlg_subclass), 1);
            DefSubclassProc(hwnd, msg, wp, lp)
        }
        _ => DefSubclassProc(hwnd, msg, wp, lp),
    }
}

unsafe fn reposition(hwnd: HWND) {
    let mut rc = RECT::default();
    if GetWindowRect(hwnd, &mut rc).is_err() {
        return;
    }
    let h = rc.bottom - rc.top;
    let margin = scaled(12);
    let x = margin;
    let y = (ANCHOR_TOP.get() - h - margin).max(margin);
    let _ = SetWindowPos(
        hwnd,
        HWND::default(),
        x,
        y,
        0,
        0,
        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
    );
}
