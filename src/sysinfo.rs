// SPDX-License-Identifier: GPL-3.0-or-later
//! A from-scratch, dark **System Information** window — StartPE's replacement for
//! msinfo32 and the sysdm.cpl summary page. PE has little software to report, so
//! the content is hardware-first: a System summary plus CPU & memory, graphics &
//! displays, and storage & network sections.
//!
//! Like [`crate::run_window`], it is a borderless `WS_POPUP` we own and paint
//! entirely with double-buffered GDI in the StartPE dark palette — no DWM, so it
//! renders identically in plain PE. It is **fixed size** (overflow scrolls with
//! the wheel), deliberately avoiding any custom-chrome resizing. It reuses the
//! taskbar's fonts/palette and the same accent as the Start button
//! ([`crate::taskbar::start_button_color`]), so the three match.
//!
//! Hardware is gathered on a background thread (WMI init is slow in PE): the
//! window opens immediately showing "Gathering…", the worker collects a plain
//! [`SysInfo`] (WMI over `ROOT\CIMV2`, with documented Win32/registry fallbacks),
//! then `PostMessage`s it back to the UI thread, which stores and repaints. STATE
//! is only ever touched on the UI thread.

use std::cell::{Cell, RefCell};

use windows::core::{w, BSTR, PCWSTR, PWSTR, VARIANT};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
use windows::Win32::System::SystemInformation::*;
use windows::Win32::System::Wmi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::taskbar::{
    make_font, make_font_face, scaled, COL_ACTIVE, COL_BG, COL_HOVER, COL_TEXT, COL_TEXT_DIM,
};

const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_APP_DATA: u32 = WM_APP + 1; // worker -> UI: SysInfo ready (LPARAM = Box<SysInfo>)

// Layout metrics in 96-DPI px (run through `scaled`).
const WIDTH: i32 = 720;
const HEIGHT: i32 = 520;
const TITLE_H: i32 = 30;
const CLOSE: i32 = 30;
const NAV_W: i32 = 192;
const NAV_TOP: i32 = 8;
const NAV_ITEM_H: i32 = 46;
const PAD: i32 = 16;
const LABEL_W: i32 = 150;
const HEAD_GAP: i32 = 16;
const HEAD_H: i32 = 24;
const ROW_H: i32 = 24;
const GAP_H: i32 = 8;

const GLYPH_CLOSE: char = '\u{E8BB}'; // ChromeClose
const GLYPH_TITLE: char = '\u{E946}'; // Info

/// Nav sections: (label, Segoe MDL2 glyph).
const SECTIONS: [(&str, char); 4] = [
    ("System", '\u{E770}'),          // System
    ("CPU & Memory", '\u{E950}'),    // Processor
    ("Graphics & Displays", '\u{E7F4}'), // TVMonitor
    ("Storage & Network", '\u{EDA2}'),   // Hard drive
];

// ---- collected data -------------------------------------------------------

#[derive(Default, Clone)]
pub struct MemModule {
    pub capacity: u64, // bytes
    pub speed: u32,    // MT/s
    pub slot: String,
}

#[derive(Default, Clone)]
pub struct Gpu {
    pub name: String,
    pub vram: u64, // bytes
    pub driver: String,
}

#[derive(Default, Clone)]
pub struct Disk {
    pub model: String,
    pub size: u64, // bytes
    pub bus: String,
}

#[derive(Default, Clone)]
pub struct Nic {
    pub name: String,
    pub mac: String,
}

#[derive(Default, Clone)]
pub struct SysInfo {
    pub os_caption: String,
    pub os_version: String,
    pub os_build: String,
    pub computer_name: String,
    pub manufacturer: String,
    pub model: String,
    pub system_type: String,
    pub cpu_name: String,
    pub cpu_cores: u32,
    pub cpu_threads: u32,
    pub cpu_clock_mhz: u32,
    pub cpu_arch: String,
    pub board: String,
    pub bios: String,
    pub ram_total: u64,
    pub ram_avail: u64,
    pub mem_modules: Vec<MemModule>,
    pub gpus: Vec<Gpu>,
    pub displays: Vec<String>,
    pub disks: Vec<Disk>,
    pub nics: Vec<Nic>,
}

// ---- window state ---------------------------------------------------------

struct State {
    hwnd: HWND,
    accent: u32,
    section: usize,
    hover: i32, // -1 none, -2 close, >=0 nav index
    scroll: Cell<i32>,
    content_h: Cell<i32>,
    tracking: bool,
    info: Option<SysInfo>,
    font: HFONT,
    font_head: HFONT,
    font_title: HFONT,
    font_nav: HFONT,
    font_glyph: HFONT,
    /// Smaller MDL2 font for the title-bar icon + close glyph (the nav uses the
    /// larger `font_glyph`).
    font_glyph_title: HFONT,
    /// Accent-tinted window icons (taskbar / Alt+Tab), destroyed on close.
    icon_big: HICON,
    icon_small: HICON,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    /// True when launched as `startpe.exe --sysinfo` (a dedicated process): then
    /// closing the window must quit this process's message loop. When false (the
    /// in-process Win+X / Win+Pause path) closing must NOT post WM_QUIT — that
    /// would tear down the taskbar's loop.
    static STANDALONE: Cell<bool> = const { Cell::new(false) };
}

