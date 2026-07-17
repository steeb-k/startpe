// SPDX-License-Identifier: GPL-3.0-or-later
//! Taskman-window registration (`SetTaskmanWindow`) — the one undocumented
//! piece of the minimize-into-button story, confined to this module the same
//! way `tray.rs` confines the WM_COPYDATA wire format and `darkmode.rs` the
//! uxtheme ordinals.
//!
//! `RegisterShellHookWindow` alone delivers HSHELL_* *notifications*, but the
//! HSHELL_GETMINRECT *query* ("where should this minimize animate to?") is
//! only answered from the registered task-manager window, which Explorer
//! claims at logon — so without this, the taskbar's GETMINRECT reply is never
//! consulted and minimizes fall back to the iconic parking slots at the
//! bottom-left. Every alternative shell that animates minimize-into-button
//! (ManagedShell/RetroBar, Cairo, bbLean) claims the slot via
//! `SetTaskmanWindow`, exported by name from user32 since NT4 — de-facto
//! stable. Resolved dynamically and fail-soft: if the export ever disappears,
//! minimize just falls back to the default animation and nothing else is
//! affected. The previous holder (Explorer's tray) is restored on clean exit.

use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::{s, w};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::UI::WindowsAndMessaging::IsWindow;

/// The taskman window that was registered before we claimed the slot.
static PREV: AtomicIsize = AtomicIsize::new(0);

type SetTaskmanWindowFn = unsafe extern "system" fn(HWND) -> i32;
type GetTaskmanWindowFn = unsafe extern "system" fn() -> HWND;

fn funcs() -> Option<(SetTaskmanWindowFn, GetTaskmanWindowFn)> {
    unsafe {
        let user32 = GetModuleHandleW(w!("user32.dll")).ok()?;
        let set = GetProcAddress(user32, s!("SetTaskmanWindow"))?;
        let get = GetProcAddress(user32, s!("GetTaskmanWindow"))?;
        Some((
            std::mem::transmute::<_, SetTaskmanWindowFn>(set),
            std::mem::transmute::<_, GetTaskmanWindowFn>(get),
        ))
    }
}

/// Claim the taskman slot for `hwnd`, remembering the previous holder.
pub fn claim(hwnd: HWND) {
    unsafe {
        if let Some((set, get)) = funcs() {
            PREV.store(get().0 as isize, Ordering::Relaxed);
            let _ = set(hwnd);
        }
    }
}

/// Give the slot back to whoever held it (Explorer's tray), if still alive.
pub fn release() {
    unsafe {
        if let Some((set, _)) = funcs() {
            let prev = HWND(PREV.swap(0, Ordering::Relaxed) as *mut _);
            if !prev.is_invalid() && IsWindow(prev).as_bool() {
                let _ = set(prev);
            }
        }
    }
}
