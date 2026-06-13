# StartPE

A free, open-source (GPLv3) **taskbar, start menu, and desktop for Windows PE**.

StartPE runs *alongside* Explorer instead of injecting into it: it draws its own
taskbar/start menu/desktop with plain GDI and documented Win32, and hides
Explorer's own taskbar. That makes it small (single ~370 KB `startpe.exe`, no
runtime dependencies).

## Status

**Usable.** StartPE is an open source alternative to StartAllBack for PE environments.
See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and the roadmap
(theming, a clock calendar flyout, and more customization).

## Features

### Taskbar
- Bottom-docked **appbar** (registered with the shell, so maximized windows
  respect it). Hides Explorer's Win11 taskbar and keeps it hidden while running;
  restores it on clean exit.
- Centered (Windows 11 style) or left-aligned button cluster (`CenterTaskbar`).
- Rounded, double-buffered GDI buttons; **icon-only** by default with same-app
  **combining** (click cycles the app's windows), or per-window buttons and
  text labels (`TaskbarCombine` / `TaskbarLabels`).
- **Hover peek**: previews of every window in a group with per-window close
  buttons — live DWM thumbnails where composition exists, icon + title rows in
  plain PE.
- Recolorable Start button glyph (`StartButtonColor`), a show-desktop button,
  and a locale-formatted clock with date.
- **Pinned apps** from the winrx-creator/PhoenixPE `PinUtil.ini` — shown even
  when not running, and launched on click.
- High-resolution app icons, cloaked/phantom UWP hosts
  filtered out, UWP windows grouped and icon'd via their real app process.

### Start menu
- Two-pane classic Win7/Win10 layout with rounded corners (window region,
  no DWM needed), floating above the taskbar; follows the taskbar's alignment
  (centered, or bottom-left when the taskbar is left-aligned).
- Left pane: apps from the Start Menu folders (`%ProgramData%` + `%APPDATA%`)
  with shell icons and folder drill-down, or a **pinned view** from `PinUtil.ini`
  with an All apps / Pinned toggle.
- **Live search box** — type to filter the indexed shortcuts (with a blinking
  caret); Enter launches the top hit.
- Right pane: circular user picture (`UserPicture` .bmp) protruding above the
  menu, plus links — user profile, Downloads, This PC, Control Panel, Command
  Prompt, and Run… (the real shell Run dialog).
- Shut down button with a Restart / Shut down flyout (`wpeutil`).

### Keyboard navigation
- Opens with the search box focused; typing always searches.
- Arrow keys move a focus highlight across the program list, the right-pane
  links, the search box, and the power button; **Enter** activates.
- **Right** expands a `>` folder row; from the search box, **Right** reaches the
  Shut down button, and **Right** again opens the power flyout (Restart
  highlighted by default).

### System tray
- StartPE hosts `Shell_NotifyIcon` registrations itself (its own `Shell_TrayWnd`,
  `TaskbarCreated` broadcast on startup), draws the icons next to the clock, and
  forwards left/right clicks to the owning apps. Appbar traffic is proxied to
  Explorer's tray, and NIM traffic mirrored, so Explorer stays consistent.

### Settings pane
- Right-click the taskbar → **Settings** for an in-app, dark, owner-drawn
  settings window: on/off switches grouped by surface (Taskbar / Menus), plus a
  **Start button color** picker (preset swatches + a Custom… color dialog).
  Changes are written to the registry and applied live where possible.

### Window switching & hotkeys
- Windows 11-style **Alt+Tab** switcher (centered overlay, `PrintWindow`
  screenshots, no DWM dependency).
- The **Win key** opens the StartPE menu; **Win+R** (Run), **Win+E** (Explorer),
  **Win+D** (show desktop), and **Win+X** (power-user menu) are handled directly
  (other Win combos pass through).
- A Windows 11-style **power-user menu** (Win+X, or right-click the start button):
  Event Viewer, System (System Properties), Device Manager, Disk Management,
  Computer Management, Terminal (the default `%ComSpec%` processor), Task Manager,
  File Explorer, Run, a Shut down / Restart flyout, and Desktop — the PE-relevant
  subset of the Windows 11 menu. Drawn as a rounded, dark, custom popup (no DWM
  required), with hover/keyboard navigation and a submenu flyout.

### Dark theming
- Dark, rounded, custom-drawn popup menus (taskbar context menu, power flyout,
  Win+X menu) — rounded corners without DWM, correct separator behavior.
- Dark-mode for the shell-rendered menus StartPE raises (chiefly the hosted
  desktop's right-click menu) via uxtheme app mode (`DarkMenus`, opt-out).

### Desktop (when Explorer can't provide one)
On Win11 24H2/25H2 PE sources, Explorer's modern taskbar init fail-fasts and the
desktop (`Progman`/`SHELLDLL_DefView`) is never created. When StartPE detects
this it **provides the desktop itself** (`OwnDesktop`): a `Progman`-style window
painting the wallpaper (BMP/PNG/JPG via GDI+) and hosting a *real* shell icon
view of the Public Desktop — with working right-click menus, double-click, icon
drag, and layout save/restore to `desktop-layout.txt`. On a normal box (or a PE
where Explorer's desktop appears) it detects that and stays out of the way.

A small companion DLL, `startpe_loader.dll`, can be COM-registered so Explorer
loads it early to keep its shell thread alive past the Win11 taskbar init; this
is the one component permitted to touch Explorer internals (`startpe.exe` itself
stays documented Win32).

## Configuration

Read once at startup from `HKLM\Software\StartPE`, then overlaid by
`HKCU\Software\StartPE`. PE runs the shell as `SYSTEM`, so a PEBakery build
writes config machine-wide into the offline SOFTWARE hive; the in-app settings
pane writes runtime changes to `HKCU`. See the value table in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Building

```
cargo build --release --workspace                                   # x64 (startpe.exe + startpe_loader.dll)
cargo build --release --workspace --target aarch64-pc-windows-msvc  # ARM64
```

Produces a single self-contained `startpe.exe` (~370 KB, no runtime
dependencies) and the optional `startpe_loader.dll`.

## Testing on a full Windows machine

StartPE hides the real Explorer taskbar and grabs the Win key while it runs. It
restores both on clean exit, but a force-kill skips that (recover by restarting
`explorer.exe`). A Windows 11 VM or a PE VM is strongly recommended — behavior
differs between full Windows (DWM, UWP windows) and PE (neither).

## PE integration

`pebakery/StartPE.script` copies the binary into the image and writes the launch
and configuration registry values into the mounted SOFTWARE hive at build time.
It's modeled on PhoenixPE scripts but can be modified to work in any PEBakery build.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
