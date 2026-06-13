// SPDX-License-Identifier: GPL-3.0-or-later
//! The start menu: a two-pane popup styled after the classic Win7/SAB layout.
//!
//! Left pane: app list from the Start Menu folders (`%ProgramData%` and
//! `%APPDATA%\Microsoft\Windows\Start Menu\Programs`) with folder drill-down,
//! an "All Programs" row, and a live search box. Right pane: user picture
//! (circular, protruding above the menu — done with a window region, no DWM
//! required) and a column of system links. Footer: search + Shut down.
//!
//! Keyboard: the search box is focused on open (caret in it), and typing always
//! feeds the search. Arrow keys move a shared focus highlight (`hover`, reused by
//! mouse and keyboard) across the program list, the right-pane links, the search
//! box, and the power controls; Enter activates the focused item, and Right on a
//! ">" folder row expands it. Spatially: from the search box Right → Shut down →
//! its flyout chevron. See `navigate` / `resolve` / `perform`.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VK_BACK, VK_DOWN, VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT, VK_UP,
};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::Config;
use crate::taskbar::{
    make_font, make_font_face, scaled, COL_ACCENT, COL_BG, COL_HOVER, COL_TEXT, COL_TEXT_DIM,
};
use crate::util;

// Geometry (unscaled px).
const ROW: i32 = 34; // left pane row height
const RIGHT_ROW: i32 = 38; // right pane row height
const AVATAR: i32 = 64; // user picture diameter
const OVERHANG: i32 = AVATAR / 2; // how far the avatar pokes above the body
const RADIUS: i32 = 12; // corner radius
const GAP: i32 = 8; // gap between menu and taskbar
const FOOTER_H: i32 = 48;
const ALLPROG_H: i32 = 34;
const RIGHT_W: i32 = 170; // right pane width

// Extra colors for the menu (COLORREF, 0x00BBGGRR).
const COL_FOOTER: u32 = 0x00191818;
const COL_SEP: u32 = 0x00343232;
const COL_SEARCH_BG: u32 = 0x00121111;
const COL_AVATAR_BG: u32 = 0x003F3D3C;
const COL_RING: u32 = 0x00121111;

// Segoe MDL2 Assets glyphs.
const GLYPH_PERSON: &str = "\u{E77B}";
const GLYPH_DOWNLOAD: &str = "\u{E896}";
const GLYPH_PC: &str = "\u{E977}";
const GLYPH_CONTROL: &str = "\u{E713}";
const GLYPH_CMD: &str = "\u{E756}";
const GLYPH_RUN: &str = "\u{E7AC}";
const GLYPH_POWER: &str = "\u{E7E8}";
const GLYPH_SEARCH: &str = "\u{E721}";
const GLYPH_CHEVRON: &str = "\u{E76C}";
const GLYPH_CHEVRON_LEFT: &str = "\u{E76B}";

enum ItemKind {
    Back,
    Folder(PathBuf),
    Launch(PathBuf),
}

struct Item {
    kind: ItemKind,
    name: String,
    icon: Option<HICON>,
}

/// Sentinel `RightItem::cmd` that opens the shell Run dialog instead of being
/// `ShellExecute`'d.
const RUN_DIALOG_CMD: &str = "@startpe:run-dialog";

/// Right-pane link. Launched as `ShellExecute(cmd, args)`; a bare folder
/// path as `cmd` opens in Explorer.
struct RightItem {
    glyph: &'static str,
    label: String,
    cmd: String,
    args: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    None,
    Row(usize),
    Right(usize),
    AllPrograms,
    Shutdown,
    ShutdownMenu,
    /// The search box (focused on open; typing always feeds it).
    Search,
}

struct MenuState {
    hwnd: HWND,
    taskbar: HWND,
    width: i32,
    height: i32, // total, including avatar overhang
    items: Vec<Item>,
    rights: Vec<RightItem>,
    /// Start-menu pinned program paths (from PinUtil.ini), in pin order. When
    /// non-empty the menu opens to these instead of the full app list.
    pinned: Vec<PathBuf>,
    /// True once the user switched from the pinned view to the full app list.
    showing_all: bool,
    /// Folder navigation stack; empty means the merged top level.
    stack: Vec<PathBuf>,
    /// Live search query; non-empty switches the left pane to results.
    query: String,
    scroll: i32,
    hover: Hit,
    font: HFONT,
    font_small: HFONT,
    font_glyph: HFONT,
    font_glyph_big: HFONT,
    avatar: Option<HBITMAP>,
}

thread_local! {
    static MENU: RefCell<Option<MenuState>> = const { RefCell::new(None) };
}

// ---------------------------------------------------------------------------
// Creation

pub fn create(cfg: &Config, taskbar: HWND) -> Result<()> {
    unsafe {
        let hinstance: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = w!("StartPE_Menu");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let width = scaled(cfg.menu_width);
        let height = scaled(cfg.menu_height) + scaled(OVERHANG);

        let avatar = cfg.user_picture.as_ref().and_then(|p| {
            let wp = util::WideStr::new(p);
            LoadImageW(
                None,
                wp.pcwstr(),
                IMAGE_BITMAP,
                scaled(AVATAR),
                scaled(AVATAR),
                LR_LOADFROMFILE,
            )
            .ok()
            .map(|h| HBITMAP(h.0))
        });

        MENU.with_borrow_mut(|m| {
            *m = Some(MenuState {
                hwnd: HWND::default(),
                taskbar,
                width,
                height,
                items: Vec::new(),
                rights: build_right_items(),
                pinned: crate::pins::Pins::load()
                    .start_menu
                    .into_iter()
                    .map(PathBuf::from)
                    .collect(),
                showing_all: false,
                stack: Vec::new(),
                query: String::new(),
                scroll: 0,
                hover: Hit::None,
                font: make_font(scaled(15), 400),
                font_small: make_font(scaled(13), 400),
                font_glyph: make_font_face(scaled(15), 400, w!("Segoe MDL2 Assets")),
                font_glyph_big: make_font_face(scaled(30), 400, w!("Segoe MDL2 Assets")),
                avatar,
            })
        });

        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class,
            w!("StartPE Menu"),
            WS_POPUP,
            0,
            0,
            width,
            height,
            None,
            None,
            hinstance,
            None,
        )?;
        MENU.with_borrow_mut(|m| m.as_mut().unwrap().hwnd = hwnd);

        apply_window_region(hwnd, width, height);
        Ok(())
    }
}

