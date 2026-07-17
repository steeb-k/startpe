// SPDX-License-Identifier: GPL-3.0-or-later
//! Read/write StartPE's registry settings and signal the live taskbar.
//!
//! Mirrors `startpe/src/config.rs`: values are read `HKLM` first then overlaid by
//! `HKCU` (the PE shell runs as SYSTEM and writes machine-wide; the running shell
//! reads HKCU too). Writes go to `HKCU\Software\StartPE` like the in-process pane.
//! After each write we post the registered `StartPE_ReloadConfig` message to the
//! taskbar window so it re-reads config and applies the change live — the
//! cross-process equivalent of the in-process pane's `taskbar::reload_config()`.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, RegisterWindowMessageW};
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
use winreg::RegKey;

const KEY: &str = "Software\\StartPE";

/// Current values of the settings exposed by the pane. Defaults match
/// `config::Config::default` in StartPE.
pub struct Settings {
    pub show_labels: bool,
    pub combine: bool,
    pub center_taskbar: bool,
    pub show_network_icon: bool,
    pub window_borders: bool,
    pub dark_menus: bool,
    pub start_color: u32, // COLORREF 0x00BBGGRR
}

fn read_u32(name: &str, default: u32) -> u32 {
    let mut v = default;
    for hive in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        if let Ok(k) = RegKey::predef(hive).open_subkey(KEY) {
            if let Ok(x) = k.get_value::<u32, _>(name) {
                v = x;
            }
        }
    }
    v
}

pub fn load() -> Settings {
    Settings {
        show_labels: read_u32("TaskbarLabels", 0) != 0,
        combine: read_u32("TaskbarCombine", 1) != 0,
        center_taskbar: read_u32("CenterTaskbar", 1) != 0,
        show_network_icon: read_u32("ShowNetworkIcon", 1) != 0,
        window_borders: read_u32("WindowBorders", 1) != 0,
        dark_menus: read_u32("DarkMenus", 1) != 0,
        start_color: read_u32("StartButtonColor", 0x00E6_5AB4),
    }
}

/// Persist a `REG_DWORD` to `HKCU\Software\StartPE` and ask the taskbar to apply
/// it live.
pub fn save_u32(name: &str, value: u32) {
    if let Ok((k, _)) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(KEY) {
        let _ = k.set_value(name, &value);
    }
    notify_taskbar();
}

pub fn save_bool(name: &str, value: bool) {
    save_u32(name, value as u32);
}

/// Post `StartPE_ReloadConfig` to the running taskbar so it re-reads config and
/// applies the change live. No-op if StartPE isn't running.
fn notify_taskbar() {
    unsafe {
        let msg = RegisterWindowMessageW(w!("StartPE_ReloadConfig"));
        if msg == 0 {
            return;
        }
        if let Ok(hwnd) = FindWindowW(w!("StartPE_Taskbar"), PCWSTR::null()) {
            if !hwnd.is_invalid() {
                let _ = PostMessageW(hwnd, msg, WPARAM(0), LPARAM(0));
            }
        }
    }
}
