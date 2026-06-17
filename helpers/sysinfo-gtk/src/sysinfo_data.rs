// SPDX-License-Identifier: GPL-3.0-or-later
//! Hardware/OS data collection for the System Information window.
//!
//! This is a toolkit-agnostic port of the data layer from StartPE's
//! `src/sysinfo.rs` (the Win32/GDI System Information window): the `SysInfo`
//! model and the WMI (`ROOT\CIMV2`) + documented-Win32/registry fallback
//! collection are reused verbatim, with all GDI/window code removed. The GTK UI
//! consumes [`section_groups`], which mirrors the GDI `build_rows` content.
//!
//! `gather()` is safe to call on a background thread (it does its own COM init).

use windows::core::{BSTR, PCWSTR, PWSTR, VARIANT};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
use windows::Win32::System::SystemInformation::*;
use windows::Win32::System::Wmi::*;

// ---- collected data -------------------------------------------------------

#[derive(Default, Clone)]
pub struct MemModule {
    pub capacity: u64, // bytes
    pub speed: u32,    // MT/s
    pub slot: String,
}

#[derive(Default, Clone)]
pub struct Gpu {
    pub name: String,
    pub vram: u64, // bytes
    pub driver: String,
}

#[derive(Default, Clone)]
pub struct Disk {
    pub model: String,
    pub size: u64, // bytes
    pub bus: String,
}

#[derive(Default, Clone)]
pub struct Nic {
    pub name: String,
    pub mac: String,
}

#[derive(Default, Clone)]
pub struct SysInfo {
    pub os_caption: String,
    pub os_version: String,
    pub os_build: String,
    pub computer_name: String,
    pub manufacturer: String,
    pub model: String,
    pub system_type: String,
    pub cpu_name: String,
    pub cpu_cores: u32,
    pub cpu_threads: u32,
    pub cpu_clock_mhz: u32,
    pub cpu_arch: String,
    pub board: String,
    pub bios: String,
    pub ram_total: u64,
    pub ram_avail: u64,
    pub mem_modules: Vec<MemModule>,
    pub gpus: Vec<Gpu>,
    pub displays: Vec<String>,
    pub disks: Vec<Disk>,
    pub nics: Vec<Nic>,
}

/// The four nav sections: (label, symbolic icon name).
pub const SECTIONS: [(&str, &str); 4] = [
    ("System", "computer-symbolic"),
    ("CPU & Memory", "processor-symbolic"),
    ("Graphics & Displays", "video-display-symbolic"),
    ("Storage & Network", "drive-harddisk-symbolic"),
];

// ---- UI-facing content model ----------------------------------------------

/// A titled group of key/value rows (one `AdwPreferencesGroup` in the UI).
pub struct Group {
    pub title: String,
    pub rows: Vec<(String, String)>,
}

const EM_DASH: &str = "\u{2014}";

fn dash(s: &str) -> String {
    if s.is_empty() {
        EM_DASH.into()
    } else {
        s.to_string()
    }
}

fn num(n: u32) -> String {
    if n == 0 {
        EM_DASH.into()
    } else {
        n.to_string()
    }
}

fn fmt_bytes(b: u64) -> String {
    if b == 0 {
        return EM_DASH.into();
    }
    let gb = b as f64 / (1024.0 * 1024.0 * 1024.0);
    if gb >= 1.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{:.0} MB", b as f64 / (1024.0 * 1024.0))
    }
}

fn fmt_mhz(mhz: u32) -> String {
    if mhz == 0 {
        EM_DASH.into()
    } else if mhz >= 1000 {
        format!("{:.2} GHz", mhz as f64 / 1000.0)
    } else {
        format!("{mhz} MHz")
    }
}

