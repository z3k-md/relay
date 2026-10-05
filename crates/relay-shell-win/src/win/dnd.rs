//! OLE drag and drop for a WebView2 window: our own drop target in place of
//! WebView2's (and wry's), plus shell drags out of the window.
//!
//! WebView2 only hands web content plain `File` objects, without paths, and
//! never sees virtual files (Outlook attachments, zip entries, RDP clipboard).
//! Registering our own `IDropTarget` on the WebView2 child windows gives us
//! the full `IDataObject`, as wry does for its path-only drop events.
//!
//! Everything here runs on the window's thread (Tauri's main thread), which is
//! an OLE STA.

use std::cell::{Cell, RefCell};
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use windows::Win32::Foundation::{DRAGDROP_E_INVALIDHWND, HWND, LPARAM, POINT, POINTL, S_OK};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, DATADIR_GET, DVASPECT_CONTENT,
    FORMATETC, IDataObject, STGMEDIUM, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::{GetClipboardFormatNameW, RegisterClipboardFormatW};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::MK_ALT;
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE,
    IDropSource, IDropTarget, IDropTarget_Impl, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop,
};
use windows::Win32::System::SystemServices::{MK_CONTROL, MK_SHIFT, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    CLSID_DragDropHelper, DragQueryFileW, FD_ATTRIBUTES, FD_FILESIZE, FILEDESCRIPTORW,
    FILEGROUPDESCRIPTORW, HDROP, IDropTargetHelper, ILFindLastID, IShellFolder, SHBindToParent,
    SHDoDragDrop,
};
use windows::Win32::UI::WindowsAndMessaging::EnumChildWindows;
use windows::core::{BOOL, PCWSTR, Ref, implement};

use super::com::{Context, Error, Pidl, from_wide, hwnd as to_hwnd, wide};

/// Drop effects, as OLE's `DROPEFFECT` bits.
pub const EFFECT_NONE: u32 = DROPEFFECT_NONE.0;
pub const EFFECT_COPY: u32 = DROPEFFECT_COPY.0;
pub const EFFECT_MOVE: u32 = DROPEFFECT_MOVE.0;
pub const EFFECT_LINK: u32 = DROPEFFECT_LINK.0;

/// Where and how a drag is hovering.
#[derive(Debug, Clone)]
pub struct DragState {
    /// Position in the top-level window's client area, physical pixels.
    pub client_x: i32,
    pub client_y: i32,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    /// Effects the source allows (`EFFECT_*` bits).
    pub allowed: u32,
}

/// Receives drag events. Runs on the window thread.
pub trait DropHandler {
    /// The drag entered (`enter`) or moved. Return the effect to show.
    fn over(&self, state: &DragState, data: &DropData<'_>, enter: bool) -> u32;
    fn leave(&self);
    /// The user dropped. Return the effect performed.
    fn dropped(&self, state: &DragState, data: &DropData<'_>) -> u32;
}

/// A file the source offers by content rather than by path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualFile {
    /// Relative name; may contain `\` for nested items.
    pub name: String,
    pub size: Option<u64>,
    pub is_dir: bool,
}

/// Read access to the dragged data object for the duration of a callback.
pub struct DropData<'a> {
    object: &'a IDataObject,
}

