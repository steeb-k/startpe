// SPDX-License-Identifier: GPL-3.0-or-later
//! Native taskbar / Alt+Tab icon for a GTK helper window.
//!
//! GDK gives the window GTK's default icon; StartPE's taskbar probes the native
//! per-window icon (`WM_GETICON`). So once the window maps we render an
//! accent-tinted Segoe MDL2 glyph — the same treatment StartPE's GDI applets
//! use (see `startpe/src/sysinfo.rs::make_glyph_icon`, of which this is a
//! port) — and attach it with `WM_SETICON`. Plain documented GDI.
//!
//! This module is duplicated verbatim across the GTK helpers (each is its own
//! crate); each helper passes its own glyph so the apps read as distinct.

use windows::core::w;
use windows::Win32::Foundation::{BOOL, COLORREF, HWND, LPARAM, RECT, TRUE, WPARAM};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject,
    DrawTextW, GdiFlush, GetDC, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
    ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
    DIB_RGB_COLORS, DT_CENTER, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HGDIOBJ,
    OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, EnumWindows, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, SendMessageW, HICON, ICONINFO, ICON_BIG, ICON_SMALL, WM_SETICON,
};

/// Find this process's top-level window titled `title` and apply the glyph
/// icon to it — for helpers that don't already hold their native HWND.
pub fn apply_to_own_window(title: &str, glyph: char) {
    unsafe {
        let mut data = (GetCurrentProcessId(), HWND::default(), title);
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut data as *mut _ as isize));
        if !data.1.is_invalid() {
            apply(data.1, glyph);
        }
    }
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let data = &mut *(lparam.0 as *mut (u32, HWND, &str));
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == data.0 && window_title(hwnd) == data.2 {
        data.1 = hwnd;
        return BOOL(0); // found it; stop enumerating
    }
    BOOL(1)
}

unsafe fn window_title(hwnd: HWND) -> String {
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let n = GetWindowTextW(hwnd, &mut buf);
    String::from_utf16_lossy(&buf[..n as usize])
}

/// Attach the accent-tinted `glyph` as this window's big + small icon.
pub fn apply(hwnd: HWND, glyph: char) {
    unsafe {
        let color = start_button_color();
        for (which, size) in [(ICON_BIG, 32), (ICON_SMALL, 16)] {
            let icon = make_glyph_icon(glyph, color, size);
            if !icon.is_invalid() {
                // The previous icon (GTK's) is the window's to keep; ours lives
                // for the process lifetime, so no cleanup either.
                SendMessageW(hwnd, WM_SETICON, WPARAM(which as usize), LPARAM(icon.0 as isize));
            }
        }
    }
}

/// StartPE's accent (`StartButtonColor`, COLORREF 0x00BBGGRR) — HKLM then HKCU,
/// the same order StartPE's own config reader uses; defaults to its purple.
fn start_button_color() -> u32 {
    let mut v = 0x00E6_5AB4u32;
    for hive in [
        winreg::enums::HKEY_LOCAL_MACHINE,
        winreg::enums::HKEY_CURRENT_USER,
    ] {
        if let Ok(k) = winreg::RegKey::predef(hive).open_subkey("Software\\StartPE") {
            if let Ok(x) = k.get_value::<u32, _>("StartButtonColor") {
                v = x;
            }
        }
    }
    v
}

/// Build a `size`px `HICON` from a Segoe MDL2 `glyph`, tinted `color`
/// (COLORREF 0x00BBGGRR) with an antialiased alpha edge. We draw the glyph
/// white into a 32bpp DIB (GDI leaves alpha at 0), then read its luminance as
/// the alpha coverage and recolor to `color`, premultiplied.
unsafe fn make_glyph_icon(glyph: char, color: u32, size: i32) -> HICON {
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
