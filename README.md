# StartPE

A free, open-source (GPLv3) **taskbar and start menu for Windows PE** environments — a drop-in replacement for StartAllBack in PEBakery-based builds (PhoenixPE, winrx-creator, and friends), with no license nag and no dependence on undocumented Explorer internals.

## Status: early skeleton (milestone M0)

Working today:

- Bottom-docked taskbar registered as a real appbar (maximized windows respect it)
- Start button; icon-only task buttons by default, with same-app combining (click cycles the app's windows) — labels and per-window buttons available via `TaskbarLabels` / `TaskbarCombine`
- Hover peek: previews of every window in a group with per-window close buttons — live DWM thumbnails where composition is available, icon + title rows in plain PE
- Cloaked/phantom windows (suspended UWP hosts) are filtered from the taskbar; UWP windows group and get icons via their real app process
- **System tray**: StartPE hosts `Shell_NotifyIcon` registrations itself (own `Shell_TrayWnd` window, `TaskbarCreated` broadcast on startup), draws the icons next to the clock, and forwards left/right clicks to the owning apps; appbar traffic is proxied to Explorer's tray
- Win key opens the StartPE menu (low-level hook; Win+E/Win+R combos unaffected)
- Clock with date
- Two-pane start menu in the classic Win7/StartAllBack layout: rounded corners (window region — no DWM needed), floating gap above the taskbar, centered over the start button
- Left pane: apps from the classic Start Menu folders (`%ProgramData%` + `%APPDATA%`) with shell icons, folder drill-down, an All Programs row, and a **working search box** (type to filter, Enter launches the top hit)
- Right pane: circular user picture (configurable via `UserPicture` .bmp, set from the PEBakery script) protruding above the menu, plus links: user profile, Downloads, This PC, Control Panel, Command Prompt, Run…
- Shut down button with Restart/Shut down flyout (`wpeutil`)
- Hides Explorer's Win11 taskbar and keeps it hidden (Explorer stays alive as the shell); restores it on clean exit
- Configuration read from `HKCU\Software\StartPE` (see `src/config.rs`) so a PEBakery script can preconfigure it offline in the mounted Default hive

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and the roadmap to StartAllBack feature parity.

## Building

```
cargo build --release                                   # x64
cargo build --release --target aarch64-pc-windows-msvc  # ARM64
```

Produces a single self-contained `startpe.exe` (~170 KB, no runtime dependencies).

## Testing on a full Windows machine

StartPE hides the real Explorer taskbar while it runs. Exit it cleanly (end `startpe.exe` from Task Manager *after* noting that a force-kill skips the restore; if the taskbar stays hidden, restart `explorer.exe`). Testing inside a PE VM is recommended.

## PE integration

`pebakery/StartPE.script` is a PEBakery script template that copies the binary into the image and writes launch + configuration registry values into the mounted Default hive. It is modeled on the PhoenixPE StartAllBack script's structure so it can sit alongside your existing scripts.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
