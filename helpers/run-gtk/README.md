# pe-run-gtk

A **Run** window for Windows PE, built with **GTK4 + Libadwaita** (Rust /
gtk4-rs). The libadwaita counterpart to StartPE's Win32/GDI Run box
(`startpe/src/run_window.rs`).

The command/history core (`src/run_exec.rs`) is reused from StartPE: expand env
vars, split program from args, and `ShellExecute` (resolving bare names via
PATH/App Paths like the classic Run box). History is shared with StartPE through
`HKCU\Software\StartPE\RunHistory`. Only the presentation is rebuilt in gtk4-rs —
a small libadwaita dialog with a prompt, an "Open:" entry with inline history
autocomplete (no dropdown), and Browse / Cancel / OK. It opens bottom-left above
the taskbar, Escape closes it, and a named mutex enforces single instance.

This is the second GTK4 *shell helper* (after `sysinfo-gtk`): StartPE keeps its
lean Win32 core and auto-detects this `RunBox.exe` as a sibling, falling back to
its built-in Run box if it (or the GTK runtime) is absent.

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
cargo build --release      # -> target/release/RunBox.exe
```