impl DropData<'_> {
    /// Real file system paths (`CF_HDROP`).
    pub fn paths(&self) -> Vec<PathBuf> {
        let format = formatetc(CF_HDROP.0, -1, TYMED_HGLOBAL.0 as u32);
        let Ok(mut medium) = (unsafe { self.object.GetData(&format) }) else {
            return Vec::new();
        };
        let hdrop = HDROP(unsafe { medium.u.hGlobal }.0);
        let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
        let mut paths = Vec::with_capacity(count as usize);
        for i in 0..count {
            let len = unsafe { DragQueryFileW(hdrop, i, None) } as usize;
            let mut buf = vec![0u16; len + 1];
            unsafe { DragQueryFileW(hdrop, i, Some(&mut buf)) };
            paths.push(PathBuf::from(from_wide(&buf)));
        }
        unsafe { ReleaseStgMedium(&mut medium) };
        paths
    }

    /// Files offered as `FileGroupDescriptorW` + `FileContents`.
    pub fn virtual_files(&self) -> Vec<VirtualFile> {
        let cf = registered_format("FileGroupDescriptorW");
        let format = formatetc(cf, -1, TYMED_HGLOBAL.0 as u32);
        let Ok(mut medium) = (unsafe { self.object.GetData(&format) }) else {
            return Vec::new();
        };
        let mut files = Vec::new();
        unsafe {
            let global = medium.u.hGlobal;
            let base = GlobalLock(global) as *const FILEGROUPDESCRIPTORW;
            if !base.is_null() {
                let available = GlobalSize(global);
                let count = (*base).cItems as usize;
                let first = std::ptr::addr_of!((*base).fgd) as *const FILEDESCRIPTORW;
                let header = std::mem::size_of::<u32>();
                let fits =
                    (available.saturating_sub(header)) / std::mem::size_of::<FILEDESCRIPTORW>();
                for i in 0..count.min(fits) {
                    // Packed struct: copy it out rather than borrow fields.
                    let d = std::ptr::read_unaligned(first.add(i));
                    let flags = d.dwFlags;
                    let attributes = d.dwFileAttributes;
                    let name = d.cFileName;
                    let is_dir = flags & FD_ATTRIBUTES.0 as u32 != 0 && attributes & 0x10 != 0;
                    let size = (flags & FD_FILESIZE.0 as u32 != 0)
                        .then(|| (u64::from(d.nFileSizeHigh) << 32) | u64::from(d.nFileSizeLow));
                    files.push(VirtualFile {
                        name: from_wide(&name),
                        size,
                        is_dir,
                    });
                }
                let _ = GlobalUnlock(global);
            }
            ReleaseStgMedium(&mut medium);
        }
        files
    }

    /// Write the virtual files into `dir`, never overwriting. Returns the
    /// paths written. Must be called during the drop: the source may free
    /// the data as soon as the drop returns.
    pub fn save_virtual_files(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        let contents = registered_format("FileContents");
        let mut written = Vec::new();
        for (index, file) in self.virtual_files().into_iter().enumerate() {
            let Some(relative) = safe_relative(&file.name) else {
                continue;
            };
            if file.is_dir {
                fs::create_dir_all(dir.join(&relative))?;
                continue;
            }
            let target = unique_path(&dir.join(&relative));
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let format = formatetc(
                contents,
                index as i32,
                (TYMED_ISTREAM.0 | TYMED_HGLOBAL.0) as u32,
            );
            let mut medium = unsafe { self.object.GetData(&format) }.map_err(io::Error::from)?;
            let result = write_medium(&medium, &target);
            unsafe { ReleaseStgMedium(&mut medium) };
            result?;
            written.push(target);
        }
        Ok(written)
    }

    /// Clipboard format names on offer, for diagnostics.
    pub fn formats(&self) -> Vec<String> {
        let Ok(list) = (unsafe { self.object.EnumFormatEtc(DATADIR_GET.0 as u32) }) else {
            return Vec::new();
        };
        let mut names = Vec::new();
        loop {
            let mut item = [FORMATETC::default()];
            let mut fetched = 0u32;
            if unsafe { list.Next(&mut item, Some(&mut fetched)) } != S_OK || fetched == 0 {
                break;
            }
            if !item[0].ptd.is_null() {
                unsafe { CoTaskMemFree(Some(item[0].ptd as *const _)) };
            }
            let name = format_name(item[0].cfFormat);
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }
}

fn write_medium(medium: &STGMEDIUM, target: &Path) -> io::Result<()> {
    let mut out = fs::File::create(target)?;
    if medium.tymed == TYMED_ISTREAM.0 as u32 {
        let stream = unsafe { &*medium.u.pstm };
        let Some(stream) = stream.as_ref() else {
            return Err(io::Error::other("empty stream"));
        };
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let mut read = 0u32;
            let hr =
                unsafe { stream.Read(buf.as_mut_ptr().cast(), buf.len() as u32, Some(&mut read)) };
            if hr.is_err() {
                return Err(io::Error::from(windows::core::Error::from(hr)));
            }
            if read == 0 {
                break;
            }
            out.write_all(&buf[..read as usize])?;
        }
    } else {
        unsafe {
            let global = medium.u.hGlobal;
            let size = GlobalSize(global);
            let ptr = GlobalLock(global) as *const u8;
            if ptr.is_null() {
                return Err(io::Error::other("GlobalLock failed"));
            }
            let bytes = std::slice::from_raw_parts(ptr, size);
            let result = out.write_all(bytes);
            let _ = GlobalUnlock(global);
            result?;
        }
    }
    out.flush()
}

