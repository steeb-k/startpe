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
        }
        cfg.taskbar_height = cfg.taskbar_height.clamp(24, 120);
        cfg
    }
}
