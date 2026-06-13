// SPDX-License-Identifier: GPL-3.0-or-later
//! A filtering `IShellFolder` wrapper that hides the desktop namespace
//! junctions (This PC, Home, Network, Control Panel, Recycle Bin) from a hosted
//! desktop view, programmatically — no registry or script hacks.
//!
//! StartPE hosts the *namespace* desktop (so the `Shell\Bags\1\Desktop` icon
//! layout the PEBakery `DesktopLayout` tweak writes applies), then drops the
//! junction items by wrapping the real desktop `IShellFolder`: `EnumObjects`
//! returns an enumerator that skips any child whose parsing name is one of the
//! junction CLSIDs. Everything else forwards to the real folder unchanged, so
//! icons, context menus, sorting and folder navigation behave normally.

use core::ffi::c_void;

use windows::core::{implement, Interface, Result, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, S_OK};
use windows::Win32::System::Com::{IBindCtx, IPersist_Impl};
use windows::Win32::UI::Shell::Common::{ITEMIDLIST, STRRET};
use windows::Win32::UI::Shell::{
    IEnumIDList, IEnumIDList_Impl, IPersistFolder2, IPersistFolder2_Impl, IPersistFolder_Impl,
    IShellFolder, IShellFolder_Impl, IShellView, ILFree, SHCreateShellFolderView,
    SHGetDesktopFolder, StrRetToBufW, SFV_CREATE, SHGDN_FORPARSING, SHGDNF,
};

/// Parsing-name CLSIDs of the desktop junctions we hide.
const JUNCTION_CLSIDS: [&str; 5] = [
    "{20D04FE0-3AEA-1069-A2D8-08002B30309D}", // This PC
    "{59031A47-3F72-44A7-89C5-5595FE6B30EE}", // User's Files (Home)
    "{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}", // Network
    "{645FF040-5081-101B-9F08-00AA002F954E}", // Recycle Bin
    "{5399E694-6CE5-4D6C-8FCE-1D8870FDCBA0}", // Control Panel
];

/// Build a desktop view that hosts the real namespace desktop but hides the
/// junction items. Returns `None` on any failure (caller falls back).
pub unsafe fn create_filtered_desktop_view() -> Option<IShellView> {
    let desktop = SHGetDesktopFolder().ok()?;
    let filtered: IShellFolder = FilterFolder {
        inner: desktop.clone(),
    }
    .into();
    let create = SFV_CREATE {
        cbSize: core::mem::size_of::<SFV_CREATE>() as u32,
        pshf: core::mem::ManuallyDrop::new(Some(filtered)),
        psvOuter: core::mem::ManuallyDrop::new(None),
        psfvcb: core::mem::ManuallyDrop::new(None),
    };
    SHCreateShellFolderView(&create).ok()
}

/// True if `pidl` (a child of `folder`) is one of the hidden junctions, decided
/// by its `SHGDN_FORPARSING` name (locale-independent).
unsafe fn is_junction(folder: &IShellFolder, pidl: *const ITEMIDLIST) -> bool {
    let mut sr = STRRET::default();
    if folder.GetDisplayNameOf(pidl, SHGDN_FORPARSING, &mut sr).is_err() {
        return false;
    }
    let mut buf = [0u16; 260];
    if StrRetToBufW(&mut sr, Some(pidl), &mut buf).is_err() {
        return false;
    }
    let name = String::from_utf16_lossy(&buf)
        .trim_end_matches('\0')
        .to_ascii_uppercase();
    JUNCTION_CLSIDS.iter().any(|c| name.contains(c))
}

#[implement(IShellFolder, IPersistFolder2)]
struct FilterFolder {
    inner: IShellFolder,
}

// Forward folder identity so the hosted view selects the desktop's icon-layout
// bag (Shell\Bags\1\Desktop) — that's what makes the PEBakery DesktopLayout
// positions apply.
#[allow(non_snake_case)]
impl IPersist_Impl for FilterFolder_Impl {
    fn GetClassID(&self) -> Result<GUID> {
        unsafe { self.inner.cast::<IPersistFolder2>()?.GetClassID() }
    }
}

#[allow(non_snake_case)]
impl IPersistFolder_Impl for FilterFolder_Impl {
    fn Initialize(&self, pidl: *const ITEMIDLIST) -> Result<()> {
        unsafe { self.inner.cast::<IPersistFolder2>()?.Initialize(pidl) }
    }
}

#[allow(non_snake_case)]
impl IPersistFolder2_Impl for FilterFolder_Impl {
    fn GetCurFolder(&self) -> Result<*mut ITEMIDLIST> {
        unsafe { self.inner.cast::<IPersistFolder2>()?.GetCurFolder() }
    }
}

#[allow(non_snake_case)]
impl IShellFolder_Impl for FilterFolder_Impl {
    fn ParseDisplayName(
        &self,
        hwnd: HWND,
        pbc: Option<&IBindCtx>,
        pszdisplayname: &PCWSTR,
        pcheaten: *const u32,
        ppidl: *mut *mut ITEMIDLIST,
        pdwattributes: *mut u32,
    ) -> Result<()> {
        unsafe {
            self.inner.ParseDisplayName(
                hwnd,
                pbc,
                *pszdisplayname,
                (!pcheaten.is_null()).then_some(pcheaten),
                ppidl,
                pdwattributes,
            )
        }
    }

