// SPDX-License-Identifier: GPL-3.0-or-later
//! Native taskbar / Alt+Tab icon for the Run helper.
//!
//! GDK gives the window GTK's default icon; StartPE's taskbar probes the native
//! per-window icon (`WM_GETICON`). So once the window maps we render the same
//! accent-tinted Segoe MDL2 "Run" glyph (U+E74C) that StartPE's GDI Run box uses
//! (see `startpe/src/run_window.rs` + `sysinfo::make_glyph_icon`, of which this
//! is a port) and attach it with `WM_SETICON`. Plain documented GDI.

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, RECT, TRUE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject,
    DrawTextW, GdiFlush, GetDC, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
    ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
    DIB_RGB_COLORS, DT_CENTER, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HGDIOBJ,
    OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, SendMessageW, HICON, ICONINFO, ICON_BIG, ICON_SMALL, WM_SETICON,
};

/// Attach the accent-tinted Run glyph as this window's big + small icon.
pub fn apply(hwnd: HWND) {
    unsafe {
        let color = start_button_color();
        for (which, size) in [(ICON_BIG, 32), (ICON_SMALL, 16)] {
            let icon = make_glyph_icon('\u{E74C}', color, size);
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
