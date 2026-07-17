// SPDX-License-Identifier: GPL-3.0-or-later
//! Place the wifi flyout above StartPE's taskbar, near the tray (bottom-right).
//!
//! GTK4 can't position its own windows, so once the flyout is shown we move the
//! native HWND: above the taskbar with a small gap, flush right like the
//! Windows 11 network flyout. It's marked `WS_EX_TOOLWINDOW` (out of the
//! taskbar/Alt+Tab; StartPE also excludes it by its "StartPE Network" title)
//! and brought to the front with the topmost-dance (z-order changes don't need
//! foreground rights; the final `SetForegroundWindow` is best-effort).

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowW, GetSystemMetrics, GetWindowLongPtrW, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, SystemParametersInfoW, GWL_EXSTYLE, HWND_NOTOPMOST,
    HWND_TOPMOST, SM_CXSCREEN, SM_CYSCREEN, SPI_GETWORKAREA, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WS_EX_TOOLWINDOW,
};

const GAP: i32 = 8; // gap between the flyout and the taskbar
const MARGIN: i32 = 8;

/// Position the (already-shown) flyout bottom-right above the taskbar and
/// bring it forward.
pub fn place_and_show() {
    unsafe {
        let Some(hwnd) = own_window("StartPE Network") else {
            return;
        };
        // Keep the flyout out of the taskbar / Alt+Tab.
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_TOOLWINDOW.0 as isize);

        let mut wr = RECT::default();
        if GetWindowRect(hwnd, &mut wr).is_err() {
            return;
        }
        let (w, h) = (wr.right - wr.left, wr.bottom - wr.top);
        let bottom = taskbar_top().unwrap_or_else(work_area_bottom);
        let y = (bottom - GAP - h).max(MARGIN);
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let x = (sw - MARGIN - w).max(MARGIN);
        let _ = SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        let _ = SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        let _ = SetForegroundWindow(hwnd);
    }
}

/// Top edge of StartPE's taskbar.
fn taskbar_top() -> Option<i32> {
    unsafe {
        let bar = FindWindowW(w!("StartPE_Taskbar"), PCWSTR::null()).ok()?;
        if bar.is_invalid() {
            return None;
        }
        let mut rc = RECT::default();
        (GetWindowRect(bar, &mut rc).is_ok() && rc.top > 0).then_some(rc.top)
    }
}

fn work_area_bottom() -> i32 {
    unsafe {
        let mut wa = RECT::default();
        let ok = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut wa as *mut _ as *mut std::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok();
        if ok && wa.bottom > 0 {
            wa.bottom
        } else {
            GetSystemMetrics(SM_CYSCREEN)
        }
    }
}

fn own_window(title: &str) -> Option<HWND> {
    unsafe {
        let mut data = (GetCurrentProcessId(), HWND::default(), title);
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut data as *mut _ as isize));
        (!data.1.is_invalid()).then_some(data.1)
    }
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let data = &mut *(lparam.0 as *mut (u32, HWND, &str));
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == data.0 && window_title(hwnd) == data.2 {
        data.1 = hwnd;
        return BOOL(0);
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
