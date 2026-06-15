// SPDX-License-Identifier: GPL-3.0-or-later
//! syslaunch — run a program as SYSTEM on a chosen interactive session's desktop.
//!
//! Why this exists: on a 25H2 WinPE the shell runs as SYSTEM in a session with no
//! interactive logon, so `winlogon` never spawns `dwm.exe` and nothing is
//! composited (no dark titlebars, no DWM frames). When the build's "Logon as
//! Admin" feature is on, an Administrator logon creates a *separate* interactive
//! session whose desktop **is** DWM-composited. This tool bridges the two: it
//! gets a SYSTEM token and launches a program with it onto the target session's
//! `winsta0\default` desktop. The launched program runs as SYSTEM (full
//! ACL-skipping for data recovery) but is composited like any other window in
//! that session — so it gets the DWM frame.
//!
//! Two ways to obtain the SYSTEM token, tried in order:
//!   1. **Direct** — duplicate the token from a SYSTEM process in the target
//!      session (`winlogon`). Works when syslaunch itself already runs as SYSTEM
//!      (the production case: launched by a SYSTEM autorun/service).
//!   2. **Service route** — when run as a mere Administrator, `winlogon`'s token
//!      is not openable even with `SeDebugPrivilege`. So we install a tiny
//!      transient LocalSystem service; the SCM starts it *as SYSTEM*; it sets its
//!      own token to the target session and spawns the program there, then stops
//!      and is deleted. This is exactly how PsExec's `-s` works and needs no
//!      `SeDebugPrivilege`.
//!
//! Usage:
//!   syslaunch [--session N | --console] [program [args...]]
//!     (no args)        spawn cmd.exe as SYSTEM on the *current* session desktop
//!     --console        target the active console session instead of the current
//!     --session N      target session id N explicitly
//!     program [args]   the program to run (default: cmd.exe)
//!   syslaunch --service-run   (internal: the entry point the SCM starts)

#![cfg_attr(not(debug_assertions), windows_subsystem = "console")]

use std::io::Write;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, LUID};
use windows::Win32::Security::{
    AdjustTokenPrivileges, DuplicateTokenEx, LookupPrivilegeValueW, SecurityImpersonation,
    SetTokenInformation, TokenPrimary, TokenSessionId, LUID_AND_ATTRIBUTES, SE_DEBUG_NAME,
    SE_PRIVILEGE_ENABLED, SE_TCB_NAME, TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_ADJUST_SESSIONID, TOKEN_ALL_ACCESS, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
    TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId};
use windows::Win32::System::Services::{
    CloseServiceHandle, CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW,
    QueryServiceStatus, RegisterServiceCtrlHandlerW, SetServiceStatus,
    StartServiceCtrlDispatcherW, StartServiceW, SC_MANAGER_ALL_ACCESS, SERVICE_ACCEPT_STOP,
    SERVICE_ALL_ACCESS, SERVICE_CONTROL_STOP, SERVICE_DEMAND_START, SERVICE_ERROR_NORMAL,
    SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE,
    SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken,
    CREATE_NEW_CONSOLE, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, PROCESS_QUERY_INFORMATION,
    STARTUPINFOW,
};

/// Dispatch-table service name. For a `SERVICE_WIN32_OWN_PROCESS` service the
/// name in the dispatch table / `RegisterServiceCtrlHandlerW` is ignored, so a
/// constant is fine even though the *installed* service name is unique per pid.
const SVC_NAME: &str = "syslaunch_svc";

/// Pull the `--job N` id out of our own command line (set in the service's
/// binary path so the SCM-started service process can find its job file).
fn job_id_arg() -> Option<u32> {
    let mut it = std::env::args();
    while let Some(a) = it.next() {
        if a.eq_ignore_ascii_case("--job") {
            return it.next().and_then(|s| s.parse::<u32>().ok());
        }
    }
    None
}

/// Best-effort version-stamped line to `X:\startpe.log` and stdout, so the PE
/// (which has no Event Viewer) keeps a trail of what happened.
fn log(msg: &str) {
    let line = format!("syslaunch v{}: {}", env!("CARGO_PKG_VERSION"), msg);
    println!("{line}");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("X:\\startpe.log")
    {
        let _ = writeln!(f, "{line}");
    }
}

fn last_err() -> u32 {
    unsafe { GetLastError().0 }
}

/// Session id of a process, or None if it can't be queried.
fn session_of(pid: u32) -> Option<u32> {
    let mut sid = 0u32;
    if unsafe { ProcessIdToSessionId(pid, &mut sid) }.is_ok() {
        Some(sid)
    } else {
        None
    }
}

