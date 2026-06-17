// SPDX-License-Identifier: GPL-3.0-or-later
//! Toggle IPC for the pre-warmed start menu.
//!
//! The taskbar drives the menu by posting the registered `StartPE_ToggleStartMenu`
//! message. We host a hidden top-level window (class `StartPE_StartMenuIPC`, never
//! shown — but a real top-level so `FindWindowW` can locate it) on a dedicated
//! thread with its own message loop, and forward each toggle to the GTK main
//! thread over an `async_channel`. The main thread then shows/hides the menu.

use std::cell::{Cell, RefCell};

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
    RegisterWindowMessageW, TranslateMessage, MSG, WINDOW_EX_STYLE, WNDCLASSW, WS_POPUP,
};

thread_local! {
    static TX: RefCell<Option<async_channel::Sender<()>>> = const { RefCell::new(None) };
    static TOGGLE_MSG: Cell<u32> = const { Cell::new(0) };
}

/// Spawn the IPC thread. Each `StartPE_ToggleStartMenu` it receives is forwarded
/// as `()` on `tx` (received on the GTK main thread).
pub fn start(tx: async_channel::Sender<()>) {
    std::thread::spawn(move || unsafe {
        TX.with(|t| *t.borrow_mut() = Some(tx));
        TOGGLE_MSG.with(|m| m.set(RegisterWindowMessageW(w!("StartPE_ToggleStartMenu"))));

        let Ok(hinst) = GetModuleHandleW(None) else {
            return;
        };
        let class = w!("StartPE_StartMenuIPC");
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
    let toggle = TOGGLE_MSG.with(|m| m.get());
    if toggle != 0 && msg == toggle {
        TX.with(|t| {
            if let Some(tx) = t.borrow().as_ref() {
                let _ = tx.send_blocking(());
            }
        });
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}
