// SPDX-License-Identifier: GPL-3.0-or-later
//! Small helpers shared by the taskbar and start menu.

use core::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};

/// UTF-16, NUL-terminated copy of `s`.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// File name of `path` without directory or extension
/// (`B:\Programs\7-Zip\7zFM.exe` -> `7zFM`).
pub fn file_stem(path: &str) -> String {
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    match name.rfind('.') {
        Some(dot) => name[..dot].to_string(),
        None => name.to_string(),
    }
}

/// The friendly application name from `path`'s version resource — its
/// `FileDescription` (what Explorer shows for a shortcut), then `ProductName`,
/// falling back to the file stem. So `hwinfo64.exe` -> "HWiNFO64",
/// `Explorer.exe` -> "Windows Explorer".
pub fn app_display_name(path: &str) -> String {
    version_string(path, "FileDescription")
        .or_else(|| version_string(path, "ProductName"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| file_stem(path))
}

/// Read one `StringFileInfo` field from a file's version resource.
fn version_string(path: &str, field: &str) -> Option<String> {
    unsafe {
        let wpath = WideStr::new(path);
        let size = GetFileVersionInfoSizeW(wpath.pcwstr(), None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(wpath.pcwstr(), 0, size, data.as_mut_ptr() as *mut c_void).ok()?;

        // Pick the first language/codepage translation the file declares.
        let tr_sub = WideStr::new("\\VarFileInfo\\Translation");
        let mut tr_ptr: *mut c_void = std::ptr::null_mut();
        let mut tr_len: u32 = 0;
        if !VerQueryValueW(
            data.as_ptr() as *const c_void,
            tr_sub.pcwstr(),
            &mut tr_ptr,
            &mut tr_len,
        )
        .as_bool()
            || tr_ptr.is_null()
            || tr_len < 4
        {
            return None;
        }
        let lang = *(tr_ptr as *const u16);
        let codepage = *((tr_ptr as *const u16).add(1));

        let sub = WideStr::new(&format!(
            "\\StringFileInfo\\{lang:04x}{codepage:04x}\\{field}"
        ));
        let mut val_ptr: *mut c_void = std::ptr::null_mut();
        let mut val_len: u32 = 0;
        if !VerQueryValueW(
            data.as_ptr() as *const c_void,
            sub.pcwstr(),
            &mut val_ptr,
            &mut val_len,
        )
        .as_bool()
            || val_ptr.is_null()
            || val_len == 0
        {
            return None;
        }
        let slice = std::slice::from_raw_parts(val_ptr as *const u16, val_len as usize);
        Some(String::from_utf16_lossy(slice).trim_end_matches('\0').to_string())
    }
}

/// Owns a NUL-terminated UTF-16 buffer so a PCWSTR stays valid for a call.
pub struct WideStr(Vec<u16>);

impl WideStr {
    pub fn new(s: &str) -> Self {
        Self(wide(s))
    }

    pub fn pcwstr(&self) -> PCWSTR {
        PCWSTR(self.0.as_ptr())
    }
}

pub fn loword(v: isize) -> i32 {
    (v & 0xffff) as i16 as i32
}

pub fn hiword(v: isize) -> i32 {
    ((v >> 16) & 0xffff) as i16 as i32
}