/// Keep descriptor names inside the drop folder: no roots, no `..`.
fn safe_relative(name: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in Path::new(name).components() {
        match part {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// `name.ext`, then `name (2).ext`, `name (3).ext`, …
fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (2..)
        .map(|n| path.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap_or_else(|| path.to_path_buf())
}

fn formatetc(cf: u16, index: i32, tymed: u32) -> FORMATETC {
    FORMATETC {
        cfFormat: cf,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: index,
        tymed,
    }
}

fn registered_format(name: &str) -> u16 {
    let name = wide(name);
    unsafe { RegisterClipboardFormatW(PCWSTR(name.as_ptr())) as u16 }
}

fn format_name(cf: u16) -> String {
    const STANDARD: &[(u16, &str)] = &[
        (1, "CF_TEXT"),
        (2, "CF_BITMAP"),
        (7, "CF_OEMTEXT"),
        (8, "CF_DIB"),
        (13, "CF_UNICODETEXT"),
        (15, "CF_HDROP"),
        (16, "CF_LOCALE"),
        (17, "CF_DIBV5"),
    ];
    if let Some((_, name)) = STANDARD.iter().find(|(id, _)| *id == cf) {
        return (*name).to_string();
    }
    let mut buf = [0u16; 128];
    let len = unsafe { GetClipboardFormatNameW(u32::from(cf), &mut buf) };
    if len > 0 {
        String::from_utf16_lossy(&buf[..len as usize])
    } else {
        format!("#{cf}")
    }
}

/// Keeps our drop target registered on a window's WebView2 children until
/// dropped. Not `Send`: keep it on the window thread.
pub struct Registration {
    windows: Vec<HWND>,
    _target: IDropTarget,
}

impl Drop for Registration {
    fn drop(&mut self) {
        for hwnd in &self.windows {
            let _ = unsafe { RevokeDragDrop(*hwnd) };
        }
    }
}

/// Replace the drop targets on every child of `top_level` (the WebView2
/// host windows) with one that reports to `handler`. Call after the webview
/// has loaded, when its child windows exist.
pub fn register(top_level: isize, handler: Rc<dyn DropHandler>) -> Result<Registration, Error> {
    let top = to_hwnd(top_level);
    let helper: Option<IDropTargetHelper> =
        unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER) }.ok();
    let target: IDropTarget = Target {
        top,
        handler,
        helper,
        current: RefCell::new(None),
        effect: Cell::new(EFFECT_NONE),
    }
    .into();

    let mut children: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumChildWindows(
            Some(top),
            Some(collect_child),
            LPARAM((&mut children as *mut Vec<HWND>) as isize),
        );
    }
    let mut registered = Vec::new();
    for child in children {
        if unsafe { RevokeDragDrop(child) } == Err(DRAGDROP_E_INVALIDHWND.into()) {
            continue;
        }
        if unsafe { RegisterDragDrop(child, &target) }.is_ok() {
            registered.push(child);
        }
    }
    if registered.is_empty() {
        return Err(Error::new(
            "RegisterDragDrop",
            windows::core::Error::new(DRAGDROP_E_INVALIDHWND, "no WebView2 child windows yet"),
        ));
    }
    Ok(Registration {
        windows: registered,
        _target: target,
    })
}

unsafe extern "system" fn collect_child(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let children = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    children.push(hwnd);
    true.into()
}

#[implement(IDropTarget)]
struct Target {
    top: HWND,
    handler: Rc<dyn DropHandler>,
    helper: Option<IDropTargetHelper>,
    current: RefCell<Option<IDataObject>>,
    effect: Cell<u32>,
}

impl Target {
    fn state(&self, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, allowed: u32) -> DragState {
        let mut client = POINT { x: pt.x, y: pt.y };
        let _ = unsafe { ScreenToClient(self.top, &mut client) };
        DragState {
            client_x: client.x,
            client_y: client.y,
            ctrl: keys.0 & MK_CONTROL.0 != 0,
            shift: keys.0 & MK_SHIFT.0 != 0,
            alt: keys.0 & MK_ALT != 0,
            allowed,
        }
    }