    fn EnumObjects(
        &self,
        hwnd: HWND,
        grfflags: u32,
        ppenumidlist: *mut Option<IEnumIDList>,
    ) -> HRESULT {
        unsafe {
            let mut inner_enum: Option<IEnumIDList> = None;
            let hr = self.inner.EnumObjects(hwnd, grfflags, &mut inner_enum);
            match inner_enum {
                Some(e) if hr.is_ok() => {
                    let filt: IEnumIDList = FilterEnum {
                        inner: e,
                        folder: self.inner.clone(),
                    }
                    .into();
                    *ppenumidlist = Some(filt);
                    S_OK
                }
                _ => {
                    *ppenumidlist = None;
                    hr
                }
            }
        }
    }

    fn BindToObject(
        &self,
        pidl: *const ITEMIDLIST,
        pbc: Option<&IBindCtx>,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> Result<()> {
        unsafe {
            (self.inner.vtable().BindToObject)(
                self.inner.as_raw(),
                pidl,
                pbc.map_or(core::ptr::null_mut(), |b| b.as_raw()),
                riid,
                ppv,
            )
            .ok()
        }
    }

    fn BindToStorage(
        &self,
        pidl: *const ITEMIDLIST,
        pbc: Option<&IBindCtx>,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> Result<()> {
        unsafe {
            (self.inner.vtable().BindToStorage)(
                self.inner.as_raw(),
                pidl,
                pbc.map_or(core::ptr::null_mut(), |b| b.as_raw()),
                riid,
                ppv,
            )
            .ok()
        }
    }

    fn CompareIDs(
        &self,
        lparam: LPARAM,
        pidl1: *const ITEMIDLIST,
        pidl2: *const ITEMIDLIST,
    ) -> HRESULT {
        unsafe { self.inner.CompareIDs(lparam, pidl1, pidl2) }
    }

    fn CreateViewObject(
        &self,
        hwndowner: HWND,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> Result<()> {
        unsafe {
            (self.inner.vtable().CreateViewObject)(self.inner.as_raw(), hwndowner, riid, ppv).ok()
        }
    }

    fn GetAttributesOf(
        &self,
        cidl: u32,
        apidl: *const *const ITEMIDLIST,
        rgfinout: *mut u32,
    ) -> Result<()> {
        unsafe {
            let slice = std::slice::from_raw_parts(apidl, cidl as usize);
            self.inner.GetAttributesOf(slice, rgfinout)
        }
    }

    fn GetUIObjectOf(
        &self,
        hwndowner: HWND,
        cidl: u32,
        apidl: *const *const ITEMIDLIST,
        riid: *const GUID,
        rgfreserved: *const u32,
        ppv: *mut *mut c_void,
    ) -> Result<()> {
        unsafe {
            (self.inner.vtable().GetUIObjectOf)(
                self.inner.as_raw(),
                hwndowner,
                cidl,
                apidl,
                riid,
                rgfreserved,
                ppv,
            )
            .ok()
        }
    }

    fn GetDisplayNameOf(
        &self,
        pidl: *const ITEMIDLIST,
        uflags: SHGDNF,
        pname: *mut STRRET,
    ) -> Result<()> {
        unsafe { self.inner.GetDisplayNameOf(pidl, uflags, pname) }
    }

    fn SetNameOf(
        &self,
        hwnd: HWND,
        pidl: *const ITEMIDLIST,
        pszname: &PCWSTR,
        uflags: SHGDNF,
        ppidlout: *mut *mut ITEMIDLIST,
    ) -> Result<()> {
        unsafe {
            self.inner.SetNameOf(
                hwnd,
                pidl,
                *pszname,
                uflags,
                (!ppidlout.is_null()).then_some(ppidlout),
            )
        }
    }
}

#[implement(IEnumIDList)]
struct FilterEnum {
    inner: IEnumIDList,
    folder: IShellFolder,
}

#[allow(non_snake_case)]
impl IEnumIDList_Impl for FilterEnum_Impl {
    fn Next(&self, celt: u32, rgelt: *mut *mut ITEMIDLIST, pceltfetched: *mut u32) -> HRESULT {
        unsafe {
            let mut written = 0u32;
            while written < celt {
                let mut one: *mut ITEMIDLIST = core::ptr::null_mut();
                let mut got = 0u32;
                let hr = self
                    .inner
                    .Next(std::slice::from_raw_parts_mut(&mut one, 1), Some(&mut got));
                if hr != S_OK || got == 0 || one.is_null() {
                    break;
                }
                if is_junction(&self.folder, one) {
                    ILFree(Some(one));
                    continue;
                }
                *rgelt.add(written as usize) = one;
                written += 1;
            }
            if !pceltfetched.is_null() {
                *pceltfetched = written;
            }
            // S_OK only if we filled the whole request, else S_FALSE (HRESULT 1).
            if written == celt {
                S_OK
            } else {
                HRESULT(1)
            }
        }
    }

    fn Skip(&self, celt: u32) -> HRESULT {
        unsafe { self.inner.Skip(celt) }
    }

    fn Reset(&self) -> HRESULT {
        unsafe { self.inner.Reset() }
    }

    fn Clone(&self, ppenum: *mut Option<IEnumIDList>) -> HRESULT {
        unsafe {
            let mut c: Option<IEnumIDList> = None;
            let hr = self.inner.Clone(&mut c);
            match c {
                Some(inner_clone) if hr.is_ok() => {
                    let f: IEnumIDList = FilterEnum {
                        inner: inner_clone,
                        folder: self.folder.clone(),
                    }
                    .into();
                    *ppenum = Some(f);
                    S_OK
                }
                _ => {
                    *ppenum = None;
                    hr
                }
            }
        }
    }
}
