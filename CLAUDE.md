# CLAUDE.md

Guidance for AI assistants working on this repository.

## What this is

StartPE is a free, GPLv3 **taskbar + start menu for Windows PE** environments —
a drop-in alternative to StartAllBack for PEBakery-based PE builds (PhoenixPE,
winrx-creator). Single Rust binary (~200 KB), no runtime dependencies.

Read `docs/ARCHITECTURE.md` first — it explains the design, the registry
configuration contract, and the milestone roadmap. Keep it and `README.md`
updated when behavior or config values change.

## Hard constraints (do not violate)

- **Documented Win32 APIs only — in `startpe.exe`.** No undocumented internals
 in the main binary. Pragmatic, confined exceptions exist, each isolated to one
 module: (1) `tray.rs`, the `Shell_NotifyIcon` WM_COPYDATA wire format (de-facto
 stable, used by every alternative shell); (2) `darkmode.rs`, the uxtheme
 dark-mode ordinals (build-gated, behind `DarkMenus`, fails closed to light
 menus); (3) `run_dialog.rs`, shell32's `RunFileDlg` (ordinal 61) to pop the
 real Run dialog with a proper icon/prompt. Keep any undocumented-ordinal work
 confined to its module; do not scatter such calls elsewhere.
- **`loader/` is the sandboxed exception.** `startpe_loader.dll` is loaded into
 `explorer.exe` (via a `Drive\shellex\FolderExtensions` COM registration) to
 keep Explorer's shell thread alive past the Win11 taskbar init on PE sources
 that would otherwise black-screen. This is the one place undocumented
 Explorer-internals work is allowed; keep it confined to `loader/` and keep
 `startpe.exe` clean so the main binary still survives Windows updates.
- **Must work in plain WinPE**: no DWM composition (rounded corners use GDI
  window regions, peek falls back from thumbnails to rows), no .NET, possibly
  limited fonts (UI glyphs use Segoe MDL2 Assets — degrade gracefully if you
  add more). Anything DWM-dependent needs a non-DWM fallback.
- **Explorer stays the shell.** StartPE runs alongside it, hides its taskbar
  (auto-hide via `ABM_SETSTATE` + `ShowWindow` hide — both, or you leave a
  black dead strip), and restores it on clean exit.
- Rendering is plain GDI, double-buffered. No UI framework. Keep the binary
  small; `Cargo.toml` release profile is tuned for size.

## Code conventions

- `windows` crate **0.58** (pinned style: handles are pointer wrappers,
  `CreateFontW` takes raw u32s, `GetLocalTime()` returns by value,
  `HTHUMBNAIL` is `isize`, optional PCWSTR params want `PCWSTR::null()` not
  `Option`). Check existing call sites before writing new FFI.
- Per-window state lives in `thread_local!` `RefCell`s (single UI thread).
  **The borrow rule:** inside a wndproc, *resolve* what to do while holding
  the borrow, then drop it and *act* — `ShellExecuteW`, `SetForegroundWindow`,
  `TrackPopupMenu` etc. pump messages and re-enter the wndproc. Every
  `WM_LBUTTONUP` handler follows this pattern; copy it.
- Module map: `taskbar.rs` (appbar, buttons, clock, tray rendering, Win-key
  hook, Explorer suppression), `start_menu.rs` (two-pane menu, search),
  `tray.rs` (Shell_TrayWnd host, icon registrations, click forwarding),
  `peek.rs` (hover previews), `alttab.rs` (Win11-style Alt+Tab switcher: LL
  keyboard hook + `PrintWindow` screenshot grid), `menu.rs` (dark owner-drawn
  popup menus), `darkmode.rs` (uxtheme dark app mode for shell menus),
  `run_dialog.rs` (shell Run dialog via RunFileDlg), `settings.rs` (dark
  settings pane: boolean config switches + Start button color picker, opened
  from the taskbar menu), `config.rs` (registry), `util.rs` (UTF-16).
- New user-facing settings: add to `config.rs` (registry value under
  `HKCU\Software\StartPE`), document in the `docs/ARCHITECTURE.md` table, and
  write the default in `pebakery/StartPE.script`. All three, every time. If the
  setting is a simple on/off, also add it to the `TOGGLES` table in
  `settings.rs` so it shows up in the settings pane.
- Changing an existing **default** is not enough to change behavior in a PE
  build. The PEBakery scripts write *every* StartPE value explicitly into
  `HKLM` at image-build time, and `config.rs` reads `HKLM` first — so an
  explicit script value always overrides the Rust default (which only applies
  when the key is absent). When you change a default, also update the matching
  `RegWrite` in **both** `pebakery/StartPE.script` and the deployed
  winrx-creator `050-StartPE.script` (`D:\winrx-creator\Projects\winrx-creator\Shell\050-StartPE.script`).
- License headers: `// SPDX-License-Identifier: GPL-3.0-or-later` on new files.

## Build & test

```
cargo build --workspace                                 # dev (startpe.exe + startpe_loader.dll)
cargo build --release --workspace                       # x64
cargo build --release --workspace --target aarch64-pc-windows-msvc  # ARM64
```

Builds must stay warning-free. There are no unit tests; verification is
manual. **Warning when testing on a real desktop:** `startpe.exe` hides the
actual Explorer taskbar and grabs the Win key while running. It restores both
on clean exit, but a force-kill skips that (recover by restarting
explorer.exe). Prefer a Windows 11 VM or a PE VM. Behavior differs between
full Windows (DWM on, UWP windows exist) and PE (no DWM, no UWP) — consider
both paths for any change in window enumeration, peek, or drawing.

## Releases / CI

`.github/workflows/build.yml` builds x64 + ARM64 on every push; pushing a tag
`v*` creates a GitHub release with assets `startpe.exe` and
`startpe-arm64.exe` — those exact names are what `pebakery/StartPE.script`
downloads from `releases/latest/download/`, so never rename them. Bump the
version in `Cargo.toml` and the script's `[Main]` section when tagging.

## PE integration context

The PEBakery script writes config into the mounted **Default user** hive at
image build time (offline), and launches StartPE via the Run key processed by
Explorer at logon. The reference for conventions is the PhoenixPE
StartAllBack script (MIT, by Homes32). A future milestone (M4) adds a compat
layer reading existing `Software\StartIsBack` values — see the roadmap.
