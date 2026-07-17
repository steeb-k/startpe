// SPDX-License-Identifier: GPL-3.0-or-later
//! Windows-native styling for the GTK helpers: Segoe UI at the Windows point
//! size, Win11 corner radii and window-control buttons, and libadwaita's
//! accent named colors redefined from StartPE's `StartButtonColor` so every
//! helper follows the shell accent (switches, suggested-action buttons,
//! selected rows, focus rings — anything Adwaita draws with `@accent_*`).
//!
//! Installed at `STYLE_PROVIDER_PRIORITY_APPLICATION`; that priority is what
//! lets the `@define-color` overrides beat Adwaita's own named colors.
//!
//! This module is duplicated verbatim across the GTK helpers (each is its own
//! crate), like `winicon.rs`.

/// The Windows layer. `{accent}` is substituted before loading.
///
/// GTK4 on Windows has no desktop font / text-scaling setting to read, so the
/// default UI font renders smaller than native apps. libadwaita sizes controls
/// relative to the font, so setting the Windows 11 body size (Segoe UI 11pt)
/// scales the whole UI to match. Segoe UI Variable is first choice on full
/// Windows; a plain PE only ships classic Segoe UI, hence the fallback chain.
const WIN_CSS: &str = r#"
window,
popover,
tooltip {
  font-family: "Segoe UI Variable Text", "Segoe UI", sans-serif;
  font-size: 11pt;
}

@define-color accent_color {accent};
@define-color accent_bg_color {accent};
@define-color accent_fg_color #ffffff;

button {
  border-radius: 6px;
}

entry,
.boxed-list {
  border-radius: 8px;
}

/* Merge the title bar into the window body (one solid surface, like Win11
   apps) and give it a Win11-ish height. */
headerbar {
  background-color: @window_bg_color;
  box-shadow: none;
  min-height: 40px;
  padding: 0;
}

/* Window control buttons: Windows 11 style — rectangular, flush to the frame,
   subtle hover, red close. */
windowcontrols button {
  border-radius: 0;
  min-width: 0;
  min-height: 0;
  margin: -5px;
  padding: 0 13px;
  background-color: transparent;
  box-shadow: none;
}

windowcontrols button:hover {
  background-color: alpha(@window_fg_color, 0.09);
}

windowcontrols button image {
  background: none;
  background-color: transparent;
  border-radius: 0;
  box-shadow: none;
}

/* Round only the close button's top-right corner to match the window corner. */
windowcontrols button.close {
  border-top-right-radius: 8px;
}

windowcontrols button.close:hover {
  background-color: #c42b1c;
  color: #ffffff;
}

windowcontrols button.close:hover image {
  color: #ffffff;
}

windowcontrols button.close:active {
  background-color: #b0271a;
  color: #ffffff;
}
"#;

/// Install the Windows layer on the default display. Call once per process,
/// alongside (after) the helper's own CSS provider.
pub fn apply() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_data(&WIN_CSS.replace("{accent}", &accent_hex()));
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

/// StartPE's accent (`StartButtonColor`, COLORREF 0x00BBGGRR) as CSS
/// `#rrggbb` — HKLM then HKCU, the same order StartPE's own config reader
/// uses; defaults to its purple.
fn accent_hex() -> String {
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
    format!(
        "#{:02x}{:02x}{:02x}",
        v & 0xFF,
        (v >> 8) & 0xFF,
        (v >> 16) & 0xFF
    )
}