/// Build the groups/rows for one nav section. Mirrors the GDI `build_rows`
/// content, restructured into libadwaita preference groups (the GDI "headings"
/// become group titles; the indented sub-rows are flattened).
pub fn section_groups(section: usize, info: &SysInfo) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    macro_rules! group {
        ($title:expr) => {
            groups.push(Group {
                title: $title.to_string(),
                rows: Vec::new(),
            });
        };
    }
    macro_rules! kv {
        ($k:expr, $v:expr) => {
            groups
                .last_mut()
                .expect("a group is open")
                .rows
                .push(($k.to_string(), $v));
        };
    }

    match section {
        0 => {
            group!("Operating system");
            kv!("Edition", dash(&info.os_caption));
            kv!("Version", dash(&info.os_version));
            kv!("Build", dash(&info.os_build));
            group!("Device");
            kv!("Device name", dash(&info.computer_name));
            kv!("Manufacturer", dash(&info.manufacturer));
            kv!("Model", dash(&info.model));
            kv!("System type", dash(&info.system_type));
            group!("Processor");
            kv!("CPU", dash(&info.cpu_name));
            group!("Memory");
            kv!("Installed RAM", fmt_bytes(info.ram_total));
        }
        1 => {
            group!("Processor");
            kv!("Model", dash(&info.cpu_name));
            kv!("Cores", num(info.cpu_cores));
            kv!("Logical processors", num(info.cpu_threads));
            kv!("Max clock", fmt_mhz(info.cpu_clock_mhz));
            kv!("Architecture", dash(&info.cpu_arch));
            group!("Firmware");
            kv!("Baseboard", dash(&info.board));
            kv!("BIOS", dash(&info.bios));
            group!("Physical memory");
            kv!("Total", fmt_bytes(info.ram_total));
            kv!("Available", fmt_bytes(info.ram_avail));
            if info.mem_modules.is_empty() {
                kv!("Modules", EM_DASH.to_string());
            } else {
                for (i, m) in info.mem_modules.iter().enumerate() {
                    let slot = if m.slot.is_empty() {
                        format!("Slot {}", i + 1)
                    } else {
                        m.slot.clone()
                    };
                    let speed = if m.speed > 0 {
                        format!(" @ {} MT/s", m.speed)
                    } else {
                        String::new()
                    };
                    kv!(&slot, format!("{}{}", fmt_bytes(m.capacity), speed));
                }
            }
        }
        2 => {
            group!("Graphics");
            if info.gpus.is_empty() {
                kv!("GPU", EM_DASH.to_string());
            } else {
                let many = info.gpus.len() > 1;
                for (i, g) in info.gpus.iter().enumerate() {
                    let label = if many {
                        format!("GPU {}", i + 1)
                    } else {
                        "GPU".into()
                    };
                    kv!(&label, dash(&g.name));
                    if g.vram > 0 {
                        kv!("VRAM", fmt_bytes(g.vram));
                    }
                    if !g.driver.is_empty() {
                        kv!("Driver", g.driver.clone());
                    }
                }
            }
            group!("Displays");
            if info.displays.is_empty() {
                kv!("Displays", EM_DASH.to_string());
            } else {
                for (i, d) in info.displays.iter().enumerate() {
                    kv!(&format!("Display {}", i + 1), d.clone());
                }
            }
        }
        _ => {
            group!("Disks");
            if info.disks.is_empty() {
                kv!("Disks", EM_DASH.to_string());
            } else {
                for (i, d) in info.disks.iter().enumerate() {
                    kv!(&format!("Disk {}", i + 1), dash(&d.model));
                    kv!("Capacity", fmt_bytes(d.size));
                    if !d.bus.is_empty() {
                        kv!("Bus", d.bus.clone());
                    }
                }
            }
            group!("Network adapters");
            if info.nics.is_empty() {
                kv!("Adapters", EM_DASH.to_string());
            } else {
                for n in &info.nics {
                    let label = if n.name.is_empty() {
                        "Adapter".to_string()
                    } else {
                        n.name.clone()
                    };
                    kv!(&label, n.mac.clone());
                }
            }
        }
    }
    groups
}

// ---- data collection ------------------------------------------------------

/// Collect everything; safe to call on a background thread.
pub fn gather() -> SysInfo {
    let mut info = SysInfo::default();
    unsafe {
        let _ = gather_wmi(&mut info);
        gather_win32(&mut info);
    }
    info
}

