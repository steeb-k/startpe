// SPDX-License-Identifier: GPL-3.0-or-later
//! Accent border for real (framed) windows when DWM composition is on.
//!
//! Where the GDI overlay in [`crate::border`] substitutes for a missing DWM
//! frame in a plain PE, this module is used when DWM *is* present (the
//! Administrator-auto-login PE that gets a composited session): it recolors the
//! window's real 1px Win11 border via `DWMWA_BORDER_COLOR` — accent on the
//! foreground window, gray on windows that lose focus. No overlay window, no
//! drawing; just the documented DWM attribute, which keeps the rounded corners
//! and exact frame Windows already draws.
//!
//! Only real application windows are touched. StartPE's own windows are
//! borderless `WS_POPUP`s (no native frame for `DWMWA_BORDER_COLOR` to color) and
//! draw their own 1px ring instead, so they are excluded here by window class.

use std::cell::RefCell;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{
    DwmIsCompositionEnabled, DwmSetWindowAttribute, DWMWA_BORDER_COLOR,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::taskbar::start_button_color;

/// Border color for windows that don't have focus (a neutral gray).
const GRAY: u32 = 0x0055_5555;

/// Whether DWM desktop composition is on (the PE booted with an interactive
/// logon, so windows are composited and have a real frame to recolor). Decides
/// between this module and the GDI overlay in [`crate::border`].
pub fn composition_on() -> bool {
    unsafe {
        DwmIsCompositionEnabled()
            .map(|b| b.as_bool())
            .unwrap_or(false)
    }
}

struct State {
    hooks: Vec<HWINEVENTHOOK>,
    /// The window currently set to the accent color (so we can gray it when
    /// focus moves elsewhere).
    accented: HWND,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Set a window's DWM border color (a `COLORREF`, 0x00BBGGRR).
fn set_color(hwnd: HWND, color: u32) {
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &color as *const u32 as *const core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        );
    }
}

fn class_of(hwnd: HWND) -> String {
    unsafe {
        let mut buf = [0u16; 64];
        let n = GetClassNameW(hwnd, &mut buf) as usize;
        String::from_utf16_lossy(&buf[..n])
    }
}

/// True if `hwnd` is a real application window we should color — visible,
/// non-tool, not one of StartPE's own windows (those are borderless and ring
/// themselves) and not the shell's desktop/tray.
fn borderable(hwnd: HWND) -> bool {
    unsafe {
        if hwnd.is_invalid() || !IsWindowVisible(hwnd).as_bool() {
            return false;
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }
        let mut pid = 0u32;
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == GetCurrentProcessId() {
            return false; // the main StartPE process's own windows
        }
        let class = class_of(hwnd);
        // StartPE's separate-process windows (Run, SysInfo, …) plus the shell's
        // desktop/tray are not real app windows to frame.
        !class.starts_with("StartPE")
            && !matches!(
                class.as_str(),
                "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
            )
    }
}

/// Color the new foreground window accent and gray the one that lost it.
fn on_foreground(new: HWND) {
    let accent = start_button_color();
    STATE.with_borrow_mut(|s| {
        let Some(s) = s.as_mut() else {
            return;
        };
        if borderable(new) {
            set_color(new, accent);
            if !s.accented.is_invalid() && s.accented != new {
                set_color(s.accented, GRAY);
            }
            s.accented = new;
        } else {
            // Focus went to a StartPE window / the desktop: gray the last one.
            if !s.accented.is_invalid() {
                set_color(s.accented, GRAY);
            }
            s.accented = HWND::default();
        }
    });
}

unsafe extern "system" fn winevent_hook(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object != OBJID_WINDOW.0 || id_child != 0 {
        return;
    }
    match event {
        EVENT_SYSTEM_FOREGROUND | EVENT_SYSTEM_MINIMIZEEND => on_foreground(hwnd),
        EVENT_OBJECT_SHOW => {
            // A newly shown background window starts gray; the active one is
            // handled by the foreground event.
            if borderable(hwnd) && hwnd != GetForegroundWindow() {
                set_color(hwnd, GRAY);
            }
        }
        _ => {}
    }
}

/// Install the foreground tracking if `enabled`. Called once at startup.
pub fn install(enabled: bool) {
    if enabled {
        ensure_installed();
    }
}

/// Turn the feature on/off at runtime (settings pane via `reload_config`).
pub fn set_enabled(enabled: bool) {
    let installed = STATE.with_borrow(|s| s.is_some());
    if enabled && !installed {
        ensure_installed();
    } else if !enabled && installed {
        teardown();
    } else if enabled {
        refresh();
    }
}

/// Re-color the current foreground window (e.g. after the accent color changed).
pub fn refresh() {
    if STATE.with_borrow(|s| s.is_some()) {
        on_foreground(unsafe { GetForegroundWindow() });
    }
}

fn ensure_installed() {
    if STATE.with_borrow(|s| s.is_some()) {
        return;
    }
    unsafe {
        let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
        let mut hooks = Vec::new();
        for (lo, hi) in [
            (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND),
            (EVENT_OBJECT_SHOW, EVENT_OBJECT_SHOW),
        ] {
            let h = SetWinEventHook(lo, hi, None, Some(winevent_hook), 0, 0, flags);
            if !h.is_invalid() {
                hooks.push(h);
            }
        }
        STATE.with_borrow_mut(|s| {
            *s = Some(State {
                hooks,
                accented: HWND::default(),
            });
        });
    }
    // Color whatever is already focused.
    on_foreground(unsafe { GetForegroundWindow() });
}

fn teardown() {
    if let Some(s) = STATE.with_borrow_mut(|s| s.take()) {
        unsafe {
            for h in s.hooks {
                let _ = UnhookWinEvent(h);
            }
        }
    }
}
