// SPDX-License-Identifier: GPL-3.0-or-later
//! The taskbar: an appbar docked to the bottom edge with a start button,
//! task buttons for top-level windows, and a clock.
//!
//! Window tracking uses `RegisterShellHookWindow` (documented, stable) plus a
//! slow polling timer as a safety net — no Explorer injection anywhere.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use windows::core::{w, Result, PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Globalization::{
    GetDateFormatEx, GetTimeFormatEx, DATE_SHORTDATE, TIME_NOSECONDS,
};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, Sleep, PROCESS_NAME_FORMAT,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::Config;
use crate::start_menu;
use crate::util;

const APPBAR_CALLBACK: u32 = WM_APP + 1;
const MSG_TOGGLE_MENU: u32 = WM_APP + 2;
/// Posted from the Win-key hook to run a Win+<key> hotkey (WPARAM = action id
/// below) from the message loop — LL hooks must return fast.
const MSG_HOTKEY: u32 = WM_APP + 4;
const HOTKEY_RUN: u32 = 1;
const HOTKEY_EXPLORER: u32 = 2;
const HOTKEY_DESKTOP: u32 = 3;
const TIMER_PEEK: usize = 3;
// Defined in the Win32_UI_Controls module of windows-rs; declared here to
// avoid pulling in that entire feature for one constant.
const WM_MOUSELEAVE: u32 = 0x02A3;
const TIMER_CLOCK: usize = 1;
const TIMER_WATCHDOG: usize = 2;

// Colors are COLORREF values (0x00BBGGRR).
pub const COL_BG: u32 = 0x00201F1F;
pub const COL_HOVER: u32 = 0x00353333;
pub const COL_ACTIVE: u32 = 0x00403D3D;
pub const COL_ACCENT: u32 = 0x00D47800; // RGB(0, 120, 212)
pub const COL_TEXT: u32 = 0x00F0F0F0;
pub const COL_TEXT_DIM: u32 = 0x00B4B4B4;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    None,
    Start,
    Task(usize),
    TrayIcon(usize),
    ShowDesktop,
}

/// Width reserved per tray icon (unscaled).
const TRAY_SLOT: i32 = 24;

/// One taskbar button. With combining enabled this can represent several
/// windows of the same application; clicking cycles through them.
struct TaskButton {
    /// Grouping key (exe path with combining, per-window otherwise). Also
    /// used to keep button order stable across refreshes.
    key: String,
    windows: Vec<HWND>,
    title: String,
    icon: Option<HICON>,
    rect: RECT,
    /// For a pinned app: the exe to launch when the button has no open windows.
    /// `None` for ordinary (unpinned) running-window buttons.
    pinned_exe: Option<String>,
}

struct State {
    cfg: Config,
    hwnd: HWND,
    /// Left edge of the start button (cluster may be centered).
    start_x: i32,
    shellhook_msg: u32,
    font: HFONT,
    font_small: HFONT,
    buttons: Vec<TaskButton>,
    /// Pinned taskbar app exe paths (from PinUtil.ini), in pin order. Shown as
    /// buttons even when not running; clicking a not-running pin launches it.
    pinned: Vec<String>,
    /// Snapshot of visible tray icons (drawn left of the clock).
    tray_icons: Vec<HICON>,
    /// Icons extracted from exe files (UWP fallback), keyed by exe path.
    /// Owned by us, kept for the process lifetime.
    icon_cache: HashMap<String, HICON>,
    hover: Hit,
    pressed: Hit,
    tracking_mouse: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    /// True while the Win key is down with no other key pressed since.
    static WIN_PENDING: Cell<bool> = const { Cell::new(false) };
    /// Windows minimized by the last Win+D ("show desktop"); restored on the next.
    static MINIMIZED: RefCell<Vec<HWND>> = const { RefCell::new(Vec::new()) };
}

pub struct Taskbar {
    pub hwnd: HWND,
}

impl Taskbar {
    pub fn create(cfg: &Config) -> Result<Taskbar> {
        unsafe {
            let hinstance: HINSTANCE = GetModuleHandleW(None)?.into();
            let class = w!("StartPE_Taskbar");
            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassW(&wc);

            let height = scaled(cfg.taskbar_height);
            let (sw, sh) = screen_size();

            let font = make_font(scaled(15), 400);
            let font_small = make_font(scaled(12), 400);
            STATE.with_borrow_mut(|s| {
                *s = Some(State {
                    cfg: cfg.clone(),
                    hwnd: HWND::default(),
                    start_x: 0,
                    shellhook_msg: 0,
                    font,
                    font_small,
                    buttons: Vec::new(),
                    pinned: crate::pins::Pins::load().taskbar,
                    tray_icons: Vec::new(),
                    icon_cache: HashMap::new(),
                    hover: Hit::None,
                    pressed: Hit::None,
                    tracking_mouse: false,
                })
            });

            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                class,
                w!("StartPE Taskbar"),
                WS_POPUP | WS_VISIBLE,
                0,
                sh - height,
                sw,
                height,
                None,
                None,
                hinstance,
                None,
            )?;

            log_features(cfg);
            Ok(Taskbar { hwnd })
        }
    }
}

/// Version-stamped record of the taskbar feature config, so a PE log shows
/// which binary (and which settings) is actually running. PE has no Event
/// Viewer, and the user iterates by rebuilding the image.
fn log_features(cfg: &Config) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("X:\\startpe.log")
    {
        let _ = writeln!(
            f,
            "StartPE v{} taskbar: rounded buttons, show-desktop button, \
             locale clock, StartButtonColor=0x{:06X}",
            env!("CARGO_PKG_VERSION"),
            cfg.start_button_color
        );
    }
}

/// Block until Explorer has created the desktop shell (Progman + icon view),
/// or until `timeout_ms` elapses. winrx-creator starts StartPE in PostShell
/// alongside a still-initializing Explorer; hiding the taskbar or creating
/// our own `Shell_TrayWnd` too early can prevent wallpaper and desktop icons
/// from appearing.
pub fn wait_for_explorer_shell_ready(timeout_ms: u32) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
    while std::time::Instant::now() < deadline {
        if explorer_shell_ready() {
            return;
        }
        unsafe {
            Sleep(100);
        }
    }
}

fn explorer_shell_ready() -> bool {
    unsafe {
        let Ok(progman) = FindWindowW(w!("Progman"), None) else {
            return false;
        };
        if progman.is_invalid() || !is_explorer_window(progman) {
            return false;
        }
        let Ok(defview) = FindWindowExW(progman, None, w!("SHELLDLL_DefView"), None) else {
            return false;
        };
        !defview.is_invalid()
    }
}

