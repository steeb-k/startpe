// SPDX-License-Identifier: GPL-3.0-or-later
//! Command execution + history for the Run window — a toolkit-agnostic port of
//! the logic in StartPE's `src/run_window.rs`: expand env vars, split the program
//! from its args, and `ShellExecute` it (which resolves bare names via PATH/App
//! Paths, URLs and documents the way the classic Run box does). History persists
//! in `HKCU\Software\StartPE\RunHistory` (newline-joined, oldest first), matching
//! StartPE's `config::{load,save}_run_history` so the two share one store.

use windows::core::{w, PCWSTR};
use windows::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

/// Most history entries to keep (newest wins; PE wipes the store each reboot).
const HISTORY_MAX: usize = 30;
const KEY: &str = "Software\\StartPE";
const RUN_HISTORY: &str = "RunHistory";

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Run history (oldest first) from `HKCU\Software\StartPE\RunHistory`.
pub fn load_history() -> Vec<String> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(KEY)
        .ok()
        .and_then(|k| k.get_value::<String, _>(RUN_HISTORY).ok())
        .map(|s| {
            s.split('\n')
                .filter(|e| !e.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn save_history(items: &[String]) {
    if let Ok((k, _)) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(KEY) {
        let _ = k.set_value(RUN_HISTORY, &items.join("\n"));
    }
}

/// Record `cmd` as the most-recent entry (de-duplicated, capped at `HISTORY_MAX`).
pub fn record(cmd: &str) {
    let mut h = load_history();
    h.retain(|e| e != cmd);
    h.push(cmd.to_string());
    if h.len() > HISTORY_MAX {
        let drop = h.len() - HISTORY_MAX;
        h.drain(0..drop);
    }
    save_history(&h);
}

/// Run a command the way the Run box does: expand env vars, split program from
/// args, and `ShellExecute` it. Returns whether the launch succeeded.
pub fn execute(raw: &str) -> bool {
    unsafe {
        let expanded = expand_env(raw);
        let (program, args) = split_command(&expanded);
        let p = to_wide(&program);
        let a = to_wide(&args);
        let params = if args.is_empty() {
            PCWSTR::null()
        } else {
            PCWSTR(a.as_ptr())
        };
        let r = ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(p.as_ptr()),
            params,
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        r.0 as usize > 32
    }
}

unsafe fn expand_env(s: &str) -> String {
    let src = to_wide(s);
    let n = ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), None);
    if n == 0 {
        return s.to_string();
    }
    let mut buf = vec![0u16; n as usize];
    let n2 = ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), Some(&mut buf));
    if n2 == 0 {
        return s.to_string();
    }
    String::from_utf16_lossy(&buf[..(n2 as usize).saturating_sub(1)])
}

/// Split an entered command into (program, args). If the whole string is an
/// existing path it runs as-is (handles paths with spaces and no args); a quoted
/// program is honored; otherwise it splits on the first whitespace (the shell
/// resolves bare names like `notepad` via PATH/App Paths).
fn split_command(s: &str) -> (String, String) {
    let s = s.trim();
    if path_exists(s) {
        return (s.to_string(), String::new());
    }
    if let Some(rest) = s.strip_prefix('"') {
        if let Some(end) = rest.find('"') {
            return (
                rest[..end].to_string(),
                rest[end + 1..].trim_start().to_string(),
            );
        }
    }
    match s.find(char::is_whitespace) {
        Some(i) => (s[..i].to_string(), s[i + 1..].trim_start().to_string()),
        None => (s.to_string(), String::new()),
    }
}

fn path_exists(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let w = to_wide(s);
    unsafe { GetFileAttributesW(PCWSTR(w.as_ptr())) != INVALID_FILE_ATTRIBUTES }
}
