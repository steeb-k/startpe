// SPDX-License-Identifier: GPL-3.0-or-later
//! Dark mode for shell-rendered menus.
//!
//! There is no documented way to dark-mode a standard Win32 menu. The only
//! mechanism — used by Explorer itself and most dark-mode apps — is a handful of
//! *undocumented* `uxtheme.dll` functions exported by ordinal only. This module
//! is a deliberate, scoped exception to the "documented Win32 APIs only in
//! startpe.exe" rule (see CLAUDE.md): the undocumented surface is confined here,
//! gated on the Windows build, gated behind the `DarkMenus` config value
//! (default on), and fails closed to ordinary light menus if anything is
//! missing or the build is too old.
//!
//! It puts *our* process into forced-dark app mode, which themes menus created
//! in this process — chiefly the desktop right-click context menu, since StartPE
//! hosts the desktop shell view itself (`desktop.rs`). Menus owned by other
//! processes (per-app tray menus, Explorer folder windows) live in those
//! processes and are unaffected; they remain whatever the system theme dictates.

use std::cell::Cell;

use windows::core::{w, PCSTR, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Controls::SetWindowTheme;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetClassNameW, SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG,
    WM_SETTINGCHANGE,
};

/// `SetPreferredAppMode` argument: force dark regardless of the system theme
/// (PE's default apps theme is usually light, so `AllowDark` wouldn't be enough).
const APPMODE_FORCE_DARK: i32 = 2;

/// First build with the dark-mode ordinals (1809). `SetPreferredAppMode`
/// (ordinal 135) replaced `AllowDarkModeForApp` at build 18334; before that,
/// ordinal 135 *is* `AllowDarkModeForApp(BOOL)`.
const BUILD_DARK_MODE: u32 = 17763;
const BUILD_SET_PREFERRED: u32 = 18334;

/// Undocumented uxtheme entry points, resolved by ordinal. `set_preferred` and
/// `allow_for_app` are the two shapes ordinal 135 takes across builds; exactly
/// one is populated.
#[derive(Clone, Copy)]
struct Fns {
    set_preferred: Option<unsafe extern "system" fn(i32) -> i32>,
    allow_for_app: Option<unsafe extern "system" fn(BOOL) -> BOOL>,
    allow_for_window: unsafe extern "system" fn(HWND, BOOL) -> BOOL,
    flush_menu_themes: unsafe extern "system" fn(),
    refresh_color_policy: unsafe extern "system" fn(),
}

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static FNS: Cell<Option<Fns>> = const { Cell::new(None) };
}

