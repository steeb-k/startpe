// SPDX-License-Identifier: GPL-3.0-or-later
//! Reader for the winrx-creator / PhoenixPE `PinUtil.ini` pin staging file.
//!
//! Apps declare pins with the PEBakery `PinShortcut,Taskbar|StartMenu,…` macro,
//! which records them in `%Windir%\System32\PinUtil.ini` under `[PinUtil]` as
//! `Taskbar<n>=<exe>` / `StartMenu<n>=<exe>` (n = 0..99, position = order). At
//! boot `PinUtil.exe` applies them to Explorer's taskbar — which StartPE hides —
//! so StartPE reads the same file directly to render its own pinned items.

/// Pinned program paths, in pin-position order (lowest index first).
pub struct Pins {
    pub taskbar: Vec<String>,
    pub start_menu: Vec<String>,
}

impl Pins {
    pub fn load() -> Pins {
        let mut taskbar: Vec<(u32, String)> = Vec::new();
        let mut start_menu: Vec<(u32, String)> = Vec::new();

        if let Ok(bytes) = std::fs::read(ini_path()) {
            let text = String::from_utf8_lossy(&bytes);
            let mut in_section = false;
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('[') {
                    in_section = line.eq_ignore_ascii_case("[PinUtil]");
                    continue;
                }
                if !in_section || line.is_empty() || line.starts_with(';') || line.starts_with("//")
                {
                    continue;
                }
                let Some((key, val)) = line.split_once('=') else {
                    continue;
                };
                let val = val.trim();
                if val.is_empty() {
                    continue;
                }
                let key = key.trim().to_ascii_lowercase();
                if let Some(idx) = key.strip_prefix("taskbar").and_then(|s| s.parse::<u32>().ok()) {
                    taskbar.push((idx, expand_env(val)));
                } else if let Some(idx) =
                    key.strip_prefix("startmenu").and_then(|s| s.parse::<u32>().ok())
                {
                    start_menu.push((idx, expand_env(val)));
                }
            }
        }

        taskbar.sort_by_key(|(i, _)| *i);
        start_menu.sort_by_key(|(i, _)| *i);
        Pins {
            taskbar: taskbar.into_iter().map(|(_, v)| v).collect(),
            start_menu: start_menu.into_iter().map(|(_, v)| v).collect(),
        }
    }
}

/// Expand environment variables in a pin path (e.g. `%windir%\Explorer.exe`).
/// PEBakery writes some pins with unexpanded vars.
fn expand_env(s: &str) -> String {
    use windows::core::PCWSTR;
    use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
    let src: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let needed = ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), None);
        if needed == 0 {
            return s.to_string();
        }
        let mut buf = vec![0u16; needed as usize];
        let written = ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), Some(&mut buf));
        if written == 0 {
            return s.to_string();
        }
        // The count includes the NUL terminator.
        String::from_utf16_lossy(&buf[..(written as usize).saturating_sub(1)])
    }
}

fn ini_path() -> String {
    let windir = std::env::var("windir")
        .or_else(|_| std::env::var("SystemRoot"))
        .unwrap_or_else(|_| "X:\\Windows".to_string());
    format!("{windir}\\System32\\PinUtil.ini")
}
