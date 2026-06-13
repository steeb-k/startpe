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
   instead. Win+<key> shortcuts are handled by StartPE itself (there is no
   working shell on these PE images to handle them): Win+R (Run), Win+E (file
   explorer), Win+D (show desktop). Other Win+<key> combos pass through.

Note on hiding Explorer's taskbar: hiding the `Shell_TrayWnd` window is not
enough, because its appbar *work-area reservation* survives and leaves a dead
black strip (and pushes our appbar up). StartPE therefore first flips
Explorer's taskbar to auto-hide via `ABM_SETSTATE` (releasing the
reservation), then hides the window, and always docks itself to the true
bottom edge. Both are undone on clean exit.

Everything is documented Win32. Nothing depends on a specific Windows build.

At startup StartPE waits (up to 60 s) for Explorer's `Progman` desktop and
`SHELLDLL_DefView` before hiding Explorer's taskbar, and only targets
`Shell_TrayWnd` windows owned by `explorer.exe` so our own tray host is never
 mistaken for Explorer's.

## Process model

Single process, single UI thread, two top-level windows:

| Window class      | Role                                            |
| ----------------- | ----------------------------------------------- |
| `StartPE_Taskbar` | appbar; start button, task buttons, clock       |
| `StartPE_Menu`    | popup start menu; hidden until toggled          |
| `StartPE_Desktop` | desktop window (wallpaper + hosted `SHELLDLL_DefView`); created only when Explorer's own desktop is absent |

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
- `src/peek.rs` — taskbar-button hover previews (DWM thumbnails where available,
  icon/title rows otherwise)
- `src/alttab.rs` — Windows 11–style Alt+Tab switcher. A `WH_KEYBOARD_LL` hook
  captures Alt+Tab before the system switcher fires and drives a centered,
  rounded overlay: one tile per top-level window (app icon + title + a
  `PrintWindow` screenshot), flowing left-to-right and wrapping into a grid once
  a row would pass ~85% of the screen width. No DWM dependency (static
  screenshots, not live thumbnails); releasing Alt / Enter / a click activates
  the selection, Esc cancels
- `src/desktop.rs` — StartPE-owned desktop (wallpaper + hosted Public Desktop
  icon view with its own icon-layout save/restore), created only when Explorer's
  own desktop never appears
- `src/pins.rs` — reads the winrx-creator/PhoenixPE `PinUtil.ini` staging file
  (`%Windir%\System32\PinUtil.ini`, `[PinUtil]` `Taskbar<n>`/`StartMenu<n>` =
  exe path) so StartPE can render pinned taskbar/start-menu items
- `src/config.rs` — registry-backed configuration (`HKCU\Software\StartPE`)
- `src/util.rs` — UTF-16 helpers, LOWORD/HIWORD
- `loader/src/lib.rs` — `startpe_loader.dll`, the Explorer-side shim (see below)

## Configuration contract for PEBakery

