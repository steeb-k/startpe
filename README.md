# StartPE

A free, open-source (GPLv3) **taskbar, start menu, and desktop for Windows PE**.

StartPE runs *alongside* Explorer instead of injecting into it: it draws its own
taskbar/start menu/desktop with plain GDI and documented Win32, and hides
Explorer's own taskbar. The core is a single self-contained `startpe.exe`
(~520 KB, no runtime dependencies, x64-only); an optional suite of
GTK4/Libadwaita helper apps (start menu, Run, Settings, System Information)
rides on top for a modern look where the shared GTK runtime is present, with the
built-in GDI windows always available as the fallback.

## Status

**Usable.** StartPE is an open source alternative to StartAllBack for PE environments.
See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and the roadmap
(theming, a clock calendar flyout, and more customization).

## Features

### Taskbar
- Bottom-docked **appbar** that reserves its strip in the work area itself
  (`SPI_SETWORKAREA` — the shell's appbar reservation doesn't function on
  stripped PEs), so maximized windows land above the bar. Hides Explorer's
  Win11 taskbar and keeps it hidden while running; restores it (and the work
  area) on clean exit.
- Centered (Windows 11 style) or left-aligned button cluster (`CenterTaskbar`).
- Rounded, double-buffered GDI buttons; **icon-only** by default with same-app
  **combining** (click cycles the app's windows), or per-window buttons and
  text labels (`TaskbarCombine` / `TaskbarLabels`).
- **Instant updates**: new/closed windows appear and disappear immediately
  (WinEvent hook), with the shell hook and a slow watchdog as backstops.
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
Two start menus ship; both float above the taskbar and follow its alignment.

- **GTK menu (`StartMenu.exe`, the default when present)** — a
  GTK4/Libadwaita two-pane menu, pre-warmed hidden at startup so the Win key
  opens it instantly. Left pane: a **pinned view** from `PinUtil.ini`
  (32px icons, classic start-menu sizing) with an "All apps ›" toggle that
  slides the full program list in from the right ("‹ Back" slides the pins
  back in), folder drill-down, and a search box at the bottom. Right pane:
  user avatar (`UserPicture`), Downloads / This PC / Control Panel / Terminal
  links, Run…, and a Power flyout (Restart / Shut down).
- **Built-in GDI menu** — the same two-pane layout drawn with plain GDI
  (rounded via window regions, no DWM needed): Start Menu folders with shell
  icons and drill-down, pinned view, live search with Enter-launches-top-hit,
  full keyboard navigation, and the same right-pane links. Always available;
  used automatically when the GTK helper or runtime is absent.

### System tray
- StartPE hosts `Shell_NotifyIcon` registrations itself (its own `Shell_TrayWnd`,
  `TaskbarCreated` broadcast on startup, repeated a few times over the first
  seconds to catch apps launched alongside it), draws the icons next to the clock, and
  forwards left/right clicks to the owning apps. Appbar traffic is proxied to
  Explorer's tray, and NIM traffic mirrored, so Explorer stays consistent.

### Settings
- Right-click the taskbar → **Settings**: a GTK4/Libadwaita settings app
  (`Settings.exe`) when present — grouped switches plus a Start button color
  picker, applied live via a registered message to the running shell — or the
  built-in dark, owner-drawn GDI pane with the same options.

### Networking
- A built-in **network status glyph** next to the clock — wireframe globe when
  nothing is connected, a wifi symbol on wireless, an ethernet symbol on wired
  (ethernet wins when both are up) — polled via documented `GetAdaptersAddresses`
  (`ShowNetworkIcon`, opt-out).
- Clicking it opens the GTK4/Libadwaita **wifi flyout** (`Network.exe`),
  Windows 11-style: available networks with signal strength, inline security-key
  entry on the network you pick, and a live status line while it connects
  (needs the WinPE WLAN stack — `wlansvc` + wifi drivers — in the image;
  degrades to status-only without it).
- **Network settings…** at the flyout's bottom (or right-clicking the glyph)
  opens the full **Network Settings** window: per-adapter DHCP/static IPv4 +
  DNS, and **export/import of the whole setup** — adapter config plus saved
  wireless networks (keys included) — as a `network-profile.ini` dropped next
  to `startpe.exe`, applied automatically at shell startup (bake one into the
  PE image the same way as `desktop-layout.txt`).

### Window switching & hotkeys
- **Accent border on the active window** (`WindowBorders`, opt-out) — a thin
  frame in the Start-button accent color around the foreground window. With DWM
  it recolors the real window border (`DWMWA_BORDER_COLOR`); in plain WinPE a
  click-through GDI overlay follows the window via `SetWinEventHook`.
- Windows 11-style **Alt+Tab** switcher: a centered overlay grid of
  `PrintWindow` screenshots (no DWM dependency); minimized windows show their
  app icon.
- The **Win key** opens the StartPE menu; **Win+R** (Run), **Win+E** (file
  manager), **Win+D** (show desktop), **Win+X** (power-user menu), and
  **Win+Pause** (System Information) are handled directly (other Win combos
  pass through).
- A Windows 11-style **power-user menu** (Win+X, or right-click the start
  button): Event Viewer, System, Device Manager, Disk Management, Computer
  Management, Terminal, Task Manager, File Explorer, Run, a Shut down / Restart
  flyout, and Desktop — the PE-relevant subset of the Windows 11 menu, drawn as
  a rounded dark custom popup with hover/keyboard navigation and access keys.
- **Terminal everywhere honors `TerminalApp`**: the Win+X Terminal entry and
  both start menus' Terminal link launch the configured terminal (falling back
  to `%ComSpec%`, then `cmd.exe`) — set the registry value from your terminal's
  PE component and every surface opens it.

