// SPDX-License-Identifier: GPL-3.0-or-later
//! Adapter enumeration (`GetAdaptersAddresses`) and IPv4 configuration.
//!
//! Reading uses the documented iphlpapi snapshot; writing shells out to
//! `netsh interface ipv4` (hidden window) — the same route PENetwork takes,
//! and the only sane one: the DHCP-enable flag has no clean Win32 setter.

use std::os::windows::process::CommandExt;

use windows::Win32::NetworkManagement::IpHelper::{
    GetAdaptersAddresses, GAA_FLAG_INCLUDE_GATEWAYS, IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::Networking::WinSock::{AF_INET, AF_UNSPEC, SOCKADDR_IN};

/// IANA ifType values.
const IF_TYPE_ETHERNET_CSMACD: u32 = 6;
const IF_TYPE_IEEE80211: u32 = 71;
/// `IP_ADAPTER_ADDRESSES.Flags`: IPv4 DHCP enabled.
const FLAG_DHCP_V4: u32 = 0x4;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Ethernet,
    Wifi,
}

/// One physical adapter's current IPv4 state.
#[derive(Clone)]
pub struct Adapter {
    /// Connection name netsh addresses (e.g. "Ethernet", "Wi-Fi").
    pub name: String,
    /// Hardware description (e.g. "Intel(R) I211 Gigabit Network Connection").
    pub desc: String,
    pub kind: Kind,
    pub up: bool,
    pub dhcp: bool,
    pub ip: String,
    pub mask: String,
    pub gateway: String,
    pub dns: Vec<String>,
}

/// Snapshot all physical (ethernet/wifi) adapters.
pub fn adapters() -> Vec<Adapter> {
    unsafe {
        let flags = GAA_FLAG_INCLUDE_GATEWAYS;
        let mut size = 0u32;
        let mut buf: Vec<u8> = Vec::new();
        for _ in 0..3 {
            let ptr = if buf.is_empty() {
                None
            } else {
                Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH)
            };
            let err = GetAdaptersAddresses(AF_UNSPEC.0 as u32, flags, None, ptr, &mut size);
            const ERROR_BUFFER_OVERFLOW: u32 = 111;
            match err {
                0 if !buf.is_empty() => break,
                0 | ERROR_BUFFER_OVERFLOW => {
                    buf.resize(size as usize, 0);
                    if buf.is_empty() {
                        return Vec::new();
                    }
                }
                _ => return Vec::new(),
            }
        }
        if buf.is_empty() {
            return Vec::new();
        }

        let mut out = Vec::new();
        let mut cur = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !cur.is_null() {
            let a = &*cur;
            let kind = match a.IfType {
                IF_TYPE_ETHERNET_CSMACD => Some(Kind::Ethernet),
                IF_TYPE_IEEE80211 => Some(Kind::Wifi),
                _ => None,
            };
            if let Some(kind) = kind {
                let (ip, mask) = first_unicast_v4(a);
                out.push(Adapter {
                    name: a.FriendlyName.to_string().unwrap_or_default(),
                    desc: a.Description.to_string().unwrap_or_default(),
                    kind,
                    up: a.OperStatus == IfOperStatusUp,
                    dhcp: a.Anonymous2.Flags & FLAG_DHCP_V4 != 0,
                    ip,
                    mask,
                    gateway: first_gateway_v4(a),
                    dns: dns_v4(a),
                });
            }
            cur = a.Next;
        }
        out
    }
}

unsafe fn sockaddr_v4(sa: *const windows::Win32::Networking::WinSock::SOCKADDR) -> Option<String> {
    if sa.is_null() || (*sa).sa_family != AF_INET {
        return None;
    }
    let sin = &*(sa as *const SOCKADDR_IN);
    let b = sin.sin_addr.S_un.S_addr.to_ne_bytes();
    Some(format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3]))
}

unsafe fn first_unicast_v4(a: &IP_ADAPTER_ADDRESSES_LH) -> (String, String) {
    let mut u = a.FirstUnicastAddress;
    while !u.is_null() {
        if let Some(ip) = sockaddr_v4((*u).Address.lpSockaddr) {
            let prefix = (*u).OnLinkPrefixLength;
            return (ip, prefix_to_mask(prefix));
        }
        u = (*u).Next;
    }
    (String::new(), String::new())
}

unsafe fn first_gateway_v4(a: &IP_ADAPTER_ADDRESSES_LH) -> String {
    let mut g = a.FirstGatewayAddress;
    while !g.is_null() {
        if let Some(ip) = sockaddr_v4((*g).Address.lpSockaddr) {
            return ip;
        }
        g = (*g).Next;
    }
    String::new()
}

unsafe fn dns_v4(a: &IP_ADAPTER_ADDRESSES_LH) -> Vec<String> {
    let mut out = Vec::new();
    let mut d = a.FirstDnsServerAddress;
    while !d.is_null() {
        if let Some(ip) = sockaddr_v4((*d).Address.lpSockaddr) {
            out.push(ip);
        }
        d = (*d).Next;
    }
    out
}

fn prefix_to_mask(prefix: u8) -> String {
    let m: u32 = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix.min(32) as u32)
    };
    let b = m.to_be_bytes();
    format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
}

/// The IPv4 settings to apply to one adapter.
pub struct ApplyV4 {
    pub dhcp: bool,
    pub ip: String,
    pub mask: String,
    pub gateway: String,
    pub dns: Vec<String>,
}

/// Apply `cfg` to the adapter named `name` via netsh (blocking — call off the
/// UI thread). Returns netsh's first failing output line, if any.
pub fn apply(name: &str, cfg: &ApplyV4) -> Result<(), String> {
    let addr_args: Vec<String> = if cfg.dhcp {
        vec![
            "interface".into(),
            "ipv4".into(),
            "set".into(),
            "address".into(),
            format!("name={name}"),
            "source=dhcp".into(),
        ]
    } else {
        let mut v = vec![
            "interface".into(),
            "ipv4".into(),
            "set".into(),
            "address".into(),
            format!("name={name}"),
            "source=static".into(),
            format!("address={}", cfg.ip),
            format!("mask={}", cfg.mask),
        ];
        if !cfg.gateway.is_empty() {
            v.push(format!("gateway={}", cfg.gateway));
        } else {
            v.push("gateway=none".into());
        }
        v
    };
    netsh(&addr_args)?;

    if cfg.dhcp {
        netsh(&[
            "interface".into(),
            "ipv4".into(),
            "set".into(),
            "dnsservers".into(),
            format!("name={name}"),
            "source=dhcp".into(),
        ])?;
    } else if !cfg.dns.is_empty() {
        netsh(&[
            "interface".into(),
            "ipv4".into(),
            "set".into(),
            "dnsservers".into(),
            format!("name={name}"),
            "source=static".into(),
            format!("address={}", cfg.dns[0]),
            "register=none".into(),
            "validate=no".into(),
        ])?;
        for (i, d) in cfg.dns.iter().enumerate().skip(1) {
            netsh(&[
                "interface".into(),
                "ipv4".into(),
                "add".into(),
                "dnsservers".into(),
                format!("name={name}"),
                format!("address={d}"),
                format!("index={}", i + 1),
                "validate=no".into(),
            ])?;
        }
    }
    Ok(())
}

fn netsh(args: &[String]) -> Result<(), String> {
    let out = std::process::Command::new("netsh")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("netsh failed to launch: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let text = String::from_utf8_lossy(&out.stdout);
        Err(text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("netsh reported an error")
            .to_string())
    }
}