/// Read property `name` off a WMI object as a trimmed string. `VARIANT`'s
/// `Display` coerces BSTR/numeric/bool values to text, and the `VARIANT` clears
/// itself on drop.
unsafe fn prop(obj: &IWbemClassObject, name: &str) -> Option<String> {
    let wname: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut var = VARIANT::default();
    obj.Get(PCWSTR(wname.as_ptr()), 0, &mut var, None, None).ok()?;
    let s = var.to_string().trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

unsafe fn prop_u64(obj: &IWbemClassObject, name: &str) -> Option<u64> {
    prop(obj, name).and_then(|s| s.parse::<u64>().ok())
}

unsafe fn prop_u32(obj: &IWbemClassObject, name: &str) -> Option<u32> {
    prop(obj, name).and_then(|s| s.parse::<u32>().ok())
}

unsafe fn query<F: FnMut(&IWbemClassObject)>(svc: &IWbemServices, wql: &str, mut f: F) {
    let Ok(enumerator) = svc.ExecQuery(
        &BSTR::from("WQL"),
        &BSTR::from(wql),
        WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
        None,
    ) else {
        return;
    };
    loop {
        let mut objs: [Option<IWbemClassObject>; 1] = [None];
        let mut returned: u32 = 0;
        let _ = enumerator.Next(WBEM_INFINITE as i32, &mut objs, &mut returned);
        if returned == 0 {
            break;
        }
        if let Some(obj) = objs[0].take() {
            f(&obj);
        }
    }
}

unsafe fn gather_wmi(info: &mut SysInfo) -> windows::core::Result<()> {
    CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
    let r = gather_wmi_inner(info);
    CoUninitialize();
    r
}

unsafe fn gather_wmi_inner(info: &mut SysInfo) -> windows::core::Result<()> {
    // Process-wide security; harmless if already set (returns RPC_E_TOO_LATE).
    let _ = CoInitializeSecurity(
        None,
        -1,
        None,
        None,
        RPC_C_AUTHN_LEVEL_DEFAULT,
        RPC_C_IMP_LEVEL_IMPERSONATE,
        None,
        EOAC_NONE,
        None,
    );

    let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)?;
    let svc: IWbemServices = locator.ConnectServer(
        &BSTR::from("ROOT\\CIMV2"),
        &BSTR::new(),
        &BSTR::new(),
        &BSTR::new(),
        0,
        &BSTR::new(),
        None,
    )?;
    CoSetProxyBlanket(
        &svc,
        RPC_C_AUTHN_WINNT,
        RPC_C_AUTHZ_NONE,
        None,
        RPC_C_AUTHN_LEVEL_CALL,
        RPC_C_IMP_LEVEL_IMPERSONATE,
        None,
        EOAC_NONE,
    )?;

    query(
        &svc,
        "SELECT Caption,Version,BuildNumber FROM Win32_OperatingSystem",
        |o| {
            if let Some(v) = prop(o, "Caption") {
                info.os_caption = v;
            }
            if let Some(v) = prop(o, "Version") {
                info.os_version = v;
            }
            if let Some(v) = prop(o, "BuildNumber") {
                info.os_build = v;
            }
        },
    );
    query(
        &svc,
        "SELECT Name,Manufacturer,Model,SystemType,TotalPhysicalMemory FROM Win32_ComputerSystem",
        |o| {
            if let Some(v) = prop(o, "Name") {
                info.computer_name = v;
            }
            if let Some(v) = prop(o, "Manufacturer") {
                info.manufacturer = v;
            }
            if let Some(v) = prop(o, "Model") {
                info.model = v;
            }
            if let Some(v) = prop(o, "SystemType") {
                info.system_type = v;
            }
            if let Some(v) = prop_u64(o, "TotalPhysicalMemory") {
                info.ram_total = v;
            }
        },
    );
    query(
        &svc,
        "SELECT Name,NumberOfCores,NumberOfLogicalProcessors,MaxClockSpeed FROM Win32_Processor",
        |o| {
            if info.cpu_name.is_empty() {
                if let Some(v) = prop(o, "Name") {
                    info.cpu_name = v;
                }
                if let Some(v) = prop_u32(o, "NumberOfCores") {
                    info.cpu_cores = v;
                }
                if let Some(v) = prop_u32(o, "NumberOfLogicalProcessors") {
                    info.cpu_threads = v;
                }
                if let Some(v) = prop_u32(o, "MaxClockSpeed") {
                    info.cpu_clock_mhz = v;
                }
            }
        },
    );
    query(&svc, "SELECT Manufacturer,Product FROM Win32_BaseBoard", |o| {
        let m = prop(o, "Manufacturer").unwrap_or_default();
        let p = prop(o, "Product").unwrap_or_default();
        info.board = format!("{m} {p}").trim().to_string();
    });
    query(
        &svc,
        "SELECT SMBIOSBIOSVersion,ReleaseDate FROM Win32_BIOS",
        |o| {
            let ver = prop(o, "SMBIOSBIOSVersion").unwrap_or_default();
            let date = prop(o, "ReleaseDate").unwrap_or_default();
            let date = if date.len() >= 8 {
                format!("{}-{}-{}", &date[0..4], &date[4..6], &date[6..8])
            } else {
                date
            };
            info.bios = format!("{ver} ({date})").trim().to_string();
        },
    );
    query(
        &svc,
        "SELECT Capacity,Speed,DeviceLocator FROM Win32_PhysicalMemory",
        |o| {
            info.mem_modules.push(MemModule {
                capacity: prop_u64(o, "Capacity").unwrap_or(0),
                speed: prop_u32(o, "Speed").unwrap_or(0),
                slot: prop(o, "DeviceLocator").unwrap_or_default(),
            });
        },
    );
    query(
        &svc,
        "SELECT Name,AdapterRAM,DriverVersion FROM Win32_VideoController",
        |o| {
            let name = prop(o, "Name").unwrap_or_default();
            if name.is_empty() {
                return;
            }
            info.gpus.push(Gpu {
                name,
                vram: prop_u64(o, "AdapterRAM").unwrap_or(0),
                driver: prop(o, "DriverVersion").unwrap_or_default(),
            });
        },
    );
    query(
        &svc,
        "SELECT Model,Size,InterfaceType FROM Win32_DiskDrive",
        |o| {
            let model = prop(o, "Model").unwrap_or_default();
            if model.is_empty() {
                return;
            }
            info.disks.push(Disk {
                model,
                size: prop_u64(o, "Size").unwrap_or(0),
                bus: prop(o, "InterfaceType").unwrap_or_default(),
            });
        },
    );
    query(
        &svc,
        "SELECT Name,MACAddress FROM Win32_NetworkAdapter WHERE PhysicalAdapter=TRUE",
        |o| {
            let mac = prop(o, "MACAddress").unwrap_or_default();
            if mac.is_empty() {
                return;
            }
            info.nics.push(Nic {
                name: prop(o, "Name").unwrap_or_default(),
                mac,
            });
        },
    );

    Ok(())
}