/// Entry point for `startpe.exe --sysinfo`: show the window and pump messages
/// until it closes, then return (the process exits). Used by the PE image's
/// sysdm.cpl / "This PC → Properties" redirection.
pub fn run_standalone() {
    // Single instance: focus an existing System Information window (built-in or
    // the GTK helper — both titled "System Information") instead of opening a
    // second one.
    if unsafe { focus_existing() } {
        return;
    }
    // This is also where "This PC → Properties" / sysdm.cpl lands, so honor the
    // external-app redirect here too: prefer the configured GTK `SystemInfo.exe`.
    if let Some(app) = crate::config::sysinfo_app() {
        if spawn_external(&app) {
            return;
        }
    }
    STANDALONE.with(|f| f.set(true));
    show();
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Launch System Information. Prefers the external GTK app configured in
/// `SysInfoApp` (the libadwaita `SystemInfo.exe` helper); otherwise opens our
/// built-in window as a separate `startpe.exe --sysinfo` process. Either way the
/// shell treats it as an ordinary app (taskbar/Alt+Tab, normal Z order, accent
/// border). Called from the taskbar process (Win+X → System, Win+Pause).
pub fn launch() {
    // Repeated presses focus the open window rather than stacking new ones; the
    // built-in window and the GTK helper share the title "System Information".
    if unsafe { focus_existing() } {
        return;
    }
    if let Some(app) = crate::config::sysinfo_app() {
        if spawn_external(&app) {
            return;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Ok(child) = std::process::Command::new(exe).arg("--sysinfo").spawn() {
            // Let the new process come to the foreground (it isn't yet, so its
            // own SetForegroundWindow would otherwise be denied).
            unsafe {
                let _ = AllowSetForegroundWindow(child.id());
            }
        }
    }
}

/// Focus an already-open System Information window (built-in or the GTK helper —
/// both use the window title "System Information"). Returns true if one was found.
unsafe fn focus_existing() -> bool {
    if let Ok(h) = FindWindowW(PCWSTR::null(), w!("System Information")) {
        if !h.is_invalid() {
            let _ = SetForegroundWindow(h);
            return true;
        }
    }
    false
}

/// Spawn the configured external System Information app (the GTK `SystemInfo.exe`
/// helper). Returns false if it couldn't be started, so the caller falls back to
/// the built-in window. The child inherits StartPE's token (SYSTEM in the PE) and
/// environment (where the GTK4 runtime is on `PATH`).
fn spawn_external(app: &str) -> bool {
    match std::process::Command::new(app).spawn() {
        Ok(child) => {
            unsafe {
                let _ = AllowSetForegroundWindow(child.id());
            }
            log_redirect(app);
            true
        }
        Err(_) => false,
    }
}

fn log_redirect(app: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("X:\\startpe.log")
    {
        let _ = writeln!(
            f,
            "StartPE v{} System Information -> external app: {app}",
            env!("CARGO_PKG_VERSION")
        );
    }
}

struct Layout {
    width: i32,
    height: i32,
    close: RECT,
    nav: [RECT; 4],
    content: RECT,
}

fn layout() -> Layout {
    let w = scaled(WIDTH);
    let h = scaled(HEIGHT);
    let title = scaled(TITLE_H);
    let close = RECT {
        left: w - scaled(CLOSE),
        top: 0,
        right: w,
        bottom: scaled(CLOSE),
    };
    let nav_top = title + scaled(NAV_TOP);
    let item = scaled(NAV_ITEM_H);
    let nav = std::array::from_fn(|i| {
        let top = nav_top + i as i32 * item;
        RECT {
            left: scaled(6),
            top,
            right: scaled(NAV_W) - scaled(6),
            bottom: top + item - scaled(4),
        }
    });
    let content = RECT {
        left: scaled(NAV_W) + scaled(PAD),
        top: title + scaled(PAD),
        right: w - scaled(PAD),
        bottom: h - scaled(PAD),
    };
    Layout {
        width: w,
        height: h,
        close,
        nav,
        content,
    }
}

/// Open the System Information window (or focus it if already open).
pub fn show() {
    unsafe {
        let existing = STATE.with_borrow(|s| s.as_ref().map(|s| s.hwnd));
        if let Some(hwnd) = existing {
            if IsWindow(hwnd).as_bool() {
                let _ = SetForegroundWindow(hwnd);
                return;
            }
        }

        let Ok(hinstance) = GetModuleHandleW(None) else {
            return;
        };
        let hinstance: HINSTANCE = hinstance.into();
        let class = w!("StartPE_SysInfo");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc); // idempotent

        let lay = layout();
        // Center on the work area (excludes the taskbar/appbar reservation).
        let mut wa = RECT::default();
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut wa as *mut _ as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        let x = wa.left + ((wa.right - wa.left) - lay.width) / 2;
        let y = wa.top + ((wa.bottom - wa.top) - lay.height) / 2;

        let hwnd = CreateWindowExW(
            // WS_EX_APPWINDOW (not WS_EX_TOOLWINDOW) so StartPE's taskbar and
            // Alt+Tab list it like any app window.
            WS_EX_APPWINDOW,
            class,
            w!("System Information"),
            WS_POPUP,
            x.max(0),
            y.max(0),
            lay.width,
            lay.height,
            None,
            None,
            hinstance,
            None,
        );
        let Ok(hwnd) = hwnd else {
            return;
        };

        // Rounded corners via a GDI region (no DWM needed in PE). Radius 8 to
        // match the accent window border (border.rs CORNER).
        let rgn = CreateRoundRectRgn(0, 0, lay.width + 1, lay.height + 1, scaled(16), scaled(16));
        let _ = SetWindowRgn(hwnd, rgn, true);

        // Accent-tinted icon (matches the title glyph) for the taskbar / Alt+Tab,
        // set per-window via WM_SETICON so StartPE's WM_GETICON probe finds it.
        let accent = crate::taskbar::start_button_color();
        let icon_big = make_glyph_icon(GLYPH_TITLE, accent, scaled(32));
        let icon_small = make_glyph_icon(GLYPH_TITLE, accent, scaled(16));
        // ICON_BIG = 1, ICON_SMALL = 0.
        SendMessageW(hwnd, WM_SETICON, WPARAM(1), LPARAM(icon_big.0 as isize));
        SendMessageW(hwnd, WM_SETICON, WPARAM(0), LPARAM(icon_small.0 as isize));

        STATE.with_borrow_mut(|s| {
            *s = Some(State {
                hwnd,
                accent,
                section: 0,
                hover: -1,
                scroll: Cell::new(0),
                content_h: Cell::new(0),
                tracking: false,
                info: None,
                font: make_font(scaled(13), 400),
                font_head: make_font(scaled(14), 700),
                font_title: make_font(scaled(13), 400),
                font_nav: make_font(scaled(13), 400),
                font_glyph: make_font_face(scaled(16), 400, w!("Segoe MDL2 Assets")),
                font_glyph_title: make_font_face(scaled(13), 400, w!("Segoe MDL2 Assets")),
                icon_big,
                icon_small,
            });
        });

        let _ = ShowWindow(hwnd, SW_SHOW);
        // Raise above everything, then drop back to the normal band — opens in
        // front but stays an ordinary window. SetWindowPos Z-order changes don't
        // need foreground rights (a just-spawned process's SetForegroundWindow
        // can be denied).
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        let _ = SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        let _ = SetForegroundWindow(hwnd);
        log_open();

        // Gather on a worker thread (WMI is slow); post the result back. HWND
        // isn't Send, so ferry it as an isize and rebuild it on the worker.
        let hwnd_val = hwnd.0 as isize;
        std::thread::spawn(move || {
            let info = gather();
            let boxed = Box::into_raw(Box::new(info));
            let target = HWND(hwnd_val as *mut core::ffi::c_void);
            let _ = PostMessageW(target, WM_APP_DATA, WPARAM(0), LPARAM(boxed as isize));
        });
    }
}

fn log_open() {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("X:\\startpe.log")
    {
        let _ = writeln!(
            f,
            "StartPE v{} System Information window opened",
            env!("CARGO_PKG_VERSION")
        );
    }
}

fn lo(l: isize) -> i32 {
    (l & 0xFFFF) as i16 as i32
}
fn hi(l: isize) -> i32 {
    ((l >> 16) & 0xFFFF) as i16 as i32
}

fn point_in(rc: &RECT, x: i32, y: i32) -> bool {
    x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom
}

/// Which interactive element is at (x, y): -2 = close, 0..4 = nav, -1 = none.
fn hit(x: i32, y: i32) -> i32 {
    let lay = layout();
    if point_in(&lay.close, x, y) {
        return -2;
    }
    for (i, r) in lay.nav.iter().enumerate() {
        if point_in(r, x, y) {
            return i as i32;
        }
    }
    -1
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1), // painted in WM_PAINT (double-buffered)
        WM_ACTIVATE => {
            // Repaint so the accent ring switches accent <-> gray with focus.
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_PAINT => {
            STATE.with_borrow(|s| {
                if let Some(s) = s.as_ref() {
                    paint(s);
                }
            });
            LRESULT(0)
        }
        WM_APP_DATA => {
            let ptr = lp.0 as *mut SysInfo;
            if !ptr.is_null() {
                let info = *Box::from_raw(ptr);
                STATE.with_borrow_mut(|s| {
                    if let Some(s) = s.as_mut() {
                        s.info = Some(info);
                        s.scroll.set(0);
                    }
                });
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = lo(lp.0);
            let y = hi(lp.0);
            let h = hit(x, y);
            STATE.with_borrow_mut(|s| {
                if let Some(s) = s.as_mut() {
                    if !s.tracking {
                        let mut tme = TRACKMOUSEEVENT {
                            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE,
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        };
                        let _ = TrackMouseEvent(&mut tme);
                        s.tracking = true;
                    }
                    if s.hover != h {
                        s.hover = h;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
            });
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            STATE.with_borrow_mut(|s| {
                if let Some(s) = s.as_mut() {
                    s.tracking = false;
                    if s.hover != -1 {
                        s.hover = -1;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
            });
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = lo(lp.0);
            let y = hi(lp.0);
            let h = hit(x, y);
            if h == -2 {
                let _ = DestroyWindow(hwnd);
            } else if h >= 0 {
                STATE.with_borrow_mut(|s| {
                    if let Some(s) = s.as_mut() {
                        if s.section != h as usize {
                            s.section = h as usize;
                            s.scroll.set(0);
                            let _ = InvalidateRect(hwnd, None, false);
                        }
                    }
                });
            } else if y < scaled(TITLE_H) {
                // Drag the window by its title bar.
                let _ = ReleaseCapture();
                SendMessageW(hwnd, WM_NCLBUTTONDOWN, WPARAM(HTCAPTION as usize), LPARAM(0));
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wp.0 >> 16) & 0xFFFF) as i16 as i32;
            STATE.with_borrow(|s| {
                if let Some(s) = s.as_ref() {
                    let step = scaled(40) * delta / 120;
                    let max = s.content_h.get();
                    let next = (s.scroll.get() - step).clamp(0, max);
                    if next != s.scroll.get() {
                        s.scroll.set(next);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
            });
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let vk = VIRTUAL_KEY(wp.0 as u16);
            let mut repaint = false;
            STATE.with_borrow_mut(|s| {
                if let Some(s) = s.as_mut() {
                    match vk {
                        VK_ESCAPE => {
                            let _ = DestroyWindow(hwnd);
                            return;
                        }
                        VK_UP => {
                            if s.section > 0 {
                                s.section -= 1;
                                s.scroll.set(0);
                                repaint = true;
                            }
                        }
                        VK_DOWN => {
                            if s.section < SECTIONS.len() - 1 {
                                s.section += 1;
                                s.scroll.set(0);
                                repaint = true;
                            }
                        }
                        VK_PRIOR | VK_NEXT => {
                            let dir = if vk == VK_PRIOR { -1 } else { 1 };
                            let max = s.content_h.get();
                            let next = (s.scroll.get() + dir * scaled(200)).clamp(0, max);
                            if next != s.scroll.get() {
                                s.scroll.set(next);
                                repaint = true;
                            }
                        }
                        VK_HOME => {
                            s.scroll.set(0);
                            repaint = true;
                        }
                        _ => {}
                    }
                }
            });
            if repaint {
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            STATE.with_borrow_mut(|s| {
                if let Some(s) = s.take() {
                    for f in [
                        s.font,
                        s.font_head,
                        s.font_title,
                        s.font_nav,
                        s.font_glyph,
                        s.font_glyph_title,
                    ] {
                        let _ = DeleteObject(HGDIOBJ(f.0));
                    }
                    let _ = DestroyIcon(s.icon_big);
                    let _ = DestroyIcon(s.icon_small);
                }
            });
            // A dedicated --sysinfo process ends its loop when the window closes;
            // the in-process path must leave the taskbar's loop running.
            if STANDALONE.with(|f| f.get()) {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ---- content rows ---------------------------------------------------------

enum Row {
    Head(String),
    Kv(String, String),
    Gap,
}

fn dash(s: &str) -> String {
    if s.is_empty() {
        "\u{2014}".into()
    } else {
        s.to_string()
    }
}

fn num(n: u32) -> String {
    if n == 0 {
        "\u{2014}".into()
    } else {
        n.to_string()
    }
}

fn fmt_bytes(b: u64) -> String {
    if b == 0 {
        return "\u{2014}".into();
    }
    let gb = b as f64 / (1024.0 * 1024.0 * 1024.0);
    if gb >= 1.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{:.0} MB", b as f64 / (1024.0 * 1024.0))
    }
}

fn fmt_mhz(mhz: u32) -> String {
    if mhz == 0 {
        "\u{2014}".into()
    } else if mhz >= 1000 {
        format!("{:.2} GHz", mhz as f64 / 1000.0)
    } else {
        format!("{mhz} MHz")
    }
}

fn build_rows(section: usize, info: &SysInfo) -> Vec<Row> {
    let mut r = Vec::new();
    let kv = |r: &mut Vec<Row>, k: &str, v: String| r.push(Row::Kv(k.to_string(), v));
    match section {
        0 => {
            r.push(Row::Head("Operating system".into()));
            kv(&mut r, "Edition", dash(&info.os_caption));
            kv(&mut r, "Version", dash(&info.os_version));
            kv(&mut r, "Build", dash(&info.os_build));
            r.push(Row::Head("Device".into()));
            kv(&mut r, "Device name", dash(&info.computer_name));
            kv(&mut r, "Manufacturer", dash(&info.manufacturer));
            kv(&mut r, "Model", dash(&info.model));
            kv(&mut r, "System type", dash(&info.system_type));
            r.push(Row::Head("Processor".into()));
            kv(&mut r, "CPU", dash(&info.cpu_name));
            r.push(Row::Head("Memory".into()));
            kv(&mut r, "Installed RAM", fmt_bytes(info.ram_total));
        }
        1 => {
            r.push(Row::Head("Processor".into()));
            kv(&mut r, "Model", dash(&info.cpu_name));
            kv(&mut r, "Cores", num(info.cpu_cores));
            kv(&mut r, "Logical processors", num(info.cpu_threads));
            kv(&mut r, "Max clock", fmt_mhz(info.cpu_clock_mhz));
            kv(&mut r, "Architecture", dash(&info.cpu_arch));
            r.push(Row::Head("Firmware".into()));
            kv(&mut r, "Baseboard", dash(&info.board));
            kv(&mut r, "BIOS", dash(&info.bios));
            r.push(Row::Head("Physical memory".into()));
            kv(&mut r, "Total", fmt_bytes(info.ram_total));
            kv(&mut r, "Available", fmt_bytes(info.ram_avail));
            if info.mem_modules.is_empty() {
                kv(&mut r, "Modules", "\u{2014}".into());
            } else {
                for (i, m) in info.mem_modules.iter().enumerate() {
                    let slot = if m.slot.is_empty() {
                        format!("Slot {}", i + 1)
                    } else {
                        m.slot.clone()
                    };
                    let speed = if m.speed > 0 {
                        format!(" @ {} MT/s", m.speed)
                    } else {
                        String::new()
                    };
                    kv(&mut r, &slot, format!("{}{}", fmt_bytes(m.capacity), speed));
                }
            }
        }
        2 => {
            r.push(Row::Head("Graphics".into()));
            if info.gpus.is_empty() {
                kv(&mut r, "GPU", "\u{2014}".into());
            } else {
                let many = info.gpus.len() > 1;
                for (i, g) in info.gpus.iter().enumerate() {
                    let label = if many {
                        format!("GPU {}", i + 1)
                    } else {
                        "GPU".into()
                    };
                    kv(&mut r, &label, dash(&g.name));
                    if g.vram > 0 {
                        kv(&mut r, "  VRAM", fmt_bytes(g.vram));
                    }
                    if !g.driver.is_empty() {
                        kv(&mut r, "  Driver", g.driver.clone());
                    }
                    r.push(Row::Gap);
                }
            }
            r.push(Row::Head("Displays".into()));
            if info.displays.is_empty() {
                kv(&mut r, "Displays", "\u{2014}".into());
            } else {
                for (i, d) in info.displays.iter().enumerate() {
                    kv(&mut r, &format!("Display {}", i + 1), d.clone());
                }
            }
        }
        _ => {
            r.push(Row::Head("Disks".into()));
            if info.disks.is_empty() {
                kv(&mut r, "Disks", "\u{2014}".into());
            } else {
                for (i, d) in info.disks.iter().enumerate() {
                    kv(&mut r, &format!("Disk {}", i + 1), dash(&d.model));
                    kv(&mut r, "  Capacity", fmt_bytes(d.size));
                    if !d.bus.is_empty() {
                        kv(&mut r, "  Bus", d.bus.clone());
                    }
                    r.push(Row::Gap);
                }
            }
            r.push(Row::Head("Network adapters".into()));
            if info.nics.is_empty() {
                kv(&mut r, "Adapters", "\u{2014}".into());
            } else {
                for n in &info.nics {
                    let label = if n.name.is_empty() { "Adapter" } else { &n.name };
                    kv(&mut r, label, n.mac.clone());
                }
            }
        }
    }
    r
}

fn rows_height(rows: &[Row]) -> i32 {
    let mut h = 0;
    for row in rows {
        h += match row {
            Row::Head(_) => scaled(HEAD_GAP) + scaled(HEAD_H),
            Row::Kv(..) => scaled(ROW_H),
            Row::Gap => scaled(GAP_H),
        };
    }
    h
}

// ---- painting -------------------------------------------------------------

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn fill(hdc: HDC, rc: &RECT, color: u32) {
    unsafe {
        let b = CreateSolidBrush(COLORREF(color));
        FillRect(hdc, rc, b);
        let _ = DeleteObject(HGDIOBJ(b.0));
    }
}

fn fill_round(hdc: HDC, rc: &RECT, color: u32, radius: i32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color));
        let pen = CreatePen(PS_SOLID, 1, COLORREF(color));
        let ob = SelectObject(hdc, HGDIOBJ(brush.0));
        let op = SelectObject(hdc, HGDIOBJ(pen.0));
        let _ = RoundRect(hdc, rc.left, rc.top, rc.right, rc.bottom, radius, radius);
        SelectObject(hdc, ob);
        SelectObject(hdc, op);
        let _ = DeleteObject(HGDIOBJ(brush.0));
        let _ = DeleteObject(HGDIOBJ(pen.0));
    }
}

fn draw_text(hdc: HDC, font: HFONT, color: u32, s: &str, mut rc: RECT, flags: DRAW_TEXT_FORMAT) {
    if s.is_empty() {
        return;
    }
    unsafe {
        SelectObject(hdc, HGDIOBJ(font.0));
        SetTextColor(hdc, COLORREF(color));
        let mut t = wide(s);
        DrawTextW(hdc, &mut t, &mut rc, flags);
    }
}

fn draw_glyph(hdc: HDC, font: HFONT, color: u32, ch: char, rc: RECT) {
    unsafe {
        SelectObject(hdc, HGDIOBJ(font.0));
        SetTextColor(hdc, COLORREF(color));
        let mut g = [ch as u16];
        let mut r = rc;
        DrawTextW(
            hdc,
            &mut g,
            &mut r,
            DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
        );
    }
}

/// Build a `size`px `HICON` from a Segoe MDL2 `glyph`, tinted `color`
/// (COLORREF 0x00BBGGRR) with an antialiased alpha edge — so the window's
/// taskbar / Alt+Tab icon matches its title glyph. We draw the glyph white into
/// a 32bpp DIB (GDI leaves alpha at 0), then read its luminance as the alpha
/// coverage and recolor to `color`, premultiplied.
/// Build an HICON of a single Segoe MDL2 glyph tinted `color`, sized `size`.
/// Shared with `run_window` for its taskbar icon.
pub(crate) unsafe fn make_glyph_icon(glyph: char, color: u32, size: i32) -> HICON {
    let (cr, cg, cb) = (color & 0xFF, (color >> 8) & 0xFF, (color >> 16) & 0xFF);

    let screen = GetDC(None);
    let dc = CreateCompatibleDC(screen);
    ReleaseDC(None, screen);

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0, // BI_RGB
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let Ok(dib) = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
        let _ = DeleteDC(dc);
        return HICON::default();
    };
    let old = SelectObject(dc, HGDIOBJ(dib.0));

    let font = CreateFontW(
        size * 72 / 100,
        0,
        0,
        0,
        400,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        ANTIALIASED_QUALITY.0 as u32,
        0,
        w!("Segoe MDL2 Assets"),
    );
    let oldf = SelectObject(dc, HGDIOBJ(font.0));
    SetBkMode(dc, TRANSPARENT);
    SetTextColor(dc, COLORREF(0x00FF_FFFF));
    let mut g = [glyph as u16];
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: size,
        bottom: size,
    };
    DrawTextW(
        dc,
        &mut g,
        &mut rc,
        DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
    );
    let _ = GdiFlush();

    // Recolor: alpha = drawn (white) intensity, color premultiplied by it.
    let px = bits as *mut u32;
    for i in 0..(size * size) as isize {
        let p = *px.offset(i);
        let a = (p & 0xFF).max((p >> 8) & 0xFF).max((p >> 16) & 0xFF);
        let (r, gr, b) = (cr * a / 255, cg * a / 255, cb * a / 255);
        *px.offset(i) = (a << 24) | (r << 16) | (gr << 8) | b;
    }

    SelectObject(dc, oldf);
    let _ = DeleteObject(HGDIOBJ(font.0));
    SelectObject(dc, old);
    let _ = DeleteDC(dc);

    // 32bpp alpha drives transparency; the mask just needs to exist (all-opaque).
    let mask = CreateBitmap(size, size, 1, 1, None);
    let ii = ICONINFO {
        fIcon: TRUE,
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask,
        hbmColor: dib,
    };
    let icon = CreateIconIndirect(&ii).unwrap_or_default();
    let _ = DeleteObject(HGDIOBJ(dib.0));
    let _ = DeleteObject(HGDIOBJ(mask.0));
    icon
}

fn paint(state: &State) {
    unsafe {
        let hwnd = state.hwnd;
        let lay = layout();
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let (width, height) = (rc.right, rc.bottom);

        let mem = CreateCompatibleDC(hdc);
        let bmp = CreateCompatibleBitmap(hdc, width, height);
        let old_bmp = SelectObject(mem, bmp);

        fill(mem, &rc, COL_BG);
        SetBkMode(mem, TRANSPARENT);

        // Title bar.
        let title_bar = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: scaled(TITLE_H),
        };
        fill(mem, &title_bar, COL_HOVER);
        draw_glyph(
            mem,
            state.font_glyph_title,
            state.accent,
            GLYPH_TITLE,
            RECT {
                left: scaled(10),
                top: 0,
                right: scaled(10) + scaled(18),
                bottom: scaled(TITLE_H),
            },
        );
        draw_text(
            mem,
            state.font_title,
            COL_TEXT,
            "System Information",
            RECT {
                left: scaled(34),
                top: 0,
                right: width - scaled(CLOSE),
                bottom: scaled(TITLE_H),
            },
            DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX,
        );
        draw_glyph(
            mem,
            state.font_glyph_title,
            if state.hover == -2 {
                COL_TEXT
            } else {
                COL_TEXT_DIM
            },
            GLYPH_CLOSE,
            lay.close,
        );

        // Left nav.
        for (i, r) in lay.nav.iter().enumerate() {
            let selected = i == state.section;
            let hovered = state.hover == i as i32;
            if selected {
                fill_round(mem, r, COL_ACTIVE, scaled(6));
            } else if hovered {
                fill_round(mem, r, COL_HOVER, scaled(6));
            }
            let (glyph_col, text_col) = if selected {
                (state.accent, COL_TEXT)
            } else {
                (COL_TEXT_DIM, COL_TEXT_DIM)
            };
            draw_glyph(
                mem,
                state.font_glyph,
                glyph_col,
                SECTIONS[i].1,
                RECT {
                    left: r.left + scaled(6),
                    top: r.top,
                    right: r.left + scaled(36),
                    bottom: r.bottom,
                },
            );
            draw_text(
                mem,
                state.font_nav,
                text_col,
                SECTIONS[i].0,
                RECT {
                    left: r.left + scaled(40),
                    top: r.top,
                    right: r.right - scaled(6),
                    bottom: r.bottom,
                },
                DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
        }
        // Separator between nav and content.
        let sep = RECT {
            left: scaled(NAV_W),
            top: scaled(TITLE_H),
            right: scaled(NAV_W) + 1,
            bottom: height,
        };
        fill(mem, &sep, COL_HOVER);

        // Content.
        match &state.info {
            None => {
                draw_text(
                    mem,
                    state.font,
                    COL_TEXT_DIM,
                    "Gathering system information\u{2026}",
                    lay.content,
                    DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
                );
            }
            Some(info) => {
                let rows = build_rows(state.section, info);
                let total = rows_height(&rows);
                let visible = lay.content.bottom - lay.content.top;
                let max = (total - visible).max(0);
                let scroll = state.scroll.get().clamp(0, max);
                state.scroll.set(scroll);
                state.content_h.set(max);

                // Clip drawing to the content rect so scrolled rows don't bleed.
                IntersectClipRect(
                    mem,
                    lay.content.left,
                    lay.content.top,
                    lay.content.right,
                    lay.content.bottom,
                );
                let mut y = lay.content.top - scroll;
                for row in &rows {
                    match row {
                        Row::Head(t) => {
                            y += scaled(HEAD_GAP);
                            draw_text(
                                mem,
                                state.font_head,
                                state.accent,
                                t,
                                RECT {
                                    left: lay.content.left,
                                    top: y,
                                    right: lay.content.right,
                                    bottom: y + scaled(HEAD_H),
                                },
                                DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX,
                            );
                            y += scaled(HEAD_H);
                        }
                        Row::Kv(k, v) => {
                            draw_text(
                                mem,
                                state.font,
                                COL_TEXT_DIM,
                                k,
                                RECT {
                                    left: lay.content.left,
                                    top: y,
                                    right: lay.content.left + scaled(LABEL_W),
                                    bottom: y + scaled(ROW_H),
                                },
                                DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX | DT_END_ELLIPSIS,
                            );
                            draw_text(
                                mem,
                                state.font,
                                COL_TEXT,
                                v,
                                RECT {
                                    left: lay.content.left + scaled(LABEL_W),
                                    top: y,
                                    right: lay.content.right,
                                    bottom: y + scaled(ROW_H),
                                },
                                DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX | DT_END_ELLIPSIS,
                            );
                            y += scaled(ROW_H);
                        }
                        Row::Gap => y += scaled(GAP_H),
                    }
                }
                SelectClipRgn(mem, None);
            }
        }

        // 1px ring (borderless window: accent when focused, gray otherwise).
        let ring = RECT { left: 0, top: 0, right: width, bottom: height };
        crate::taskbar::accent_ring(mem, hwnd, &ring, scaled(16));
        let _ = BitBlt(hdc, 0, 0, width, height, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old_bmp);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        let _ = EndPaint(hwnd, &ps);
    }
}

// ---- data collection ------------------------------------------------------

/// Collect everything; safe to call on a background thread.
fn gather() -> SysInfo {
    let mut info = SysInfo::default();
    unsafe {
        let _ = gather_wmi(&mut info);
        gather_win32(&mut info);
    }
    info
}

/// Read property `name` off a WMI object as a trimmed string. `VARIANT`'s
/// `Display` coerces BSTR/numeric/bool values to text via `PropVariantToBSTR`,
/// and the `VARIANT` clears itself on drop.
unsafe fn prop(obj: &IWbemClassObject, name: &str) -> Option<String> {
    let wname: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut var = VARIANT::default();
    obj.Get(PCWSTR(wname.as_ptr()), 0, &mut var, None, None).ok()?;
    let s = var.to_string().trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

unsafe fn prop_u64(obj: &IWbemClassObject, name: &str) -> Option<u64> {
    prop(obj, name).and_then(|s| s.parse::<u64>().ok())
}

unsafe fn prop_u32(obj: &IWbemClassObject, name: &str) -> Option<u32> {
    prop(obj, name).and_then(|s| s.parse::<u32>().ok())
}

unsafe fn query<F: FnMut(&IWbemClassObject)>(svc: &IWbemServices, wql: &str, mut f: F) {
    let Ok(enumerator) = svc.ExecQuery(
        &BSTR::from("WQL"),
        &BSTR::from(wql),
        WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
        None,
    ) else {
        return;
    };
    loop {
        let mut objs: [Option<IWbemClassObject>; 1] = [None];
        let mut returned: u32 = 0;
        let _ = enumerator.Next(WBEM_INFINITE as i32, &mut objs, &mut returned);
        if returned == 0 {
            break;
        }
        if let Some(obj) = objs[0].take() {
            f(&obj);
        }
    }
}

unsafe fn gather_wmi(info: &mut SysInfo) -> windows::core::Result<()> {
    CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
    let r = gather_wmi_inner(info);
    CoUninitialize();
    r
}

unsafe fn gather_wmi_inner(info: &mut SysInfo) -> windows::core::Result<()> {
    // Process-wide security; harmless if already set (returns RPC_E_TOO_LATE).
    let _ = CoInitializeSecurity(
        None,
        -1,
        None,
        None,
        RPC_C_AUTHN_LEVEL_DEFAULT,
        RPC_C_IMP_LEVEL_IMPERSONATE,
        None,
        EOAC_NONE,
        None,
    );

    let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)?;
    let svc: IWbemServices = locator.ConnectServer(
        &BSTR::from("ROOT\\CIMV2"),
        &BSTR::new(),
        &BSTR::new(),
        &BSTR::new(),
        0,
        &BSTR::new(),
        None,
    )?;
    CoSetProxyBlanket(
        &svc,
        RPC_C_AUTHN_WINNT,
        RPC_C_AUTHZ_NONE,
        None,
        RPC_C_AUTHN_LEVEL_CALL,
        RPC_C_IMP_LEVEL_IMPERSONATE,
        None,
        EOAC_NONE,
    )?;

    query(
        &svc,
        "SELECT Caption,Version,BuildNumber FROM Win32_OperatingSystem",
        |o| {
            if let Some(v) = prop(o, "Caption") {
                info.os_caption = v;
            }
            if let Some(v) = prop(o, "Version") {
                info.os_version = v;
            }
            if let Some(v) = prop(o, "BuildNumber") {
                info.os_build = v;
            }
        },
    );
    query(
        &svc,
        "SELECT Name,Manufacturer,Model,SystemType,TotalPhysicalMemory FROM Win32_ComputerSystem",
        |o| {
            if let Some(v) = prop(o, "Name") {
                info.computer_name = v;
            }
            if let Some(v) = prop(o, "Manufacturer") {
                info.manufacturer = v;
            }
            if let Some(v) = prop(o, "Model") {
                info.model = v;
            }
            if let Some(v) = prop(o, "SystemType") {
                info.system_type = v;
            }
            if let Some(v) = prop_u64(o, "TotalPhysicalMemory") {
                info.ram_total = v;
            }
        },
    );
    query(
        &svc,
        "SELECT Name,NumberOfCores,NumberOfLogicalProcessors,MaxClockSpeed FROM Win32_Processor",
        |o| {
            if info.cpu_name.is_empty() {
                if let Some(v) = prop(o, "Name") {
                    info.cpu_name = v;
                }
                if let Some(v) = prop_u32(o, "NumberOfCores") {
                    info.cpu_cores = v;
                }
                if let Some(v) = prop_u32(o, "NumberOfLogicalProcessors") {
                    info.cpu_threads = v;
                }
                if let Some(v) = prop_u32(o, "MaxClockSpeed") {
                    info.cpu_clock_mhz = v;
                }
            }
        },
    );
    query(&svc, "SELECT Manufacturer,Product FROM Win32_BaseBoard", |o| {
        let m = prop(o, "Manufacturer").unwrap_or_default();
        let p = prop(o, "Product").unwrap_or_default();
        info.board = format!("{m} {p}").trim().to_string();
    });
    query(
        &svc,
        "SELECT SMBIOSBIOSVersion,ReleaseDate FROM Win32_BIOS",
        |o| {
            let ver = prop(o, "SMBIOSBIOSVersion").unwrap_or_default();
            let date = prop(o, "ReleaseDate").unwrap_or_default();
            let date = if date.len() >= 8 {
                format!("{}-{}-{}", &date[0..4], &date[4..6], &date[6..8])
            } else {
                date
            };
            info.bios = format!("{ver} ({date})").trim().to_string();
        },
    );
    query(
        &svc,
        "SELECT Capacity,Speed,DeviceLocator FROM Win32_PhysicalMemory",
        |o| {
            info.mem_modules.push(MemModule {
                capacity: prop_u64(o, "Capacity").unwrap_or(0),
                speed: prop_u32(o, "Speed").unwrap_or(0),
                slot: prop(o, "DeviceLocator").unwrap_or_default(),
            });
        },
    );
    query(
        &svc,
        "SELECT Name,AdapterRAM,DriverVersion FROM Win32_VideoController",
        |o| {
            let name = prop(o, "Name").unwrap_or_default();
            if name.is_empty() {
                return;
            }
            info.gpus.push(Gpu {
                name,
                vram: prop_u64(o, "AdapterRAM").unwrap_or(0),
                driver: prop(o, "DriverVersion").unwrap_or_default(),
            });
        },
    );
    query(
        &svc,
        "SELECT Model,Size,InterfaceType FROM Win32_DiskDrive",
        |o| {
            let model = prop(o, "Model").unwrap_or_default();
            if model.is_empty() {
                return;
            }
            info.disks.push(Disk {
                model,
                size: prop_u64(o, "Size").unwrap_or(0),
                bus: prop(o, "InterfaceType").unwrap_or_default(),
            });
        },
    );
    query(
        &svc,
        "SELECT Name,MACAddress FROM Win32_NetworkAdapter WHERE PhysicalAdapter=TRUE",
        |o| {
            let mac = prop(o, "MACAddress").unwrap_or_default();
            if mac.is_empty() {
                return;
            }
            info.nics.push(Nic {
                name: prop(o, "Name").unwrap_or_default(),
                mac,
            });
        },
    );

    Ok(())
}

unsafe fn gather_win32(info: &mut SysInfo) {
    let mut si = SYSTEM_INFO::default();
    GetNativeSystemInfo(&mut si);
    let arch = match si.Anonymous.Anonymous.wProcessorArchitecture {
        PROCESSOR_ARCHITECTURE_AMD64 => "x64 (AMD64)",
        PROCESSOR_ARCHITECTURE_ARM64 => "ARM64",
        PROCESSOR_ARCHITECTURE_INTEL => "x86",
        _ => "Unknown",
    };
    if info.cpu_arch.is_empty() {
        info.cpu_arch = arch.to_string();
    }
    if info.cpu_threads == 0 {
        info.cpu_threads = si.dwNumberOfProcessors;
    }
    if info.system_type.is_empty() {
        info.system_type = format!("{arch}-based PC");
    }

    let mut ms = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    if GlobalMemoryStatusEx(&mut ms).is_ok() {
        if info.ram_total == 0 {
            info.ram_total = ms.ullTotalPhys;
        }
        info.ram_avail = ms.ullAvailPhys;
    }

    if info.computer_name.is_empty() {
        let mut buf = [0u16; 256];
        let mut len = buf.len() as u32;
        if GetComputerNameExW(ComputerNameDnsHostname, PWSTR(buf.as_mut_ptr()), &mut len).is_ok() {
            info.computer_name = String::from_utf16_lossy(&buf[..len as usize]);
        }
    }

    let _ = EnumDisplayMonitors(
        HDC::default(),
        None,
        Some(monitor_proc),
        LPARAM(info as *mut SysInfo as isize),
    );

    if info.os_caption.is_empty() || info.os_build.is_empty() {
        use winreg::enums::HKEY_LOCAL_MACHINE;
        use winreg::RegKey;
        if let Ok(k) = RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion")
        {
            if info.os_caption.is_empty() {
                if let Ok(v) = k.get_value::<String, _>("ProductName") {
                    info.os_caption = v;
                }
            }
            if info.os_build.is_empty() {
                if let Ok(v) = k.get_value::<String, _>("CurrentBuildNumber") {
                    info.os_build = v;
                }
            }
        }
    }
}

unsafe extern "system" fn monitor_proc(
    hmon: HMONITOR,
    _hdc: HDC,
    _rc: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let info = &mut *(data.0 as *mut SysInfo);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(hmon, &mut mi).as_bool() {
        let w = mi.rcMonitor.right - mi.rcMonitor.left;
        let h = mi.rcMonitor.bottom - mi.rcMonitor.top;
        info.displays.push(format!("{w} \u{00d7} {h}"));
    }
    TRUE
}