fn is_explorer_window(hwnd: HWND) -> bool {
    window_exe(hwnd).is_some_and(|p| p.ends_with("\\explorer.exe"))
}

/// Explorer's primary `Shell_TrayWnd`, if any. Used to proxy tray traffic and
/// to avoid confusing our own tray window with Explorer's.
pub fn find_explorer_tray() -> HWND {
    find_explorer_window_by_class(w!("Shell_TrayWnd")).unwrap_or(HWND::default())
}

fn find_explorer_window_by_class(class: PCWSTR) -> Option<HWND> {
    unsafe {
        let mut after = HWND::default();
        loop {
            let Ok(hwnd) = FindWindowExW(None, after, class, None) else {
                break;
            };
            if hwnd.is_invalid() {
                break;
            }
            after = hwnd;
            if is_explorer_window(hwnd) {
                return Some(hwnd);
            }
        }
    }
    None
}

fn for_each_explorer_window(class: PCWSTR, mut f: impl FnMut(HWND)) {
    unsafe {
        let mut after = HWND::default();
        loop {
            let Ok(hwnd) = FindWindowExW(None, after, class, None) else {
                break;
            };
            if hwnd.is_invalid() {
                break;
            }
            after = hwnd;
            if is_explorer_window(hwnd) {
                f(hwnd);
            }
        }
    }
}

/// Put Explorer's taskbar into auto-hide. Hiding the window alone is not
/// enough: its appbar *work-area reservation* stays behind, which pushes our
/// appbar up and leaves a black strip where the old taskbar was. Auto-hide
/// makes Explorer release that reservation; any process may set it.
fn set_explorer_taskbar_state(tray: HWND, autohide: bool) {
    unsafe {
        let mut abd = APPBARDATA {
            cbSize: std::mem::size_of::<APPBARDATA>() as u32,
            hWnd: tray,
            lParam: LPARAM(if autohide { ABS_AUTOHIDE } else { ABS_ALWAYSONTOP } as isize),
            ..Default::default()
        };
        SHAppBarMessage(ABM_SETSTATE, &mut abd);
    }
}

/// Hide Explorer's own taskbar(s) so ours is the only one visible. Explorer
/// stays alive as the shell (desktop, file windows, drag & drop). Called at
/// startup and again from a watchdog timer in case Explorer restarts.
pub fn hide_explorer_taskbar() {
    unsafe {
        for_each_explorer_window(w!("Shell_TrayWnd"), |tray| {
            if IsWindowVisible(tray).as_bool() {
                set_explorer_taskbar_state(tray, true);
                let _ = ShowWindow(tray, SW_HIDE);
            }
        });
        for_each_explorer_window(w!("Shell_SecondaryTrayWnd"), |tray| {
            if IsWindowVisible(tray).as_bool() {
                let _ = ShowWindow(tray, SW_HIDE);
            }
        });
    }
}

/// Undo `hide_explorer_taskbar` — used on clean exit so testing on a full
/// Windows machine leaves the desktop usable.
pub fn show_explorer_taskbar() {
    unsafe {
        for_each_explorer_window(w!("Shell_TrayWnd"), |tray| {
            set_explorer_taskbar_state(tray, false);
            let _ = ShowWindow(tray, SW_SHOW);
        });
        for_each_explorer_window(w!("Shell_SecondaryTrayWnd"), |tray| {
            let _ = ShowWindow(tray, SW_SHOW);
        });
    }
}

/// Make a bare Win-key press open *our* start menu instead of Explorer's.
///
/// A low-level keyboard hook watches for the Win key being pressed and
/// released with no other key in between. The release is swallowed and the
/// key state repaired with synthetic input (a dummy keystroke between down
/// and up — the Open-Shell technique), so Explorer never sees the
/// "bare Win tap" sequence that triggers its start menu. Win+E, Win+R and
/// other combos pass through untouched.
pub fn install_win_key_hook() {
    unsafe {
        // Hook callbacks are delivered to this (installing) thread's message
        // loop, so thread_local state is safe to use inside the hook.
        let _ = SetWindowsHookExW(WH_KEYBOARD_LL, Some(win_key_hook), None, 0);
    }
}

