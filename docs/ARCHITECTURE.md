# StartPE Architecture

## Goals

A free, GPLv3 taskbar + start menu for Windows PE that PEBakery builders can
drop into an image in place of StartAllBack. Near feature parity with
StartAllBack for the features that matter in PE, without StartAllBack's
approach (DLL injection into Explorer and undocumented taskbar internals),
which is unmaintainable for an open-source project.

## Approach: own windows alongside Explorer-as-shell

StartAllBack restyles Explorer's taskbar in-process. StartPE instead:

1. Lets Explorer run as the shell exactly as the PE image already does —
   desktop, wallpaper, file windows, drag & drop all keep working.
2. Hides Explorer's `Shell_TrayWnd` (and `Shell_SecondaryTrayWnd`) at startup
   and re-hides from a watchdog timer in case Explorer restarts.
3. Creates its own appbar (`SHAppBarMessage`/`ABM_NEW`) docked to a screen
   edge, so maximized windows respect the taskbar area.
4. Tracks top-level windows with `RegisterShellHookWindow` + the registered
   `SHELLHOOK` message — documented and stable since Windows 2000 — plus a
   slow `EnumWindows` polling fallback.
5. Captures bare Win-key taps with a `WH_KEYBOARD_LL` hook: the key-up is
   swallowed and replaced with synthetic input (dummy key between Win-down
   and Win-up) so Explorer's start menu never triggers; our menu opens
   instead. Win+<key> combos pass through untouched.

Note on hiding Explorer's taskbar: hiding the `Shell_TrayWnd` window is not
enough, because its appbar *work-area reservation* survives and leaves a dead
black strip (and pushes our appbar up). StartPE therefore first flips
Explorer's taskbar to auto-hide via `ABM_SETSTATE` (releasing the
reservation), then hides the window, and always docks itself to the true
bottom edge. Both are undone on clean exit.

Everything is documented Win32. Nothing depends on a specific Windows build.

## Process model

Single process, single UI thread, two top-level windows:

| Window class      | Role                                            |
| ----------------- | ----------------------------------------------- |
| `StartPE_Taskbar` | appbar; start button, task buttons, clock       |
| `StartPE_Menu`    | popup start menu; hidden until toggled          |

Per-window state lives in `thread_local!` `RefCell`s. The rule for window
procedures: *resolve* an action while holding the borrow, *perform* it after
releasing the borrow — actions like `ShellExecuteW` and `SetForegroundWindow`
can pump messages and re-enter the wndproc.

Rendering is plain GDI into a double buffer. No UI framework; the binary is
~170 KB and runs on any PE image with no runtime dependencies.

## Module map

- `src/main.rs` — single-instance guard, DPI awareness, startup, message loop
- `src/taskbar.rs` — appbar, task button list, painting, shell hook, clock,
  Explorer-taskbar suppression
- `src/start_menu.rs` — start menu popup, Start Menu folder enumeration,
  folder navigation, footer actions (Run / Cmd / Reboot / Shutdown)
- `src/config.rs` — registry-backed configuration (`HKCU\Software\StartPE`)
- `src/util.rs` — UTF-16 helpers, LOWORD/HIWORD

## Configuration contract for PEBakery

All configuration is read once at startup from `HKCU\Software\StartPE`. In a
PE image the build script writes these values into the mounted **Default
user** hive (the same mechanism the StartAllBack script uses for
`Software\StartIsBack`), so the menu is fully configured on first boot with
no per-boot setup step.

Current values (all `REG_DWORD`):

| Value            | Default | Meaning                                            |
| ---------------- | ------- | --------------------------------------------------- |
| `TaskbarHeight`  | 40      | taskbar height in px at 96 DPI (24–120)              |
| `ButtonMaxWidth` | 220     | max task button width in px (labels mode)            |
| `MenuWidth`      | 340     | start menu width in px                                |
| `MenuHeight`     | 480     | start menu height in px                               |
| `TaskbarLabels`  | 0       | 1 = show window titles on buttons; 0 = icon-only      |
| `TaskbarCombine` | 1       | 1 = one button per app (click cycles its windows)     |
| `CenterTaskbar`  | 1       | 1 = center the start button + task button cluster     |
| `UserPicture`    | —       | REG_SZ path to a square .bmp for the start menu avatar |

## Roadmap to StartAllBack parity

- **M0 (done): skeleton.** Taskbar + start menu + clock as described above.
- **M1 (mostly done): system tray.** `src/tray.rs` creates our own
  `Shell_TrayWnd` (+ `TrayNotifyWnd` child), parses `Shell_NotifyIcon`
  WM_COPYDATA registrations (32-bit NOTIFYICONDATA wire layout), broadcasts
  `TaskbarCreated` so running apps re-register, draws icons left of the
  clock, and forwards clicks (v0 and NOTIFYICON_VERSION_4 packing). Unhandled
  copy-data (appbar protocol) is proxied to Explorer's tray; NIM traffic is
  mirrored there too so Explorer stays consistent if StartPE exits.
  Remaining: tooltips, balloon notifications, overflow area.
- **M2: taskbar parity.** Pinned items, button grouping/combining modes,
  labels on/off, taskbar on any screen edge, multi-monitor, auto-hide,
  jump-list-style context menus (close/restore/minimize).
- **M3: start menu parity.** Search box (filter as you type over the indexed
  shortcut list), right-pane links (Computer, Control Panel, Downloads, Run),
  user picture, recent/frequent programs list, keyboard navigation.
- **M4: theming + StartIsBack config compatibility.** Win7/Win10/Win11 visual
  styles, orb bitmaps, transparency; read the `Software\StartIsBack` values
  the existing PEBakery scripts already write (`Start_ShowRun`,
  `TaskbarLocation`, icon sizes, …) and map them onto StartPE settings so
  existing build scripts work with minimal changes.

## Why not …

- **Explorer injection (StartAllBack's way):** undocumented, breaks per
  Windows build, hostile to code review. Rejected by design.
- **Full shell replacement (`Winlogon\Shell = startpe.exe`):** viable later
  (and would make tray ownership trivial), but PE images built from PhoenixPE
  already rely on Explorer-as-shell behaviors; keeping Explorer maximizes
  drop-in compatibility.