/// Rounded body + the circle for the protruding avatar, as a window region.
/// Works without DWM composition, which PE may not have.
fn apply_window_region(hwnd: HWND, width: i32, height: i32) {
    unsafe {
        let body_top = scaled(OVERHANG);
        let corner = scaled(RADIUS) * 2;
        let body = CreateRoundRectRgn(0, body_top, width + 1, height + 1, corner, corner);
        let acx = avatar_center_x(width);
        let d = scaled(AVATAR);
        let circle = CreateEllipticRgn(acx - d / 2, 0, acx + d / 2 + 1, d + 1);
        let combined = CreateRectRgn(0, 0, 0, 0);
        CombineRgn(combined, body, circle, RGN_OR);
        let _ = DeleteObject(body);
        let _ = DeleteObject(circle);
        // The system owns the region after SetWindowRgn.
        SetWindowRgn(hwnd, combined, true);
    }
}

fn avatar_center_x(width: i32) -> i32 {
    // Centered over the right pane.
    width - scaled(RIGHT_W) / 2 - scaled(10)
}

fn build_right_items() -> Vec<RightItem> {
    let username = std::env::var("USERNAME").unwrap_or_else(|_| "User".to_string());
    let profile = std::env::var("USERPROFILE").unwrap_or_else(|_| "X:\\Users\\Default".to_string());
    vec![
        RightItem {
            glyph: GLYPH_PERSON,
            label: username,
            cmd: profile.clone(),
            args: String::new(),
        },
        RightItem {
            glyph: GLYPH_DOWNLOAD,
            label: "Downloads".to_string(),
            cmd: format!("{profile}\\Downloads"),
            args: String::new(),
        },
        RightItem {
            glyph: GLYPH_PC,
            label: "This PC".to_string(),
            cmd: "explorer.exe".to_string(),
            args: "shell:MyComputerFolder".to_string(),
        },
        RightItem {
            glyph: GLYPH_CONTROL,
            label: "Control Panel".to_string(),
            cmd: "control.exe".to_string(),
            args: String::new(),
        },
        RightItem {
            glyph: GLYPH_CMD,
            label: "Command Prompt".to_string(),
            cmd: "cmd.exe".to_string(),
            args: String::new(),
        },
        RightItem {
            glyph: GLYPH_RUN,
            label: "Run…".to_string(),
            // Sentinel: routed to the shell Run dialog (run_dialog.rs), not
            // ShellExecute'd — so it gets a proper icon/prompt and placement.
            cmd: RUN_DIALOG_CMD.to_string(),
            args: String::new(),
        },
    ]
}

// ---------------------------------------------------------------------------
// Show / hide

pub fn toggle() {
    unsafe {
        let info = MENU.with_borrow(|m| m.as_ref().map(|m| (m.hwnd, m.taskbar, m.width, m.height)));
        let Some((hwnd, taskbar, width, height)) = info else { return };
        if IsWindowVisible(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_HIDE);
        } else {
            MENU.with_borrow_mut(|m| {
                let m = m.as_mut().unwrap();
                m.stack.clear();
                m.query.clear();
                m.scroll = 0;
                m.showing_all = false;
                rebuild(m);
                // Open with the search box focused (keyboard-first): typing
                // searches immediately, Right moves to the Shut down button.
                m.hover = Hit::Search;
            });
            // Anchor above the taskbar with a gap. Centered taskbar → centered
            // menu (Windows 11 style); left-aligned taskbar → flush bottom-left,
            // matching the start button's left edge (Windows 10 / SAB style).
            let mut tb = RECT::default();
            let _ = GetWindowRect(taskbar, &mut tb);
            let y = tb.top - scaled(GAP) - height;
            let sw = GetSystemMetrics(SM_CXSCREEN);
            let x = if crate::taskbar::is_centered() {
                ((sw - width) / 2).clamp(scaled(8), (sw - width - scaled(8)).max(scaled(8)))
            } else {
                scaled(8)
            };
            let _ = SetWindowPos(hwnd, HWND_TOPMOST, x, y, width, height, SWP_SHOWWINDOW);
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

fn hide(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

// ---------------------------------------------------------------------------
// Item list

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

fn collect_dir(dir: &Path, items: &mut Vec<Item>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
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
                items.push(Item {
                    kind: ItemKind::Folder(path),
                    name,
                    icon: None,
                });
            }
        } else if is_launchable(&path) && !name.eq_ignore_ascii_case("desktop") {
            items.push(Item {
                kind: ItemKind::Launch(path),
                name,
                icon: None,
            });
        }
    }
}

fn search_walk(dir: &Path, query: &str, depth: u32, out: &mut Vec<Item>) {
    if depth > 4 || out.len() >= 60 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
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
                    out.push(Item {
                        kind: ItemKind::Launch(path.clone()),
                        name: name.to_string(),
                        icon: None,
                    });
                }
            }
        }
    }
}

