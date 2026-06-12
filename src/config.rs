// SPDX-License-Identifier: GPL-3.0-or-later
//! Configuration, read from the registry at startup.
//!
//! Values live under `HKCU\Software\StartPE` so a PEBakery script can write
//! them into the mounted Default hive at image-build time, exactly the way
//! the StartAllBack script writes `Software\StartIsBack`. A compatibility
//! layer that also honors existing StartIsBack values is planned (see
//! docs/ARCHITECTURE.md, milestone M4).

use winreg::enums::HKEY_CURRENT_USER;
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
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let mut cfg = Self::default();
        if let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(KEY) {
            let read = |name: &str, target: &mut i32| {
                if let Ok(v) = key.get_value::<u32, _>(name) {
                    *target = v as i32;
                }
            };
            read("TaskbarHeight", &mut cfg.taskbar_height);
            read("ButtonMaxWidth", &mut cfg.button_max_width);
            read("MenuWidth", &mut cfg.menu_width);
            read("MenuHeight", &mut cfg.menu_height);
            if let Ok(v) = key.get_value::<u32, _>("TaskbarLabels") {
                cfg.show_labels = v != 0;
            }
            if let Ok(v) = key.get_value::<u32, _>("TaskbarCombine") {
                cfg.combine = v != 0;
            }
            if let Ok(v) = key.get_value::<u32, _>("CenterTaskbar") {
                cfg.center_taskbar = v != 0;
            }
            if let Ok(v) = key.get_value::<String, _>("UserPicture") {
                if !v.is_empty() {
                    cfg.user_picture = Some(v);
                }
            }
            if let Ok(v) = key.get_value::<u32, _>("OwnDesktop") {
                cfg.own_desktop = v;
            }
            if let Ok(v) = key.get_value::<String, _>("Wallpaper") {
                if !v.is_empty() {
                    cfg.wallpaper = Some(v);
                }
            }
            if let Ok(v) = key.get_value::<u32, _>("DesktopColor") {
                cfg.desktop_color = v;
            }
        }
        cfg.taskbar_height = cfg.taskbar_height.clamp(24, 120);
        cfg
    }
}
