// SPDX-License-Identifier: GPL-3.0-or-later
//! Accent-colored border around the foreground (non-maximized) window.
//!
//! Windows 11 draws a thin accent frame on the active window via DWM
//! (`DWMWA_BORDER_COLOR`). That needs DWM composition, which the typical WinPE
//! target lacks — so StartPE paints the frame itself with a small GDI overlay:
//! a frame-shaped (`SetWindowRgn`) `WS_POPUP` we keep positioned directly over
//! the target window and just above it in Z order. It is click-through
//! (`WS_EX_TRANSPARENT` + `HTTRANSPARENT`) and never activates
//! (`WS_EX_NOACTIVATE`), so dragging/resizing the bordered window still works.
//!
//! Only the **foreground** window is bordered. Without DWM there is no way to
//! make a background window's frame get occluded by whatever sits in front of
//! it, so a per-window approach would leave borders floating over other apps;
//! tracking just the active window sidesteps that entirely. We follow it with
//! `SetWinEventHook` (foreground changes, move/size via `LOCATIONCHANGE`,
//! minimize, destroy) rather than polling.
//!
//! The accent color is `taskbar::start_button_color()` (read live), so the
//! border tracks the Start-button color and any runtime change to it.

use std::cell::RefCell;

use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmIsCompositionEnabled, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::taskbar::{scaled, start_button_color};

/// Border thickness in 96-DPI px (run through `scaled`).
const THICKNESS: i32 = 4;
/// Corner radius in 96-DPI px, to match Win11's rounded window corners. Only
/// applied when DWM composition is on (a plain PE has square corners).
const CORNER: i32 = 8;

struct Border {
    /// The overlay frame window.
    hwnd: HWND,
    /// The window currently bordered, or invalid when nothing is.
    target: HWND,
    /// WinEvent hooks (foreground/minimize range + object range).
    hooks: Vec<HWINEVENTHOOK>,
    /// Round the frame to match Win11 corners (DWM composition on). False in a
    /// plain PE, where windows are square and the frame is square too.
    rounded: bool,
}

thread_local! {
    static BORDER: RefCell<Option<Border>> = const { RefCell::new(None) };
}

/// Create the overlay + install the WinEvent hooks if `enabled`. Called once at
/// startup; a no-op (leaves nothing installed) when the feature is off.
pub fn install(enabled: bool) {
    if enabled {
        ensure_installed();
    }
}

/// Turn the feature on or off at runtime (from the settings pane via
/// `reload_config`). Installs or tears down the overlay + hooks to match.
pub fn set_enabled(enabled: bool) {
    let installed = BORDER.with_borrow(|b| b.is_some());
    if enabled && !installed {
        ensure_installed();
    } else if !enabled && installed {
        teardown();
    } else if enabled {
        // Already running — re-evaluate (e.g. accent color changed) and repaint.
        refresh();
    }
}

/// Re-evaluate the current foreground window and repaint the border. Cheap; safe
/// to call after a config change.
pub fn refresh() {
    let (hwnd, fg) = BORDER.with_borrow(|b| match b.as_ref() {
        Some(b) => (b.hwnd, unsafe { GetForegroundWindow() }),
        None => (HWND::default(), HWND::default()),
    });
    if hwnd.is_invalid() {
        return;
    }
    update_target(fg);
    unsafe {
        let _ = InvalidateRect(hwnd, None, true);
    }
}

fn ensure_installed() {
    if BORDER.with_borrow(|b| b.is_some()) {
        return;
    }
    unsafe {
        let Ok(hmod) = GetModuleHandleW(None) else {
            return;
        };
        let hinstance: HINSTANCE = hmod.into();
        let class = w!("StartPE_Border");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc); // idempotent

        // Click-through, never-activated, off the taskbar/Alt-Tab. Not topmost:
        // we re-stack it just above its target on every move so it never floats
        // above unrelated windows.
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            class,
            PCWSTR_NULL,
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            hinstance,
            None,
        );
        let Ok(hwnd) = hwnd else {
            return;
        };

        // Two hooks cover everything we react to, filtered in the proc:
        //   foreground change / minimize start+end  (0x0003..0x0017)
        //   object destroy / location change        (0x8001..0x800B)
        // SKIPOWNPROCESS keeps our own overlay's churn out of the stream.
        let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
        let mut hooks = Vec::new();
        for (lo, hi) in [
            (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND),
            (EVENT_OBJECT_DESTROY, EVENT_OBJECT_LOCATIONCHANGE),
        ] {
            let h = SetWinEventHook(lo, hi, None, Some(winevent_hook), 0, 0, flags);
            if !h.is_invalid() {
                hooks.push(h);
            }
        }

        let rounded = DwmIsCompositionEnabled().map(|b| b.as_bool()).unwrap_or(false);
        BORDER.with_borrow_mut(|b| {
            *b = Some(Border {
                hwnd,
                target: HWND::default(),
                hooks,
                rounded,
            });
        });
    }
    // Border whatever is already focused.
    update_target(unsafe { GetForegroundWindow() });
}