fn rebuild(m: &mut MenuState) {
    for item in &m.items {
        if let Some(icon) = item.icon {
            unsafe {
                let _ = DestroyIcon(icon);
            }
        }
    }
    m.items.clear();

    if !m.query.is_empty() {
        let q = m.query.to_lowercase();
        for root in start_menu_roots() {
            search_walk(&root, &q, 0, &mut m.items);
        }
        m.items
            .sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    } else if let Some(current) = m.stack.last().cloned() {
        m.items.push(Item {
            kind: ItemKind::Back,
            name: "‹ Back".to_string(),
            icon: None,
        });
        collect_dir(&current, &mut m.items);
        // The same subfolder may exist in the other root: merge it too.
        if m.stack.len() == 1 {
            if let Some(tail) = current.file_name() {
                for root in start_menu_roots() {
                    let twin = root.join(tail);
                    if twin != current && twin.is_dir() {
                        collect_dir(&twin, &mut m.items);
                    }
                }
            }
        }
        sort_items(m);
    } else if pinned_view(m) {
        // Pinned view (default when start-menu pins exist): show the pins in
        // pin order, no sorting, no folders.
        for path in &m.pinned {
            m.items.push(Item {
                kind: ItemKind::Launch(path.clone()),
                name: pin_display_name(path),
                icon: None,
            });
        }
    } else {
        for root in start_menu_roots() {
            collect_dir(&root, &mut m.items);
        }
        sort_items(m);
    }

    load_icons(m);
    m.scroll = 0;
}

/// Whether the menu should currently show the pinned view (pins exist and the
/// user hasn't switched to All apps).
fn pinned_view(m: &MenuState) -> bool {
    !m.pinned.is_empty() && !m.showing_all
}

/// Display label for a pinned program: its version-info application name
/// (e.g. "HWiNFO64"), falling back to the file stem.
fn pin_display_name(path: &Path) -> String {
    util::app_display_name(&path.to_string_lossy())
}