unsafe extern "system" fn win_key_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let injected = kb.flags.0 & LLKHF_INJECTED.0 != 0;
        let down = wparam.0 as u32 == WM_KEYDOWN || wparam.0 as u32 == WM_SYSKEYDOWN;
        let is_win = kb.vkCode == VK_LWIN.0 as u32 || kb.vkCode == VK_RWIN.0 as u32;
        if !injected {
            if is_win {
                if down {
                    WIN_PENDING.set(true);
                } else if WIN_PENDING.replace(false) {
                    // Bare Win tap: eat the real key-up, then resynthesize
                    // dummy-down, dummy-up, Win-up. The dummy key breaks the
                    // start menu sequence; the synthetic Win-up keeps the key
                    // state consistent.
                    let mk = |vk: u16, up: bool| INPUT {
                        r#type: INPUT_KEYBOARD,
                        Anonymous: INPUT_0 {
                            ki: KEYBDINPUT {
                                wVk: VIRTUAL_KEY(vk),
                                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                                ..Default::default()
                            },
                        },
                    };
                    let inputs = [
                        mk(0xFF, false),
                        mk(0xFF, true),
                        mk(kb.vkCode as u16, true),
                    ];
                    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
                    // Toggle from the message loop, not from inside the hook:
                    // LL hooks must return fast or the system drops them.
                    let hwnd = STATE.with_borrow(|s| s.as_ref().map(|s| s.hwnd));
                    if let Some(hwnd) = hwnd {
                        let _ = PostMessageW(hwnd, MSG_TOGGLE_MENU, WPARAM(0), LPARAM(0));
                    }
                    return LRESULT(1);
                }
            } else if down {
                // Any other key while Win is held: it's a combo, not a bare tap.
                WIN_PENDING.set(false);
                // On this PE there's no working shell to handle Win+<key>, so we
                // do it ourselves. Dispatch to the message loop (hooks must be
                // fast) and swallow the key so nothing else sees it.
                let win_held = (GetAsyncKeyState(VK_LWIN.0 as i32) as u32 & 0x8000) != 0
                    || (GetAsyncKeyState(VK_RWIN.0 as i32) as u32 & 0x8000) != 0;
                if win_held {
                    if let Some(id) = win_hotkey(kb.vkCode) {
                        let hwnd = STATE.with_borrow(|s| s.as_ref().map(|s| s.hwnd));
                        if let Some(hwnd) = hwnd {
                            let _ = PostMessageW(hwnd, MSG_HOTKEY, WPARAM(id as usize), LPARAM(0));
                        }
                        return LRESULT(1);
                    }
                }
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// Map a Win+<key> virtual-key code to a hotkey action id, or `None`.
fn win_hotkey(vk: u32) -> Option<u32> {
    match vk {
        0x52 => Some(HOTKEY_RUN),      // R — Run dialog
        0x45 => Some(HOTKEY_EXPLORER), // E — file explorer
        0x44 => Some(HOTKEY_DESKTOP),  // D — show desktop (toggle)
        _ => None,
    }
}

/// Run a command via the shell (same path the start menu uses).
fn run(cmd: &str, args: &str) {
    unsafe {
        let c = util::WideStr::new(cmd);
        let a = util::WideStr::new(args);
        ShellExecuteW(None, w!("open"), c.pcwstr(), a.pcwstr(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// Win+D: minimize all task-bar app windows; pressing again restores them.
unsafe fn toggle_show_desktop() {
    let restore = MINIMIZED.with_borrow(|m| !m.is_empty());
    if restore {
        let wins = MINIMIZED.with_borrow_mut(|m| std::mem::take(m));
        for h in wins {
            let _ = ShowWindow(h, SW_RESTORE);
        }
    } else {
        let wins: Vec<HWND> = STATE.with_borrow(|s| {
            s.as_ref()
                .map(|s| {
                    s.buttons
                        .iter()
                        .flat_map(|b| b.windows.iter().copied())
                        .filter(|h| IsWindowVisible(*h).as_bool() && !IsIconic(*h).as_bool())
                        .collect()
                })
                .unwrap_or_default()
        });
        for &h in &wins {
            let _ = ShowWindow(h, SW_MINIMIZE);
        }
        MINIMIZED.with_borrow_mut(|m| *m = wins);
    }
}

fn screen_size() -> (i32, i32) {
    unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
}

/// Scale a 96-DPI pixel value to the primary monitor's DPI.
pub fn scaled(v: i32) -> i32 {
    unsafe {
        let hdc = GetDC(None);
        let dpi = GetDeviceCaps(hdc, LOGPIXELSY);
        ReleaseDC(None, hdc);
        v * dpi / 96
    }
}

pub fn make_font(height_px: i32, weight: i32) -> HFONT {
    make_font_face(height_px, weight, w!("Segoe UI"))
}

pub fn make_font_face(height_px: i32, weight: i32, face: PCWSTR) -> HFONT {
    unsafe {
        CreateFontW(
            -height_px,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            0,
            face,
        )
    }
}

fn register_appbar(hwnd: HWND, height: i32) {
    unsafe {
        let mut abd = APPBARDATA {
            cbSize: std::mem::size_of::<APPBARDATA>() as u32,
            hWnd: hwnd,
            uCallbackMessage: APPBAR_CALLBACK,
            ..Default::default()
        };
        SHAppBarMessage(ABM_NEW, &mut abd);
        position_appbar(hwnd, height);
    }
}

fn position_appbar(hwnd: HWND, height: i32) {
    unsafe {
        let (sw, sh) = screen_size();
        let mut abd = APPBARDATA {
            cbSize: std::mem::size_of::<APPBARDATA>() as u32,
            hWnd: hwnd,
            uEdge: ABE_BOTTOM,
            rc: RECT {
                left: 0,
                top: sh - height,
                right: sw,
                bottom: sh,
            },
            ..Default::default()
        };
        SHAppBarMessage(ABM_QUERYPOS, &mut abd);
        // QUERYPOS may shrink the rect to dodge other appbars (e.g. Explorer's
        // hidden-but-still-registered taskbar before our auto-hide takes
        // effect). We always own the bottom edge.
        abd.rc.bottom = sh;
        abd.rc.top = sh - height;
        SHAppBarMessage(ABM_SETPOS, &mut abd);
        let _ = MoveWindow(
            hwnd,
            abd.rc.left,
            abd.rc.top,
            abd.rc.right - abd.rc.left,
            abd.rc.bottom - abd.rc.top,
            true,
        );
    }
}

// ---------------------------------------------------------------------------
// Window enumeration

fn is_task_window(hwnd: HWND, own: HWND) -> bool {
    unsafe {
        if hwnd == own || !IsWindowVisible(hwnd).as_bool() {
            return false;
        }
        if GetWindowTextLengthW(hwnd) == 0 {
            return false;
        }
        let exstyle = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if exstyle & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }
        // DWM-cloaked windows (suspended UWP frame hosts, etc.) report as
        // visible but render nothing — skip them or they show up as dead,
        // icon-less buttons. Harmless no-op where DWM is absent (PE).
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        );
        if cloaked != 0 {
            return false;
        }
        // Zero-area windows can't be real task windows either.
        let mut wr = RECT::default();
        if GetWindowRect(hwnd, &mut wr).is_ok() && (wr.right <= wr.left || wr.bottom <= wr.top) {
            return false;
        }
        if let Ok(owner) = GetWindow(hwnd, GW_OWNER) {
            if !owner.is_invalid() {
                return false;
            }
        }
        !matches!(
            window_class(hwnd).as_str(),
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "StartPE_Taskbar" | "StartPE_Menu"
        )
    }
}

fn window_class(hwnd: HWND) -> String {
    unsafe {
        let mut buf = [0u16; 64];
        let n = GetClassNameW(hwnd, &mut buf) as usize;
        String::from_utf16_lossy(&buf[..n])
    }
}

fn window_title(hwnd: HWND) -> String {
    unsafe {
        let mut buf = [0u16; 256];
        let n = GetWindowTextW(hwnd, &mut buf) as usize;
        String::from_utf16_lossy(&buf[..n])
    }
}

fn window_icon(hwnd: HWND) -> Option<HICON> {
    unsafe {
        // Prefer the largest published icon. The small icons are 16px and look
        // blurry once upscaled to a task button (drawn at scaled(24)), so ask
        // for the big icon first and only fall back to the small ones.
        for wparam in [ICON_BIG as usize, ICON_SMALL as usize, ICON_SMALL2 as usize] {
            let mut result: usize = 0;
            let _ = SendMessageTimeoutW(
                hwnd,
                WM_GETICON,
                WPARAM(wparam),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                100,
                Some(&mut result),
            );
            if result != 0 {
                return Some(HICON(result as *mut _));
            }
        }
        let h = GetClassLongPtrW(hwnd, GCLP_HICON);
        if h != 0 {
            return Some(HICON(h as *mut _));
        }
        let h = GetClassLongPtrW(hwnd, GCLP_HICONSM);
        if h != 0 {
            return Some(HICON(h as *mut _));
        }
        None
    }
}

/// Full path of the process owning `hwnd`, lowercased — the grouping key.
fn window_exe(hwnd: HWND) -> Option<String> {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(process);
        result.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]).to_lowercase())
    }
}

