# pe-sysinfo-gtk

A **System Information** window for Windows PE, built with **GTK4 + Libadwaita**
(Rust / gtk4-rs). It is the libadwaita counterpart to StartPE's Win32/GDI System
Information window (`startpe/src/sysinfo.rs`).

The data layer (`src/sysinfo_data.rs`) is reused almost verbatim from StartPE:
hardware/OS facts come from WMI (`ROOT\CIMV2`) with documented Win32/registry
fallbacks. Only the presentation is rebuilt in gtk4-rs — an `AdwNavigationSplitView`
with a section sidebar (System / CPU & Memory / Graphics & Displays / Storage &
Network) and `.property` rows inside `AdwPreferencesGroup`s. Hardware is gathered
on a worker thread, so the window opens immediately with a spinner and repaints
when the data arrives.

This is the first GTK4 *shell helper* pilot: StartPE keeps its lean Win32 core
(taskbar, tray, hooks) and delegates ordinary windows like this one to separate
libadwaita helper executables that share the look of the rest of the PE's apps.

## Runtime requirement

A thin client of the shared GTK4 + Libadwaita runtime shipped by winrx-creator's
**GTK4Runtime** component (the `steeb-k/winrx-gtk4-runtime` release). It links no
C libraries beyond that runtime. In WinPE the runtime sets `GSK_RENDERER=cairo`
(no GPU/DWM), which this app inherits.

## Building

Requires an **MSYS2 ucrt64** toolchain so the binary links the same GTK DLLs the
runtime ships:

```bash
# in the MSYS2 UCRT64 shell
pacman -S --needed mingw-w64-ucrt-x86_64-rust mingw-w64-ucrt-x86_64-pkgconf \
                   mingw-w64-ucrt-x86_64-gcc  mingw-w64-ucrt-x86_64-gtk4 \
                   mingw-w64-ucrt-x86_64-libadwaita
cargo build --release      # -> target/release/SystemInfo.exe
```

## Features

- System summary, CPU & memory, graphics & displays, storage & network
- Selectable values (copy with the mouse)
- Dark theme (Libadwaita `ForceDark`)
