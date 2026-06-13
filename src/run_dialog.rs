// SPDX-License-Identifier: GPL-3.0-or-later
//! The shell Run dialog, invoked properly.
//!
//! `rundll32 shell32.dll,#61` (the usual way to pop the Run box) calls the
//! shell's `RunFileDlg` with *no* arguments, so it shows the wrong prompt
//! ("RunDLL"), no icon, and a default position. We instead call ordinal 61
//! directly with a real icon and the standard prompt, and nudge the dialog to
//! the bottom-left above the taskbar with a one-shot thread-local `WH_CBT` hook.
//!
//! `RunFileDlg` (shell32 ordinal 61) is undocumented — a confined exception like
//! `darkmode.rs`. The icon (`SHGetStockIconInfo`) and positioning (`WH_CBT`) are
//! documented. The dialog is shell-rendered and stays light by design.

use std::cell::Cell;

use windows::core::{w, PCSTR, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::{SHGetStockIconInfo, SHGSI_ICON, SHSTOCKICONINFO, SIID_APPLICATION};
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
}

/// Show the shell Run dialog with a proper icon + prompt, positioned bottom-left
/// above the taskbar (whose top edge is `taskbar_top`). Blocks until dismissed:
/// `RunFileDlg` is modal but pumps messages, so the taskbar stays responsive.
pub fn show(owner: HWND, taskbar_top: i32) {
    unsafe {
        let Ok(shell32) = GetModuleHandleW(w!("shell32.dll")) else {
            return;
        };
        // Ordinal 61 = RunFileDlg (MAKEINTRESOURCEA(61)).
        let Some(proc) = GetProcAddress(shell32, PCSTR(61usize as *const u8)) else {
            return;
        };
        let run_file_dlg: RunFileDlg = std::mem::transmute(proc);

        let icon = stock_app_icon();
        ANCHOR_TOP.set(taskbar_top);
        install_cbt();

        let title = util::WideStr::new("Run");
        let prompt = util::WideStr::new(RUN_PROMPT);
        run_file_dlg(owner, icon, PCWSTR::null(), title.pcwstr(), prompt.pcwstr(), 0);

        // The hook normally removes itself on first activation; this covers the
        // case where the dialog never showed.
        remove_cbt();
    }
}

/// The standard application stock icon — a documented, version-stable icon for
/// the Run box (the shell's own Run icon isn't exposed by a documented API).
unsafe fn stock_app_icon() -> HICON {
    let mut info = SHSTOCKICONINFO {
        cbSize: std::mem::size_of::<SHSTOCKICONINFO>() as u32,
        ..Default::default()
    };
    match SHGetStockIconInfo(SIID_APPLICATION, SHGSI_ICON, &mut info) {
        Ok(()) => info.hIcon,
        Err(_) => HICON::default(),
    }
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
