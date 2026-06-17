# pe-settings-gtk

A **Settings** window for Windows PE, built with **GTK4 + Libadwaita** (Rust /
gtk4-rs). The libadwaita counterpart to StartPE's Win32/GDI settings pane
(`startpe/src/settings.rs`).

Toggles are `AdwSwitchRow`s grouped by surface (Taskbar / Windows / Menus); the
Start-button glyph color is a `GtkColorDialogButton`. Each change writes
`HKCU\Software\StartPE` (`src/settings_io.rs`) and **posts the registered
`StartPE_ReloadConfig` message to the running taskbar**, which re-reads its config
and applies the change live — the cross-process equivalent of the in-process
pane's `taskbar::reload_config()`. A named mutex enforces single instance and
Escape closes the window.

This is the third GTK4 *shell helper* (after `sysinfo-gtk` and `run-gtk`):
StartPE keeps its lean Win32 core and auto-detects this `Settings.exe` as a
sibling, falling back to its built-in pane if it (or the GTK runtime) is absent.

## Runtime requirement

A thin client of the shared GTK4 + Libadwaita runtime shipped by winrx-creator's
**GTK4Runtime** component. In WinPE the runtime sets `GSK_RENDERER=cairo` (no
GPU/DWM), which this app inherits.

## Building

Requires an **MSYS2 ucrt64** toolchain:

```bash
# in the MSYS2 UCRT64 shell
pacman -S --needed mingw-w64-ucrt-x86_64-rust mingw-w64-ucrt-x86_64-pkgconf \
                   mingw-w64-ucrt-x86_64-gcc  mingw-w64-ucrt-x86_64-gtk4 \
                   mingw-w64-ucrt-x86_64-libadwaita
cargo build --release      # -> target/release/Settings.exe
```