fn sort_items(m: &mut MenuState) {
    // Folders first, then alphabetical; Back pinned to the top.
    m.items.sort_by(|a, b| {
        let rank = |i: &Item| match i.kind {
            ItemKind::Back => 0,
            ItemKind::Folder(_) => 1,
            ItemKind::Launch(_) => 2,
        };
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

fn load_icons(m: &mut MenuState) {
    unsafe {
        for item in &mut m.items {
            let path = match &item.kind {
                ItemKind::Folder(p) | ItemKind::Launch(p) => p,
                ItemKind::Back => continue,
            };
            let wide = util::WideStr::new(&path.to_string_lossy());
            let mut sfi = SHFILEINFOW::default();
            let ok = SHGetFileInfoW(
                wide.pcwstr(),
                FILE_ATTRIBUTE_NORMAL,
                Some(&mut sfi),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_ICON | SHGFI_SMALLICON,
            );
            if ok != 0 && !sfi.hIcon.is_invalid() {
                item.icon = Some(sfi.hIcon);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Layout / hit testing

fn body_top() -> i32 {
    scaled(OVERHANG)
}

fn footer_top(m: &MenuState) -> i32 {
    m.height - scaled(FOOTER_H)
}

fn allprog_top(m: &MenuState) -> i32 {
    footer_top(m) - scaled(ALLPROG_H)
}

fn left_pane_right(m: &MenuState) -> i32 {
    m.width - scaled(RIGHT_W) - scaled(4)
}

fn list_top() -> i32 {
    body_top() + scaled(8)
}

fn list_height(m: &MenuState) -> i32 {
    allprog_top(m) - list_top()
}

fn right_rows_top() -> i32 {
    scaled(AVATAR) + scaled(12)
}

fn search_box_rect(m: &MenuState) -> RECT {
    let ft = footer_top(m);
    RECT {
        left: scaled(12),
        top: ft + (scaled(FOOTER_H) - scaled(30)) / 2,
        right: left_pane_right(m) - scaled(4),
        bottom: ft + (scaled(FOOTER_H) + scaled(30)) / 2,
    }
}

fn shutdown_rect(m: &MenuState) -> RECT {
    let ft = footer_top(m);
    RECT {
        left: m.width - scaled(140),
        top: ft,
        right: m.width - scaled(28),
        bottom: m.height,
    }
}

fn shutdown_chevron_rect(m: &MenuState) -> RECT {
    let ft = footer_top(m);
    RECT {
        left: m.width - scaled(28),
        top: ft,
        right: m.width - scaled(6),
        bottom: m.height,
    }
}

fn in_rect(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn hit_test(m: &MenuState, x: i32, y: i32) -> Hit {
    if y >= footer_top(m) {
        if in_rect(&shutdown_rect(m), x, y) {
            return Hit::Shutdown;
        }
        if in_rect(&shutdown_chevron_rect(m), x, y) {
            return Hit::ShutdownMenu;
        }
        if in_rect(&search_box_rect(m), x, y) {
            return Hit::Search;
        }
        return Hit::None;
    }
    if x < left_pane_right(m) {
        if y >= allprog_top(m) {
            return Hit::AllPrograms;
        }
        if y >= list_top() {
            let idx = (y - list_top() + m.scroll) / scaled(ROW);
            if idx >= 0 && (idx as usize) < m.items.len() {
                return Hit::Row(idx as usize);
            }
        }
        return Hit::None;
    }
    // Right pane rows.
    if y >= right_rows_top() {
        let idx = (y - right_rows_top()) / scaled(RIGHT_ROW);
        if idx >= 0 && (idx as usize) < m.rights.len() {
            return Hit::Right(idx as usize);
        }
    }
    Hit::None
}

fn max_scroll(m: &MenuState) -> i32 {
    (m.items.len() as i32 * scaled(ROW) - list_height(m)).max(0)
}

// ---------------------------------------------------------------------------
// Painting

unsafe fn draw_str(hdc: HDC, s: &str, rect: &mut RECT, flags: DRAW_TEXT_FORMAT) {
    let mut t = util::wide(s);
    t.pop();
    DrawTextW(hdc, &mut t, rect, flags);
}

fn fill(hdc: HDC, rect: &RECT, color: u32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color));
        FillRect(hdc, rect, brush);
        let _ = DeleteObject(brush);
    }
}

fn paint(m: &MenuState) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(m.hwnd, &mut ps);

        let mem = CreateCompatibleDC(hdc);
        let bmp = CreateCompatibleBitmap(hdc, m.width, m.height);
        let old_bmp = SelectObject(mem, bmp);
        SetBkMode(mem, TRANSPARENT);

        // Body background (the window region clips the rounded shape).
        let full = RECT {
            left: 0,
            top: 0,
            right: m.width,
            bottom: m.height,
        };
        fill(mem, &full, COL_BG);
        let footer = RECT {
            left: 0,
            top: footer_top(m),
            right: m.width,
            bottom: m.height,
        };
        fill(mem, &footer, COL_FOOTER);

        draw_left_pane(m, mem);
        draw_right_pane(m, mem);
        draw_footer(m, mem);
        draw_avatar(m, mem);

        // Hide the caret across the blit so it isn't corrupted, then restore it.
        let _ = HideCaret(m.hwnd);
        let _ = BitBlt(hdc, 0, 0, m.width, m.height, mem, 0, 0, SRCCOPY);
        let _ = ShowCaret(m.hwnd);
        SelectObject(mem, old_bmp);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem);
        let _ = EndPaint(m.hwnd, &ps);
    }
}

fn draw_left_pane(m: &MenuState, hdc: HDC) {
    unsafe {
        let lp_right = left_pane_right(m);
        let row_h = scaled(ROW);
        let lt = list_top();
        let lh = list_height(m);
        let old_font = SelectObject(hdc, m.font);

        // Clip the scrolling list so partial rows don't bleed out.
        let _ = SaveDC(hdc);
        IntersectClipRect(hdc, scaled(6), lt, lp_right, lt + lh);

        for (i, item) in m.items.iter().enumerate() {
            let top = lt + i as i32 * row_h - m.scroll;
            if top + row_h < lt || top >= lt + lh {
                continue;
            }
            let rect = RECT {
                left: scaled(6),
                top,
                right: lp_right - scaled(4),
                bottom: top + row_h,
            };
            if m.hover == Hit::Row(i) {
                fill(hdc, &rect, COL_HOVER);
            }
            let mut text_left = scaled(14);
            if let Some(icon) = item.icon {
                let sz = scaled(22);
                let _ = DrawIconEx(
                    hdc,
                    text_left,
                    top + (row_h - sz) / 2,
                    icon,
                    sz,
                    sz,
                    0,
                    None,
                    DI_NORMAL,
                );
            }
            text_left += scaled(32);
            SetTextColor(
                hdc,
                COLORREF(if matches!(item.kind, ItemKind::Back) {
                    COL_TEXT_DIM
                } else {
                    COL_TEXT
                }),
            );
            let mut tr = RECT {
                left: text_left,
                top,
                right: lp_right - scaled(24),
                bottom: top + row_h,
            };
            draw_str(
                hdc,
                &item.name,
                &mut tr,
                DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
            );
            if matches!(item.kind, ItemKind::Folder(_)) {
                SelectObject(hdc, m.font_glyph);
                SetTextColor(hdc, COLORREF(COL_TEXT_DIM));
                let mut ar = RECT {
                    left: lp_right - scaled(26),
                    top,
                    right: lp_right - scaled(10),
                    bottom: top + row_h,
                };
                draw_str(hdc, GLYPH_CHEVRON, &mut ar, DT_SINGLELINE | DT_VCENTER | DT_RIGHT);
                SelectObject(hdc, m.font);
            }
        }
        if m.items.is_empty() && !m.query.is_empty() {
            SetTextColor(hdc, COLORREF(COL_TEXT_DIM));
            let mut tr = RECT {
                left: scaled(16),
                top: lt,
                right: lp_right,
                bottom: lt + row_h,
            };
            draw_str(hdc, "No results", &mut tr, DT_SINGLELINE | DT_VCENTER);
        }
        let _ = RestoreDC(hdc, -1);

        // Separator + "All Programs" row.
        let at = allprog_top(m);
        let sep = RECT {
            left: scaled(12),
            top: at,
            right: lp_right - scaled(8),
            bottom: at + 1,
        };
        fill(hdc, &sep, COL_SEP);
        let ap = RECT {
            left: scaled(6),
            top: at + scaled(2),
            right: lp_right - scaled(4),
            bottom: footer_top(m) - scaled(2),
        };
        if m.hover == Hit::AllPrograms {
            fill(hdc, &ap, COL_HOVER);
        }
        // The row flips between "All apps ›" (in the pinned view) and
        // "‹ Pinned" (in the full list, when pins exist).
        let (glyph, label) = if pinned_view(m) {
            (GLYPH_CHEVRON, "All apps")
        } else if !m.pinned.is_empty() {
            (GLYPH_CHEVRON_LEFT, "Pinned")
        } else {
            (GLYPH_CHEVRON, "All Programs")
        };
        SelectObject(hdc, m.font_glyph);
        SetTextColor(hdc, COLORREF(COL_TEXT));
        let mut gr = RECT {
            left: scaled(16),
            top: ap.top,
            right: scaled(36),
            bottom: ap.bottom,
        };
        draw_str(hdc, glyph, &mut gr, DT_SINGLELINE | DT_VCENTER);
        SelectObject(hdc, m.font);
        let mut tr = RECT {
            left: scaled(46),
            top: ap.top,
            right: lp_right - scaled(8),
            bottom: ap.bottom,
        };
        draw_str(hdc, label, &mut tr, DT_SINGLELINE | DT_VCENTER);

        // Vertical separator between panes.
        let vsep = RECT {
            left: lp_right,
            top: body_top() + scaled(10),
            right: lp_right + 1,
            bottom: footer_top(m) - scaled(6),
        };
        fill(hdc, &vsep, COL_SEP);

        SelectObject(hdc, old_font);
    }
}

fn draw_right_pane(m: &MenuState, hdc: HDC) {
    unsafe {
        let left = left_pane_right(m) + scaled(8);
        let row_h = scaled(RIGHT_ROW);
        let old_font = SelectObject(hdc, m.font);
        for (i, item) in m.rights.iter().enumerate() {
            let top = right_rows_top() + i as i32 * row_h;
            let rect = RECT {
                left,
                top,
                right: m.width - scaled(8),
                bottom: top + row_h,
            };
            if m.hover == Hit::Right(i) {
                fill(hdc, &rect, COL_HOVER);
            }
            SelectObject(hdc, m.font_glyph);
            SetTextColor(hdc, COLORREF(COL_TEXT));
            let mut gr = RECT {
                left: left + scaled(8),
                top,
                right: left + scaled(30),
                bottom: top + row_h,
            };
            draw_str(hdc, item.glyph, &mut gr, DT_SINGLELINE | DT_VCENTER);
            SelectObject(hdc, m.font);
            let mut tr = RECT {
                left: left + scaled(38),
                top,
                right: m.width - scaled(10),
                bottom: top + row_h,
            };
            draw_str(
                hdc,
                &item.label,
                &mut tr,
                DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
            );
        }
        SelectObject(hdc, old_font);
    }
}

fn draw_footer(m: &MenuState, hdc: HDC) {
    unsafe {
        // Search box (pill). Accent border when it holds keyboard focus.
        let sb = search_box_rect(m);
        let focused = m.hover == Hit::Search;
        let brush = CreateSolidBrush(COLORREF(COL_SEARCH_BG));
        let pen = CreatePen(
            PS_SOLID,
            if focused { scaled(1).max(1) } else { 1 },
            COLORREF(if focused { COL_ACCENT } else { COL_SEP }),
        );
        let old_brush = SelectObject(hdc, brush);
        let old_pen = SelectObject(hdc, pen);
        let r = sb.bottom - sb.top;
        let _ = RoundRect(hdc, sb.left, sb.top, sb.right, sb.bottom, r, r);
        SelectObject(hdc, old_brush);
        SelectObject(hdc, old_pen);
        let _ = DeleteObject(brush);
        let _ = DeleteObject(pen);

        SelectObject(hdc, m.font_glyph);
        SetTextColor(hdc, COLORREF(COL_TEXT_DIM));
        let mut gr = RECT {
            left: sb.left + scaled(10),
            top: sb.top,
            right: sb.left + scaled(28),
            bottom: sb.bottom,
        };
        draw_str(hdc, GLYPH_SEARCH, &mut gr, DT_SINGLELINE | DT_VCENTER);

        SelectObject(hdc, m.font_small);
        let mut tr = RECT {
            left: sb.left + scaled(34),
            top: sb.top,
            right: sb.right - scaled(10),
            bottom: sb.bottom,
        };
        if m.query.is_empty() {
            SetTextColor(hdc, COLORREF(COL_TEXT_DIM));
            draw_str(
                hdc,
                "Search programs and files",
                &mut tr,
                DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            );
        } else {
            SetTextColor(hdc, COLORREF(COL_TEXT));
            draw_str(
                hdc,
                &m.query,
                &mut tr,
                DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            );
        }

        // Shut down + chevron.
        let sd = shutdown_rect(m);
        if m.hover == Hit::Shutdown {
            fill(hdc, &sd, COL_HOVER);
        }
        SelectObject(hdc, m.font_glyph);
        SetTextColor(hdc, COLORREF(COL_TEXT));
        let mut gr = RECT {
            left: sd.left + scaled(6),
            top: sd.top,
            right: sd.left + scaled(26),
            bottom: sd.bottom,
        };
        draw_str(hdc, GLYPH_POWER, &mut gr, DT_SINGLELINE | DT_VCENTER);
        SelectObject(hdc, m.font);
        let mut tr = RECT {
            left: sd.left + scaled(32),
            top: sd.top,
            right: sd.right,
            bottom: sd.bottom,
        };
        draw_str(hdc, "Shut down", &mut tr, DT_SINGLELINE | DT_VCENTER);

        let cv = shutdown_chevron_rect(m);
        if m.hover == Hit::ShutdownMenu {
            fill(hdc, &cv, COL_HOVER);
        }
        SelectObject(hdc, m.font_glyph);
        SetTextColor(hdc, COLORREF(COL_TEXT_DIM));
        let mut gr = RECT {
            left: cv.left,
            top: cv.top,
            right: cv.right,
            bottom: cv.bottom,
        };
        draw_str(hdc, GLYPH_CHEVRON, &mut gr, DT_SINGLELINE | DT_VCENTER | DT_CENTER);
    }
}

fn draw_avatar(m: &MenuState, hdc: HDC) {
    unsafe {
        let d = scaled(AVATAR);
        let cx = avatar_center_x(m.width);
        let rx = cx - d / 2;

        let circle = CreateEllipticRgn(rx, 0, rx + d + 1, d + 1);
        SelectClipRgn(hdc, circle);
        if let Some(bmp) = m.avatar {
            let src = CreateCompatibleDC(hdc);
            let old = SelectObject(src, bmp);
            let _ = BitBlt(hdc, rx, 0, d, d, src, 0, 0, SRCCOPY);
            SelectObject(src, old);
            let _ = DeleteDC(src);
        } else {
            let r = RECT {
                left: rx,
                top: 0,
                right: rx + d,
                bottom: d,
            };
            fill(hdc, &r, COL_AVATAR_BG);
            let old_font = SelectObject(hdc, m.font_glyph_big);
            SetTextColor(hdc, COLORREF(COL_TEXT_DIM));
            let mut gr = r;
            draw_str(hdc, GLYPH_PERSON, &mut gr, DT_SINGLELINE | DT_VCENTER | DT_CENTER);
            SelectObject(hdc, old_font);
        }
        SelectClipRgn(hdc, None);
        let _ = DeleteObject(circle);

        // Ring around the picture.
        let pen = CreatePen(PS_SOLID, scaled(3), COLORREF(COL_RING));
        let old_pen = SelectObject(hdc, pen);
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        let _ = Ellipse(hdc, rx, 0, rx + d, d);
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(pen);
    }
}

// ---------------------------------------------------------------------------
// Actions

fn launch(path: &Path) {
    unsafe {
        let file = util::WideStr::new(&path.to_string_lossy());
        let dir = path
            .parent()
            .map(|d| util::WideStr::new(&d.to_string_lossy()));
        ShellExecuteW(
            None,
            w!("open"),
            file.pcwstr(),
            None,
            dir.as_ref().map_or(PCWSTR::null(), |d| d.pcwstr()),
            SW_SHOWNORMAL,
        );
    }
}

fn exec(cmd: &str, args: &str) {
    unsafe {
        let cmd = util::WideStr::new(cmd);
        let args = util::WideStr::new(args);
        ShellExecuteW(None, w!("open"), cmd.pcwstr(), args.pcwstr(), None, SW_SHOWNORMAL);
    }
}

/// Restart / Shut down flyout for the chevron next to the Shut down button.
/// `(x, y)` is the screen anchor (the flyout is bottom-aligned above it), so the
/// same call works whether opened by mouse or keyboard.
fn shutdown_flyout(hwnd: HWND, x: i32, y: i32) {
    let cmd = crate::menu::track(
        hwnd,
        x,
        y,
        TPM_BOTTOMALIGN,
        &[(1, "Restart"), (2, "Shut down")],
    );
    match cmd {
        1 => {
            hide(hwnd);
            exec("wpeutil.exe", "reboot");
        }
        2 => {
            hide(hwnd);
            exec("wpeutil.exe", "shutdown");
        }
        _ => {}
    }
}

/// What activating a `Hit` (by click or Enter) does. Resolved while holding the
/// `MENU` borrow, then performed after it is dropped (launching pumps messages).
enum Action {
    None,
    /// Folder/Back/All-apps navigation already applied to the state; just repaint.
    Navigate,
    Launch(PathBuf),
    Exec(String, String),
    RunDialog(HWND),
    Shutdown,
    /// Open the power flyout at this screen anchor.
    ShutdownMenu(i32, i32),
}

/// Screen anchor (top-left of the Shut down button) for the power flyout.
fn shutdown_anchor(m: &MenuState) -> (i32, i32) {
    let mut wr = RECT::default();
    unsafe {
        let _ = GetWindowRect(m.hwnd, &mut wr);
    }
    let sd = shutdown_rect(m);
    (wr.left + sd.left, wr.top + sd.top)
}

/// Resolve the action for activating `hit`, mutating navigation state in place.
fn resolve(m: &mut MenuState, hit: Hit) -> Action {
    // Guard against a focus index left stale by a rebuild.
    if let Hit::Row(i) = hit {
        if i >= m.items.len() {
            return Action::None;
        }
    }
    if let Hit::Right(i) = hit {
        if i >= m.rights.len() {
            return Action::None;
        }
    }
    match hit {
        Hit::Row(i) => match &m.items[i].kind {
            ItemKind::Back => {
                m.stack.pop();
                rebuild(m);
                m.hover = first_focus(m);
                Action::Navigate
            }
            ItemKind::Folder(p) => {
                let p = p.clone();
                m.stack.push(p);
                rebuild(m);
                m.hover = first_focus(m);
                Action::Navigate
            }
            ItemKind::Launch(p) => Action::Launch(p.clone()),
        },
        Hit::AllPrograms => {
            m.stack.clear();
            m.query.clear();
            if !m.pinned.is_empty() {
                m.showing_all = !m.showing_all;
            }
            rebuild(m);
            m.hover = first_focus(m);
            Action::Navigate
        }
        Hit::Right(i) => {
            let it = &m.rights[i];
            if it.cmd == RUN_DIALOG_CMD {
                Action::RunDialog(m.taskbar)
            } else {
                Action::Exec(it.cmd.clone(), it.args.clone())
            }
        }
        Hit::Shutdown => Action::Shutdown,
        Hit::ShutdownMenu => {
            let (x, y) = shutdown_anchor(m);
            Action::ShutdownMenu(x, y)
        }
        Hit::Search | Hit::None => Action::None,
    }
}

/// Carry out an `Action` (after the `MENU` borrow has been dropped).
fn perform(hwnd: HWND, action: Action) {
    match action {
        Action::Navigate => unsafe {
            let _ = InvalidateRect(hwnd, None, true);
        },
        Action::Launch(path) => {
            hide(hwnd);
            launch(&path);
        }
        Action::Exec(cmd, args) => {
            hide(hwnd);
            exec(&cmd, &args);
        }
        Action::RunDialog(taskbar) => {
            hide(hwnd);
            let mut rc = RECT::default();
            unsafe {
                let _ = GetWindowRect(taskbar, &mut rc);
            }
            crate::run_dialog::show(rc.top);
        }
        Action::Shutdown => {
            hide(hwnd);
            exec("wpeutil.exe", "shutdown");
        }
        Action::ShutdownMenu(x, y) => shutdown_flyout(hwnd, x, y),
        Action::None => {}
    }
}

// ---------------------------------------------------------------------------
// Keyboard navigation

#[derive(Clone, Copy)]
enum Dir {
    Up,
    Down,
    Left,
    Right,
}

/// The first focusable item in the left pane (the top program row, or the
/// "All apps" row when the list is empty).
fn first_focus(m: &MenuState) -> Hit {
    if m.items.is_empty() {
        Hit::AllPrograms
    } else {
        Hit::Row(0)
    }
}

/// Scroll the left-pane list so row `i` is fully visible.
fn scroll_into_view(m: &mut MenuState, i: usize) {
    let row_h = scaled(ROW);
    let lh = list_height(m);
    let top = i as i32 * row_h;
    let bottom = top + row_h;
    if top < m.scroll {
        m.scroll = top;
    } else if bottom > m.scroll + lh {
        m.scroll = bottom - lh;
    }
    m.scroll = m.scroll.clamp(0, max_scroll(m));
}

/// Move keyboard focus (the shared `hover` highlight). Regions left→right are:
/// the program list (+ All apps), the right-pane links, and the power controls
/// (Shut down, then its flyout chevron). Up/Down move within a region; Left/Right
/// move between them — so from the list, Right→Right reaches the power flyout.
fn navigate(m: &mut MenuState, dir: Dir) {
    let n = m.items.len();
    let r = m.rights.len();
    let cur = m.hover;
    let new = match (cur, dir) {
        // Search box (footer-left, focused on open): Right → Shut down, Up → list.
        (Hit::Search, Dir::Right) => Hit::Shutdown,
        (Hit::Search, Dir::Up) => Hit::AllPrograms,
        (Hit::Search, _) => Hit::Search,

        // Nothing focused yet: arrow into the natural region.
        (Hit::None, Dir::Right) => {
            if r > 0 {
                Hit::Right(0)
            } else {
                Hit::ShutdownMenu
            }
        }
        (Hit::None, _) => first_focus(m),

        // Left pane (program rows + All apps), vertical. Down past the bottom
        // (the All apps row) lands on the search box below it.
        (Hit::Row(i), Dir::Down) => {
            if i + 1 < n {
                Hit::Row(i + 1)
            } else {
                Hit::AllPrograms
            }
        }
        (Hit::Row(i), Dir::Up) => Hit::Row(i.saturating_sub(1)),
        (Hit::AllPrograms, Dir::Up) => {
            if n > 0 {
                Hit::Row(n - 1)
            } else {
                Hit::AllPrograms
            }
        }
        (Hit::AllPrograms, Dir::Down) => Hit::Search,
        // Left pane → right pane. (Folder rows are intercepted before navigate
        // so Right expands them instead; this is for launcher / All-apps rows.)
        (Hit::Row(_) | Hit::AllPrograms, Dir::Right) => {
            if r > 0 {
                Hit::Right(0)
            } else {
                Hit::ShutdownMenu
            }
        }

        // Right pane, vertical. Down past the bottom drops to the power button.
        (Hit::Right(i), Dir::Down) => {
            if i + 1 < r {
                Hit::Right(i + 1)
            } else {
                Hit::Shutdown
            }
        }
        (Hit::Right(i), Dir::Up) => Hit::Right(i.saturating_sub(1)),
        (Hit::Right(_), Dir::Left) => first_focus(m),

        // Power controls (footer-right).
        (Hit::Shutdown, Dir::Right) => Hit::ShutdownMenu,
        (Hit::Shutdown, Dir::Left) => Hit::Search,
        (Hit::ShutdownMenu, Dir::Left) => Hit::Shutdown,
        (Hit::Shutdown | Hit::ShutdownMenu, Dir::Up) => {
            if r > 0 {
                Hit::Right(r - 1)
            } else {
                first_focus(m)
            }
        }

        // Anything else: stay put (e.g. Left from the list, Down from power).
        _ => cur,
    };
    m.hover = new;
    if let Hit::Row(i) = new {
        scroll_into_view(m, i);
    }
}

/// Place the blinking text caret just after the current search query inside the
/// search box, signalling "type to search". Typing always feeds the search
/// regardless of arrow-key focus, so the caret lives here whenever the menu has
/// focus. No-op (harmless) if no caret is currently owned.
fn position_caret(m: &MenuState) {
    unsafe {
        let sb = search_box_rect(m);
        let text_left = sb.left + scaled(34);
        let mut w = 0;
        if !m.query.is_empty() {
            let hdc = GetDC(m.hwnd);
            let old = SelectObject(hdc, m.font_small);
            let q: Vec<u16> = m.query.encode_utf16().collect();
            let mut sz = SIZE::default();
            let _ = GetTextExtentPoint32W(hdc, &q, &mut sz);
            w = sz.cx;
            SelectObject(hdc, old);
            let _ = ReleaseDC(m.hwnd, hdc);
        }
        let h = scaled(16);
        let y = (sb.top + sb.bottom) / 2 - h / 2;
        let _ = SetCaretPos(text_left + w, y);
    }
}

/// Map an arrow virtual-key to a navigation direction.
fn arrow_dir(vk: u32) -> Option<Dir> {
    match vk {
        v if v == VK_UP.0 as u32 => Some(Dir::Up),
        v if v == VK_DOWN.0 as u32 => Some(Dir::Down),
        v if v == VK_LEFT.0 as u32 => Some(Dir::Left),
        v if v == VK_RIGHT.0 as u32 => Some(Dir::Right),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Window procedure

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_ACTIVATE => {
            if util::loword(wparam.0 as isize) == WA_INACTIVE as i32 {
                hide(hwnd);
            }
            LRESULT(0)
        }
        WM_SETFOCUS => {
            // A blinking caret in the search box (the menu is type-to-search).
            MENU.with_borrow(|m| {
                if let Some(m) = m.as_ref() {
                    let _ = CreateCaret(hwnd, HBITMAP::default(), scaled(1).max(1), scaled(16));
                    position_caret(m);
                    let _ = ShowCaret(hwnd);
                }
            });
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            let _ = DestroyCaret();
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let vk = wparam.0 as u32;
            if vk == VK_ESCAPE.0 as u32 {
                // First Esc clears an active search, second closes.
                let had_query = MENU.with_borrow_mut(|m| {
                    let m = m.as_mut().unwrap();
                    if m.query.is_empty() {
                        false
                    } else {
                        m.query.clear();
                        rebuild(m);
                        m.hover = Hit::Search;
                        true
                    }
                });
                if had_query {
                    MENU.with_borrow(|m| {
                        if let Some(m) = m.as_ref() {
                            position_caret(m);
                        }
                    });
                    let _ = InvalidateRect(hwnd, None, true);
                } else {
                    hide(hwnd);
                }
            } else if vk == VK_RETURN.0 as u32 {
                // Activate the focused item; with the search box (or nothing)
                // focused, fall back to the first launchable result.
                let action = MENU.with_borrow_mut(|m| {
                    let m = m.as_mut().unwrap();
                    let hit = match m.hover {
                        Hit::Search | Hit::None => m
                            .items
                            .iter()
                            .position(|i| matches!(i.kind, ItemKind::Launch(_)))
                            .map_or(Hit::None, Hit::Row),
                        h => h,
                    };
                    resolve(m, hit)
                });
                perform(hwnd, action);
            } else if let Some(dir) = arrow_dir(vk) {
                // Right on a ">" folder row expands it (like Enter); otherwise
                // it's plain focus movement.
                let action = MENU.with_borrow_mut(|m| {
                    let m = m.as_mut().unwrap();
                    if matches!(dir, Dir::Right) {
                        if let Hit::Row(i) = m.hover {
                            if matches!(m.items.get(i).map(|it| &it.kind), Some(ItemKind::Folder(_)))
                            {
                                return Some(resolve(m, Hit::Row(i)));
                            }
                        }
                    }
                    navigate(m, dir);
                    None
                });
                match action {
                    Some(a) => perform(hwnd, a),
                    None => {
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
            }
            LRESULT(0)
        }
        WM_CHAR => {
            let c = wparam.0 as u32;
            let changed = MENU.with_borrow_mut(|m| {
                let m = m.as_mut().unwrap();
                let edited = if c == VK_BACK.0 as u32 {
                    m.query.pop().is_some()
                } else if let Some(ch) = char::from_u32(c).filter(|ch| !ch.is_control()) {
                    m.query.push(ch);
                    true
                } else {
                    false
                };
                if edited {
                    rebuild(m);
                    // Typing keeps focus in the search box (Enter launches the
                    // top result via the fallback).
                    m.hover = Hit::Search;
                }
                edited
            });
            if changed {
                MENU.with_borrow(|m| {
                    if let Some(m) = m.as_ref() {
                        position_caret(m);
                    }
                });
                let _ = InvalidateRect(hwnd, None, true);
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = util::hiword(wparam.0 as isize);
            MENU.with_borrow_mut(|m| {
                let m = m.as_mut().unwrap();
                m.scroll = (m.scroll - delta * scaled(ROW) / 120).clamp(0, max_scroll(m));
            });
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = util::loword(lparam.0);
            let y = util::hiword(lparam.0);
            let changed = MENU.with_borrow_mut(|m| {
                let m = m.as_mut().unwrap();
                let hit = hit_test(m, x, y);
                let changed = hit != m.hover;
                m.hover = hit;
                changed
            });
            if changed {
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let x = util::loword(lparam.0);
            let y = util::hiword(lparam.0);
            // Resolve inside the borrow, act outside (launching pumps messages).
            let action = MENU.with_borrow_mut(|m| {
                let m = m.as_mut().unwrap();
                let hit = hit_test(m, x, y);
                resolve(m, hit)
            });
            perform(hwnd, action);
            LRESULT(0)
        }
        WM_MEASUREITEM => {
            if crate::menu::on_measure(lparam) {
                LRESULT(1)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_DRAWITEM => {
            if crate::menu::on_draw(lparam) {
                LRESULT(1)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_PAINT => {
            MENU.with_borrow(|m| {
                if let Some(m) = m.as_ref() {
                    paint(m);
                }
            });
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
