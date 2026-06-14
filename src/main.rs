// SPDX-License-Identifier: GPL-3.0-or-later
//! StartPE — a free, open-source taskbar + start menu for Windows PE.
//!
//! Draws its own taskbar and start menu, and hides Explorer's Win11 taskbar.
//! Explorer keeps providing the file manager (folder windows, copy/paste,
//! context menus). When Explorer can't bring up its own desktop — a 24H2/25H2
//! PE whose modern-shell packages are stripped, so its taskbar init fail-fasts
//! and `Progman` is never created — StartPE provides the desktop too (wallpaper
//! + hosted `SHELLDLL_DefView`); see `desktop.rs`. See docs/ARCHITECTURE.md.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod alttab;
mod config;
mod darkmode;
mod desktop;
mod menu;
mod peek;
mod run_window;
mod pins;
mod settings;
mod start_menu;
mod sysinfo;
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

/// Append a version-stamped startup line to `X:\startpe.log` (best-effort).
fn log_startup() {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("X:\\startpe.log")
    {
        let _ = writeln!(
            f,
            "StartPE v{} starting (pid {})",
            env!("CARGO_PKG_VERSION"),
            std::process::id()
        );
    }
}

fn main() -> windows::core::Result<()> {
    // A dedicated System Information invocation: `startpe.exe --sysinfo` just
    // shows the panel and exits. This is the entry point the PE image redirects
    // sysdm.cpl / "This PC → Properties" to, so those open our dark System
    // Information window instead of the legacy applet. It must run *before* the
    // single-instance guard below, since the real StartPE already holds that
    // mutex — this is a separate, short-lived process.
    if std::env::args().skip(1).any(|a| a.eq_ignore_ascii_case("--sysinfo")) {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        sysinfo::run_standalone();
        return Ok(());
    }

    unsafe {
        // Single instance: a second launch (e.g. from a Run key after an
        // Explorer restart) exits quietly.
        let _mutex = CreateMutexW(None, true, w!("StartPE.SingleInstance"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(());
        }

        // Always log the running version at startup so there's a baseline to
        // check against (PE has no Event Viewer); new features should add their
        // own version-stamped logging too.
        log_startup();

        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let cfg = config::Config::load();

        // Put the process into dark app mode *before* any windows exist, so
        // shell menus we raise (the hosted desktop's context menu) theme dark.
        darkmode::init(cfg.dark_menus);

        // If Explorer can't bring up its own desktop (a PE whose modern-shell
        // packages are stripped, so its taskbar init fail-fasts), StartPE
        // provides the desktop itself — wallpaper + the real shell icon view.
        // On a normal box / working PE this detects Explorer's desktop and
        // defers to it. `create_if_needed` already waited for Explorer in auto
        // mode, so only wait again here when Explorer still owns the desktop.
        let own_desktop = desktop::create_if_needed(&cfg);
        if !own_desktop {
            taskbar::wait_for_explorer_shell_ready(60_000);
        }
        taskbar::hide_explorer_taskbar();
        let taskbar = taskbar::Taskbar::create(&cfg)?;
        start_menu::create(&cfg, taskbar.hwnd)?;
        tray::create(taskbar.hwnd)?;
        taskbar::install_win_key_hook();
        alttab::install();

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}
