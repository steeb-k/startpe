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

use windows::core::{implement, w, Interface, Result, PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CoTaskMemFree, IStream};
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows::Win32::System::Ole::{IOleWindow_Impl, OleInitialize, OLEMENUGROUPWIDTHS};
use windows::Win32::UI::Controls::{LVITEMW, TBBUTTON};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IFolderView2, IShellBrowser, IShellBrowser_Impl, IShellFolder, IShellView, SHBindToObject,
    SHGetDesktopFolder, SHGetKnownFolderIDList, FOLDERID_PublicDesktop, FOLDERSETTINGS, FVM_ICON,
    FWF_AUTOARRANGE, FWF_DESKTOP, FWF_NOCLIENTEDGE, FWF_NOSCROLL, FWF_SNAPTOGRID,
    SVUIA_ACTIVATE_NOFOCUS,
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
    /// The desktop `SysListView32` (icon list), for layout save/restore.
    listview: HWND,
    /// Layout-timer ticks (first few apply the saved layout, then we capture).
    ticks: u32,
    /// Last captured layout text, to avoid rewriting the file unchanged.
    last_layout: String,
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
        // OleInitialize (not just CoInitializeEx) so the hosted desktop view's
        // OLE drag-and-drop works — without it, dragging icons silently no-ops.
        // It also puts us on an STA, which the shell view needs.
        let _ = OleInitialize(None);
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
            listview: HWND::default(),
            ticks: 0,
            last_layout: String::new(),
        })
    });

    let hwnd = CreateWindowExW(
        // WS_EX_TOOLWINDOW keeps the desktop out of the taskbar / Alt-Tab so it
        // never shows up as a "Desktop" task button.
        WS_EX_TOOLWINDOW,
        class,
        w!("Desktop"),
        // No WS_CLIPCHILDREN: the FWF_DESKTOP shell view's list is transparent,
        // so the parent must paint the wallpaper *under* it. Clipping children
        // would leave the icon area unpainted (black) instead of wallpaper.
        WS_POPUP | WS_VISIBLE,
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

    // Only BMPs are reliably accepted by SPI_SETDESKWALLPAPER; other formats
    // are shown by our own GDI+ parent-paint, so don't risk handing the shell a
    // format it can't paint (it could paint black over our wallpaper).
    if let Some(path) = resolve_wallpaper_path(cfg) {
        if path.to_ascii_lowercase().ends_with(".bmp") {
            set_system_wallpaper(&path);
        }
    }

    host_shell_view(hwnd, cfg);
    Ok(())
}

/// Point the desktop wallpaper at `path` so the FWF_DESKTOP view paints it.
unsafe fn set_system_wallpaper(path: &str) {
    let wp = util::WideStr::new(path);
    let _ = SystemParametersInfoW(
        SPI_SETDESKWALLPAPER,
        0,
        Some(wp.pcwstr().0 as *mut c_void),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    );
}

/// The full namespace-desktop view (includes the junctions). Used when the user
/// opts to show the system icons, and as a fallback.
unsafe fn full_desktop_view(parent: HWND) -> Option<IShellView> {
    SHGetDesktopFolder().ok()?.CreateViewObject(parent).ok()
}

/// A view of the Public Desktop file-system folder (`%PUBLIC%\Desktop`), where
/// PE builds place shortcuts. Hosting it shows only those real items — none of
/// the desktop namespace junctions (This PC, Home, Network, Control Panel,
/// Recycle Bin).
unsafe fn public_desktop_view(parent: HWND) -> Option<IShellView> {
    let pidl = SHGetKnownFolderIDList(&FOLDERID_PublicDesktop, 0, None).ok()?;
    let folder: windows::core::Result<IShellFolder> = SHBindToObject(None, pidl, None);
    CoTaskMemFree(Some(pidl as *const c_void));
    folder.ok()?.CreateViewObject(parent).ok()
}

/// Host the desktop icon view (`SHELLDLL_DefView`) as a child filling the desktop
/// window. Best-effort: on failure we still have a wallpaper desktop.
unsafe fn host_shell_view(parent: HWND, cfg: &Config) {
    // Default: the Public Desktop folder (junction-free). ShowSystemDesktopIcons
    // hosts the full namespace desktop (with junctions). A `CreateViewObject`
    // view + FWF_DESKTOP works with our minimal browser; the generic
    // SHCreateShellFolderView view does not, so we host the folder directly.
    let view: IShellView = if cfg.show_system_desktop_icons {
        match full_desktop_view(parent) {
            Some(v) => v,
            None => return,
        }
    } else {
        match public_desktop_view(parent) {
            Some(v) => v,
            None => match full_desktop_view(parent) {
                Some(v) => v,
                None => return,
            },
        }
    };

    let mut rc = RECT::default();
    let _ = GetClientRect(parent, &mut rc);
    let fs = FOLDERSETTINGS {
        ViewMode: FVM_ICON.0 as u32,
        fFlags: (FWF_DESKTOP | FWF_NOCLIENTEDGE | FWF_NOSCROLL).0 as u32,
    };
    let browser: IShellBrowser = DesktopBrowser {
        hwnd: parent,
        view: RefCell::new(None),
    }
    .into();
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

    // A 1s timer finds the (asynchronously created) icon list, sets its flags,
    // applies the saved layout for the first ticks, then captures changes.
    let _ = SetTimer(parent, TIMER_LAYOUT, 1000, None);
}

