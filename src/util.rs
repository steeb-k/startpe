// SPDX-License-Identifier: GPL-3.0-or-later
//! Small helpers shared by the taskbar and start menu.

use windows::core::PCWSTR;

/// UTF-16, NUL-terminated copy of `s`.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
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
