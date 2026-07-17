# pe-network-gtk

StartPE's **network manager** for Windows PE, built with **GTK4 + Libadwaita**
(Rust / gtk4-rs) — the shell's replacement for AutoIt-based PENetwork.

One resident process (`Network.exe`), two surfaces:

- **Wifi flyout** ("StartPE Network", undecorated, excluded from the taskbar):
  a Windows 11-style network list via the native WiFi API (`wlanapi`) — signal
  strength, inline security-key entry on the network you pick, and a live
  status line while it connects. An ethernet status row appears on top when
  wired. Needs the WinPE WLAN stack (`wlansvc` + drivers) in the image;
  without it the flyout explains and shows ethernet only.
- **Network Settings** (decorated, gets a taskbar button + native MDL2 network
  icon): per-adapter DHCP/static IPv4 + DNS (applied via `netsh`), and
  export/import of the whole setup — adapter config plus saved wireless
  networks including keys — as `network-profile.ini` next to `startpe.exe`.
  StartPE applies a dropped file automatically once per session at startup,
  mirroring the `desktop-layout.txt` convention.

Pre-warmed hidden at StartPE startup (with `--apply-profile` when a drop-file
exists) and driven by the taskbar's built-in network glyph via the registered
`StartPE_ToggleNetworkFlyout` message (WPARAM 0 = flyout, 1 = settings). A
named mutex enforces single instance; a second launch forwards its request to
the resident process.

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
cargo build --release      # -> target/release/Network.exe
```