/// UTF-16, NUL-terminated, mutable buffer for the `PWSTR`/`PCWSTR` params.
fn wbuf(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Re-quote a split command line so a program path containing spaces (e.g.
/// `X:\Program Files\StartPE\startpe.exe`) survives `CreateProcess`'s own
/// tokenizing. The CRT already stripped the quotes the caller passed, so we
/// restore them around any token that needs them.
fn quote_cmdline(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| {
            if p.contains(' ') && !p.starts_with('"') {
                format!("\"{p}\"")
            } else {
                p.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The job file (target session + command line) handed from the installer to the
/// service. Kept beside the exe so both the Administrator and the SYSTEM service
/// can reach it regardless of differing %TEMP%. Named per installer pid so
/// concurrent invocations (e.g. several launch vectors firing at once) don't
/// clobber each other.
fn job_path(id: u32) -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(format!("syslaunch-{id}.job"))))
        .unwrap_or_else(|| std::path::PathBuf::from(format!("X:\\syslaunch-{id}.job")))
}

/// Enable a privilege on our own process token. Logs whether it was actually
/// granted (`ERROR_NOT_ALL_ASSIGNED` means the token doesn't hold it).
fn enable_privilege(name: PCWSTR, label: &str) -> bool {
    unsafe {
        let mut tok = HANDLE::default();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut tok,
        )
        .is_err()
        {
            log(&format!("OpenProcessToken(self) failed: {}", last_err()));
            return false;
        }
        let mut luid = LUID::default();
        let mut ok = false;
        if LookupPrivilegeValueW(PCWSTR::null(), name, &mut luid).is_ok() {
            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            let _ = AdjustTokenPrivileges(tok, false, Some(&tp), 0, None, None);
            match last_err() {
                0 => {
                    log(&format!("{label} enabled"));
                    ok = true;
                }
                1300 => log(&format!("{label} NOT held by this token")),
                e => log(&format!("AdjustTokenPrivileges({label}) returned {e}")),
            }
        } else {
            log(&format!("LookupPrivilegeValue({label}) failed: {}", last_err()));
        }
        let _ = CloseHandle(tok);
        ok
    }
}

/// All (pid, exe-name) pairs in the given session.
fn processes_in_session(session: u32) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut pe = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut pe).is_ok() {
            loop {
                if session_of(pe.th32ProcessID) == Some(session) {
                    let n = pe.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
                    out.push((pe.th32ProcessID, String::from_utf16_lossy(&pe.szExeFile[..n])));
                }
                if Process32NextW(snap, &mut pe).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// Try to duplicate a SYSTEM primary token from `pid`. Logs the outcome.
fn try_token_from(pid: u32, name: &str) -> Option<HANDLE> {
    unsafe {
        // OpenProcessToken needs PROCESS_QUERY_INFORMATION (not the LIMITED one).
        let Ok(hproc) = OpenProcess(PROCESS_QUERY_INFORMATION, false, pid) else {
            log(&format!("OpenProcess({name} pid {pid}) failed: {}", last_err()));
            return None;
        };
        let mut htok = HANDLE::default();
        let access = TOKEN_DUPLICATE
            | TOKEN_QUERY
            | TOKEN_ASSIGN_PRIMARY
            | TOKEN_ADJUST_DEFAULT
            | TOKEN_ADJUST_SESSIONID;
        let opened = OpenProcessToken(hproc, access, &mut htok).is_ok();
        let _ = CloseHandle(hproc);
        if !opened {
            log(&format!("OpenProcessToken({name} pid {pid}) failed: {}", last_err()));
            return None;
        }
        let dup = duplicate_primary(htok);
        let _ = CloseHandle(htok);
        if dup.is_some() {
            log(&format!("got SYSTEM token from {name} (pid {pid})"));
        }
        dup
    }
}

/// Duplicate a token into a primary token (for CreateProcessAsUser).
fn duplicate_primary(src: HANDLE) -> Option<HANDLE> {
    unsafe {
        let mut dup = HANDLE::default();
        if DuplicateTokenEx(
            src,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut dup,
        )
        .is_ok()
        {
            Some(dup)
        } else {
            log(&format!("DuplicateTokenEx failed: {}", last_err()));
            None
        }
    }
}

/// Find a SYSTEM primary token bound to `session` by duplicating it from a SYSTEM
/// process there. In an interactive session `winlogon` is the only SYSTEM
/// process; opening it succeeds only when *we* are SYSTEM (production) — an
/// Administrator is denied even with SeDebug, which is why the service route
/// exists.
fn find_system_token(session: u32) -> Option<HANDLE> {
    let procs = processes_in_session(session);
    log(&format!("{} processes in session {session}", procs.len()));
    for (pid, name) in &procs {
        if name.eq_ignore_ascii_case("winlogon.exe") {
            if let Some(t) = try_token_from(*pid, name) {
                return Some(t);
            }
        }
    }
    None
}

/// Set a primary token's session id (requires SeTcbPrivilege, which SYSTEM holds).
fn set_token_session(token: HANDLE, session: u32) -> bool {
    unsafe {
        SetTokenInformation(
            token,
            TokenSessionId,
            &session as *const u32 as *const core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        )
        .is_ok()
    }
}

/// Launch `cmdline` with `token` on the `winsta0\default` desktop. The session is
/// whatever the token carries, so set it beforehand if needed.
fn spawn_with_token(token: HANDLE, cmdline: &str) -> Option<u32> {
    let mut desktop = wbuf("winsta0\\default");
    let mut cmd = wbuf(cmdline);
    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();
    let ok = unsafe {
        CreateProcessAsUserW(
            token,
            PCWSTR::null(),
            PWSTR(cmd.as_mut_ptr()),
            None,
            None,
            false,
            CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT,
            None,
            PCWSTR::null(),
            &si,
            &mut pi,
        )
    };
    match ok {
        Ok(()) => {
            let id = pi.dwProcessId;
            unsafe {
                let _ = CloseHandle(pi.hProcess);
                let _ = CloseHandle(pi.hThread);
            }
            Some(id)
        }
        Err(_) => {
            log(&format!("CreateProcessAsUserW failed: {}", last_err()));
            None
        }
    }
}

// ===========================================================================
// Service side (runs as LocalSystem when started by the SCM)
// ===========================================================================

static STATUS_HANDLE: AtomicIsize = AtomicIsize::new(0);

fn set_service_state(state: SERVICE_STATUS_CURRENT_STATE, accept: u32) {
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accept,
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: 0,
    };
    let h = SERVICE_STATUS_HANDLE(STATUS_HANDLE.load(Ordering::SeqCst) as *mut core::ffi::c_void);
    unsafe {
        let _ = SetServiceStatus(h, &status);
    }
}

unsafe extern "system" fn service_ctrl_handler(control: u32) {
    if control == SERVICE_CONTROL_STOP {
        set_service_state(SERVICE_STOPPED, 0);
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let name = wbuf(SVC_NAME);
    if let Ok(h) = RegisterServiceCtrlHandlerW(PCWSTR(name.as_ptr()), Some(service_ctrl_handler)) {
        STATUS_HANDLE.store(h.0 as isize, Ordering::SeqCst);
    }
    set_service_state(SERVICE_RUNNING, SERVICE_ACCEPT_STOP);
    log("service running as SYSTEM; performing spawn");

    do_service_spawn();

    set_service_state(SERVICE_STOPPED, 0);
}

/// The work the service does: read the job, take our own SYSTEM token, retarget
/// it to the requested session, and launch the program there.
fn do_service_spawn() {
    let Some(id) = job_id_arg() else {
        log("service: no --job id on the command line");
        return;
    };
    let Some((session, cmdline)) = read_job(id) else {
        log("service: job file missing/invalid");
        return;
    };
    log(&format!("service: target session {session}, command: {cmdline}"));

    enable_privilege(SE_TCB_NAME, "SeTcbPrivilege");

    let mut self_tok = HANDLE::default();
    let access = TOKEN_DUPLICATE
        | TOKEN_QUERY
        | TOKEN_ASSIGN_PRIMARY
        | TOKEN_ADJUST_DEFAULT
        | TOKEN_ADJUST_SESSIONID;
    if unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut self_tok) }.is_err() {
        log(&format!("service: OpenProcessToken(self) failed: {}", last_err()));
        return;
    }
    let Some(dup) = duplicate_primary(self_tok) else {
        unsafe {
            let _ = CloseHandle(self_tok);
        }
        return;
    };
    unsafe {
        let _ = CloseHandle(self_tok);
    }

    if !set_token_session(dup, session) {
        log(&format!("service: set session id {session} failed: {} (SeTcb?)", last_err()));
        unsafe {
            let _ = CloseHandle(dup);
        }
        return;
    }

    match spawn_with_token(dup, &cmdline) {
        Some(pid) => log(&format!("service: launched pid {pid} as SYSTEM on session {session}")),
        None => log("service: spawn failed"),
    }
    unsafe {
        let _ = CloseHandle(dup);
    }
}