Configuration is read once at startup from `HKLM\Software\StartPE`, then
overlaid by `HKCU\Software\StartPE` (so a per-user install can override). In a
PE image the build script writes the values into the **SOFTWARE** hive
(`HKLM\Software\StartPE`) offline — **not** the Default-user hive: PE runs the
shell as `SYSTEM`, whose `HKCU` is the SYSTEM profile and never the offline
Default-user hive, so `HKCU\Software\StartPE` would be empty at runtime. (This is
the same reason the Default-user `Run` key isn't honored under SYSTEM.) Writing
machine-wide makes the menu fully configured on first boot with no per-boot step.

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
| `OwnDesktop`     | 0       | StartPE provides the desktop itself: 0 = auto (only if Explorer's desktop never appears), 1 = always, 2 = never |
| `Wallpaper`      | —       | REG_SZ path to a wallpaper image (BMP/PNG/JPG/GIF, loaded via GDI+) used when StartPE owns the desktop (falls back to `Control Panel\Desktop\WallPaper`, then a solid fill) |
| `DesktopColor`   | 3158560 | solid desktop background COLORREF (0x00BBGGRR) when no wallpaper bitmap is available (default 0x00302820) |
| `ShowSystemDesktopIcons` | 0 | 1 = show the built-in desktop namespace icons (This PC, Home, Network, Control Panel, Recycle Bin); 0 = hide them so only real shortcuts show |
| `StartButtonColor` | 15790320 | Start button glyph color COLORREF (0x00BBGGRR); default 0x00F0F0F0 (near-white) |

Launch: the PEBakery script writes the Run key for classic logon flows and
calls `AddAutoRun,PostShell` so winrx-creator/PhoenixPE images start StartPE
from `WinRxCreator.au3` after Explorer is up (the Default-user Run key is not
read when Explorer runs as SYSTEM). A future option is HKLM shell/COM
registration so Explorer loads StartPE the way StartAllBack hooks in-process;
that is not implemented yet.

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
- **M2: taskbar parity.** Pinned items (done — taskbar pins from `PinUtil.ini`
  show even when not running and launch on click; see `src/pins.rs`), button
  grouping/combining modes,
  labels on/off, taskbar on any screen edge, multi-monitor, auto-hide,
  jump-list-style context menus (close/restore/minimize).
- **M3: start menu parity.** Pinned view (done — opens to the `PinUtil.ini`
  `StartMenu` pins with an All apps / Pinned toggle), search box (filter as you
  type over the indexed shortcut list), right-pane links (Computer, Control
  Panel, Downloads, Run), user picture, recent/frequent programs list, keyboard
  navigation.
- **M4: theming + StartIsBack config compatibility.** Win7/Win10/Win11 visual
  styles, orb bitmaps, transparency; read the `Software\StartIsBack` values
  the existing PEBakery scripts already write (`Start_ShowRun`,
  `TaskbarLocation`, icon sizes, …) and map them onto StartPE settings so
  existing build scripts work with minimal changes.

## The Win11 24H2/25H2 PE desktop problem (and why StartPE owns the desktop)

On Win11 24H2/25H2 PE sources (observed on `10.0.26100`/`26200`) Explorer's
modern (XAML) taskbar init fail-fasts during shell startup — a WIL `FAIL_FAST`
in `taskbar.cpp` (`taskbarInitTiPReason::sync_thread_created`) — and takes down
the shell thread *before* the desktop (`Progman`/`SHELLDLL_DefView`) is created,
so wallpaper and icons never appear. This is the documented Win11 XAML-package
failure: the modern taskbar depends on the `MicrosoftWindows.Client.CBS`,
`Microsoft.UI.Xaml.CBS`, and `MicrosoftWindows.Client.Core` packages being
registered before Explorer starts. PE images strip those packages entirely
(no `SystemApps`/`WindowsApps`), and PE has no working AppX registration stack
(the `Appx` PowerShell module won't even load), so they cannot be registered.

Two consequences fix the design:

1. **Patching Explorer can't produce a desktop.** On 24H2 the desktop is built
   only if the taskbar init *succeeds*; merely skipping the fail-fast (tried
   both as return-at-entry and NOP-the-branch in `loader/`) leaves Explorer with
   no valid taskbar object, so it tears itself down before creating `Progman` —
   or restart-loops. StartAllBack gets a desktop because it injects a complete
   Win32 *replacement* taskbar so Explorer's init genuinely completes; that is
   unbounded per-build maintenance we won't take on.
2. **So StartPE provides the desktop itself** (`src/desktop.rs`) when Explorer's
   never appears: it creates a `Progman`-style window at the bottom of the
   z-order, paints the wallpaper (BMP/PNG/JPG via GDI+), and hosts a *real* shell
   icon view (`SHELLDLL_DefView` — plain shell32, works in PE) of the **Public
   Desktop** folder (`%PUBLIC%\Desktop`), where PE builds place shortcuts — so
   only real shortcuts show, none of the desktop namespace junctions (This PC,
   Home, Network, Control Panel, Recycle Bin). `ShowSystemDesktopIcons=1` hosts
   the full namespace desktop (with junctions) instead. The icon list is set to
   auto-arrange off + snap-to-grid, and StartPE saves/restores icon positions to
   `desktop-layout.txt` next to the exe (the per-session shell bag is wiped each
   PE boot, so StartPE persists the layout itself: bake a `desktop-layout.txt`
   to define positions; it is rewritten as icons move so it can be re-captured).
   The hosted view's right-click menu and double-click-opens-a-folder behave as
   users expect. Explorer is still launched on demand as the file manager; it
   just no longer has to be the shell. On a normal box (or a PE where Explorer's
   desktop does come up) StartPE detects it and stays out of the way. Behavior is
   `OwnDesktop` (0 auto / 1 always / 2 never).

### Explorer loader shim (`loader/`)

The build deliberately does not ship the modern taskbar's AppX/WinRT packages,
which is why a third-party taskbar was always required (StartAllBack solved this
by injecting a DLL that replaced the taskbar init in-process).

`loader/` builds `startpe_loader.dll`, a small companion that PEBakery
registers as a `Drive\shellex\FolderExtensions` COM handler (CLSID
`{6F3D9B2A-…}`). shell32 CoCreates it early in Explorer startup, pulling the
DLL into `explorer.exe`. The loader:

1. launches `startpe.exe` (our taskbar/start menu) from its own directory
   (exe name derived from the DLL name, so the arch pair stays matched), and
2. records the shell crash's faulting module + stack to `X:\startpe_loader.log`
   (WinPE has no Event Viewer) so the targeted suppression hook can be written.

The active taskbar-suppression hook lives here once the crash signature is
known. This DLL is the **one** component permitted to touch Explorer internals;
everything in `startpe.exe` remains documented Win32.

## Why not …

- **Full StartAllBack-style replacement:** reimplementing Explorer's taskbar
  in-process is unbounded per-build maintenance. The loader instead only keeps
  Explorer's shell thread alive; `startpe.exe` still draws the taskbar.
- **Full shell replacement (`Winlogon\Shell = startpe.exe`):** we don't
  reimplement Explorer's *file manager* — Explorer is still launched on demand
  for folder windows, copy/paste, and context-menu handlers (all shell32, all
  working in PE). StartPE only supplies the *shell surface* Explorer can't bring
  up on a stripped 24H2/25H2 PE: taskbar, start menu, and (via `src/desktop.rs`)
  the desktop. The desktop icon view is the same `SHELLDLL_DefView` control
  Explorer hosts, so it is the real desktop, not a reimplementation.
