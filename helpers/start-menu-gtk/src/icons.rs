// SPDX-License-Identifier: GPL-3.0-or-later
//! Convert a Win32 `HICON` (shell file icon) into a `gdk::Texture` for display in
//! GTK. Reads the icon's 32-bpp color bitmap via `GetDIBits` (top-down BGRA) and
//! wraps it as a `GdkMemoryTexture`. Falls back to opaque if the icon carries no
//! alpha channel. Consumes (destroys) the `HICON`.

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;

use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

/// Convert and then destroy `hicon`. Returns `None` if anything fails.
pub fn texture_from_hicon(hicon: HICON) -> Option<gdk::Texture> {
    let tex = unsafe { convert(hicon) };
    unsafe {
        let _ = DestroyIcon(hicon);
    }
    tex
}

unsafe fn convert(hicon: HICON) -> Option<gdk::Texture> {
    let mut ii = ICONINFO::default();
    if GetIconInfo(hicon, &mut ii).is_err() {
        return None;
    }
    let color = ii.hbmColor;
    let mask = ii.hbmMask;
    let cleanup = || {
        if !color.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(color.0));
        }
        if !mask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(mask.0));
        }
    };
    if color.is_invalid() {
        cleanup();
        return None;
    }

    let mut bm = BITMAP::default();
    let got = GetObjectW(
        HGDIOBJ(color.0),
        std::mem::size_of::<BITMAP>() as i32,
        Some(&mut bm as *mut _ as *mut core::ffi::c_void),
    );
    let (w, h) = (bm.bmWidth, bm.bmHeight);
    if got == 0 || w <= 0 || h <= 0 {
        cleanup();
        return None;
    }

    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0, // BI_RGB
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = vec![0u8; (w * h * 4) as usize];
    let dc = GetDC(None);
    let lines = GetDIBits(
        dc,
        color,
        0,
        h as u32,
        Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
        &mut bmi,
        DIB_RGB_COLORS,
    );
    ReleaseDC(None, dc);
    cleanup();
    if lines == 0 {
        return None;
    }

    // Some shell icons come back with no alpha (all zero) — show them opaque.
    if buf.chunks_exact(4).all(|p| p[3] == 0) {
        for p in buf.chunks_exact_mut(4) {
            p[3] = 255;
        }
    }

    let bytes = glib::Bytes::from(&buf[..]);
    let texture =
        gdk::MemoryTexture::new(w, h, gdk::MemoryFormat::B8g8r8a8, &bytes, (w * 4) as usize);
    Some(texture.upcast())
}
