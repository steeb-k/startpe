// SPDX-License-Identifier: GPL-3.0-or-later
//! Start-menu program source — a toolkit-agnostic port of the enumeration/launch
//! logic from StartPE's `src/start_menu.rs`: walk the per-machine and per-user
//! Start Menu\Programs folders, with folder drill-down and a recursive search,
//! load each item's shell icon (`SHGetFileInfoW` → `HICON`), and launch via
//! `ShellExecuteW`. The GTK UI turns these into rows; `HICON` → texture conversion
//! lives in the UI layer (`icons.rs`).

use std::path::{Path, PathBuf};

use windows::core::PCWSTR;
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, ShellExecuteW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON,
};
use windows::Win32::UI::WindowsAndMessaging::{HICON, SW_SHOWNORMAL};

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub enum ItemKind {
    Back,
    Folder(PathBuf),
    Launch(PathBuf),
}

pub struct AppItem {
    pub kind: ItemKind,
    pub name: String,
    /// Shell icon handle; the UI converts it to a texture and destroys it.
    pub icon: Option<HICON>,
}

fn start_menu_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for var in ["ProgramData", "APPDATA"] {
        if let Ok(base) = std::env::var(var) {
            let p = Path::new(&base).join("Microsoft\\Windows\\Start Menu\\Programs");
            if p.is_dir() {
                roots.push(p);
            }
        }
    }
    roots
}

fn is_launchable(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()),
        Some(ref e) if ["lnk", "exe", "bat", "cmd", "url"].contains(&e.as_str())
    )
}

fn collect_dir(dir: &Path, items: &mut Vec<AppItem>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if path.is_dir() {
            // Merge duplicate folder names (common + user start menu).
            if !items
                .iter()
                .any(|i| matches!(i.kind, ItemKind::Folder(_)) && i.name.eq_ignore_ascii_case(&name))
            {
                items.push(AppItem {
                    kind: ItemKind::Folder(path),
                    name,
                    icon: None,
                });
            }
        } else if is_launchable(&path) && !name.eq_ignore_ascii_case("desktop") {
            items.push(AppItem {
                kind: ItemKind::Launch(path),
                name,
                icon: None,
            });
        }
    }
}

fn search_walk(dir: &Path, query: &str, depth: u32, out: &mut Vec<AppItem>) {
    if depth > 4 || out.len() >= 60 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        if out.len() >= 60 {
            return;
        }
        let path = entry.path();
        if path.is_dir() {
            search_walk(&path, query, depth + 1, out);
        } else if is_launchable(&path) {
            if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                if name.to_lowercase().contains(query) && !name.eq_ignore_ascii_case("desktop") {
                    out.push(AppItem {
                        kind: ItemKind::Launch(path.clone()),
                        name: name.to_string(),
                        icon: None,
                    });
                }
            }
        }
    }
}

fn sort_items(items: &mut [AppItem]) {
    // Back first, then folders, then alphabetical.
    items.sort_by(|a, b| {
        let rank = |i: &AppItem| match i.kind {
            ItemKind::Back => 0,
            ItemKind::Folder(_) => 1,
            ItemKind::Launch(_) => 2,
        };
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// Build the item list for the current drill-down `stack` and `query`. With a
/// non-empty query it searches recursively; inside a folder it lists that folder
/// (with a Back row); at the root it lists both Start Menu roots merged. Icons are
/// loaded for every file/folder item.
pub fn enumerate(stack: &[PathBuf], query: &str, showing_all: bool) -> Vec<AppItem> {
    let mut items = Vec::new();
    let query = query.trim().to_lowercase();

    if !query.is_empty() {
        for root in start_menu_roots() {
            search_walk(&root, &query, 0, &mut items);
        }
        items.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    } else if let Some(current) = stack.last().cloned() {
        items.push(AppItem {
            kind: ItemKind::Back,
            name: "Back".to_string(),
            icon: None,
        });
        collect_dir(&current, &mut items);
        // The same subfolder may exist in the other root: merge it too.
        if stack.len() == 1 {
            if let Some(tail) = current.file_name() {
                for root in start_menu_roots() {
                    let twin = root.join(tail);
                    if twin != current && twin.is_dir() {
                        collect_dir(&twin, &mut items);
                    }
                }
            }
        }
        sort_items(&mut items);
    } else {
        // Root: the pinned view (PinUtil.ini start-menu pins, in pin order) unless
        // the user switched to "All apps", or there are no pins.
        let pins = if showing_all { Vec::new() } else { start_menu_pins() };
        if pins.is_empty() {
            for root in start_menu_roots() {
                collect_dir(&root, &mut items);
            }
            sort_items(&mut items);
        } else {
            for p in pins {
                let name = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                items.push(AppItem {
                    kind: ItemKind::Launch(p),
                    name,
                    icon: None,
                });
            }
        }
    }

    load_icons(&mut items);
    items
}

/// Whether any start-menu pins are configured (so the menu shows the pinned view
/// and an "All apps" toggle).
pub fn has_pins() -> bool {
    !start_menu_pins().is_empty()
}

/// Start-menu pins from `%Windir%\System32\PinUtil.ini` (`[PinUtil]` `StartMenu<n>`),
/// in pin-position order. Ported from StartPE's `pins.rs`.
fn start_menu_pins() -> Vec<PathBuf> {
    let windir = std::env::var("windir")
        .or_else(|_| std::env::var("SystemRoot"))
        .unwrap_or_else(|_| "X:\\Windows".to_string());
    let ini = format!("{windir}\\System32\\PinUtil.ini");
    let Ok(bytes) = std::fs::read(&ini) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut items: Vec<(u32, PathBuf)> = Vec::new();
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line.eq_ignore_ascii_case("[PinUtil]");
            continue;
        }
        if !in_section || line.is_empty() || line.starts_with(';') || line.starts_with("//") {
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
        if let Some(idx) = key.strip_prefix("startmenu").and_then(|s| s.parse::<u32>().ok()) {
            items.push((idx, PathBuf::from(expand_env(val))));
        }
    }
    items.sort_by_key(|(i, _)| *i);
    items.into_iter().map(|(_, p)| p).collect()
}

fn expand_env(s: &str) -> String {
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
        String::from_utf16_lossy(&buf[..(written as usize).saturating_sub(1)])
    }
}

fn load_icons(items: &mut [AppItem]) {
    unsafe {
        for item in items.iter_mut() {
            let path = match &item.kind {
                ItemKind::Folder(p) | ItemKind::Launch(p) => p,
                ItemKind::Back => continue,
            };
            let wide = to_wide(&path.to_string_lossy());
            let mut sfi = SHFILEINFOW::default();
            let ok = SHGetFileInfoW(
                PCWSTR(wide.as_ptr()),
                Default::default(),
                Some(&mut sfi),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_ICON | SHGFI_LARGEICON,
            );
            if ok != 0 && !sfi.hIcon.is_invalid() {
                item.icon = Some(sfi.hIcon);
            }
        }
    }
}

/// Launch a Start-menu item via the shell.
pub fn launch_path(path: &Path) {
    launch(&path.to_string_lossy(), "");
}

/// `ShellExecute(open)` a command (a bare folder path opens in Explorer).
pub fn launch(cmd: &str, args: &str) {
    unsafe {
        let c = to_wide(cmd);
        let a = to_wide(args);
        let params = if args.is_empty() {
            PCWSTR::null()
        } else {
            PCWSTR(a.as_ptr())
        };
        ShellExecuteW(
            None,
            windows::core::w!("open"),
            PCWSTR(c.as_ptr()),
            params,
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}