/// Exe path of the application a window really belongs to. UWP windows are
/// hosted by ApplicationFrameHost.exe; the actual app (e.g. SystemSettings)
/// owns the CoreWindow child, so resolve through it for correct grouping
/// and icons.
fn effective_exe(hwnd: HWND) -> Option<String> {
    unsafe {
        if window_class(hwnd) == "ApplicationFrameWindow" {
            if let Ok(core) = FindWindowExW(hwnd, None, w!("Windows.UI.Core.CoreWindow"), None) {
                if !core.is_invalid() {
                    if let Some(exe) = window_exe(core) {
                        return Some(exe);
                    }
                }
            }
        }
        window_exe(hwnd)
    }
}

/// Icon extracted from an exe file — the fallback when a window publishes no
/// icon of its own (typical for UWP). Cached because extraction is not free
/// and refresh runs every few seconds.
fn cached_exe_icon(cache: &mut HashMap<String, HICON>, exe: &str) -> Option<HICON> {
    if let Some(h) = cache.get(exe) {
        return Some(*h);
    }
    unsafe {
        // PrivateExtractIconsW wants a fixed MAX_PATH buffer; copy the path in,
        // truncating defensively (PE exe paths are short).
        let mut path = [0u16; 260];
        for (dst, src) in path.iter_mut().zip(exe.encode_utf16()).take(259) {
            *dst = src;
        }
        // Pull the best-matching image from the exe's icon group rather than the
        // fixed 32px system "large" icon. PrivateExtractIconsW picks the closest
        // size in the resource set for the requested dimension, so a high-DPI
        // taskbar gets a 48/256px source instead of an upscaled 32px one.
        let size = scaled(32).max(32);
        let mut hicon = [HICON::default(); 1];
        let n = PrivateExtractIconsW(&path, 0, size, size, Some(&mut hicon), None, 0);
        if n > 0 && !hicon[0].is_invalid() {
            cache.insert(exe.to_string(), hicon[0]);
            return Some(hicon[0]);
        }
    }
    None
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut (HWND, Vec<HWND>));
    if is_task_window(hwnd, ctx.0) {
        ctx.1.push(hwnd);
    }
    TRUE
}

// ---------------------------------------------------------------------------
// Layout

/// Horizontal space the start button occupies. Matches the icon-only task
/// button square (`height + scaled(8)`) so the start button reads as just
/// another taskbar icon rather than an oversized slab.
fn start_width(height: i32) -> i32 {
    height + scaled(8)
}

/// Drawn/hit rect of the start button: a square inset vertically by the same
/// `scaled(3)` margin as the task buttons, so its hover background lines up
/// with theirs. Hit-testing only checks the x range, so the inset is cosmetic.
fn start_rect_at(x: i32, height: i32) -> RECT {
    RECT {
        left: x,
        top: scaled(3),
        right: x + start_width(height),
        bottom: height - scaled(3),
    }
}

/// Peek data for the hovered button: entries, button screen rect, taskbar top.
fn peek_request(index: usize) -> Option<(Vec<crate::peek::PeekEntry>, RECT, i32)> {
    STATE.with_borrow(|s| {
        let s = s.as_ref()?;
        let b = s.buttons.get(index)?;
        let mut wr = RECT::default();
        unsafe {
            GetWindowRect(s.hwnd, &mut wr).ok()?;
        }
        let entries = b
            .windows
            .iter()
            .map(|&w| crate::peek::PeekEntry {
                hwnd: w,
                title: window_title(w),
                // Fall back to the button icon (covers UWP windows).
                icon: window_icon(w).or(b.icon),
            })
            .collect();
        let anchor = RECT {
            left: wr.left + b.rect.left,
            top: wr.top,
            right: wr.left + b.rect.right,
            bottom: wr.bottom,
        };
        Some((entries, anchor, wr.top))
    })
}

fn show_peek(index: usize) {
    if let Some((entries, anchor, top)) = peek_request(index) {
        crate::peek::show(entries, anchor, top);
    }
}

/// Width of the show-desktop sliver at the far right edge (unscaled).
const SHOW_DESKTOP_SLIVER: i32 = 12;

/// The thin "show desktop / minimize all" button pinned to the far right edge,
/// like the Windows peek button.
fn show_desktop_rect(width: i32, height: i32) -> RECT {
    RECT {
        left: width - scaled(SHOW_DESKTOP_SLIVER),
        top: 0,
        right: width,
        bottom: height,
    }
}

fn clock_rect(width: i32, height: i32) -> RECT {
    let right = width - scaled(SHOW_DESKTOP_SLIVER);
    RECT {
        left: right - scaled(86),
        top: 0,
        right,
        bottom: height,
    }
}

/// Rect of tray icon `i` out of `n`, laid out right-to-left from the clock.
fn tray_icon_rect(i: usize, n: usize, width: i32, height: i32) -> RECT {
    let slot = scaled(TRAY_SLOT);
    let right_edge = clock_rect(width, height).left - scaled(4);
    let left = right_edge - (n as i32 - i as i32) * slot;
    RECT {
        left,
        top: 0,
        right: left + slot,
        bottom: height,
    }
}

fn tray_area_left(n: usize, width: i32, height: i32) -> i32 {
    clock_rect(width, height).left - scaled(4) - n as i32 * scaled(TRAY_SLOT)
}

fn client_size(hwnd: HWND) -> (i32, i32) {
    unsafe {
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        (rc.right - rc.left, rc.bottom - rc.top)
    }
}