fn teardown() {
    if let Some(b) = BORDER.with_borrow_mut(|b| b.take()) {
        unsafe {
            for h in b.hooks {
                let _ = UnhookWinEvent(h);
            }
            let _ = DestroyWindow(b.hwnd);
        }
    }
}

/// A `PCWSTR::null()` usable in const position (the windows-crate `null()` is a
/// `const fn` but reads awkwardly inline).
const PCWSTR_NULL: windows::core::PCWSTR = windows::core::PCWSTR::null();

/// True if `hwnd` is a window we should draw an accent border around: a visible,
/// non-minimized, non-maximized, non-tool top-level window owned by another
/// process (never one of StartPE's own surfaces or the shell's desktop).
fn borderable(hwnd: HWND, overlay: HWND) -> bool {
    unsafe {
        if hwnd.is_invalid() || hwnd == overlay {
            return false;
        }
        if !IsWindowVisible(hwnd).as_bool()
            || IsIconic(hwnd).as_bool()
            || IsZoomed(hwnd).as_bool()
        {
            return false;
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }
        // Skip every StartPE-owned window in one shot (taskbar, menu, desktop,
        // peek, alt-tab, run, settings, sysinfo, this overlay).
        let mut pid = 0u32;
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == GetCurrentProcessId() {
            return false;
        }
        // The shell's desktop/tray windows are real top-level windows but not
        // things to frame.
        !matches!(
            class_of(hwnd).as_str(),
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
        )
    }
}

fn class_of(hwnd: HWND) -> String {
    unsafe {
        let mut buf = [0u16; 64];
        let n = GetClassNameW(hwnd, &mut buf) as usize;
        String::from_utf16_lossy(&buf[..n])
    }
}

/// Point the border at `candidate` if it's borderable, otherwise hide it.
fn update_target(candidate: HWND) {
    let action = BORDER.with_borrow_mut(|b| {
        let b = b.as_mut()?;
        if borderable(candidate, b.hwnd) {
            b.target = candidate;
            Some((b.hwnd, candidate))
        } else {
            b.target = HWND::default();
            unsafe {
                let _ = ShowWindow(b.hwnd, SW_HIDE);
            }
            None
        }
    });
    if let Some((overlay, target)) = action {
        position(overlay, target);
    }
}

/// The window's *visible* outer rect in screen coordinates. Win11 pads the plain
/// `GetWindowRect` with an invisible resize border (~7px on the sides/bottom), so
/// a frame drawn on it floats outside the glass; `DWMWA_EXTENDED_FRAME_BOUNDS`
/// gives the true edge instead. Falls back to `GetWindowRect` when DWM is absent
/// (a plain PE, where the window rect *is* the visible rect).
unsafe fn visible_rect(hwnd: HWND) -> Option<RECT> {
    let mut rc = RECT::default();
    let ok = DwmGetWindowAttribute(
        hwnd,
        DWMWA_EXTENDED_FRAME_BOUNDS,
        &mut rc as *mut _ as *mut core::ffi::c_void,
        std::mem::size_of::<RECT>() as u32,
    )
    .is_ok();
    if ok && rc.right > rc.left && rc.bottom > rc.top {
        return Some(rc);
    }
    let mut wr = RECT::default();
    GetWindowRect(hwnd, &mut wr).ok().map(|_| wr)
}

