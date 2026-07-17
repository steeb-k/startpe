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
mod border;
mod config;
mod darkmode;
mod desktop;
mod dwm_border;
mod menu;
mod network;
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
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::Security::{
    GetTokenInformation, IsWellKnownSid, TokenUser, WinLocalSystemSid, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, OpenProcessToken};
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

/// Append a version-stamped line to `X:\startpe.log` (best-effort).
fn log(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("X:\\startpe.log")
    {
        let _ = writeln!(f, "StartPE v{} {}", env!("CARGO_PKG_VERSION"), msg);
    }
}

/// True if this process's token is the Local System account (S-1-5-18).
fn is_system() -> bool {
    unsafe {
        let mut tok = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok).is_err() {
            return false;
        }
        // First call sizes the buffer, second fills it.
        let mut len = 0u32;
        let _ = GetTokenInformation(tok, TokenUser, None, 0, &mut len);
        let mut buf = vec![0u8; len as usize];
        let ok = len > 0
            && GetTokenInformation(
                tok,
                TokenUser,
                Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
                len,
                &mut len,
            )
            .is_ok();
        let result = ok && {
            let tu = &*(buf.as_ptr() as *const TOKEN_USER);
            IsWellKnownSid(tu.User.Sid, WinLocalSystemSid).as_bool()
        };
        let _ = CloseHandle(tok);
        result
    }
}

/// Re-launch this exe as SYSTEM via the sibling `syslaunch.exe`. Returns true if
/// the hand-off was dispatched (the caller should then exit so the SYSTEM
/// instance takes the single-instance slot).
fn relaunch_as_system() -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let Some(syslaunch) = exe.parent().map(|d| d.join("syslaunch.exe")) else {
        return false;
    };
    if !syslaunch.exists() {
        log("launch-as-system: syslaunch.exe not found next to startpe.exe");
        return false;
    }
    match std::process::Command::new(&syslaunch)
        .arg(&exe)
        .arg("--from-syslaunch")
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(_) => {
            log("launch-as-system: handed off to SYSTEM via syslaunch");
            true
        }
        Err(e) => {
            log(&format!("launch-as-system: failed to spawn syslaunch: {e}"));
            false
        }
    }
}

fn main() -> windows::core::Result<()> {
    // A dedicated System Information invocation: `startpe.exe --sysinfo` just
    // shows the panel and exits. This is the entry point the PE image redirects
    // sysdm.cpl / "This PC → Properties" to, so those open our dark System
    // Information window instead of the legacy applet. It must run *before* the
    // single-instance guard below, since the real StartPE already holds that
    // mutex — this is a separate, short-lived process.
    // `--sysinfo` / `--run` run one StartPE applet as its own short-lived process
    // and exit. Routing them through separate processes (rather than hosting the
    // windows inside the main taskbar process) means the shell treats them as
    // ordinary apps: they list in the taskbar / Alt+Tab, stack in normal Z order,
    // and pick up the accent window border like any other window. Must run
    // *before* the single-instance guard below, since the real StartPE already
    // holds that mutex — these are separate, short-lived processes.
    let arg = |name: &str| std::env::args().skip(1).any(|a| a.eq_ignore_ascii_case(name));
    if arg("--sysinfo") || arg("--run") {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        if arg("--run") {
            run_window::run_standalone();
        } else {
            sysinfo::run_standalone();
        }
        return Ok(());
    }

    unsafe {
        // Single instance: a second launch (e.g. from a Run key after an
        // Explorer restart) exits quietly.
        let _mutex = CreateMutexW(None, true, w!("StartPE.SingleInstance"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(());
        }

        let cfg = config::Config::load();

        // SYSTEM hand-off (PE with Administrator auto-login): the admin logon is
        // what makes winlogon spawn dwm.exe and composite the session, but the
        // shell + recovery tools must run as SYSTEM. If we came up under a lesser
        // token, re-launch via syslaunch as SYSTEM and exit. We hold the
        // single-instance mutex here, so returning releases it and lets the
        // SYSTEM instance take the slot — this beats the launch-vector race
        // (Run key / loader can start an Administrator instance before the
        // intended SYSTEM one). `--from-syslaunch` marks the re-launched instance
        // so it never loops even if elevation didn't take. See syslaunch/.
        if cfg.launch_as_system && !arg("--from-syslaunch") && !is_system() {
            if relaunch_as_system() {
                return Ok(());
            }
            log("launch-as-system: hand-off unavailable; running with the current token");
        }

        // Always log the running version at startup so there's a baseline to
        // check against (PE has no Event Viewer); new features should add their
        // own version-stamped logging too.
        log_startup();

        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

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
        // Pre-warm the GTK start-menu helper (if a sibling StartMenu.exe is
        // present); `start_menu::toggle()` then drives it, with the GDI menu above
        // as the fallback.
        start_menu::launch_helper();
        // Pre-warm the GTK network helper too (wifi flyout + Network Settings);
        // it applies a dropped network-profile.ini on this first launch.
        network::launch_helper();
        tray::create(taskbar.hwnd)?;
        taskbar::install_win_key_hook();
        alttab::install();
        // Real (framed) windows: recolor their native border via DWM when it's
        // present (accent when focused, gray otherwise); fall back to the GDI
        // accent overlay only in a plain PE with no composition.
        if dwm_border::composition_on() {
            dwm_border::install(cfg.window_borders);
        } else {
            border::install(cfg.window_borders);
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            // Keyboard first goes to the hosted desktop view when it has focus
            // (Delete, F2, Ctrl+C/X/V… — the IShellBrowser-host contract).
            if desktop::translate_accelerator(&msg) {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}