fn refresh_buttons(state: &mut State) {
    unsafe {
        let mut ctx: (HWND, Vec<HWND>) = (state.hwnd, Vec::new());
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut ctx as *mut _ as isize));

        // Group by owning exe. Without combining, every window gets its own
        // button and the key is unique.
        let mut fresh: Vec<TaskButton> = Vec::new();
        for hwnd in ctx.1 {
            let exe = effective_exe(hwnd);
            let key = if state.cfg.combine {
                exe.clone().unwrap_or_else(|| format!("hwnd:{:?}", hwnd.0))
            } else {
                format!("hwnd:{:?}", hwnd.0)
            };
            if let Some(b) = fresh.iter_mut().find(|b| b.key == key) {
                b.windows.push(hwnd);
            } else {
                // UWP windows publish no icon — fall back to the app exe's.
                let icon = window_icon(hwnd)
                    .or_else(|| exe.and_then(|e| cached_exe_icon(&mut state.icon_cache, &e)));
                fresh.push(TaskButton {
                    key,
                    windows: vec![hwnd],
                    title: window_title(hwnd),
                    icon,
                    rect: RECT::default(),
                    pinned_exe: None,
                });
            }
        }

        // EnumWindows yields Z-order, which changes on every activation.
        // Keep button order stable: surviving buttons stay where they were,
        // new windows append at the end (like the real taskbar).
        let mut buttons: Vec<TaskButton> = Vec::with_capacity(fresh.len());
        for old in &state.buttons {
            if let Some(pos) = fresh.iter().position(|b| b.key == old.key) {
                buttons.push(fresh.remove(pos));
            }
        }
        buttons.extend(fresh);

        // Pinned apps come first, in pin order: a running app adopts its pinned
        // slot (so it doesn't also appear later), and a pin with no open window
        // becomes a launch button. Unpinned running buttons follow, in their
        // existing stable order.
        if !state.pinned.is_empty() {
            let pins = state.pinned.clone();
            let mut slots: Vec<Option<TaskButton>> = buttons.into_iter().map(Some).collect();
            let mut ordered: Vec<TaskButton> = Vec::with_capacity(slots.len() + pins.len());
            for pin in &pins {
                let hit = slots.iter().position(|b| {
                    b.as_ref().is_some_and(|b| b.key.eq_ignore_ascii_case(pin))
                });
                match hit {
                    Some(pos) => {
                        let mut b = slots[pos].take().unwrap();
                        b.pinned_exe = Some(pin.clone());
                        ordered.push(b);
                    }
                    None => ordered.push(TaskButton {
                        key: pin.clone(),
                        windows: Vec::new(),
                        title: util::app_display_name(pin),
                        icon: cached_exe_icon(&mut state.icon_cache, pin),
                        rect: RECT::default(),
                        pinned_exe: Some(pin.clone()),
                    }),
                }
            }
            ordered.extend(slots.into_iter().flatten());
            buttons = ordered;
        }

        let (width, height) = client_size(state.hwnd);
        state.tray_icons = crate::tray::snapshot();
        let right_bound = tray_area_left(state.tray_icons.len(), width, height);
        let avail = (right_bound - scaled(8) - start_width(height) - scaled(4)).max(0);
        let n = buttons.len() as i32;
        let max_w = if state.cfg.show_labels {
            scaled(state.cfg.button_max_width)
        } else {
            // Icon-only: a roughly square button.
            height + scaled(8)
        };
        let bw = if n > 0 { (avail / n).min(max_w) } else { 0 };

        // Win11-style: center start button + task buttons as one cluster
        // (clamped so it never slides under the tray/clock).
        let cluster_w = start_width(height) + scaled(4) + n * bw;
        let max_left = (right_bound - scaled(4) - cluster_w).max(scaled(4));
        state.start_x = if state.cfg.center_taskbar {
            ((width - cluster_w) / 2).clamp(scaled(4), max_left)
        } else {
            scaled(0)
        };

        let tasks_left = state.start_x + start_width(height) + scaled(4);
        for (i, b) in buttons.iter_mut().enumerate() {
            let x = tasks_left + bw * i as i32;
            b.rect = RECT {
                left: x,
                top: scaled(3),
                right: x + bw - scaled(2),
                bottom: height - scaled(3),
            };
        }
        state.buttons = buttons;
    }
}

