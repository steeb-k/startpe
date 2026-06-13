// SPDX-License-Identifier: GPL-3.0-or-later
//! Configuration, read from the registry at startup.
//!
//! Values live under `HKCU\Software\StartPE` so a PEBakery script can write
//! them into the mounted Default hive at image-build time, exactly the way
//! the StartAllBack script writes `Software\StartIsBack`. A compatibility
//! layer that also honors existing StartIsBack values is planned (see
//! docs/ARCHITECTURE.md, milestone M4).

use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
use winreg::RegKey;

const KEY: &str = "Software\\StartPE";

#[derive(Clone)]
pub struct Config {
    /// Taskbar height in pixels at 96 DPI.
    pub taskbar_height: i32,
    /// Maximum width of one task button in pixels.
    pub button_max_width: i32,
    /// Start menu popup width in pixels.
    pub menu_width: i32,
    /// Start menu popup height in pixels.
    pub menu_height: i32,
    /// Show window titles on task buttons (default: icon-only).
    pub show_labels: bool,
    /// Combine windows of the same application into one button.
    pub combine: bool,
    /// Center the start button + task buttons as a cluster.
    pub center_taskbar: bool,
    /// Path to a .bmp shown as the user picture on the start menu.
    pub user_picture: Option<String>,
    /// When StartPE provides the desktop itself (wallpaper + real icon view):
    /// 0 = auto (only if Explorer's desktop never appears — e.g. a PE whose
    /// modern-shell packages are stripped), 1 = always, 2 = never.
    pub own_desktop: u32,
    /// Path to a .bmp used as the desktop wallpaper when StartPE owns the
    /// desktop. Falls back to `HKCU\Control Panel\Desktop\WallPaper`, then to
    /// a solid fill of `desktop_color`.
    pub wallpaper: Option<String>,
    /// Solid desktop background COLORREF (0x00BBGGRR) used when no wallpaper
    /// bitmap is available.
    pub desktop_color: u32,
    /// Show the built-in desktop namespace icons (This PC, Home, Network,
    /// Control Panel, Recycle Bin). Default off, so only the user's real
    /// Desktop / Public-Desktop shortcuts appear.
    pub show_system_desktop_icons: bool,
    /// Color of the Start button's four-square glyph (COLORREF 0x00BBGGRR).
    /// Defaults to the same near-white as the taskbar text.
    pub start_button_color: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            taskbar_height: 40,
            button_max_width: 220,
            menu_width: 420,
            menu_height: 480,
            show_labels: false,
            combine: true,
            center_taskbar: true,
            user_picture: None,
            own_desktop: 0,
            wallpaper: None,
            desktop_color: 0x0030_2820,
            show_system_desktop_icons: false,
            start_button_color: 0x00F0_F0F0,
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let mut cfg = Self::default();
        // Read HKLM first: a PEBakery build writes config machine-wide because
        // StartPE runs as SYSTEM in PE and never sees the offline Default-user
        // hive as HKCU. Then overlay HKCU so a per-user install still wins.
        for hive in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
            if let Ok(key) = RegKey::predef(hive).open_subkey(KEY) {
                cfg.apply(&key);
            }
        }
        cfg.taskbar_height = cfg.taskbar_height.clamp(24, 120);
        cfg
    }

    /// Overlay any values present under `key` onto `self` (absent values keep
    /// whatever the lower-priority hive or the default left in place).
    fn apply(&mut self, key: &RegKey) {
        let read = |name: &str, target: &mut i32| {
            if let Ok(v) = key.get_value::<u32, _>(name) {
                *target = v as i32;
            }
        };
        read("TaskbarHeight", &mut self.taskbar_height);
        read("ButtonMaxWidth", &mut self.button_max_width);
        read("MenuWidth", &mut self.menu_width);
        read("MenuHeight", &mut self.menu_height);
        if let Ok(v) = key.get_value::<u32, _>("TaskbarLabels") {
            self.show_labels = v != 0;
        }
        if let Ok(v) = key.get_value::<u32, _>("TaskbarCombine") {
            self.combine = v != 0;
        }
        if let Ok(v) = key.get_value::<u32, _>("CenterTaskbar") {
            self.center_taskbar = v != 0;
        }
        if let Ok(v) = key.get_value::<String, _>("UserPicture") {
            if !v.is_empty() {
                self.user_picture = Some(v);
            }
        }
        if let Ok(v) = key.get_value::<u32, _>("OwnDesktop") {
            self.own_desktop = v;
        }
        if let Ok(v) = key.get_value::<String, _>("Wallpaper") {
            if !v.is_empty() {
                self.wallpaper = Some(v);
            }
        }
        if let Ok(v) = key.get_value::<u32, _>("DesktopColor") {
            self.desktop_color = v;
        }
        if let Ok(v) = key.get_value::<u32, _>("ShowSystemDesktopIcons") {
            self.show_system_desktop_icons = v != 0;
        }
        if let Ok(v) = key.get_value::<u32, _>("StartButtonColor") {
            self.start_button_color = v;
        }
    }
}