/// Default the view to auto-arrange OFF, snap-to-grid ON (free but tidy
/// positioning). Uses the documented `IFolderView2` flags, not list-view hacks.
unsafe fn configure_view_flags(view: &IShellView) {
    if let Ok(fv) = view.cast::<IFolderView2>() {
        let _ = fv.SetCurrentFolderFlags(
            (FWF_AUTOARRANGE | FWF_SNAPTOGRID).0 as u32,
            FWF_SNAPTOGRID.0 as u32,
        );
    }
}

const TIMER_LAYOUT: usize = 1;

const LVM_GETITEMCOUNT: u32 = 0x1004;
const LVM_SETITEMPOSITION: u32 = 0x100F;
const LVM_GETITEMPOSITION: u32 = 0x1010;
const LVM_GETITEMTEXTW: u32 = 0x1073;

/// Path to the desktop-layout file next to `startpe.exe`. PE builds bake one in
/// to define positions; StartPE rewrites it as icons move so it can be captured
/// and re-baked.
fn layout_path() -> Option<String> {
    let mut buf = [0u16; 520];
    let n = unsafe { GetModuleFileNameW(None, &mut buf) };
    if n == 0 {
        return None;
    }
    let full = String::from_utf16_lossy(&buf[..n as usize]);
    let pos = full.rfind('\\')?;
    Some(format!("{}desktop-layout.txt", &full[..=pos]))
}

unsafe fn list_item_count(lv: HWND) -> i32 {
    SendMessageW(lv, LVM_GETITEMCOUNT, WPARAM(0), LPARAM(0)).0 as i32
}

unsafe fn list_item_text(lv: HWND, i: i32) -> String {
    let mut buf = [0u16; 260];
    let mut it = LVITEMW {
        iSubItem: 0,
        pszText: PWSTR(buf.as_mut_ptr()),
        cchTextMax: buf.len() as i32,
        ..Default::default()
    };
    let n = SendMessageW(
        lv,
        LVM_GETITEMTEXTW,
        WPARAM(i as usize),
        LPARAM(&mut it as *mut _ as isize),
    )
    .0
    .max(0) as usize;
    String::from_utf16_lossy(&buf[..n.min(buf.len())])
}

unsafe fn list_item_pos(lv: HWND, i: i32) -> (i32, i32) {
    let mut p = POINT::default();
    let _ = SendMessageW(
        lv,
        LVM_GETITEMPOSITION,
        WPARAM(i as usize),
        LPARAM(&mut p as *mut _ as isize),
    );
    (p.x, p.y)
}

unsafe fn set_item_pos(lv: HWND, i: i32, x: i32, y: i32) {
    let lp = ((x & 0xFFFF) | ((y & 0xFFFF) << 16)) as isize;
    let _ = SendMessageW(lv, LVM_SETITEMPOSITION, WPARAM(i as usize), LPARAM(lp));
}

/// Serialize the current desktop icon positions as `x,y,Name` lines.
unsafe fn capture_layout(lv: HWND) -> String {
    let mut out = String::new();
    for i in 0..list_item_count(lv) {
        let (x, y) = list_item_pos(lv, i);
        out.push_str(&format!("{x},{y},{}\n", list_item_text(lv, i)));
    }
    out
}

/// Apply saved `x,y,Name` positions to matching desktop icons.
unsafe fn apply_layout(lv: HWND, text: &str) {
    let count = list_item_count(lv);
    for line in text.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        let mut parts = line.splitn(3, ',');
        let (Some(xs), Some(ys), Some(name)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let (Ok(x), Ok(y)) = (xs.trim().parse::<i32>(), ys.trim().parse::<i32>()) else {
            continue;
        };
        for i in 0..count {
            if list_item_text(lv, i).eq_ignore_ascii_case(name) {
                set_item_pos(lv, i, x, y);
                break;
            }
        }
    }
}