/// OS build number from the registry (documented value; avoids another
/// undocumented call just to version-gate).
fn os_build() -> u32 {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
        .ok()
        .and_then(|k| k.get_value::<String, _>("CurrentBuildNumber").ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

unsafe fn load(build: u32) -> Option<Fns> {
    let ux = LoadLibraryW(w!("uxtheme.dll")).ok()?;
    // Resolve by ordinal: MAKEINTRESOURCEA(n) is just the ordinal in the low word.
    let by_ord = |n: usize| GetProcAddress(ux, PCSTR(n as *const u8));

    let allow_for_window: unsafe extern "system" fn(HWND, BOOL) -> BOOL =
        std::mem::transmute(by_ord(133)?);
    let flush_menu_themes: unsafe extern "system" fn() = std::mem::transmute(by_ord(136)?);
    let refresh_color_policy: unsafe extern "system" fn() = std::mem::transmute(by_ord(104)?);

    let ord135 = by_ord(135)?;
    let (set_preferred, allow_for_app) = if build >= BUILD_SET_PREFERRED {
        (Some(std::mem::transmute(ord135)), None)
    } else {
        (None, Some(std::mem::transmute(ord135)))
    };

    Some(Fns {
        set_preferred,
        allow_for_app,
        allow_for_window,
        flush_menu_themes,
        refresh_color_policy,
    })
}

/// Put the process into forced-dark app mode (best-effort) so shell menus created
/// here render dark. Call once at startup, before creating windows. A no-op when
/// `enabled` is false, the build is too old, or the ordinals can't be resolved.
pub fn init(enabled: bool) {
    ENABLED.set(enabled);
    if !enabled {
        log_status("disabled by config", 0);
        return;
    }
    let build = os_build();
    if build < BUILD_DARK_MODE {
        log_status("unsupported build", build);
        return;
    }
    unsafe {
        let Some(fns) = load(build) else {
            log_status("ordinals unavailable", build);
            return;
        };
        if let Some(set) = fns.set_preferred {
            set(APPMODE_FORCE_DARK);
        } else if let Some(allow) = fns.allow_for_app {
            let _ = allow(TRUE);
        }
        (fns.refresh_color_policy)();
        (fns.flush_menu_themes)();
        FNS.set(Some(fns));
    }
    log_status("forced-dark engaged", build);
}

/// Allow dark mode for `hwnd` and re-flush menu themes, so menus this window
/// raises (e.g. the hosted desktop's context menu) pick up the dark theme.
/// No-op unless [`init`] successfully engaged dark mode.
pub fn allow_window(hwnd: HWND) {
    if !ENABLED.get() {
        return;
    }
    if let Some(fns) = FNS.get() {
        unsafe {
            let _ = (fns.allow_for_window)(hwnd, TRUE);
            (fns.flush_menu_themes)();
        }
    }
}

/// Put Windows into dark (or light) *app* mode by writing the documented system
/// theme setting, so theme-aware apps launched afterward — chiefly the Win11
/// Task Manager — come up dark on their own. Plain registry + a settings
/// broadcast (no undocumented ordinals), so this is independent of [`init`] and
/// always available.
///
/// Why write it at runtime: in a PE the shell runs as **SYSTEM**, so the `HKCU`
/// written here *is* the SYSTEM profile — the same hive those apps read
/// `AppsUseLightTheme` from. Writing it into the offline Default-user hive at
/// image-build time does nothing, since SYSTEM never sees that hive (the same
/// reason StartPE's own config goes in HKLM). This replaces that dead write.
pub fn apply_app_theme(enabled: bool) {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    // The value is "use *light* theme": 0 = dark, 1 = light.
    let light: u32 = u32::from(!enabled);
    let ok = (|| -> std::io::Result<()> {
        let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(
            r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
        )?;
        key.set_value("AppsUseLightTheme", &light)?;
        key.set_value("SystemUsesLightTheme", &light)?;
        Ok(())
    })()
    .is_ok();
    // Nudge already-running theme-aware apps to re-read the setting.
    unsafe {
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(w!("ImmersiveColorSet").as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            100,
            None,
        );
    }
    log_line(&format!(
        "app theme: {} (registry write {})",
        if enabled { "dark" } else { "light" },
        if ok { "ok" } else { "failed" }
    ));
}

/// Dark-theme a dialog that lives in *our* process — specifically the shell Run
/// dialog, whose `RunFileDlg` pumps its modal loop on this thread. Enables dark
/// mode for the dialog and each child control and applies the matching dark
/// visual style per class. The caller still has to handle `WM_CTLCOLOR*` (a
/// dialog paints its own background/text) via a subclass. Returns whether dark
/// was applied: `false` when dark mode isn't engaged, so the caller can skip the
/// subclass and leave the dialog light.
pub fn dark_dialog(hwnd: HWND) -> bool {
    if !ENABLED.get() {
        return false;
    }
    let Some(fns) = FNS.get() else {
        return false;
    };
    unsafe {
        let _ = (fns.allow_for_window)(hwnd, TRUE);
        let _ = EnumChildWindows(hwnd, Some(theme_child), LPARAM(0));
        (fns.flush_menu_themes)();
    }
    true
}

/// `EnumChildWindows` callback: enable dark mode for a child control and give it
/// the dark visual style for its window class.
unsafe extern "system" fn theme_child(hwnd: HWND, _l: LPARAM) -> BOOL {
    if let Some(fns) = FNS.get() {
        let _ = (fns.allow_for_window)(hwnd, TRUE);
    }
    let mut buf = [0u16; 32];
    let n = GetClassNameW(hwnd, &mut buf) as usize;
    let class = String::from_utf16_lossy(&buf[..n]);
    // "CFD" is the dark style comdlg32 / comboboxes use; everything else takes
    // the generic dark Explorer style (push buttons darken under it).
    let theme = match class.as_str() {
        "ComboBox" | "Edit" => w!("DarkMode_CFD"),
        _ => w!("DarkMode_Explorer"),
    };
    let _ = SetWindowTheme(hwnd, theme, PCWSTR::null());
    TRUE
}

fn log_status(state: &str, build: u32) {
    log_line(&format!("dark menus: {state} (build {build})"));
}

/// Append a version-stamped line to the PE log (no Event Viewer in WinPE).
fn log_line(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("X:\\startpe.log")
    {
        let _ = writeln!(f, "StartPE v{} {}", env!("CARGO_PKG_VERSION"), msg);
    }
}
