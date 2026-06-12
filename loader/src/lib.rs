// SPDX-License-Identifier: GPL-3.0-or-later
//! StartPE Explorer-side loader shim.
//!
//! This DLL is loaded *inside* `explorer.exe` early in shell startup via the
//! `Drive\shellex\FolderExtensions` COM registration (the same load vector
//! StartAllBack uses). On a Win11 PE image Explorer's modern (XAML) taskbar
//! init faults and takes down the shell thread before the desktop
//! (`Progman`/`SHELLDLL_DefView`) is ever created — so no wallpaper or icons
//! appear. Being in-process lets us:
//!
//!   1. Launch `startpe.exe` (our taskbar/start menu) at the right moment, and
//!   2. (diagnostics, for now) record the exact faulting module/stack of the
//!      shell crash to `X:\startpe_loader.log`, since WinPE has no Event Viewer.
//!
//! Once the crash signature is known, the active suppression hook is added
//! here. This is deliberately not "documented Win32 only" — it is the one
//! component allowed to touch Explorer internals.

#![allow(non_snake_case)]
#![allow(clippy::missing_safety_doc)]

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed};

use windows::core::{GUID, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, BOOL, FALSE, HINSTANCE, HMODULE, TRUE};
use windows::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, RtlCaptureStackBackTrace, EXCEPTION_POINTERS,
};
use windows::Win32::System::LibraryLoader::{
    DisableThreadLibraryCalls, GetModuleFileNameW, GetModuleHandleExW,
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows::Win32::System::Threading::{
    CreateProcessW, CreateThread, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTUPINFOW,
    THREAD_CREATION_FLAGS,
};

const CLASS_E_CLASSNOTAVAILABLE: HRESULT = HRESULT(0x8004_0111u32 as i32);
const S_FALSE: HRESULT = HRESULT(1);
const LOG_PATH: &str = "X:\\startpe_loader.log";

/// Our own module handle, stashed in `DllMain` for path resolution.
static G_HMODULE: AtomicUsize = AtomicUsize::new(0);
/// Whether the host process is `explorer.exe` (we only act there).
static IS_EXPLORER: AtomicBool = AtomicBool::new(false);
/// Number of fatal exceptions logged so far (capped to keep the log small).
static LOG_COUNT: AtomicU32 = AtomicU32::new(0);

#[no_mangle]
pub extern "system" fn DllMain(hinst: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        unsafe {
            G_HMODULE.store(hinst.0 as usize, Relaxed);
            let _ = DisableThreadLibraryCalls(HMODULE(hinst.0));

            // Pin ourselves so a failed DllGetClassObject can't unload us and
            // tear down the handler / worker we are about to install.
            let mut pinned = HMODULE::default();
            let _ = GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_PIN | GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                PCWSTR(&G_HMODULE as *const AtomicUsize as *const u16),
                &mut pinned,
            );

            IS_EXPLORER.store(host_is_explorer(), Relaxed);

            // Install the crash logger as early as possible (first handler).
            AddVectoredExceptionHandler(1, Some(veh));

            // Heavy work (launching startpe.exe) must not run under the loader
            // lock, so hand it to a fresh thread.
            let _ = CreateThread(
                None,
                0,
                Some(worker),
                None,
                THREAD_CREATION_FLAGS(0),
                None,
            );
        }
    }
    TRUE
}

/// COM entry points. We never actually hand Explorer a usable object — being
/// loaded (and running `DllMain`) is the entire point — so report the class as
/// unavailable and refuse to unload.
#[no_mangle]
pub extern "system" fn DllGetClassObject(
    _rclsid: *const GUID,
    _riid: *const GUID,
    _ppv: *mut *mut c_void,
) -> HRESULT {
    CLASS_E_CLASSNOTAVAILABLE
}

#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}

unsafe extern "system" fn worker(_param: *mut c_void) -> u32 {
    if IS_EXPLORER.load(Relaxed) {
        launch_startpe();
    }
    0
}