/// Launch a pinned program (clicked while it has no open window).
fn launch_path(path: &str) {
    unsafe {
        let wp = util::WideStr::new(path);
        ShellExecuteW(
            None,
            w!("open"),
            wp.pcwstr(),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Right-click on empty taskbar: a small context menu with the system admin
/// tools most useful in a PE. Resolved-then-acted outside any STATE borrow.
fn show_taskbar_menu(hwnd: HWND) {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    let cmd = crate::menu::track(
        hwnd,
        pt.x,
        pt.y,
        TRACK_POPUP_MENU_FLAGS(0),
        &[
            (1, "Task Manager"),
            (2, "Computer Management"),
            (0, ""), // separator: StartPE's own settings live below
            (3, "Settings"),
        ],
        false,
    );
    match cmd {
        1 => run("taskmgr.exe", ""),
        2 => run("mmc.exe", "compmgmt.msc"),
        3 => crate::settings::open(),
        _ => {}
    }
}

/// Whether the start button + task buttons are centered (vs left-aligned). Read
/// live from the current config, so the start menu can match the button's edge
/// even after the setting is toggled at runtime.
pub fn is_centered() -> bool {
    STATE.with_borrow(|s| s.as_ref().map(|s| s.cfg.center_taskbar).unwrap_or(true))
}

/// The Start button glyph color (COLORREF), read live from config. The start
/// menu uses it as its accent so the two match (and track a runtime change).
pub fn start_button_color() -> u32 {
    STATE.with_borrow(|s| {
        s.as_ref()
            .map(|s| s.cfg.start_button_color)
            .unwrap_or(0x00E6_5AB4)
    })
}

/// Re-read the registry config and apply the changes that can take effect live
/// (button combining, labels, centering — all recomputed in `refresh_buttons`).
/// Called by the settings pane after it writes a value. Settings that need the
/// windows recreated (the hosted desktop, dark-menu app mode) are persisted for
/// the next launch but not re-applied here.
pub fn reload_config() {
    let cfg = Config::load();
    let hwnd = STATE.with_borrow_mut(|s| {
        s.as_mut().map(|s| {
            s.cfg = cfg;
            refresh_buttons(s);
            s.hwnd
        })
    });
    if let Some(hwnd) = hwnd {
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
    }
}

fn hit_test(state: &State, x: i32, y: i32) -> Hit {
    let (width, height) = client_size(state.hwnd);
    let sr = start_rect_at(state.start_x, height);
    if x >= sr.left && x < sr.right {
        return Hit::Start;
    }
    for (i, b) in state.buttons.iter().enumerate() {
        if x >= b.rect.left && x < b.rect.right && y >= b.rect.top && y < b.rect.bottom {
            return Hit::Task(i);
        }
    }
    let n = state.tray_icons.len();
    for i in 0..n {
        let r = tray_icon_rect(i, n, width, height);
        if x >= r.left && x < r.right {
            return Hit::TrayIcon(i);
        }
    }
    let sd = show_desktop_rect(width, height);
    if x >= sd.left && x < sd.right {
        return Hit::ShowDesktop;
    }
    Hit::None
}

// ---------------------------------------------------------------------------
// Actions

fn activate_window(hwnd: HWND) {
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        } else if GetForegroundWindow() == hwnd {
            let _ = ShowWindow(hwnd, SW_MINIMIZE);
        } else {
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

/// Click behavior for a (possibly combined) button: single window toggles
/// like a classic taskbar; a group cycles through its windows.
fn activate_group(windows: &[HWND]) {
    unsafe {
        if windows.is_empty() {
            return;
        }
        if windows.len() == 1 {
            activate_window(windows[0]);
            return;
        }
        let foreground = GetForegroundWindow();
        if let Some(pos) = windows.iter().position(|&w| w == foreground) {
            let next = windows[(pos + 1) % windows.len()];
            if IsIconic(next).as_bool() {
                let _ = ShowWindow(next, SW_RESTORE);
            }
            let _ = SetForegroundWindow(next);
        } else {
            activate_window(windows[0]);
        }
    }
}

// ---------------------------------------------------------------------------
// Painting

fn paint(state: &State) {
    unsafe {
        let hwnd = state.hwnd;
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let (width, height) = client_size(hwnd);

        // Double buffer.
        let mem = CreateCompatibleDC(hdc);
        let bmp = CreateCompatibleBitmap(hdc, width, height);
        let old_bmp = SelectObject(mem, bmp);

        let bg = CreateSolidBrush(COLORREF(COL_BG));
        let full = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        FillRect(mem, &full, bg);
        let _ = DeleteObject(bg);

        SetBkMode(mem, TRANSPARENT);

        draw_start_button(state, mem, height);
        draw_task_buttons(state, mem);
        draw_tray(state, mem, width, height);
        draw_clock(state, mem, width, height);
        draw_show_desktop(state, mem, width, height);

        let _ = BitBlt(hdc, 0, 0, width, height, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old_bmp);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem);
        let _ = EndPaint(hwnd, &ps);
    }
}

fn fill(hdc: HDC, rect: &RECT, color: u32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color));
        FillRect(hdc, rect, brush);
        let _ = DeleteObject(brush);
    }
}

/// Filled rounded rectangle (plain GDI `RoundRect`, no DWM needed). The pen
/// matches the fill so the rounded edge has no hard 1px border. `radius` is the
/// corner diameter in device pixels.
fn fill_rounded(hdc: HDC, rect: &RECT, color: u32, radius: i32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color));
        let pen = CreatePen(PS_SOLID, 1, COLORREF(color));
        let old_brush = SelectObject(hdc, brush);
        let old_pen = SelectObject(hdc, pen);
        let _ = RoundRect(hdc, rect.left, rect.top, rect.right, rect.bottom, radius, radius);
        SelectObject(hdc, old_brush);
        SelectObject(hdc, old_pen);
        let _ = DeleteObject(brush);
        let _ = DeleteObject(pen);
    }
}

fn draw_start_button(state: &State, hdc: HDC, height: i32) {
    {
        let rect = start_rect_at(state.start_x, height);
        if state.hover == Hit::Start {
            fill_rounded(hdc, &rect, COL_HOVER, scaled(8));
        }
        // Four-square logo, centered. Color is configurable (StartButtonColor).
        let glyph = state.cfg.start_button_color;
        let sq = scaled(7);
        let gap = scaled(2);
        let total = sq * 2 + gap;
        let cx = (rect.left + rect.right - total) / 2;
        let cy = (rect.top + rect.bottom - total) / 2;
        for (dx, dy) in [(0, 0), (sq + gap, 0), (0, sq + gap), (sq + gap, sq + gap)] {
            let r = RECT {
                left: cx + dx,
                top: cy + dy,
                right: cx + dx + sq,
                bottom: cy + dy + sq,
            };
            fill(hdc, &r, glyph);
        }
    }
}

fn draw_task_buttons(state: &State, hdc: HDC) {
    unsafe {
        let foreground = GetForegroundWindow();
        let old_font = SelectObject(hdc, state.font);
        for (i, b) in state.buttons.iter().enumerate() {
            let active = b.windows.contains(&foreground);
            if active {
                fill_rounded(hdc, &b.rect, COL_ACTIVE, scaled(8));
            } else if state.hover == Hit::Task(i) {
                fill_rounded(hdc, &b.rect, COL_HOVER, scaled(8));
            }

            // Underline indicator: dim when running, accent when active, split
            // into segments when the button combines several windows. A pinned
            // app with no open window has no segments (and no underline).
            let segments = b.windows.len().min(3) as i32;
            if segments > 0 {
                let line_color = if active { COL_ACCENT } else { COL_TEXT_DIM };
                let line_w = b.rect.right - b.rect.left;
                let seg_w = (line_w - scaled(2) * (segments - 1)) / segments;
                for s in 0..segments {
                    let x = b.rect.left + s * (seg_w + scaled(2));
                    let line = RECT {
                        left: x,
                        top: b.rect.bottom - scaled(2),
                        right: if s == segments - 1 { b.rect.right } else { x + seg_w },
                        bottom: b.rect.bottom,
                    };
                    fill(hdc, &line, line_color);
                }
            }

            if state.cfg.show_labels {
                let icon_size = scaled(16);
                let icon_y = (b.rect.top + b.rect.bottom - icon_size) / 2;
                let mut text_left = b.rect.left + scaled(6);
                if let Some(icon) = b.icon {
                    let _ = DrawIconEx(
                        hdc,
                        text_left,
                        icon_y,
                        icon,
                        icon_size,
                        icon_size,
                        0,
                        None,
                        DI_NORMAL,
                    );
                    text_left += icon_size + scaled(6);
                }
                SetTextColor(hdc, COLORREF(COL_TEXT));
                let mut tr = RECT {
                    left: text_left,
                    top: b.rect.top,
                    right: b.rect.right - scaled(4),
                    bottom: b.rect.bottom,
                };
                if tr.right > tr.left {
                    let mut text = util::wide(&b.title);
                    // Drop the NUL; DrawTextW takes a slice.
                    text.pop();
                    DrawTextW(
                        hdc,
                        &mut text,
                        &mut tr,
                        DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
                    );
                }
            } else {
                // Icon-only: larger icon, centered.
                let icon_size = scaled(24);
                let icon_x = (b.rect.left + b.rect.right - icon_size) / 2;
                let icon_y = (b.rect.top + b.rect.bottom - icon_size) / 2 - scaled(1);
                if let Some(icon) = b.icon {
                    let _ = DrawIconEx(
                        hdc,
                        icon_x,
                        icon_y,
                        icon,
                        icon_size,
                        icon_size,
                        0,
                        None,
                        DI_NORMAL,
                    );
                }
            }
        }
        SelectObject(hdc, old_font);
    }
}

