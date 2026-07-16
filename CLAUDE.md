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
 menus). Keep any undocumented-ordinal work confined to its module; do not
 scatter such calls elsewhere. (The Run box is now `run_window.rs`, a from-
 scratch dark window built on documented APIs only — no ordinals.)
- **`loader/` is the sandboxed exception.** `startpe_loader.dll` is loaded into
 `explorer.exe` (via a `Drive\shellex\FolderExtensions` COM registration) to
 keep Explorer's shell thread alive past the Win11 taskbar init on PE sources
 that would otherwise black-screen. This is the one place undocumented
 Explorer-internals work is allowed; keep it confined to `loader/` and keep
 `startpe.exe` clean so the main binary still survives Windows updates.
- **`syslaunch/` builds `syslaunch.exe`** (separate workspace crate, NOT part of
 `startpe.exe`): runs a program as SYSTEM on a chosen interactive session's
 desktop, so StartPE can be DWM-composited while keeping SYSTEM privileges. It is
 how StartPE gets DWM on 25H2 PE — the build auto-logs-in as Administrator (so
 `winlogon` spawns `dwm.exe`) and StartPE self-promotes: with `LaunchAsSystem=1`
 it re-launches itself as SYSTEM via `syslaunch` when it starts under a lesser
 token (vector-agnostic — beats the launch-vector race for the single-instance
 mutex). syslaunch uses the LocalSystem service route (PsExec `-s` style) when run
 as a mere Administrator. Documented Win32 only (token duplication + SCM +
 `CreateProcessAsUserW`); no Explorer/undocumented internals. See
 `docs/ARCHITECTURE.md` → "DWM under SYSTEM".
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
- The hosted desktop (`desktop.rs`) has two easy-to-lose invariants: (1) the
  main message loop must offer key messages to the view's
  `IShellView::TranslateAccelerator` while focus is inside it — that is the
  documented IShellBrowser-host contract and the *only* thing that makes
  Delete/F2/Ctrl+C/V work (raw vtable call, because windows-rs collapses the
  S_OK/S_FALSE distinction the return value carries); (2) the icon-list
  subclass swallows the left button, so it must `SetFocus` the list itself or
  keyboard input never reaches the desktop. Icon dragging is a ghost drag
  (`LVM_CREATEDRAGIMAGE` + `ImageList_BeginDrag/DragMove`), items repositioned
  only on drop — don't "simplify" it back to live `LVM_SETITEMPOSITION` moves.
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
  `border.rs` (accent window frame, no-DWM GDI overlay) and `dwm_border.rs`
  (accent frame for the DWM path via `DWMWA_BORDER_COLOR`, accent/gray by focus),
  `run_window.rs` (from-scratch dark Run window), `settings.rs` (dark
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

`helpers/` holds the GTK4/Libadwaita shell helpers (`helpers/sysinfo-gtk` →
`SystemInfo.exe`, `helpers/run-gtk` → `RunBox.exe`, `helpers/settings-gtk` →
`Settings.exe`, `helpers/start-menu-gtk` → `StartMenu.exe`). They are **excluded
from the MSVC workspace** and
build with the **MSYS2 ucrt64** toolchain + the shipped GTK runtime, not with the
commands above — build them separately (`cd helpers/sysinfo-gtk && cargo build
--release` in a ucrt64 shell; CI builds all of them via the `helpers-gtk` matrix
job). They ship as extra assets in the same release; `startpe.exe` auto-detects
them as siblings and the built-in GDI windows remain the fallback (so the main
binary never depends on the GTK runtime). Keep `startpe.exe` itself free of any
GTK/runtime dependency.

Gotchas when adding/maintaining a helper:
- **No local GTK toolchain on this machine.** The helpers build only in CI
  (`helpers-gtk` matrix job, MSYS2 ucrt64 + gtk4/libadwaita); `C:\gtk-msys2-x64`
  is just the shipped runtime prefix, with no compiler or pkg-config. To verify
  helper changes locally, keep Win32-only logic in toolkit-free modules (e.g.
  `appsource.rs`, `winicon.rs` import no GTK) and `cargo check` them in a scratch
  crate with the same `windows` features. Final exes come from CI.
- **GTK windows and StartPE's own taskbar/Alt+Tab.** All GTK4 toplevels share
  one window class, and GDK caches ex-style bits — a `WS_EX_TOOLWINDOW` set from
  the helper can race the taskbar's enumeration or be rewritten by GDK. A helper
  window that must never get a task button is excluded *in StartPE* by its exact
  window title (see `is_task_window` in `taskbar.rs`, "StartPE Menu"). Windows
  that *should* get a button need a native icon: GDK's default is the generic
  GTK icon, so send `WM_SETICON` on the mapped HWND (`run-gtk/src/winicon.rs`,
  a port of `sysinfo::make_glyph_icon` — reuse it for new helpers).
- **Resizable helpers must clamp maximize.** StartPE's taskbar does *not* reserve
  the work area (`SPI_GETWORKAREA` is the full screen under StartPE), so a
  maximizable window maximizes full-screen and the bar clips it. A resizable helper
  must subclass its native HWND and clamp `WM_GETMINMAXINFO` to the work area minus
  the `StartPE_Taskbar` strip — see `helpers/sysinfo-gtk/src/winfix.rs`. Fixed-size
  helpers (run, settings) avoid this by being non-resizable.
- **Live config changes use a registered message, not a shared call.** A helper that
  changes `HKCU\Software\StartPE` and needs the *running* shell to react posts the
  registered `StartPE_ReloadConfig` message (`RegisterWindowMessageW`, same string
  both sides) to the `StartPE_Taskbar` window; the taskbar wndproc calls
  `reload_config()`. See `helpers/settings-gtk/src/settings_io.rs` and the
  `reload_msg` handler in `taskbar.rs`.

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

The PEBakery script writes config machine-wide into the offline **SOFTWARE**
hive (`HKLM\Software\StartPE`) at image build time, because the PE shell runs as
`SYSTEM` and never sees the Default-user hive as `HKCU` at runtime. It also wires
up launch (Run key / `AddAutoRun,PostShell`). The reference for script
conventions is the PhoenixPE StartAllBack script (MIT, by Homes32).
