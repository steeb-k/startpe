// SPDX-License-Identifier: GPL-3.0-or-later
//! The `network-profile.ini` drop-file: export the current network setup and
//! re-apply it later, PE-build-bakeable like `desktop-layout.txt`.
//!
//! The file sits next to the exe (`Network.exe` ships beside `startpe.exe`, so
//! "next to the exe" is the same directory for both). `startpe.exe` launches
//! us with `--apply-profile` once per session when the file exists.
//!
//! Format — hand-editable INI, StartPE's own schema:
//!
//! ```ini
//! [Wifi]
//! ; one saved wireless network per Profile.N line, value = the full WLAN
//! ; profile XML with the plaintext key, base64-wrapped so multi-line XML
//! ; survives INI (and the passphrase isn't shoulder-surfable in Notepad).
//! Profile.1 = PFdMQU5Qcm9maWxlIC4uLg==
//!
//! [Adapter.Ethernet]        ; matched by connection name
//! DHCP = 0
//! IP = 192.168.1.50
//! Mask = 255.255.255.0
//! Gateway = 192.168.1.1
//! DNS = 1.1.1.1,8.8.8.8
//! ```

use crate::{ipcfg, wlan};

pub fn path() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()
        .map(|e| e.with_file_name("network-profile.ini"))
}

// --- tiny base64 (standard alphabet, padded) — enough to round-trip XML ----

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let v = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[(v >> (18 - 6 * i)) as usize & 0x3F] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc = 0u32;
    let mut n = 0u32;
    for c in s.bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        let v = B64.iter().position(|&b| b == c)? as u32;
        acc = (acc << 6) | v;
        n += 1;
        if n == 4 {
            out.extend_from_slice(&acc.to_be_bytes()[1..]);
            acc = 0;
            n = 0;
        }
    }
    match n {
        0 => {}
        2 => out.push((acc >> 4) as u8),
        3 => {
            out.push((acc >> 10) as u8);
            out.push((acc >> 2) as u8);
        }
        _ => return None,
    }
    Some(out)
}

// --- export ----------------------------------------------------------------

/// Write the current setup (saved wifi profiles + per-adapter IPv4 config) to
/// the drop-file. Returns the path written, or an error message.
pub fn export() -> Result<std::path::PathBuf, String> {
    let path = path().ok_or("can't resolve the exe directory")?;
    let mut ini = String::from(
        "; StartPE network profile — drop this file next to startpe.exe and the\n\
         ; saved setup below is applied at shell startup. Wifi profiles are the\n\
         ; standard WLAN XML (with key), base64-wrapped.\n\n[Wifi]\n",
    );
    if let Some(w) = wlan::Wlan::open() {
        for (i, (_name, xml)) in w.export_profiles().iter().enumerate() {
            ini.push_str(&format!("Profile.{}={}\n", i + 1, b64_encode(xml.as_bytes())));
        }
    }
    for a in ipcfg::adapters() {
        ini.push_str(&format!(
            "\n[Adapter.{}]\nDHCP={}\n",
            a.name,
            if a.dhcp { 1 } else { 0 }
        ));
        if !a.dhcp {
            ini.push_str(&format!(
                "IP={}\nMask={}\nGateway={}\nDNS={}\n",
                a.ip,
                a.mask,
                a.gateway,
                a.dns.join(",")
            ));
        }
    }
    std::fs::write(&path, ini).map_err(|e| e.to_string())?;
    Ok(path)
}

// --- import ----------------------------------------------------------------

/// Apply the drop-file if present: import every wifi profile (auto-connect
/// mode, so wlansvc associates on its own), then apply per-adapter IPv4
/// settings for adapters that exist by the same connection name. Returns a
/// short human summary, or `None` when no file exists.
pub fn apply() -> Option<String> {
    let path = path()?;
    let text = std::fs::read_to_string(&path).ok()?;

    let mut wifi_ok = 0usize;
    let mut adapters_ok = 0usize;
    let mut errors: Vec<String> = Vec::new();

    let mut section = String::new();
    let mut adapter: Option<(String, ipcfg::ApplyV4)> = None;
    let wl = wlan::Wlan::open();

    fn flush_adapter(
        adapter: &mut Option<(String, ipcfg::ApplyV4)>,
        ok: &mut usize,
        errors: &mut Vec<String>,
    ) {
        if let Some((name, cfg)) = adapter.take() {
            match ipcfg::apply(&name, &cfg) {
                Ok(()) => *ok += 1,
                Err(e) => errors.push(format!("{name}: {e}")),
            }
        }
    }

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            flush_adapter(&mut adapter, &mut adapters_ok, &mut errors);
            section = name.trim().to_string();
            if let Some(a) = section.strip_prefix("Adapter.") {
                adapter = Some((
                    a.trim().to_string(),
                    ipcfg::ApplyV4 {
                        dhcp: true,
                        ip: String::new(),
                        mask: String::new(),
                        gateway: String::new(),
                        dns: Vec::new(),
                    },
                ));
            }
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if section == "Wifi" && k.starts_with("Profile.") {
            if let (Some(w), Some(bytes)) = (wl.as_ref(), b64_decode(v)) {
                if let Ok(xml) = String::from_utf8(bytes) {
                    if w.import_profile(&xml) {
                        wifi_ok += 1;
                    } else {
                        errors.push("a wifi profile failed to import".into());
                    }
                }
            }
        } else if let Some((_, cfg)) = adapter.as_mut() {
            match k {
                "DHCP" => cfg.dhcp = v != "0",
                "IP" => cfg.ip = v.to_string(),
                "Mask" => cfg.mask = v.to_string(),
                "Gateway" => cfg.gateway = v.to_string(),
                "DNS" => {
                    cfg.dns = v
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                }
                _ => {}
            }
        }
    }
    flush_adapter(&mut adapter, &mut adapters_ok, &mut errors);

    let mut summary = format!(
        "Imported {wifi_ok} wifi profile(s), configured {adapters_ok} adapter(s)"
    );
    if !errors.is_empty() {
        summary.push_str(&format!(" — {} error(s): {}", errors.len(), errors.join("; ")));
    }
    Some(summary)
}