/// Launch `startpe.exe` from the loader's own directory. The exe name is
/// derived from the DLL name so the arch-specific pair stays matched
/// (`startpe_loader.dll` -> `startpe.exe`, `startpe_loader-arm64.dll` ->
/// `startpe-arm64.exe`).
unsafe fn launch_startpe() {
    let module = HMODULE(G_HMODULE.load(Relaxed) as *mut c_void);
    let mut buf = [0u16; 520];
    let n = GetModuleFileNameW(module, &mut buf);
    if n == 0 {
        return;
    }
    let full = String::from_utf16_lossy(&buf[..n as usize]);
    let (dir, file) = match full.rfind('\\') {
        Some(pos) => (&full[..=pos], &full[pos + 1..]),
        None => ("", full.as_str()),
    };
    let exe_file = file.replace("_loader", "").replace(".dll", ".exe");
    let exe_path = format!("{dir}{exe_file}");

    let mut wpath: Vec<u16> = exe_path.encode_utf16().chain(std::iter::once(0)).collect();

    let si = STARTUPINFOW {
        cb: core::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();

    let ok = CreateProcessW(
        PCWSTR(wpath.as_ptr()),
        PWSTR(wpath.as_mut_ptr()),
        None,
        None,
        FALSE,
        PROCESS_CREATION_FLAGS(0),
        None,
        PCWSTR::null(),
        &si,
        &mut pi,
    );
    if ok.is_ok() {
        if !pi.hProcess.is_invalid() {
            let _ = CloseHandle(pi.hProcess);
        }
        if !pi.hThread.is_invalid() {
            let _ = CloseHandle(pi.hThread);
        }
    }
}

/// Vectored exception handler used purely to record the shell crash. We never
/// swallow the exception (return `EXCEPTION_CONTINUE_SEARCH`); we only note the
/// faulting module and a short stack so the precise suppression hook can be
/// written next.
unsafe extern "system" fn veh(info: *mut EXCEPTION_POINTERS) -> i32 {
    const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

    if !IS_EXPLORER.load(Relaxed) || info.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let rec = (*info).ExceptionRecord;
    if rec.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let code = (*rec).ExceptionCode.0 as u32;

    // Only the codes that actually tear down a process. Notably 0xC000027B
    // (STOWED_EXCEPTION) is how WinRT/XAML failures surface — the likely Win11
    // taskbar culprit — alongside access violations and fail-fast.
    let fatal = matches!(
        code,
        0xC000_0005 // ACCESS_VIOLATION
            | 0xC000_027B // STOWED_EXCEPTION (WinRT/XAML)
            | 0xC000_0409 // STACK_BUFFER_OVERRUN / fail-fast
            | 0xC000_0374 // HEAP_CORRUPTION
            | 0x8000_0003 // BREAKPOINT
            | 0xC000_041D // FATAL_USER_CALLBACK_EXCEPTION
            | 0xC06D_007E // MODULE_NOT_FOUND
            | 0xC06D_007F // PROCEDURE_NOT_FOUND
    );
    if !fatal {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let idx = LOG_COUNT.fetch_add(1, Relaxed);
    if idx >= 8 {
        return EXCEPTION_CONTINUE_SEARCH;
    }

    let addr = (*rec).ExceptionAddress;
    let mut out = String::new();
    out.push_str(&format!(
        "[startpe_loader] fatal exception #{idx} code=0x{code:08X} addr={addr:p}\n"
    ));
    match resolve_module(addr) {
        Some((path, base)) => out.push_str(&format!(
            "  fault module: {path} +0x{:X}\n",
            addr as usize - base
        )),
        None => out.push_str("  fault module: <unknown>\n"),
    }

    let mut frames = [core::ptr::null_mut::<c_void>(); 24];
    let captured = RtlCaptureStackBackTrace(0, &mut frames, None);
    for i in 0..captured as usize {
        let frame = frames[i];
        if frame.is_null() {
            break;
        }
        match resolve_module(frame) {
            Some((path, base)) => {
                let name = path.rsplit('\\').next().unwrap_or(&path);
                out.push_str(&format!("  [{i:2}] {name} +0x{:X}\n", frame as usize - base));
            }
            None => out.push_str(&format!("  [{i:2}] {frame:p}\n")),
        }
    }
    out.push('\n');
    append_log(&out);

    EXCEPTION_CONTINUE_SEARCH
}

/// Resolve an address to its owning module path and base, without changing the
/// module's refcount.
unsafe fn resolve_module(addr: *const c_void) -> Option<(String, usize)> {
    let mut module = HMODULE::default();
    GetModuleHandleExW(
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        PCWSTR(addr as *const u16),
        &mut module,
    )
    .ok()?;
    let mut buf = [0u16; 520];
    let n = GetModuleFileNameW(module, &mut buf);
    if n == 0 {
        return None;
    }
    Some((String::from_utf16_lossy(&buf[..n as usize]), module.0 as usize))
}

unsafe fn host_is_explorer() -> bool {
    let mut buf = [0u16; 520];
    let n = GetModuleFileNameW(HMODULE::default(), &mut buf);
    if n == 0 {
        return false;
    }
    String::from_utf16_lossy(&buf[..n as usize])
        .to_ascii_lowercase()
        .ends_with("\\explorer.exe")
}

fn append_log(text: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_PATH)
    {
        let _ = f.write_all(text.as_bytes());
    }
}
