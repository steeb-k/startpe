// SPDX-License-Identifier: GPL-3.0-or-later
//! StartPE-provided desktop.
//!
//! On Win11 PE images whose modern-shell packages (the XAML CBS packages the
//! taskbar depends on) are stripped, Explorer's shell init fail-fasts during
//! taskbar bring-up and never creates the desktop (`Progman`/`SHELLDLL_DefView`)
//! — so there is no wallpaper and no icons. This module fills that gap: when no
//! Explorer desktop appears, StartPE creates its own desktop window, paints the
//! wallpaper, and hosts the *real* shell desktop view (`SHELLDLL_DefView`, the
//! same control Explorer uses), so desktop icons, the right-click menu, and
//! "double-click a folder opens an Explorer window" all behave normally.
//!
//! On a normal Windows box — or a PE where Explorer's desktop does come up — we
//! detect that and stay out of the way (Explorer keeps owning the desktop).
//!
//! Everything here is documented Win32/shell32. Explorer is still launched on
//! demand as the file manager; it just no longer has to be the shell.

use core::ffi::c_void;
use std::cell::RefCell;

use windows::core::{implement, w, Result, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CoInitializeEx, IStream, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{IOleWindow_Impl, OLEMENUGROUPWIDTHS};
use windows::Win32::UI::Controls::TBBUTTON;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IShellBrowser, IShellBrowser_Impl, IShellView, SHGetDesktopFolder, FOLDERSETTINGS, FVM_ICON,
    FWF_DESKTOP, FWF_NOCLIENTEDGE, FWF_NOSCROLL, SVUIA_ACTIVATE_NOFOCUS,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::Config;
use crate::util;

/// Per-process desktop state (single UI thread, like the rest of StartPE).
struct DesktopState {
    /// Wallpaper bitmap, stretched to fill the screen (`None` -> solid fill).
    wallpaper: Option<HBITMAP>,
    /// Fallback background color when there is no wallpaper bitmap.
    bg_color: u32,
    /// The hosted shell view (kept alive for the process lifetime).
    _view: Option<IShellView>,
    /// The browser we hand the view (kept alive for the process lifetime).
    _browser: Option<IShellBrowser>,
    /// The `SHELLDLL_DefView` child window, resized to track the desktop.
    view_hwnd: HWND,
}

thread_local! {
    static DESKTOP: RefCell<Option<DesktopState>> = const { RefCell::new(None) };
}

/// Create a StartPE-owned desktop if appropriate. Returns `true` if StartPE now
/// owns the desktop (so the caller should not wait on Explorer's shell).
///
/// `own_desktop`: 0 = auto (create only if Explorer's desktop never appears),
/// 1 = always, 2 = never.
pub fn create_if_needed(cfg: &Config) -> bool {
    match cfg.own_desktop {
        2 => return false,
        0 => {
            // Give Explorer a chance to bring its own desktop up (normal
            // Windows, or a PE where the shell init succeeds). If it does,
            // defer to it; if it never does, we take over.
            if wait_for_explorer_desktop(15_000) {
                return false;
            }
        }
        _ => {}
    }

    unsafe {
        // The shell view is COM; host it on an STA (this UI thread).
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        match create(cfg) {
            Ok(()) => true,
            Err(_) => false,
        }
    }
}

/// True once Explorer's desktop (`Progman` hosting a `SHELLDLL_DefView`) exists.
fn explorer_desktop_present() -> bool {
    unsafe {
        let Ok(progman) = FindWindowW(w!("Progman"), None) else {
            return false;
        };
        if progman.is_invalid() {
            return false;
        }
        FindWindowExW(progman, None, w!("SHELLDLL_DefView"), None)
            .map(|h| !h.is_invalid())
            .unwrap_or(false)
    }
}

fn wait_for_explorer_desktop(timeout_ms: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    while std::time::Instant::now() < deadline {
        if explorer_desktop_present() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    explorer_desktop_present()
}

unsafe fn create(cfg: &Config) -> Result<()> {
    let hinstance: HINSTANCE = GetModuleHandleW(None)?.into();
    let class = w!("StartPE_Desktop");
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        hInstance: hinstance,
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        // We paint the background ourselves (wallpaper / solid fill).
        hbrBackground: HBRUSH(std::ptr::null_mut()),
        lpszClassName: class,
        ..Default::default()
    };
    RegisterClassW(&wc);

    let sw = GetSystemMetrics(SM_CXSCREEN);
    let sh = GetSystemMetrics(SM_CYSCREEN);
    let wallpaper = load_wallpaper(cfg);

    DESKTOP.with_borrow_mut(|d| {
        *d = Some(DesktopState {
            wallpaper,
            bg_color: cfg.desktop_color,
            _view: None,
            _browser: None,
            view_hwnd: HWND::default(),
        })
    });

    let hwnd = CreateWindowExW(
        // WS_EX_TOOLWINDOW keeps the desktop out of the taskbar / Alt-Tab so it
        // never shows up as a "Desktop" task button.
        WS_EX_TOOLWINDOW,
        class,
        w!("Desktop"),
        WS_POPUP | WS_VISIBLE | WS_CLIPCHILDREN,
        0,
        0,
        sw,
        sh,
        None,
        None,
        hinstance,
        None,
    )?;

    // Sit at the very bottom of the z-order; the taskbar (WS_EX_TOPMOST) and all
    // app windows stay above us, exactly like Explorer's Progman.
    let _ = SetWindowPos(
        hwnd,
        HWND_BOTTOM,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );

    host_shell_view(hwnd);
    Ok(())
}

/// Host the real shell desktop view (`SHELLDLL_DefView`) as a child filling the
/// desktop window. Best-effort: if it fails we still have a wallpaper desktop
/// rather than a black screen.
unsafe fn host_shell_view(parent: HWND) {
    let desktop_folder = match SHGetDesktopFolder() {
        Ok(f) => f,
        Err(_) => return,
    };

    // IShellFolder::CreateViewObject(hwnd) -> IShellView (the desktop's view).
    let view: IShellView = match desktop_folder.CreateViewObject(parent) {
        Ok(v) => v,
        Err(_) => return,
    };

    let mut rc = RECT::default();
    let _ = GetClientRect(parent, &mut rc);

    let fs = FOLDERSETTINGS {
        ViewMode: FVM_ICON.0 as u32,
        fFlags: (FWF_DESKTOP | FWF_NOCLIENTEDGE | FWF_NOSCROLL).0 as u32,
    };

    // Hand the view a minimal host browser. The desktop `SHELLDLL_DefView`
    // calls back into the browser (for its parent window, status text, etc.);
    // without one it creates no icon list. A NULL browser left the view empty.
    let browser: IShellBrowser = DesktopBrowser { hwnd: parent }.into();
    let view_hwnd = match view.CreateViewWindow(None, &fs, &browser, &rc) {
        Ok(h) => h,
        Err(_) => return,
    };
    let _ = view.UIActivate(SVUIA_ACTIVATE_NOFOCUS.0 as u32);
    let _ = ShowWindow(view_hwnd, SW_SHOW);

    DESKTOP.with_borrow_mut(|d| {
        if let Some(d) = d {
            d.view_hwnd = view_hwnd;
            d._view = Some(view);
            d._browser = Some(browser);
        }
    });
}

/// Minimal `IShellBrowser` host for the desktop's `SHELLDLL_DefView`. The view
/// only really needs `GetWindow` (its parent); the rest are no-ops or
/// not-implemented, which is all a non-navigating desktop host requires.
#[implement(IShellBrowser)]
struct DesktopBrowser {
    hwnd: HWND,
}

#[allow(non_snake_case)]
impl IOleWindow_Impl for DesktopBrowser_Impl {
    fn GetWindow(&self) -> Result<HWND> {
        Ok(self.hwnd)
    }
    fn ContextSensitiveHelp(&self, _fentermode: BOOL) -> Result<()> {
        Ok(())
    }
}

#[allow(non_snake_case)]
impl IShellBrowser_Impl for DesktopBrowser_Impl {
    fn InsertMenusSB(&self, _hmenushared: HMENU, _lpmenuwidths: *mut OLEMENUGROUPWIDTHS) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn SetMenuSB(&self, _hmenushared: HMENU, _holemenures: isize, _hwndactiveobject: HWND) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn RemoveMenusSB(&self, _hmenushared: HMENU) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn SetStatusTextSB(&self, _pszstatustext: &PCWSTR) -> Result<()> {
        Ok(())
    }
    fn EnableModelessSB(&self, _fenable: BOOL) -> Result<()> {
        Ok(())
    }
    fn TranslateAcceleratorSB(&self, _pmsg: *const MSG, _wid: u16) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn BrowseObject(&self, _pidl: *const ITEMIDLIST, _wflags: u32) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetViewStateStream(&self, _grfmode: u32) -> Result<IStream> {
        Err(E_NOTIMPL.into())
    }
    fn GetControlWindow(&self, _id: u32) -> Result<HWND> {
        Err(E_NOTIMPL.into())
    }
    fn SendControlMsg(
        &self,
        _id: u32,
        _umsg: u32,
        _wparam: WPARAM,
        _lparam: LPARAM,
        _pret: *mut LRESULT,
    ) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn QueryActiveShellView(&self) -> Result<IShellView> {
        Err(E_NOTIMPL.into())
    }
    fn OnViewWindowActive(&self, _pshv: Option<&IShellView>) -> Result<()> {
        Ok(())
    }
    fn SetToolbarItems(&self, _lpbuttons: *const TBBUTTON, _nbuttons: u32, _uflags: u32) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
}

/// Resolve and load the wallpaper bitmap (BMP). Tries the configured path, then
/// the per-user Control Panel wallpaper value. Returns `None` to fall back to a
/// solid fill. (Only BMP is supported via `LoadImageW`; PE scripts that want a
/// photo wallpaper should provide a .bmp, as the user-picture path already is.)
unsafe fn load_wallpaper(cfg: &Config) -> Option<HBITMAP> {
    let path = cfg.wallpaper.clone().or_else(control_panel_wallpaper)?;
    if !path.to_ascii_lowercase().ends_with(".bmp") {
        return None;
    }
    let wp = util::WideStr::new(&path);
    LoadImageW(None, wp.pcwstr(), IMAGE_BITMAP, 0, 0, LR_LOADFROMFILE)
        .ok()
        .map(|h| HBITMAP(h.0))
}

fn control_panel_wallpaper() -> Option<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey("Control Panel\\Desktop")
        .ok()?;
    let v: String = key.get_value("WallPaper").ok()?;
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => {
            paint_background(hwnd, HDC(wp.0 as *mut c_void));
            LRESULT(1)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            paint_background(hwnd, hdc);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_SIZE => {
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            DESKTOP.with_borrow(|d| {
                if let Some(d) = d {
                    if !d.view_hwnd.is_invalid() {
                        let _ = SetWindowPos(
                            d.view_hwnd,
                            HWND::default(),
                            0,
                            0,
                            rc.right,
                            rc.bottom,
                            SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                    }
                }
            });
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn paint_background(hwnd: HWND, hdc: HDC) {
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    DESKTOP.with_borrow(|d| {
        let Some(d) = d else {
            return;
        };
        if let Some(bmp) = d.wallpaper {
            let mem = CreateCompatibleDC(hdc);
            let old = SelectObject(mem, HGDIOBJ(bmp.0));
            let mut bm = BITMAP::default();
            GetObjectW(
                HGDIOBJ(bmp.0),
                core::mem::size_of::<BITMAP>() as i32,
                Some(&mut bm as *mut _ as *mut c_void),
            );
            SetStretchBltMode(hdc, HALFTONE);
            let _ = StretchBlt(
                hdc, 0, 0, rc.right, rc.bottom, mem, 0, 0, bm.bmWidth, bm.bmHeight, SRCCOPY,
            );
            SelectObject(mem, old);
            let _ = DeleteDC(mem);
        } else {
            let brush = CreateSolidBrush(COLORREF(d.bg_color));
            FillRect(hdc, &rc, brush);
            let _ = DeleteObject(HGDIOBJ(brush.0));
        }
    });
}