unsafe fn gather_win32(info: &mut SysInfo) {
    let mut si = SYSTEM_INFO::default();
    GetNativeSystemInfo(&mut si);
    let arch = match si.Anonymous.Anonymous.wProcessorArchitecture {
        PROCESSOR_ARCHITECTURE_AMD64 => "x64 (AMD64)",
        PROCESSOR_ARCHITECTURE_ARM64 => "ARM64",
        PROCESSOR_ARCHITECTURE_INTEL => "x86",
        _ => "Unknown",
    };
    if info.cpu_arch.is_empty() {
        info.cpu_arch = arch.to_string();
    }
    if info.cpu_threads == 0 {
        info.cpu_threads = si.dwNumberOfProcessors;
    }
    if info.system_type.is_empty() {
        info.system_type = format!("{arch}-based PC");
    }

    let mut ms = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    if GlobalMemoryStatusEx(&mut ms).is_ok() {
        if info.ram_total == 0 {
            info.ram_total = ms.ullTotalPhys;
        }
        info.ram_avail = ms.ullAvailPhys;
    }

    if info.computer_name.is_empty() {
        let mut buf = [0u16; 256];
        let mut len = buf.len() as u32;
        if GetComputerNameExW(ComputerNameDnsHostname, PWSTR(buf.as_mut_ptr()), &mut len).is_ok() {
            info.computer_name = String::from_utf16_lossy(&buf[..len as usize]);
        }
    }

    let _ = EnumDisplayMonitors(
        HDC::default(),
        None,
        Some(monitor_proc),
        LPARAM(info as *mut SysInfo as isize),
    );

    if info.os_caption.is_empty() || info.os_build.is_empty() {
        use winreg::enums::HKEY_LOCAL_MACHINE;
        use winreg::RegKey;
        if let Ok(k) = RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion")
        {
            if info.os_caption.is_empty() {
                if let Ok(v) = k.get_value::<String, _>("ProductName") {
                    info.os_caption = v;
                }
            }
            if info.os_build.is_empty() {
                if let Ok(v) = k.get_value::<String, _>("CurrentBuildNumber") {
                    info.os_build = v;
                }
            }
        }
    }
}

unsafe extern "system" fn monitor_proc(
    hmon: HMONITOR,
    _hdc: HDC,
    _rc: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let info = &mut *(data.0 as *mut SysInfo);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(hmon, &mut mi).as_bool() {
        let w = mi.rcMonitor.right - mi.rcMonitor.left;
        let h = mi.rcMonitor.bottom - mi.rcMonitor.top;
        info.displays.push(format!("{w} \u{00d7} {h}"));
    }
    TRUE
}
