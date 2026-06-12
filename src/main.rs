// SPDX-License-Identifier: GPL-3.0-or-later
//! StartPE — a free, open-source taskbar + start menu for Windows PE.
//!
//! Runs alongside Explorer-as-shell: Explorer keeps providing the desktop and
//! file management while this process hides Explorer's Win11 taskbar and
//! draws its own taskbar and start menu. See docs/ARCHITECTURE.md.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod peek;
mod start_menu;
mod taskbar;
mod tray;
mod util;

use windows::core::w;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, TranslateMessage, MSG,
};

fn main() -> windows::core::Result<()> {
    unsafe {
        // Single instance: a second launch (e.g. from a Run key after an
        // Explorer restart) exits quietly.
        let _mutex = CreateMutexW(None, true, w!("StartPE.SingleInstance"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(());
        }

        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let cfg = config::Config::load();
        taskbar::wait_for_explorer_shell_ready(60_000);
        taskbar::hide_explorer_taskbar();
        let taskbar = taskbar::Taskbar::create(&cfg)?;
        start_menu::create(&cfg, taskbar.hwnd)?;
        tray::create(taskbar.hwnd)?;
        taskbar::install_win_key_hook();

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}