fn draw_tray(state: &State, hdc: HDC, width: i32, height: i32) {
    unsafe {
        let n = state.tray_icons.len();
        for (i, &icon) in state.tray_icons.iter().enumerate() {
            let r = tray_icon_rect(i, n, width, height);
            if state.hover == Hit::TrayIcon(i) {
                fill(hdc, &r, COL_HOVER);
            }
            if icon.is_invalid() {
                continue;
            }
            let sz = scaled(16);
            let _ = DrawIconEx(
                hdc,
                (r.left + r.right - sz) / 2,
                (r.top + r.bottom - sz) / 2,
                icon,
                sz,
                sz,
                0,
                None,
                DI_NORMAL,
            );
        }
    }
}

/// Locale-formatted time without seconds (e.g. "10:30 AM" or "22:30"), exactly
/// as the standard Windows clock shows it. Falls back to ISO on failure.
fn locale_time() -> String {
    unsafe {
        let mut buf = [0u16; 64];
        let n = GetTimeFormatEx(PCWSTR::null(), TIME_NOSECONDS, None, PCWSTR::null(), Some(&mut buf));
        if n > 1 {
            return String::from_utf16_lossy(&buf[..(n - 1) as usize]);
        }
        let st = GetLocalTime();
        format!("{:02}:{:02}", st.wHour, st.wMinute)
    }
}

/// Locale short date (e.g. "6/13/2026"). Falls back to ISO on failure.
fn locale_date() -> String {
    unsafe {
        let mut buf = [0u16; 64];
        let n = GetDateFormatEx(
            PCWSTR::null(),
            DATE_SHORTDATE,
            None,
            PCWSTR::null(),
            Some(&mut buf),
            PCWSTR::null(),
        );
        if n > 1 {
            return String::from_utf16_lossy(&buf[..(n - 1) as usize]);
        }
        let st = GetLocalTime();
        format!("{:04}-{:02}-{:02}", st.wYear, st.wMonth, st.wDay)
    }
}

fn draw_clock(state: &State, hdc: HDC, width: i32, height: i32) {
    unsafe {
        let rect = clock_rect(width, height);
        let time = locale_time();
        let date = locale_date();

        // Two right-aligned lines centered as a block: time (bright) over date
        // (dim), matching the standard Windows taskbar clock.
        let mid = (rect.top + rect.bottom) / 2;
        let right = rect.right - scaled(8);

        SetTextColor(hdc, COLORREF(COL_TEXT));
        let old_font = SelectObject(hdc, state.font);
        let mut tr = RECT {
            left: rect.left,
            top: rect.top,
            right,
            bottom: mid,
        };
        let mut time_w = util::wide(&time);
        time_w.pop();
        DrawTextW(hdc, &mut time_w, &mut tr, DT_SINGLELINE | DT_RIGHT | DT_BOTTOM);

        SetTextColor(hdc, COLORREF(COL_TEXT_DIM));
        SelectObject(hdc, state.font_small);
        let mut dr = RECT {
            left: rect.left,
            top: mid + scaled(1),
            right,
            bottom: rect.bottom,
        };
        let mut date_w = util::wide(&date);
        date_w.pop();
        DrawTextW(hdc, &mut date_w, &mut dr, DT_SINGLELINE | DT_RIGHT | DT_TOP);
        SelectObject(hdc, old_font);
    }
}

/// The far-right show-desktop sliver: a thin separator with a hover highlight,
/// like the Windows "peek at desktop" button.
fn draw_show_desktop(state: &State, hdc: HDC, width: i32, height: i32) {
    let r = show_desktop_rect(width, height);
    if state.hover == Hit::ShowDesktop {
        fill(hdc, &r, COL_HOVER);
    }
    // Thin vertical separator on the left edge of the sliver.
    let sep = RECT {
        left: r.left,
        top: scaled(6),
        right: r.left + 1,
        bottom: height - scaled(6),
    };
    fill(hdc, &sep, COL_TEXT_DIM);
}