### Dark theming
- Dark, rounded, custom-drawn popup menus (taskbar context menu, power flyout,
  Win+X menu) — rounded corners without DWM, correct separator behavior.
- Dark-mode for the shell-rendered menus StartPE raises (chiefly the hosted
  desktop's right-click menu) via uxtheme app mode (`DarkMenus`, opt-out).
- A **dark Run box** — the GTK `RunBox.exe` when present, else a fully
  owner-drawn GDI replacement for the shell Run box that's actually dark in PE,
  both with history recall and Browse.
- A **dark System Information window** — the GTK `SystemInfo.exe` when present,
  else the built-in hardware-first GDI replacement for msinfo32 / sysdm.cpl
  (System, CPU & memory, graphics & displays, storage & network; WMI with
  documented fallbacks). Opens from Win+X → System, **Win+Pause**, and
  **right-click This PC → Properties** (the PE image redirects the System
  Properties verb to `startpe.exe --sysinfo`).

### Desktop (when Explorer can't provide one)
On Win11 24H2/25H2 PE sources, Explorer's modern taskbar init fail-fasts and the
desktop (`Progman`/`SHELLDLL_DefView`) is never created. When StartPE detects
this it **provides the desktop itself** (`OwnDesktop`): a `Progman`-style window
painting the wallpaper (BMP/PNG/JPG via GDI+) and hosting a *real* shell icon
view of the Public Desktop — with working right-click menus, double-click,
Windows-style **ghost icon drag** (multi-select drags as a group, snapped to the
grid on drop), **keyboard shortcuts** (Delete, F2 rename, F5, Ctrl+C/X/V/A…),
and layout save/restore to `desktop-layout.txt`. On a normal box (or a PE where
Explorer's desktop appears) it detects that and stays out of the way.

Two small companions ship alongside:
- `startpe_loader.dll` — COM-registered so Explorer loads it early and its shell
  thread survives the Win11 taskbar init; the one component permitted to touch
  Explorer internals (`startpe.exe` itself stays documented Win32).
- `syslaunch.exe` — runs a program as SYSTEM on the interactive session's
  desktop, so a DWM-composited PE (Administrator auto-login) can still run
  StartPE with SYSTEM privileges (`LaunchAsSystem`).

## Configuration

Read once at startup from `HKLM\Software\StartPE`, then overlaid by
`HKCU\Software\StartPE`. PE runs the shell as `SYSTEM`, so a PEBakery build
writes config machine-wide into the offline SOFTWARE hive; the settings app
writes runtime changes to `HKCU`. See the value table in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Building

```
cargo build --release --workspace   # x64: startpe.exe + startpe_loader.dll + syslaunch.exe
```

The GTK helpers under `helpers/` (`StartMenu.exe`, `RunBox.exe`, `Settings.exe`,
`SystemInfo.exe`, `Network.exe`) are excluded from the MSVC workspace; release binaries are
built by CI with the MSYS2 ucrt64 toolchain and attached to the same GitHub
release. `startpe.exe` auto-detects them as siblings at runtime — no helper, no
GTK dependency.

## Testing on a full Windows machine

StartPE hides the real Explorer taskbar and grabs the Win key while it runs. It
restores both on clean exit, but a force-kill skips that (recover by restarting
`explorer.exe`). A Windows 11 VM or a PE VM is strongly recommended — behavior
differs between full Windows (DWM, UWP windows) and PE (neither).

## PE integration

`pebakery/StartPE.script` copies the binaries into the image and writes the
launch and configuration registry values into the mounted SOFTWARE hive at build
time. It's modeled on PhoenixPE scripts but can be modified to work in any
PEBakery build.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
