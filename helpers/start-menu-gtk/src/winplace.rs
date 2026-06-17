// SPDX-License-Identifier: GPL-3.0-or-later
//! Place the menu above StartPE's taskbar and bring it to the front.
//!
//! GTK4 can't position its own windows, so once the menu is shown we move the
//! native HWND: above the taskbar with a small gap, centered when the taskbar is
//! centered (Win11 style) else flush bottom-left (Win10/SAB style). It's marked
//! `WS_EX_TOOLWINDOW` (out of the taskbar/Alt+Tab) and brought to the front with
//! StartPE's topmost-dance (z-order changes don't need foreground rights; the
//! final `SetForegroundWindow` is best-effort).

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowW, GetSystemMetrics, GetWindowLongPtrW, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, SystemParametersInfoW, GWL_EXSTYLE, HWND_NOTOPMOST,
    HWND_TOPMOST, SM_CXSCREEN, SM_CYSCREEN, SPI_GETWORKAREA, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WS_EX_TOOLWINDOW,
};

const GAP: i32 = 8; // gap between the menu and the taskbar
const MARGIN: i32 = 8;

/// Position the (already-shown) menu above the taskbar and bring it forward.
pub fn place_and_show() {
    unsafe {
        let Some(hwnd) = own_window() else {
            return;
        };
        // Keep the menu out of the taskbar / Alt+Tab.
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
        let x = if centered() {
            ((sw - w) / 2).clamp(MARGIN, (sw - w - MARGIN).max(MARGIN))
        } else {
            MARGIN
        };
        let _ = SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        let _ = SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        let _ = SetForegroundWindow(hwnd);
    }
}

/// Top edge of StartPE's taskbar (it doesn't reserve work area, so we find the bar).
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

/// Whether the taskbar is centered (Win11 style) — `CenterTaskbar` in the registry
/// (HKLM then HKCU), defaulting to centered, matching StartPE's default.
fn centered() -> bool {
    let mut v = 1u32;
    for hive in [
        winreg::enums::HKEY_LOCAL_MACHINE,
        winreg::enums::HKEY_CURRENT_USER,
    ] {
        if let Ok(k) = winreg::RegKey::predef(hive).open_subkey("Software\\StartPE") {
            if let Ok(x) = k.get_value::<u32, _>("CenterTaskbar") {
                v = x;
            }
        }
    }
    v != 0
}

fn own_window() -> Option<HWND> {
    unsafe {
        let mut data = (GetCurrentProcessId(), HWND::default());
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut data as *mut _ as isize));
        (!data.1.is_invalid()).then_some(data.1)
    }
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let data = &mut *(lparam.0 as *mut (u32, HWND));
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == data.0 && window_title(hwnd) == "StartPE Menu" {
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
