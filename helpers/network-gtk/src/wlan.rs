// SPDX-License-Identifier: GPL-3.0-or-later
//! Native WiFi (`wlanapi`) wrapper: scan, list, connect, and profile
//! import/export. Documented Win32 only. All calls degrade gracefully when the
//! WLAN service (`wlansvc`) or hardware is absent — `Wlan::open` just fails and
//! the flyout shows an explanatory row instead of a network list.
//!
//! Connect progress is polled (`WlanQueryInterface` current-connection) rather
//! than using `WlanRegisterNotification`: the poll runs on a glib timeout and
//! avoids marshalling callbacks off wlansvc's thread, and a 0.5 s cadence is
//! indistinguishable from notifications in the flyout's one status line.

use windows::core::{GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::WiFi::{
    dot11_BSS_type_infrastructure, wlan_connection_mode_profile, wlan_interface_state_connected,
    WlanCloseHandle, WlanConnect, WlanEnumInterfaces, WlanFreeMemory,
    WlanGetAvailableNetworkList, WlanGetProfile, WlanGetProfileList, WlanOpenHandle,
    WlanQueryInterface, WlanScan, WlanSetProfile, WLAN_AVAILABLE_NETWORK,
    WLAN_CONNECTION_ATTRIBUTES, WLAN_CONNECTION_PARAMETERS, WLAN_INTERFACE_INFO_LIST,
    WLAN_PROFILE_INFO_LIST,
};
use windows::Win32::NetworkManagement::WiFi::{
    wlan_interface_state_disconnected, wlan_interface_state_not_ready,
    wlan_intf_opcode_current_connection, wlan_intf_opcode_interface_state, DOT11_AUTH_ALGORITHM,
    DOT11_AUTH_ALGO_80211_OPEN, DOT11_AUTH_ALGO_RSNA_PSK, DOT11_AUTH_ALGO_WPA_PSK,
    DOT11_AUTH_ALGO_WPA3_SAE, WLAN_INTERFACE_STATE,
};

const ERROR_SUCCESS: u32 = 0;
/// `WLAN_AVAILABLE_NETWORK.dwFlags`: currently connected / has a saved profile.
const NET_CONNECTED: u32 = 0x1;
const NET_HAS_PROFILE: u32 = 0x2;
/// `WlanGetProfile` flag: return the key material in plain text (requires an
/// elevated caller — we run as SYSTEM in PE).
const GET_PLAINTEXT_KEY: u32 = 0x4;

/// One entry of the flyout list, deduped by SSID.
#[derive(Clone)]
pub struct Network {
    pub ssid: String,
    /// 0–100.
    pub signal: u32,
    pub secured: bool,
    pub connected: bool,
    /// A saved profile exists, so connecting needs no password prompt.
    pub has_profile: bool,
    pub auth: DOT11_AUTH_ALGORITHM,
}

/// An open wlanapi session on the first WLAN interface.
pub struct Wlan {
    handle: HANDLE,
    pub iface: GUID,
}

unsafe impl Send for Wlan {}

impl Drop for Wlan {
    fn drop(&mut self) {
        unsafe {
            let _ = WlanCloseHandle(self.handle, None);
        }
    }
}

impl Wlan {
    /// Open a session and grab the first WLAN interface. `None` when wlansvc
    /// isn't running or there is no wifi hardware.
    pub fn open() -> Option<Wlan> {
        unsafe {
            let mut negotiated = 0u32;
            let mut handle = HANDLE::default();
            if WlanOpenHandle(2, None, &mut negotiated, &mut handle) != ERROR_SUCCESS {
                return None;
            }
            let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
            if WlanEnumInterfaces(handle, None, &mut list) != ERROR_SUCCESS || list.is_null() {
                let _ = WlanCloseHandle(handle, None);
                return None;
            }
            let iface = if (*list).dwNumberOfItems > 0 {
                Some((*list).InterfaceInfo[0].InterfaceGuid)
            } else {
                None
            };
            WlanFreeMemory(list as *const _);
            match iface {
                Some(iface) => Some(Wlan { handle, iface }),
                None => {
                    let _ = WlanCloseHandle(handle, None);
                    None
                }
            }
        }
    }

    /// Kick off an async scan (results land in the available-network list a
    /// couple of seconds later; we just re-list on a timer).
    pub fn scan(&self) {
        unsafe {
            let _ = WlanScan(self.handle, &self.iface, None, None, None);
        }
    }

    /// Snapshot of visible networks, deduped by SSID (strongest wins, connected
    /// beats everything), sorted: connected first, then by signal.
    pub fn networks(&self) -> Vec<Network> {
        unsafe {
            let mut list: *mut windows::Win32::NetworkManagement::WiFi::WLAN_AVAILABLE_NETWORK_LIST =
                std::ptr::null_mut();
            if WlanGetAvailableNetworkList(self.handle, &self.iface, 0, None, &mut list)
                != ERROR_SUCCESS
                || list.is_null()
            {
                return Vec::new();
            }
            let n = (*list).dwNumberOfItems as usize;
            let first = (*list).Network.as_ptr();
            let mut nets: Vec<Network> = Vec::new();
            for i in 0..n {
                let e: &WLAN_AVAILABLE_NETWORK = &*first.add(i);
                let ssid = ssid_to_string(&e.dot11Ssid.ucSSID, e.dot11Ssid.uSSIDLength as usize);
                if ssid.is_empty() {
                    continue; // hidden networks: skip, like the Win11 flyout's default list
                }
                let net = Network {
                    ssid,
                    signal: e.wlanSignalQuality,
                    secured: e.bSecurityEnabled.as_bool(),
                    connected: e.dwFlags & NET_CONNECTED != 0,
                    has_profile: e.dwFlags & NET_HAS_PROFILE != 0,
                    auth: e.dot11DefaultAuthAlgorithm,
                };
                match nets.iter_mut().find(|x| x.ssid == net.ssid) {
                    Some(x) => {
                        x.connected |= net.connected;
                        x.has_profile |= net.has_profile;
                        if net.signal > x.signal {
                            x.signal = net.signal;
                        }
                    }
                    None => nets.push(net),
                }
            }
            WlanFreeMemory(list as *const _);
            nets.sort_by(|a, b| {
                b.connected
                    .cmp(&a.connected)
                    .then(b.signal.cmp(&a.signal))
            });
            nets
        }
    }

    /// SSID of the current connection, if associated.
    pub fn current_ssid(&self) -> Option<String> {
        unsafe {
            let mut size = 0u32;
            let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
            if WlanQueryInterface(
                self.handle,
                &self.iface,
                wlan_intf_opcode_current_connection,
                None,
                &mut size,
                &mut data,
                None,
            ) != ERROR_SUCCESS
                || data.is_null()
            {
                return None;
            }
            let attrs = &*(data as *const WLAN_CONNECTION_ATTRIBUTES);
            let ssid = if attrs.isState == wlan_interface_state_connected {
                let s = &attrs.wlanAssociationAttributes.dot11Ssid;
                Some(ssid_to_string(&s.ucSSID, s.uSSIDLength as usize))
            } else {
                None
            };
            WlanFreeMemory(data);
            ssid.filter(|s| !s.is_empty())
        }
    }

    /// Save (or overwrite) a profile for `net` and start connecting. For a
    /// secured network without a saved profile, `key` is the passphrase; open
    /// networks and saved profiles pass `None`. Returns an error string on
    /// immediate failure; the caller then polls [`Wlan::current_ssid`].
    pub fn connect(&self, net: &Network, key: Option<&str>) -> Result<(), String> {
        unsafe {
            if !net.has_profile || key.is_some() {
                let xml = profile_xml(&net.ssid, net.auth, net.secured, key);
                let xml_w = wide(&xml);
                let mut reason = 0u32;
                let err = WlanSetProfile(
                    self.handle,
                    &self.iface,
                    0,
                    PCWSTR(xml_w.as_ptr()),
                    PCWSTR::null(),
                    true,
                    None,
                    &mut reason,
                );
                if err != ERROR_SUCCESS {
                    return Err(format!("Couldn't save the network profile ({err}/{reason})"));
                }
            }
            self.connect_profile(&net.ssid)
        }
    }

    /// Start (or re-issue) a connection to an already-saved profile by SSID —
    /// no profile write. Used both by [`Wlan::connect`] after saving, and by the
    /// caller's poll to auto-retry: the first association to a never-seen AP
    /// frequently fails on stale scan data, and re-issuing (a manual retry, done
    /// automatically) is what actually gets it to associate.
    pub fn connect_profile(&self, ssid: &str) -> Result<(), String> {
        unsafe {
            let name_w = wide(ssid);
            let params = WLAN_CONNECTION_PARAMETERS {
                wlanConnectionMode: wlan_connection_mode_profile,
                strProfile: PCWSTR(name_w.as_ptr()),
                pDot11Ssid: std::ptr::null_mut(),
                pDesiredBssidList: std::ptr::null_mut(),
                dot11BssType: dot11_BSS_type_infrastructure,
                dwFlags: 0,
            };
            let err = WlanConnect(self.handle, &self.iface, &params, None);
            if err != ERROR_SUCCESS {
                return Err(format!("Couldn't start the connection ({err})"));
            }
            Ok(())
        }
    }

    /// True when the interface has settled into a non-connecting state
    /// (disconnected / not-ready) — i.e. a connect attempt has finished
    /// failing, as opposed to still associating/authenticating. The poll uses
    /// this to decide when a retry is warranted without interrupting an
    /// in-progress handshake.
    pub fn is_idle(&self) -> bool {
        unsafe {
            let mut size = 0u32;
            let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
            if WlanQueryInterface(
                self.handle,
                &self.iface,
                wlan_intf_opcode_interface_state,
                None,
                &mut size,
                &mut data,
                None,
            ) != ERROR_SUCCESS
                || data.is_null()
            {
                return false;
            }
            let state = *(data as *const WLAN_INTERFACE_STATE);
            WlanFreeMemory(data);
            state == wlan_interface_state_disconnected || state == wlan_interface_state_not_ready
        }
    }

    /// All saved profiles as `(name, xml)` — XML includes the plaintext key
    /// (SYSTEM caller), ready to re-import with [`Wlan::import_profile`].
    pub fn export_profiles(&self) -> Vec<(String, String)> {
        unsafe {
            let mut list: *mut WLAN_PROFILE_INFO_LIST = std::ptr::null_mut();
            if WlanGetProfileList(self.handle, &self.iface, None, &mut list) != ERROR_SUCCESS
                || list.is_null()
            {
                return Vec::new();
            }
            let n = (*list).dwNumberOfItems as usize;
            let first = (*list).ProfileInfo.as_ptr();
            let mut out = Vec::new();
            for i in 0..n {
                let info = &*first.add(i);
                let name: Vec<u16> = info
                    .strProfileName
                    .iter()
                    .copied()
                    .take_while(|&c| c != 0)
                    .collect();
                let mut xml = PWSTR::null();
                let mut flags = GET_PLAINTEXT_KEY;
                if WlanGetProfile(
                    self.handle,
                    &self.iface,
                    PCWSTR(name.as_ptr()),
                    None,
                    &mut xml,
                    Some(&mut flags),
                    None,
                ) == ERROR_SUCCESS
                    && !xml.is_null()
                {
                    out.push((
                        String::from_utf16_lossy(&name),
                        xml.to_string().unwrap_or_default(),
                    ));
                    WlanFreeMemory(xml.as_ptr() as *const _);
                }
            }
            WlanFreeMemory(list as *const _);
            out
        }
    }

    /// Import a profile XML (connectionMode=auto profiles then auto-connect).
    pub fn import_profile(&self, xml: &str) -> bool {
        unsafe {
            let xml_w = wide(xml);
            let mut reason = 0u32;
            WlanSetProfile(
                self.handle,
                &self.iface,
                0,
                PCWSTR(xml_w.as_ptr()),
                PCWSTR::null(),
                true,
                None,
                &mut reason,
            ) == ERROR_SUCCESS
        }
    }
}

fn ssid_to_string(bytes: &[u8], len: usize) -> String {
    String::from_utf8_lossy(&bytes[..len.min(bytes.len())]).into_owned()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Build a WLAN profile XML for `ssid`. Auth/cipher follow the network's
/// advertised default algorithm; unknown-but-secured falls back to WPA2PSK/AES
/// (what virtually every 2026-era AP negotiates).
fn profile_xml(
    ssid: &str,
    auth: DOT11_AUTH_ALGORITHM,
    secured: bool,
    key: Option<&str>,
) -> String {
    let ssid_x = xml_escape(ssid);
    let (auth_s, cipher) = if !secured || auth == DOT11_AUTH_ALGO_80211_OPEN {
        ("open", "none")
    } else if auth == DOT11_AUTH_ALGO_WPA_PSK {
        ("WPAPSK", "TKIP")
    } else if auth == DOT11_AUTH_ALGO_WPA3_SAE {
        ("WPA3SAE", "AES")
    } else {
        // RSNA_PSK and anything else secured.
        let _ = DOT11_AUTH_ALGO_RSNA_PSK;
        ("WPA2PSK", "AES")
    };
    let shared_key = match key {
        Some(k) if secured => format!(
            "<sharedKey><keyType>passPhrase</keyType><protected>false</protected>\
             <keyMaterial>{}</keyMaterial></sharedKey>",
            xml_escape(k)
        ),
        _ => String::new(),
    };
    format!(
        "<?xml version=\"1.0\"?>\
         <WLANProfile xmlns=\"http://www.microsoft.com/networking/WLAN/profile/v1\">\
         <name>{ssid_x}</name>\
         <SSIDConfig><SSID><name>{ssid_x}</name></SSID></SSIDConfig>\
         <connectionType>ESS</connectionType>\
         <connectionMode>auto</connectionMode>\
         <MSM><security>\
         <authEncryption><authentication>{auth_s}</authentication>\
         <encryption>{cipher}</encryption><useOneX>false</useOneX></authEncryption>\
         {shared_key}\
         </security></MSM>\
         </WLANProfile>"
    )
}
