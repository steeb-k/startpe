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
pub fn enumerate(stack: &[PathBuf], query: &str) -> Vec<AppItem> {
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
        for root in start_menu_roots() {
            collect_dir(&root, &mut items);
        }
        sort_items(&mut items);
    }

    load_icons(&mut items);
    items
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