    fn hover(&self, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT, enter: bool) {
        let allowed = unsafe { effect.as_ref() }.map_or(0, |e| e.0);
        let chosen = match self.current.borrow().as_ref() {
            Some(object) => {
                let state = self.state(keys, pt, allowed);
                self.handler.over(&state, &DropData { object }, enter) & allowed
            }
            None => EFFECT_NONE,
        };
        self.effect.set(chosen);
        if let Some(effect) = unsafe { effect.as_mut() } {
            *effect = DROPEFFECT(chosen);
        }
    }
}

#[allow(non_snake_case)]
impl IDropTarget_Impl for Target_Impl {
    fn DragEnter(
        &self,
        data: Ref<'_, IDataObject>,
        keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        *self.current.borrow_mut() = data.as_ref().cloned();
        self.hover(keys, pt, effect, true);
        if let (Some(helper), Some(object)) = (&self.helper, data.as_ref()) {
            let point = POINT { x: pt.x, y: pt.y };
            let _ = unsafe {
                helper.DragEnter(self.top, object, &point, DROPEFFECT(self.effect.get()))
            };
        }
        Ok(())
    }

    fn DragOver(
        &self,
        keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        self.hover(keys, pt, effect, false);
        if let Some(helper) = &self.helper {
            let point = POINT { x: pt.x, y: pt.y };
            let _ = unsafe { helper.DragOver(&point, DROPEFFECT(self.effect.get())) };
        }
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        if let Some(helper) = &self.helper {
            let _ = unsafe { helper.DragLeave() };
        }
        self.current.borrow_mut().take();
        self.handler.leave();
        Ok(())
    }

    fn Drop(
        &self,
        data: Ref<'_, IDataObject>,
        keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let allowed = unsafe { effect.as_ref() }.map_or(0, |e| e.0);
        let performed = match data.as_ref() {
            Some(object) => {
                let state = self.state(keys, pt, allowed);
                self.handler.dropped(&state, &DropData { object }) & allowed
            }
            None => EFFECT_NONE,
        };
        if let (Some(helper), Some(object)) = (&self.helper, data.as_ref()) {
            let point = POINT { x: pt.x, y: pt.y };
            let _ = unsafe { helper.Drop(object, &point, DROPEFFECT(performed)) };
        }
        self.current.borrow_mut().take();
        if let Some(effect) = unsafe { effect.as_mut() } {
            *effect = DROPEFFECT(performed);
        }
        Ok(())
    }
}

/// Start a shell drag of `paths` (which share a parent folder) from our
/// window. Blocks in OLE's modal drag loop until the drop or cancel, and
/// returns the effect the target reported. Call while the mouse button is
/// still down.
pub fn start_drag(owner: isize, paths: &[PathBuf], allowed: u32) -> Result<u32, Error> {
    if paths.is_empty() {
        return Ok(EFFECT_NONE);
    }
    let hwnd = to_hwnd(owner);
    let pidls = paths
        .iter()
        .map(|p| Pidl::parse(p))
        .collect::<Result<Vec<_>, _>>()?;
    let parent: IShellFolder =
        unsafe { SHBindToParent(pidls[0].as_ptr(), None) }.ctx("SHBindToParent")?;
    let children: Vec<*const ITEMIDLIST> = pidls
        .iter()
        .map(|p| unsafe { ILFindLastID(p.as_ptr()) } as *const ITEMIDLIST)
        .collect();
    let object: IDataObject = unsafe { parent.GetUIObjectOf(hwnd, &children, None) }
        .ctx("IShellFolder::GetUIObjectOf(IDataObject)")?;
    let performed = unsafe {
        SHDoDragDrop(
            Some(hwnd),
            &object,
            None::<&IDropSource>,
            DROPEFFECT(allowed),
        )
    }
    .ctx("SHDoDragDrop")?;
    Ok(performed.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_names_stay_inside() {
        assert_eq!(safe_relative("a.txt"), Some(PathBuf::from("a.txt")));
        assert_eq!(
            safe_relative("dir\\a.txt"),
            Some(PathBuf::from("dir\\a.txt"))
        );
        assert_eq!(safe_relative("..\\evil.txt"), None);
        assert_eq!(safe_relative("C:\\evil.txt"), None);
        assert_eq!(safe_relative("\\evil.txt"), None);
        assert_eq!(safe_relative(""), None);
    }

    #[test]
    fn unique_names() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        assert_eq!(unique_path(&p), p);
        fs::write(&p, b"").unwrap();
        assert_eq!(unique_path(&p), dir.path().join("a (2).txt"));
    }
}
