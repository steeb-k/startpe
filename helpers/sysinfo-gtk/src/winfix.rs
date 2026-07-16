// SPDX-License-Identifier: GPL-3.0-or-later
//! Keep a maximized window above StartPE's taskbar.
//!
//! StartPE's taskbar doesn't reserve the work area (SPI_GETWORKAREA reports the
//! full screen under StartPE), so a maximized window would run full-screen and the
//! taskbar would clip its bottom. GTK4 itself respects the work area, so we only
//! need to clamp the maximized size: subclass the native HWND and adjust
//! `WM_GETMINMAXINFO` to the monitor work area minus StartPE's taskbar strip. On a
//! normal desktop (no StartPE bar) this just yields the Explorer work area — the
//! same result GTK would produce — so it is a no-op there.

use adw::prelude::*;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, MINMAXINFO, WM_GETMINMAXINFO,
};

const SUBCLASS_ID: usize = 1;

/// Subclass the window's native HWND (once it maps) to clamp maximize. Calling on
/// every `map` is harmless — `SetWindowSubclass` with the same id just refreshes.
pub fn constrain_maximize(window: &adw::ApplicationWindow) {
    window.connect_map(|_| unsafe {
        if let Some(hwnd) = own_window() {
            let _ = SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0);
            // While we hold the native HWND: swap GTK's default icon for the
            // accent Info glyph (matches the GDI System Information window).
            crate::winicon::apply(hwnd, '\u{E946}');
        }
    });
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _ref: usize,
) -> LRESULT {
    // Let GTK (and DefWindowProc) fill the struct first, then override the max.
    let res = DefSubclassProc(hwnd, msg, wp, lp);
    if msg == WM_GETMINMAXINFO {
        if let Some((monitor, work)) = work_rect(hwnd) {
            let mmi = &mut *(lp.0 as *mut MINMAXINFO);
            mmi.ptMaxPosition.x = work.left - monitor.left;
            mmi.ptMaxPosition.y = work.top - monitor.top;
            mmi.ptMaxSize.x = work.right - work.left;
            mmi.ptMaxSize.y = work.bottom - work.top;
            mmi.ptMaxTrackSize.x = mmi.ptMaxSize.x;
            mmi.ptMaxTrackSize.y = mmi.ptMaxSize.y;
        }
    }
    res
}

/// (monitor rect, work-area rect) for the window's monitor, with the work area
/// clamped to exclude StartPE's taskbar if it sits on this monitor.
unsafe fn work_rect(hwnd: HWND) -> Option<(RECT, RECT)> {
    let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
        return None;
    }
    let mut work = mi.rcWork;
    if let Ok(bar) = FindWindowW(w!("StartPE_Taskbar"), PCWSTR::null()) {
        if !bar.is_invalid() {
            let mut br = RECT::default();
            // Only clamp if the (bottom-docked) taskbar overlaps this monitor.
            if GetWindowRect(bar, &mut br).is_ok()
                && br.right > mi.rcMonitor.left
                && br.left < mi.rcMonitor.right
                && br.top > mi.rcMonitor.top
                && br.top < work.bottom
            {
                work.bottom = br.top;
            }
        }
    }
    Some((mi.rcMonitor, work))
}

/// Find this process's "System Information" window.
unsafe fn own_window() -> Option<HWND> {
    let mut data = (GetCurrentProcessId(), HWND::default());
    let _ = EnumWindows(Some(enum_proc), LPARAM(&mut data as *mut _ as isize));
    (!data.1.is_invalid()).then_some(data.1)
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let data = &mut *(lparam.0 as *mut (u32, HWND));
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == data.0 && window_title(hwnd) == "System Information" {
        data.1 = hwnd;
        return BOOL(0); // found it; stop enumerating
    }
    BOOL(1)
}

unsafe fn window_title(hwnd: HWND) -> String {
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let n = GetWindowTextW(hwnd, &mut buf);
    String::from_utf16_lossy(&buf[..n as usize])
}