/// Re-fit the overlay to the current target's visible edge and stack it just
/// above the target. Hides the overlay if the target has vanished or gone
/// zero-size.
fn position(overlay: HWND, target: HWND) {
    let rounded = BORDER.with_borrow(|b| b.as_ref().map(|b| b.rounded).unwrap_or(false));
    unsafe {
        let Some(rc) = visible_rect(target) else {
            let _ = ShowWindow(overlay, SW_HIDE);
            return;
        };
        let w = rc.right - rc.left;
        let h = rc.bottom - rc.top;
        if w <= 0 || h <= 0 {
            let _ = ShowWindow(overlay, SW_HIDE);
            return;
        }
        let t = scaled(THICKNESS).max(1);
        // Frame-shaped region hugging the visible edge: the full rect minus the
        // interior hole, so only the `t`-px ring belongs to the overlay (the
        // middle is literally not part of the window — clicks pass through to the
        // app beneath). Rounded to match Win11 corners where DWM is on.
        let (outer, inner) = if rounded {
            let r = scaled(CORNER);
            let ri = (r - t).max(1);
            // CreateRoundRectRgn's rect is right/bottom-exclusive and the last two
            // args are the *ellipse* (full corner) size, i.e. 2× the radius.
            (
                CreateRoundRectRgn(0, 0, w + 1, h + 1, 2 * r, 2 * r),
                CreateRoundRectRgn(t, t, w - t + 1, h - t + 1, 2 * ri, 2 * ri),
            )
        } else {
            (CreateRectRgn(0, 0, w, h), CreateRectRgn(t, t, w - t, h - t))
        };
        CombineRgn(outer, outer, inner, RGN_DIFF);
        let _ = DeleteObject(HGDIOBJ(inner.0));
        // SetWindowRgn takes ownership of `outer`; bRedraw repaints the ring.
        SetWindowRgn(overlay, outer, true);
        let _ = SetWindowPos(
            overlay,
            target,
            rc.left,
            rc.top,
            w,
            h,
            SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
        );
    }
}

unsafe extern "system" fn winevent_hook(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    // We only ever care about whole-window events (OBJID_WINDOW), never child
    // accessibility objects like the caret — those fire constantly.
    if id_object != OBJID_WINDOW.0 || id_child != 0 {
        return;
    }
    match event {
        EVENT_SYSTEM_FOREGROUND | EVENT_SYSTEM_MINIMIZEEND => update_target(hwnd),
        // The active window is being minimized or destroyed: drop the border.
        EVENT_SYSTEM_MINIMIZESTART | EVENT_OBJECT_DESTROY => {
            let hit = BORDER.with_borrow(|b| b.as_ref().filter(|b| b.target == hwnd).map(|b| b.hwnd));
            if let Some(overlay) = hit {
                clear(overlay);
            }
        }
        EVENT_OBJECT_LOCATIONCHANGE => {
            // The tracked window moved or resized — follow it. A maximize comes
            // through here too, so re-test borderability rather than just
            // repositioning.
            let (overlay, tracked) = BORDER.with_borrow(|b| match b.as_ref() {
                Some(b) => (b.hwnd, b.target),
                None => (HWND::default(), HWND::default()),
            });
            if overlay.is_invalid() {
                return;
            }
            if tracked == hwnd {
                if borderable(hwnd, overlay) {
                    position(overlay, hwnd);
                } else {
                    update_target(hwnd); // e.g. just maximized → hide
                }
            } else if hwnd == GetForegroundWindow() && borderable(hwnd, overlay) {
                // The foreground window changed shape without a foreground event
                // — e.g. it was just restored from maximized. Adopt it.
                update_target(hwnd);
            }
        }
        _ => {}
    }
}

/// Hide the overlay and forget the current target.
fn clear(overlay: HWND) {
    BORDER.with_borrow_mut(|b| {
        if let Some(b) = b.as_mut() {
            b.target = HWND::default();
        }
    });
    unsafe {
        let _ = ShowWindow(overlay, SW_HIDE);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        // Belt-and-suspenders click-through (on top of WS_EX_TRANSPARENT): the
        // ring forwards every hit to the window beneath, which is the target.
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let brush = CreateSolidBrush(COLORREF(start_button_color()));
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            // The window region clips this fill to the ring.
            FillRect(hdc, &rc, brush);
            let _ = DeleteObject(HGDIOBJ(brush.0));
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