/// Run as the SCM-dispatched service (entry for `--service-run`).
fn run_as_service() {
    let mut name = wbuf(SVC_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR::null(),
            lpServiceProc: None,
        },
    ];
    unsafe {
        if StartServiceCtrlDispatcherW(table.as_ptr()).is_err() {
            log(&format!("StartServiceCtrlDispatcherW failed: {}", last_err()));
        }
    }
}

// ===========================================================================
// Installer side (runs as the Administrator who invoked syslaunch)
// ===========================================================================

fn write_job(id: u32, session: u32, cmdline: &str) -> bool {
    std::fs::write(job_path(id), format!("{session}\n{cmdline}\n")).is_ok()
}

fn read_job(id: u32) -> Option<(u32, String)> {
    let text = std::fs::read_to_string(job_path(id)).ok()?;
    let mut lines = text.lines();
    let session = lines.next()?.trim().parse::<u32>().ok()?;
    let cmd = lines.next()?.to_string();
    Some((session, cmd))
}

/// Install a transient LocalSystem service that runs this exe with
/// `--service-run`, start it (it spawns the program as SYSTEM on the target
/// session), wait for it to stop, then delete it.
fn run_via_service(session: u32, cmdline: &str) -> bool {
    let id = std::process::id();
    if !write_job(id, session, cmdline) {
        log("could not write job file beside the exe");
        return false;
    }
    let Ok(exe) = std::env::current_exe() else {
        log("current_exe() failed");
        return false;
    };
    // Unique service name per invocation so concurrent launch vectors can't
    // collide on CreateService / DeleteService.
    let svc_name = format!("syslaunch_svc_{id}");
    let bin = format!("\"{}\" --service-run --job {id}", exe.display());
    let name = wbuf(&svc_name);
    let bin_w = wbuf(&bin);

    unsafe {
        let scm = match OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS) {
            Ok(h) => h,
            Err(_) => {
                log(&format!("OpenSCManager failed: {} (need Administrator)", last_err()));
                return false;
            }
        };

        // Remove any leftover service from a previous run.
        if let Ok(existing) = OpenServiceW(scm, PCWSTR(name.as_ptr()), SERVICE_ALL_ACCESS) {
            let _ = DeleteService(existing);
            let _ = CloseServiceHandle(existing);
        }

        let svc = match CreateServiceW(
            scm,
            PCWSTR(name.as_ptr()),
            PCWSTR(name.as_ptr()),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_DEMAND_START,
            SERVICE_ERROR_NORMAL,
            PCWSTR(bin_w.as_ptr()),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(), // lpServiceStartName = NULL => LocalSystem
            PCWSTR::null(),
        ) {
            Ok(h) => h,
            Err(_) => {
                log(&format!("CreateService failed: {}", last_err()));
                let _ = CloseServiceHandle(scm);
                return false;
            }
        };

        let started = StartServiceW(svc, None).is_ok();
        if !started {
            log(&format!("StartService failed: {}", last_err()));
        } else {
            // Wait for the service to finish its spawn (it sets STOPPED on exit).
            let mut status = SERVICE_STATUS::default();
            for _ in 0..100 {
                if QueryServiceStatus(svc, &mut status).is_err() {
                    break;
                }
                if status.dwCurrentState == SERVICE_STOPPED {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }

        let _ = DeleteService(svc);
        let _ = CloseServiceHandle(svc);
        let _ = CloseServiceHandle(scm);
        let _ = std::fs::remove_file(job_path(id));
        started
    }
}

// ===========================================================================

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // SCM-dispatched entry.
    if args.iter().any(|a| a.eq_ignore_ascii_case("--service-run")) {
        run_as_service();
        return;
    }

    // Parse target session and the program to launch.
    let mut target: Option<u32> = None;
    let mut prog_parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--console" => target = Some(unsafe { WTSGetActiveConsoleSessionId() }),
            "--session" => {
                i += 1;
                target = args.get(i).and_then(|s| s.parse::<u32>().ok());
            }
            _ => {
                prog_parts = args[i..].to_vec();
                break;
            }
        }
        i += 1;
    }

    let target = target.unwrap_or_else(|| {
        session_of(unsafe { GetCurrentProcessId() }).unwrap_or(0)
    });
    let cmdline = if prog_parts.is_empty() {
        "cmd.exe".to_string()
    } else {
        quote_cmdline(&prog_parts)
    };

    log(&format!("target session {target}, command: {cmdline}"));

    // 1) Direct: works when syslaunch itself is SYSTEM (can open winlogon's token).
    enable_privilege(SE_DEBUG_NAME, "SeDebugPrivilege");
    if let Some(token) = find_system_token(target) {
        if let Some(pid) = spawn_with_token(token, &cmdline) {
            log(&format!("launched pid {pid} as SYSTEM on session {target} (direct)"));
            unsafe {
                let _ = CloseHandle(token);
            }
            return;
        }
        unsafe {
            let _ = CloseHandle(token);
        }
    }

    // 2) Service route: required when run as a plain Administrator.
    log("direct token grab unavailable; using the LocalSystem service route");
    if run_via_service(target, &cmdline) {
        log("service route dispatched — check the target session's desktop for the new window");
    } else {
        log("service route failed");
        std::process::exit(5);
    }
}
