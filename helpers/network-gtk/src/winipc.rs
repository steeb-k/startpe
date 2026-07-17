// SPDX-License-Identifier: GPL-3.0-or-later
//! Command IPC for the pre-warmed network helper.
//!
//! The taskbar drives us by posting the registered `StartPE_ToggleNetworkFlyout`
//! message (WPARAM 0 = toggle the wifi flyout, 1 = open Network Settings). We
//! host a hidden top-level window (class `StartPE_NetworkIPC`, never shown —
//! but a real top-level so `FindWindowW` can locate it) on a dedicated thread
//! with its own message loop, and forward each command to the GTK main thread
//! over an `async_channel`.

use std::cell::{Cell, RefCell};

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
    RegisterWindowMessageW, TranslateMessage, MSG, WINDOW_EX_STYLE, WNDCLASSW, WS_POPUP,
};

/// WPARAM values of the command message (shared contract with `startpe.exe`).
pub const CMD_FLYOUT: usize = 0;
pub const CMD_SETTINGS: usize = 1;

thread_local! {
    static TX: RefCell<Option<async_channel::Sender<usize>>> = const { RefCell::new(None) };
    static CMD_MSG: Cell<u32> = const { Cell::new(0) };
}

/// Spawn the IPC thread. Each `StartPE_ToggleNetworkFlyout` received is
/// forwarded as its WPARAM on `tx` (received on the GTK main thread).
pub fn start(tx: async_channel::Sender<usize>) {
    std::thread::spawn(move || unsafe {
        TX.with(|t| *t.borrow_mut() = Some(tx));
        CMD_MSG.with(|m| m.set(RegisterWindowMessageW(w!("StartPE_ToggleNetworkFlyout"))));

        let Ok(hinst) = GetModuleHandleW(None) else {
            return;
        };
        let class = w!("StartPE_NetworkIPC");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        // Hidden top-level (WS_POPUP, never shown) so FindWindowW can find it.
        let _ = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!(""),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            hinst,
            None,
        );

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let cmd = CMD_MSG.with(|m| m.get());
    if cmd != 0 && msg == cmd {
        TX.with(|t| {
            if let Some(tx) = t.borrow().as_ref() {
                let _ = tx.send_blocking(wp.0);
            }
        });
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

/// From a second instance: post `cmd` to the running instance's IPC window.
/// Returns false if no instance is listening.
pub fn post_to_running(cmd: usize) -> bool {
    unsafe {
        use windows::core::PCWSTR;
        use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW};
        let Ok(ipc) = FindWindowW(w!("StartPE_NetworkIPC"), PCWSTR::null()) else {
            return false;
        };
        if ipc.is_invalid() {
            return false;
        }
        let msg = RegisterWindowMessageW(w!("StartPE_ToggleNetworkFlyout"));
        msg != 0 && PostMessageW(ipc, msg, WPARAM(cmd), LPARAM(0)).is_ok()
    }
}
