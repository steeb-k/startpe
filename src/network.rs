// SPDX-License-Identifier: GPL-3.0-or-later
//! Network status for the taskbar glyph + glue to the GTK network helper.
//!
//! Status comes from polling the documented `GetAdaptersAddresses` (no
//! undocumented internals): an operational wired adapter wins over wifi,
//! neither → disconnected globe. The wifi picker flyout and the Network
//! Settings window live in the sibling GTK helper `Network.exe`
//! (`helpers/network-gtk`), pre-warmed at startup and driven via the
//! registered `StartPE_ToggleNetworkFlyout` message — same pattern as the
//! start-menu helper. There is no built-in GDI fallback for the flyout: the
//! glyph still shows status without the helper, it just isn't clickable.

use std::sync::atomic::{AtomicU32, Ordering};

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::NetworkManagement::IpHelper::{
    GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
    GAA_FLAG_SKIP_FRIENDLY_NAME, GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::Networking::WinSock::AF_UNSPEC;
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, FindWindowW, PostMessageW, RegisterWindowMessageW,
};
use windows::core::PCWSTR;

/// IANA ifType values (also what `IP_ADAPTER_ADDRESSES` reports). Declared
/// here to avoid pulling extra windows-rs features for two integers.
const IF_TYPE_ETHERNET_CSMACD: u32 = 6;
const IF_TYPE_IEEE80211: u32 = 71;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NetStatus {
    Disconnected,
    Wifi,
    Ethernet,
}

impl NetStatus {
    /// Segoe MDL2 Assets glyph for the taskbar icon: wireframe globe when
    /// nothing is connected, wifi, or ethernet (ethernet wins when both are up).
    pub fn glyph(self) -> char {
        match self {
            NetStatus::Disconnected => '\u{E774}', // Globe
            NetStatus::Wifi => '\u{E701}',         // Wifi
            NetStatus::Ethernet => '\u{E839}',     // Ethernet
        }
    }
}

/// One `GetAdaptersAddresses` pass over the physical adapters. An adapter
/// counts as connected when its operational status is up (for ethernet that
/// means media connected; a wifi NIC only reports up once associated).
pub fn poll() -> NetStatus {
    unsafe {
        let flags = GAA_FLAG_SKIP_ANYCAST
            | GAA_FLAG_SKIP_MULTICAST
            | GAA_FLAG_SKIP_DNS_SERVER
            | GAA_FLAG_SKIP_FRIENDLY_NAME;
        // Two-call pattern: size query, then fill. A retry loop guards the
        // (unlikely in PE) adapter-set change between the two calls.
        let mut size = 0u32;
        let mut buf: Vec<u8> = Vec::new();
        for _ in 0..3 {
            let ptr = if buf.is_empty() {
                None
            } else {
                Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH)
            };
            let err = GetAdaptersAddresses(AF_UNSPEC.0 as u32, flags, None, ptr, &mut size);
            const ERROR_SUCCESS: u32 = 0;
            const ERROR_BUFFER_OVERFLOW: u32 = 111;
            match err {
                ERROR_SUCCESS if !buf.is_empty() => break,
                ERROR_SUCCESS | ERROR_BUFFER_OVERFLOW => {
                    buf.resize(size as usize, 0);
                    if buf.is_empty() {
                        return NetStatus::Disconnected;
                    }
                }
                _ => return NetStatus::Disconnected,
            }
        }
        if buf.is_empty() {
            return NetStatus::Disconnected;
        }

        let mut status = NetStatus::Disconnected;
        let mut cur = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !cur.is_null() {
            let a = &*cur;
            if a.OperStatus == IfOperStatusUp {
                match a.IfType {
                    IF_TYPE_ETHERNET_CSMACD => return NetStatus::Ethernet,
                    IF_TYPE_IEEE80211 => status = NetStatus::Wifi,
                    _ => {}
                }
            }
            cur = a.Next;
        }
        status
    }
}

// ---------------------------------------------------------------------------
// GTK helper (Network.exe) glue

/// PID of the pre-warmed helper, 0 if none. Only used to grant foreground.
static HELPER_PID: AtomicU32 = AtomicU32::new(0);

fn helper_exe() -> Option<String> {
    crate::config::network_app().or_else(|| {
        std::env::current_exe()
            .ok()
            .map(|e| e.with_file_name("Network.exe"))
            .filter(|p| p.is_file())
            .map(|p| p.to_string_lossy().into_owned())
    })
}

/// True if a `network-profile.ini` sits next to `startpe.exe` (a PE build or
/// the user dropped one in — same convention as `desktop-layout.txt`).
fn profile_exists() -> bool {
    std::env::current_exe()
        .ok()
        .map(|e| e.with_file_name("network-profile.ini").is_file())
        .unwrap_or(false)
}

/// Pre-warm the helper hidden at startup. `--apply-profile` is passed only on
/// this launch (not on-demand relaunches), so a dropped profile is applied
/// exactly once per session even if the helper later crashes and is respawned.
pub fn launch_helper() {
    let Some(app) = helper_exe() else { return };
    let mut cmd = std::process::Command::new(app);
    if profile_exists() {
        cmd.arg("--apply-profile");
    }
    if let Ok(child) = cmd.spawn() {
        HELPER_PID.store(child.id(), Ordering::Relaxed);
    }
}

/// The helper's hidden IPC window, if it's running.
unsafe fn helper_ipc() -> Option<HWND> {
    let h = FindWindowW(w!("StartPE_NetworkIPC"), PCWSTR::null()).ok()?;
    (!h.is_invalid()).then_some(h)
}

/// WPARAM values of the `StartPE_ToggleNetworkFlyout` message.
const CMD_FLYOUT: usize = 0;
const CMD_SETTINGS: usize = 1;

fn post(cmd: usize) {
    unsafe {
        if let Some(ipc) = helper_ipc() {
            let pid = HELPER_PID.load(Ordering::Relaxed);
            if pid != 0 {
                let _ = AllowSetForegroundWindow(pid);
            }
            let msg = RegisterWindowMessageW(w!("StartPE_ToggleNetworkFlyout"));
            if msg != 0 {
                let _ = PostMessageW(ipc, msg, WPARAM(cmd), LPARAM(0));
                return;
            }
        }
        // Helper absent or crashed: relaunch it opening the requested surface
        // directly (it single-instances itself, so a stale race is harmless).
        if let Some(app) = helper_exe() {
            let arg = if cmd == CMD_SETTINGS { "--settings" } else { "--flyout" };
            if let Ok(child) = std::process::Command::new(app).arg(arg).spawn() {
                HELPER_PID.store(child.id(), Ordering::Relaxed);
            }
        }
    }
}

/// Left-click on the taskbar network glyph: toggle the wifi flyout.
pub fn toggle_flyout() {
    post(CMD_FLYOUT);
}

/// Right-click: open the full Network Settings window.
pub fn open_settings() {
    post(CMD_SETTINGS);
}
