// SPDX-License-Identifier: GPL-3.0-or-later
//! System tray (notification area) host.
//!
//! Applications register tray icons with `Shell_NotifyIcon`, which serializes
//! a NOTIFYICONDATA into a `WM_COPYDATA` (dwData = 1) sent to the top-level
//! window of class `Shell_TrayWnd`. We create our own hidden window of that
//! class, parse those registrations, and keep the icon list; the taskbar
//! renders it next to the clock and forwards clicks to each icon's callback
//! window.
//!
//! Anything we don't handle (dwData = 0 appbar traffic, etc.) is proxied to
//! Explorer's real tray window, so the appbar protocol keeps working while
//! Explorer is the shell. On startup we broadcast the registered
//! `TaskbarCreated` message so already-running applications re-register
//! their icons with us.
//!
//! The NOTIFYICONDATA in the COPYDATASTRUCT uses the 32-bit layout (handles
//! as u32) regardless of architecture — it is a cross-bitness wire format.

use std::cell::RefCell;

use windows::core::{w, Result};
use windows::Win32::Foundation::*;
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

// Shell_NotifyIcon message codes.
const NIM_ADD: u32 = 0;
const NIM_MODIFY: u32 = 1;
const NIM_DELETE: u32 = 2;
const NIM_SETVERSION: u32 = 4;

// NOTIFYICONDATA flags.
const NIF_MESSAGE: u32 = 0x1;
const NIF_ICON: u32 = 0x2;
const NIF_TIP: u32 = 0x4;
const NIF_STATE: u32 = 0x8;
const NIS_HIDDEN: u32 = 0x1;

/// Wire format of Shell_NotifyIcon's WM_COPYDATA payload (after the 8-byte
/// header): NOTIFYICONDATAW with 32-bit handles.
#[repr(C)]
#[derive(Clone, Copy)]
struct NotifyIconData32 {
    cb_size: u32,
    hwnd: u32,
    uid: u32,
    flags: u32,
    callback_message: u32,
    hicon: u32,
    tip: [u16; 128],
    state: u32,
    state_mask: u32,
    info: [u16; 256],
    version_or_timeout: u32,
    info_title: [u16; 64],
    info_flags: u32,
    guid: [u8; 16],
    balloon_icon: u32,
}

struct TrayIcon {
    owner: u32,
    uid: u32,
    callback: u32,
    icon: Option<HICON>,
    tip: String,
    hidden: bool,
    version: u32,
}

struct TrayState {
    hwnd: HWND,
    taskbar: HWND,
    /// Explorer's tray, for proxying messages we don't handle ourselves.
    explorer_tray: HWND,
    icons: Vec<TrayIcon>,
}

thread_local! {
    static TRAY: RefCell<Option<TrayState>> = const { RefCell::new(None) };
}

/// Posted to the taskbar whenever the icon set changes.
pub const MSG_TRAY_CHANGED: u32 = WM_APP + 3;

pub fn create(taskbar: HWND) -> Result<()> {
    unsafe {
        // Find Explorer's tray *before* creating ours so the search cannot
        // return our own window.
        let explorer_tray = crate::taskbar::find_explorer_tray();

        let hinstance: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = w!("Shell_TrayWnd");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);

        // Hidden, but positioned over our taskbar so apps that query the
        // tray rect for balloon placement get sensible coordinates.
        let mut tb = RECT::default();
        let _ = GetWindowRect(taskbar, &mut tb);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class,
            w!(""),
            WS_POPUP,
            tb.left,
            tb.top,
            tb.right - tb.left,
            tb.bottom - tb.top,
            None,
            None,
            hinstance,
            None,
        )?;

        // Compatibility child some apps look for via FindWindowEx.
        let notify_class = w!("TrayNotifyWnd");
        let wc2 = WNDCLASSW {
            lpfnWndProc: Some(passthrough_wndproc),
            hInstance: hinstance,
            lpszClassName: notify_class,
            ..Default::default()
        };
        RegisterClassW(&wc2);
        let _ = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            notify_class,
            w!(""),
            WS_CHILD,
            0,
            0,
            0,
            0,
            hwnd,
            None,
            hinstance,
            None,
        );

        TRAY.with_borrow_mut(|t| {
            *t = Some(TrayState {
                hwnd,
                taskbar,
                explorer_tray,
                icons: Vec::new(),
            })
        });

        raise();

        // Ask every running app to re-register its tray icon. They will
        // re-resolve Shell_TrayWnd and find us first (we are topmost).
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        let _ = SendNotifyMessageW(HWND_BROADCAST, taskbar_created, WPARAM(0), LPARAM(0));
        Ok(())
    }
}

