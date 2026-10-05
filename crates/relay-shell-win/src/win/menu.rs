//! The real Explorer context menu for items or a folder background, shown as
//! a native popup over our window.
//!
//! Must run on the thread that owns `hwnd` (Tauri's main thread). While the
//! menu is open the window is subclassed so owner-drawn and lazily filled
//! submenus (Send to, Open with, many extensions) get their messages through
//! `IContextMenu2/3::HandleMenuMsg`.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    CMF_CANRENAME, CMF_EXTENDEDVERBS, CMF_NORMAL, CMIC_MASK_PTINVOKE, CMINVOKECOMMANDINFO,
    CMINVOKECOMMANDINFOEX, DefSubclassProc, GCS_VERBW, IContextMenu, IContextMenu2, IContextMenu3,
    ILFindLastID, IShellFolder, RemoveWindowSubclass, SHBindToParent, SHGetDesktopFolder,
    SetWindowSubclass,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx,
    WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR,
};
use windows::core::{Interface, PCSTR, PCWSTR, PSTR};

use super::com::{Context, Error, Pidl, hwnd as to_hwnd, wide};

pub enum Target<'a> {
    /// Selected items; they must share one parent folder.
    Items(&'a [PathBuf]),
    /// Empty space in a folder: View, Sort, New, Properties of the folder.
    Background(&'a Path),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Canonical verb of the chosen command, when the handler reports one.
    pub verb: Option<String>,
    /// False when the menu was dismissed or we left the command to the app
    /// (rename needs our own in-place editor).
    pub invoked: bool,
}

/// Not in the metadata: `CMIC_MASK_UNICODE` from shobjidl_core.h.
const CMIC_MASK_UNICODE: u32 = 0x0000_4000;
const FIRST_ID: u32 = 1;
const LAST_ID: u32 = 0x7FFF;
const SUBCLASS_ID: usize = 0x5245_4C59; // "RELY"

thread_local! {
    static ACTIVE: RefCell<Option<IContextMenu>> = const { RefCell::new(None) };
}

/// Show the menu at screen coordinates and run the chosen command.
pub fn show(
    owner: isize,
    target: Target<'_>,
    screen_x: i32,
    screen_y: i32,
    extended: bool,
) -> Result<Outcome, Error> {
    let hwnd = to_hwnd(owner);
    let (menu, directory) = match target {
        Target::Items(paths) => (items_menu(hwnd, paths)?, None),
        Target::Background(dir) => (background_menu(hwnd, dir)?, Some(wide(dir))),
    };

    let popup = unsafe { CreatePopupMenu() }.ctx("CreatePopupMenu")?;
    let mut flags = CMF_NORMAL | CMF_CANRENAME;
    if extended {
        flags |= CMF_EXTENDEDVERBS;
    }
    let outcome = (|| {
        unsafe { menu.QueryContextMenu(popup, 0, FIRST_ID, LAST_ID, flags) }
            .ok()
            .ctx("IContextMenu::QueryContextMenu")?;

        ACTIVE.with(|a| *a.borrow_mut() = Some(menu.clone()));
        let _ = unsafe { SetWindowSubclass(hwnd, Some(forward_menu_messages), SUBCLASS_ID, 0) };
        let chosen = unsafe {
            TrackPopupMenuEx(
                popup,
                (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
                screen_x,
                screen_y,
                hwnd,
                None,
            )
        }
        .0 as u32;
        let _ = unsafe { RemoveWindowSubclass(hwnd, Some(forward_menu_messages), SUBCLASS_ID) };
        ACTIVE.with(|a| a.borrow_mut().take());

        if chosen < FIRST_ID {
            return Ok(Outcome::default());
        }
        let offset = (chosen - FIRST_ID) as usize;
        let verb = verb_of(&menu, offset);
        if verb.as_deref() == Some("rename") {
            return Ok(Outcome {
                verb,
                invoked: false,
            });
        }
        invoke(
            &menu,
            hwnd,
            offset,
            directory.as_deref(),
            screen_x,
            screen_y,
        )?;
        Ok(Outcome {
            verb,
            invoked: true,
        })
    })();
    let _ = unsafe { DestroyMenu(popup) };
    outcome
}

fn items_menu(hwnd: HWND, paths: &[PathBuf]) -> Result<IContextMenu, Error> {
    if paths.is_empty() {
        return Err(Error::new("context menu", windows::core::Error::empty()));
    }
    let pidls = paths
        .iter()
        .map(|p| Pidl::parse(p))
        .collect::<Result<Vec<_>, _>>()?;
    let mut last: *mut ITEMIDLIST = std::ptr::null_mut();
    let parent: IShellFolder =
        unsafe { SHBindToParent(pidls[0].as_ptr(), Some(&mut last)) }.ctx("SHBindToParent")?;
    let children: Vec<*const ITEMIDLIST> = pidls
        .iter()
        .map(|p| unsafe { ILFindLastID(p.as_ptr()) } as *const ITEMIDLIST)
        .collect();
    unsafe { parent.GetUIObjectOf(hwnd, &children, None) }.ctx("IShellFolder::GetUIObjectOf")
}

fn background_menu(hwnd: HWND, dir: &Path) -> Result<IContextMenu, Error> {
    let pidl = Pidl::parse(dir)?;
    let desktop = unsafe { SHGetDesktopFolder() }.ctx("SHGetDesktopFolder")?;
    let folder: IShellFolder =
        unsafe { desktop.BindToObject(pidl.as_ptr(), None) }.ctx("IShellFolder::BindToObject")?;
    unsafe { folder.CreateViewObject(hwnd) }.ctx("IShellFolder::CreateViewObject")
}

fn verb_of(menu: &IContextMenu, offset: usize) -> Option<String> {
    let mut buf = [0u16; 256];
    unsafe {
        menu.GetCommandString(
            offset,
            GCS_VERBW,
            None,
            PSTR(buf.as_mut_ptr().cast()),
            buf.len() as u32,
        )
    }
    .ok()?;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let verb = String::from_utf16_lossy(&buf[..len]);
    (!verb.is_empty()).then_some(verb)
}

fn invoke(
    menu: &IContextMenu,
    hwnd: HWND,
    offset: usize,
    directory: Option<&[u16]>,
    x: i32,
    y: i32,
) -> Result<(), Error> {
    let info = CMINVOKECOMMANDINFOEX {
        cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE,
        hwnd,
        // MAKEINTRESOURCE: the command offset in the low word.
        lpVerb: PCSTR(offset as *const u8),
        lpVerbW: PCWSTR(offset as *const u16),
        lpDirectoryW: directory.map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())),
        nShow: SW_SHOWNORMAL.0,
        ptInvoke: POINT { x, y },
        ..Default::default()
    };
    unsafe {
        menu.InvokeCommand((&info as *const CMINVOKECOMMANDINFOEX).cast::<CMINVOKECOMMANDINFO>())
    }
    .ctx("IContextMenu::InvokeCommand")
}

unsafe extern "system" fn forward_menu_messages(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if matches!(
        msg,
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
    ) {
        let menu = ACTIVE.with(|a| a.borrow().clone());
        if let Some(menu) = menu {
            if let Ok(menu3) = menu.cast::<IContextMenu3>() {
                let mut result = LRESULT(0);
                if unsafe { menu3.HandleMenuMsg2(msg, wparam, lparam, Some(&mut result)) }.is_ok() {
                    return result;
                }
            } else if let Ok(menu2) = menu.cast::<IContextMenu2>()
                && unsafe { menu2.HandleMenuMsg(msg, wparam, lparam) }.is_ok()
            {
                return LRESULT(0);
            }
        }
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}