// ---------------------------------------------------------------------------
// Window procedure

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Custom (registered) shell hook message: window list changed.
    let shellhook = STATE.with_borrow(|s| s.as_ref().map(|s| s.shellhook_msg).unwrap_or(0));
    if shellhook != 0 && msg == shellhook {
        STATE.with_borrow_mut(|s| {
            if let Some(s) = s.as_mut() {
                refresh_buttons(s);
            }
        });
        let _ = InvalidateRect(hwnd, None, false);
        return LRESULT(0);
    }

    match msg {
        WM_CREATE => {
            let height = STATE.with_borrow_mut(|s| {
                let s = s.as_mut().unwrap();
                s.hwnd = hwnd;
                s.shellhook_msg = RegisterWindowMessageW(w!("SHELLHOOK"));
                scaled(s.cfg.taskbar_height)
            });
            register_appbar(hwnd, height);
            let _ = RegisterShellHookWindow(hwnd);
            SetTimer(hwnd, TIMER_CLOCK, 1000, None);
            SetTimer(hwnd, TIMER_WATCHDOG, 3000, None);
            STATE.with_borrow_mut(|s| refresh_buttons(s.as_mut().unwrap()));
            LRESULT(0)
        }
        WM_TIMER => {
            match wparam.0 {
                TIMER_CLOCK => {
                    let (w, h) = client_size(hwnd);
                    let rect = clock_rect(w, h);
                    let _ = InvalidateRect(hwnd, Some(&rect), false);
                }
                TIMER_PEEK => {
                    let _ = KillTimer(hwnd, TIMER_PEEK);
                    let hover = STATE.with_borrow(|s| s.as_ref().map(|s| s.hover));
                    if let Some(Hit::Task(i)) = hover {
                        show_peek(i);
                    }
                }
                TIMER_WATCHDOG => {
                    // Re-hide Explorer's taskbar if it restarted, and catch
                    // title changes the shell hook does not deliver.
                    hide_explorer_taskbar();
                    crate::tray::prune();
                    crate::tray::raise();
                    // If appbar negotiation left us off the bottom edge
                    // (e.g. Explorer's reservation released late), re-dock.
                    let height = STATE.with_borrow(|s| {
                        s.as_ref().map(|s| scaled(s.cfg.taskbar_height)).unwrap_or(scaled(40))
                    });
                    let mut rc = RECT::default();
                    let _ = GetWindowRect(hwnd, &mut rc);
                    let (_, sh) = screen_size();
                    if rc.bottom != sh || rc.top != sh - height {
                        position_appbar(hwnd, height);
                    }
                    STATE.with_borrow_mut(|s| {
                        if let Some(s) = s.as_mut() {
                            refresh_buttons(s);
                        }
                    });
                    let _ = InvalidateRect(hwnd, None, false);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = util::loword(lparam.0);
            let y = util::hiword(lparam.0);
            let mut track = false;
            let (changed, hit) = STATE.with_borrow_mut(|s| {
                let s = s.as_mut().unwrap();
                let hit = hit_test(s, x, y);
                let changed = hit != s.hover;
                s.hover = hit;
                if !s.tracking_mouse {
                    s.tracking_mouse = true;
                    track = true;
                }
                (changed, hit)
            });
            if track {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
            }
            if changed {
                // Peek: switch instantly when one is already open, otherwise
                // open after a short hover delay.
                match hit {
                    Hit::Task(i) => {
                        if crate::peek::is_visible() {
                            show_peek(i);
                        } else {
                            SetTimer(hwnd, TIMER_PEEK, 400, None);
                        }
                    }
                    _ => {
                        let _ = KillTimer(hwnd, TIMER_PEEK);
                    }
                }
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            STATE.with_borrow_mut(|s| {
                let s = s.as_mut().unwrap();
                s.tracking_mouse = false;
                s.hover = Hit::None;
            });
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = util::loword(lparam.0);
            let y = util::hiword(lparam.0);
            STATE.with_borrow_mut(|s| {
                let s = s.as_mut().unwrap();
                s.pressed = hit_test(s, x, y);
            });
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let x = util::loword(lparam.0);
            let y = util::hiword(lparam.0);
            enum Click {
                None,
                Start,
                Group(Vec<HWND>),
                Launch(String),
                Tray(usize),
                ShowDesktop,
            }
            // Resolve the action inside the borrow, perform it outside: the
            // action may pump messages that re-enter this wndproc.
            let action = STATE.with_borrow_mut(|s| {
                let s = s.as_mut().unwrap();
                let hit = hit_test(s, x, y);
                let pressed = std::mem::replace(&mut s.pressed, Hit::None);
                if hit != pressed {
                    return Click::None;
                }
                match hit {
                    Hit::Start => Click::Start,
                    Hit::Task(i) => match s.buttons.get(i) {
                        Some(b) if !b.windows.is_empty() => Click::Group(b.windows.clone()),
                        Some(b) => match &b.pinned_exe {
                            Some(exe) => Click::Launch(exe.clone()),
                            None => Click::None,
                        },
                        None => Click::None,
                    },
                    Hit::TrayIcon(i) => Click::Tray(i),
                    Hit::ShowDesktop => Click::ShowDesktop,
                    Hit::None => Click::None,
                }
            });
            crate::peek::hide();
            match action {
                Click::Start => start_menu::toggle(),
                Click::Group(windows) => activate_group(&windows),
                Click::Launch(exe) => launch_path(&exe),
                Click::Tray(i) => crate::tray::click(i, false),
                Click::ShowDesktop => toggle_show_desktop(),
                Click::None => {}
            }
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            let x = util::loword(lparam.0);
            let y = util::hiword(lparam.0);
            // Resolve under the borrow, act outside it (TrackPopupMenu and the
            // tray forward both pump messages and re-enter this wndproc).
            enum RClick {
                None,
                Tray(usize),
                Menu,
            }
            let action = STATE.with_borrow(|s| {
                s.as_ref().map_or(RClick::None, |s| match hit_test(s, x, y) {
                    Hit::TrayIcon(i) => RClick::Tray(i),
                    Hit::None => RClick::Menu,
                    _ => RClick::None,
                })
            });
            match action {
                RClick::Tray(i) => crate::tray::click(i, true),
                RClick::Menu => show_taskbar_menu(hwnd),
                RClick::None => {}
            }
            LRESULT(0)
        }
        MSG_TOGGLE_MENU => {
            start_menu::toggle();
            LRESULT(0)
        }
        MSG_HOTKEY => {
            match wparam.0 as u32 {
                HOTKEY_RUN => {
                    // Use the taskbar's top edge to seat the dialog above it.
                    let mut rc = RECT::default();
                    let _ = GetWindowRect(hwnd, &mut rc);
                    crate::run_dialog::show(rc.top);
                }
                HOTKEY_EXPLORER => run("explorer.exe", "shell:MyComputerFolder"),
                HOTKEY_DESKTOP => toggle_show_desktop(),
                _ => {}
            }
            LRESULT(0)
        }
        crate::tray::MSG_TRAY_CHANGED => {
            STATE.with_borrow_mut(|s| {
                if let Some(s) = s.as_mut() {
                    refresh_buttons(s);
                }
            });
            let _ = InvalidateRect(hwnd, None, false);
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
            STATE.with_borrow(|s| {
                if let Some(s) = s.as_ref() {
                    paint(s);
                }
            });
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            let height = STATE.with_borrow(|s| {
                s.as_ref().map(|s| scaled(s.cfg.taskbar_height)).unwrap_or(scaled(40))
            });
            position_appbar(hwnd, height);
            LRESULT(0)
        }
        APPBAR_CALLBACK => {
            if wparam.0 as u32 == ABN_POSCHANGED {
                let height = STATE.with_borrow(|s| {
                    s.as_ref().map(|s| scaled(s.cfg.taskbar_height)).unwrap_or(scaled(40))
                });
                position_appbar(hwnd, height);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let mut abd = APPBARDATA {
                cbSize: std::mem::size_of::<APPBARDATA>() as u32,
                hWnd: hwnd,
                ..Default::default()
            };
            SHAppBarMessage(ABM_REMOVE, &mut abd);
            show_explorer_taskbar();
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