/// Keep our tray window first in FindWindow order (above Explorer's hidden
/// one). Called from the taskbar watchdog.
pub fn raise() {
    TRAY.with_borrow(|t| {
        if let Some(t) = t.as_ref() {
            unsafe {
                let _ = SetWindowPos(
                    t.hwnd,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
    });
}

/// Drop icons whose owner window died without sending NIM_DELETE.
pub fn prune() {
    let changed = TRAY.with_borrow_mut(|t| {
        let Some(t) = t.as_mut() else { return false };
        let before = t.icons.len();
        t.icons.retain(|i| unsafe { IsWindow(HWND(i.owner as usize as *mut _)).as_bool() });
        before != t.icons.len()
    });
    if changed {
        notify_taskbar();
    }
}

/// Visible icons, in registration order, for the taskbar to draw.
pub fn snapshot() -> Vec<HICON> {
    TRAY.with_borrow(|t| {
        t.as_ref()
            .map(|t| {
                t.icons
                    .iter()
                    .filter(|i| !i.hidden)
                    .map(|i| i.icon.unwrap_or_default())
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// Forward a click on visible icon `index` to its owner application.
pub fn click(index: usize, right: bool) {
    struct Target {
        owner: HWND,
        uid: u32,
        callback: u32,
        version: u32,
    }
    let target = TRAY.with_borrow(|t| {
        let t = t.as_ref()?;
        let icon = t.icons.iter().filter(|i| !i.hidden).nth(index)?;
        Some(Target {
            owner: HWND(icon.owner as usize as *mut _),
            uid: icon.uid,
            callback: icon.callback,
            version: icon.version,
        })
    });
    let Some(target) = target else { return };
    unsafe {
        if !IsWindow(target.owner).as_bool() {
            prune();
            return;
        }
        let events: &[u32] = if right {
            &[WM_RBUTTONDOWN, WM_RBUTTONUP, WM_CONTEXTMENU]
        } else {
            &[WM_LBUTTONDOWN, WM_LBUTTONUP]
        };
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        for &event in events {
            let (wparam, lparam) = if target.version >= 4 {
                // NOTIFYICON_VERSION_4: wParam = cursor pos, lParam = event | (uid << 16).
                (
                    WPARAM(((pt.x as u16 as usize) | ((pt.y as u16 as usize) << 16)) as usize),
                    LPARAM(((event as u16 as isize) | ((target.uid as u16 as isize) << 16)) as isize),
                )
            } else {
                (WPARAM(target.uid as usize), LPARAM(event as isize))
            };
            let _ = SendNotifyMessageW(target.owner, target.callback, wparam, lparam);
        }
    }
}

fn notify_taskbar() {
    TRAY.with_borrow(|t| {
        if let Some(t) = t.as_ref() {
            unsafe {
                let _ = PostMessageW(t.taskbar, MSG_TRAY_CHANGED, WPARAM(0), LPARAM(0));
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Shell_NotifyIcon parsing

fn utf16_until_nul(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

unsafe fn handle_notify_data(cds: &COPYDATASTRUCT) -> bool {
    if cds.cbData < 8 + 24 {
        return false;
    }
    let data = cds.lpData as *const u8;
    let message = *(data as *const u32).add(1);

    // Copy whatever portion of the NID we were sent; missing tail stays zero.
    let mut nid: NotifyIconData32 = std::mem::zeroed();
    let n = (cds.cbData as usize - 8).min(std::mem::size_of::<NotifyIconData32>());
    std::ptr::copy_nonoverlapping(data.add(8), &mut nid as *mut _ as *mut u8, n);

    let changed = TRAY.with_borrow_mut(|t| {
        let Some(t) = t.as_mut() else { return false };
        let pos = t
            .icons
            .iter()
            .position(|i| i.owner == nid.hwnd && i.uid == nid.uid);
        match message {
            NIM_ADD | NIM_MODIFY => {
                let idx = match pos {
                    Some(i) => i,
                    None => {
                        t.icons.push(TrayIcon {
                            owner: nid.hwnd,
                            uid: nid.uid,
                            callback: 0,
                            icon: None,
                            tip: String::new(),
                            hidden: false,
                            version: 0,
                        });
                        t.icons.len() - 1
                    }
                };
                let icon = &mut t.icons[idx];
                if nid.flags & NIF_MESSAGE != 0 {
                    icon.callback = nid.callback_message;
                }
                if nid.flags & NIF_ICON != 0 {
                    // Copy: the handle belongs to the sender and may be
                    // destroyed right after this call returns.
                    let src = HICON(nid.hicon as usize as *mut _);
                    if let Some(old) = icon.icon.take() {
                        let _ = DestroyIcon(old);
                    }
                    icon.icon = CopyIcon(src).ok();
                }
                if nid.flags & NIF_TIP != 0 {
                    icon.tip = utf16_until_nul(&nid.tip);
                }
                if nid.flags & NIF_STATE != 0 && nid.state_mask & NIS_HIDDEN != 0 {
                    icon.hidden = nid.state & NIS_HIDDEN != 0;
                }
                true
            }
            NIM_DELETE => {
                if let Some(i) = pos {
                    if let Some(h) = t.icons[i].icon.take() {
                        let _ = DestroyIcon(h);
                    }
                    t.icons.remove(i);
                    true
                } else {
                    false
                }
            }
            NIM_SETVERSION => {
                if let Some(i) = pos {
                    t.icons[i].version = nid.version_or_timeout;
                }
                false
            }
            _ => false,
        }
    });
    if changed {
        notify_taskbar();
    }
    true
}

// ---------------------------------------------------------------------------
// Window procedures

unsafe extern "system" fn passthrough_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_COPYDATA {
        let cds = &*(lparam.0 as *const COPYDATASTRUCT);
        if cds.dwData == 1 {
            // Shell_NotifyIcon traffic: ours. Also mirror it to Explorer's
            // tray so its (hidden) state stays consistent if StartPE exits.
            let handled = handle_notify_data(cds);
            let explorer = TRAY.with_borrow(|t| t.as_ref().map(|t| t.explorer_tray));
            if let Some(explorer) = explorer {
                if !explorer.is_invalid() && IsWindow(explorer).as_bool() {
                    let _ = SendMessageW(explorer, WM_COPYDATA, wparam, lparam);
                }
            }
            return LRESULT(handled as isize);
        }
        // Appbar protocol and anything else: proxy to the real shell tray.
        let explorer = TRAY.with_borrow(|t| t.as_ref().map(|t| t.explorer_tray));
        if let Some(explorer) = explorer {
            if !explorer.is_invalid() && IsWindow(explorer).as_bool() {
                return SendMessageW(explorer, WM_COPYDATA, wparam, lparam);
            }
        }
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