/// Configure the desktop `SysListView32` for free, tidy positioning: auto-arrange
/// OFF (dragged icons stay where put) and snap-to-grid ON (they align to a grid).
/// Returns the list-view handle.
/// Minimal `IShellBrowser` host for the desktop's `SHELLDLL_DefView`. The view
/// only really needs `GetWindow` (its parent); the rest are no-ops or
/// not-implemented, which is all a non-navigating desktop host requires.
#[implement(IShellBrowser)]
struct DesktopBrowser {
    hwnd: HWND,
    /// The active shell view, captured from `OnViewWindowActive`. The desktop
    /// view's drag-drop (icon repositioning) needs the browser to report it via
    /// `QueryActiveShellView`, or drops are rejected (no-drop cursor).
    view: RefCell<Option<IShellView>>,
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
    // A menu-less host returns S_OK (reserving no menu space), not E_NOTIMPL.
    fn InsertMenusSB(&self, _hmenushared: HMENU, lpmenuwidths: *mut OLEMENUGROUPWIDTHS) -> Result<()> {
        if !lpmenuwidths.is_null() {
            unsafe { (*lpmenuwidths).width = [0; 6] };
        }
        Ok(())
    }
    fn SetMenuSB(&self, _hmenushared: HMENU, _holemenures: isize, _hwndactiveobject: HWND) -> Result<()> {
        Ok(())
    }
    fn RemoveMenusSB(&self, _hmenushared: HMENU) -> Result<()> {
        Ok(())
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
        self.view.borrow().clone().ok_or_else(|| E_FAIL.into())
    }
    fn OnViewWindowActive(&self, pshv: Option<&IShellView>) -> Result<()> {
        *self.view.borrow_mut() = pshv.cloned();
        Ok(())
    }
    fn SetToolbarItems(&self, _lpbuttons: *const TBBUTTON, _nbuttons: u32, _uflags: u32) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
}

/// Load the wallpaper bitmap (BMP/PNG/JPG via GDI+). Tries the configured path,
/// then the per-user Control Panel wallpaper value; `None` falls back to a solid
/// fill.
unsafe fn load_wallpaper(cfg: &Config) -> Option<HBITMAP> {
    let path = resolve_wallpaper_path(cfg)?;
    load_image_gdiplus(&path)
}

/// Load any GDI+-supported image (BMP/PNG/JPG/GIF) from `path` into a standalone
/// GDI `HBITMAP`. The HBITMAP is independent of GDI+ and outlives its shutdown.
unsafe fn load_image_gdiplus(path: &str) -> Option<HBITMAP> {
    use windows::Win32::Graphics::GdiPlus::{
        GdipCreateBitmapFromFile, GdipCreateHBITMAPFromBitmap, GdipDisposeImage, GdiplusShutdown,
        GdiplusStartup, GdiplusStartupInput, GpBitmap, GpImage, Ok as GpOk,
    };

    let mut token: usize = 0;
    let input = GdiplusStartupInput {
        GdiplusVersion: 1,
        ..Default::default()
    };
    if GdiplusStartup(&mut token, &input, core::ptr::null_mut()) != GpOk {
        return None;
    }

    let wpath = util::WideStr::new(path);
    let mut bitmap: *mut GpBitmap = core::ptr::null_mut();
    let result = if GdipCreateBitmapFromFile(wpath.pcwstr(), &mut bitmap) == GpOk
        && !bitmap.is_null()
    {
        let mut hbm = HBITMAP::default();
        // Opaque black background for any transparent pixels (PNG/GIF).
        let st = GdipCreateHBITMAPFromBitmap(bitmap, &mut hbm, 0xFF00_0000);
        GdipDisposeImage(bitmap as *mut GpImage);
        if st == GpOk && !hbm.is_invalid() {
            Some(hbm)
        } else {
            None
        }
    } else {
        None
    };

    GdiplusShutdown(token);
    result
}

/// Resolve the wallpaper path: the configured value, else the per-user Control
/// Panel wallpaper.
fn resolve_wallpaper_path(cfg: &Config) -> Option<String> {
    cfg.wallpaper.clone().or_else(control_panel_wallpaper)
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
        WM_TIMER if wp.0 == TIMER_LAYOUT => {
            // Resolve state, then act outside the borrow (file I/O + list msgs).
            let (tick, mut lv, view_hwnd, view, last) = DESKTOP.with_borrow_mut(|d| match d {
                Some(d) => {
                    d.ticks += 1;
                    (d.ticks, d.listview, d.view_hwnd, d._view.clone(), d.last_layout.clone())
                }
                None => (0, HWND::default(), HWND::default(), None, String::new()),
            });

            // The SysListView32 is created asynchronously after CreateViewWindow.
            // Once it appears, set the view flags (auto-arrange off / snap-to-grid
            // on) and remember the list for layout save/restore.
            if lv.is_invalid() {
                if let Ok(found) = FindWindowExW(view_hwnd, None, w!("SysListView32"), None) {
                    if !found.is_invalid() {
                        lv = found;
                        if let Some(v) = &view {
                            configure_view_flags(v);
                        }
                        DESKTOP.with_borrow_mut(|d| {
                            if let Some(d) = d {
                                d.listview = lv;
                            }
                        });
                    }
                }
            }
            if lv.is_invalid() {
                return LRESULT(0); // list not up yet; try next tick
            }

            if tick <= 4 {
                // Items load asynchronously; apply the saved layout a few times.
                if let Some(p) = layout_path() {
                    if let Ok(text) = std::fs::read_to_string(&p) {
                        apply_layout(lv, &text);
                    }
                }
            } else {
                let cur = capture_layout(lv);
                if cur != last {
                    if let Some(p) = layout_path() {
                        let _ = std::fs::write(&p, &cur);
                    }
                    DESKTOP.with_borrow_mut(|d| {
                        if let Some(d) = d {
                            d.last_layout = cur;
                        }
                    });
                }
            }
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
