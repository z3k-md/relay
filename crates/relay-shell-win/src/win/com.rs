//! Small helpers shared by the shell modules: wide strings, owned PIDLs,
//! and handles that may cross threads.

use std::ffi::OsStr;
use std::fmt;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::SHParseDisplayName;
use windows::core::PCWSTR;

/// A shell or Win32 failure, with the call that produced it.
#[derive(Debug)]
pub struct Error {
    pub context: &'static str,
    pub source: windows::core::Error,
}

impl Error {
    pub(crate) fn new(context: &'static str, source: windows::core::Error) -> Self {
        Self { context, source }
    }

    /// The HRESULT, for callers that branch on specific failures.
    pub fn code(&self) -> i32 {
        self.source.code().0
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.context, self.source.message())
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl From<Error> for std::io::Error {
    fn from(err: Error) -> Self {
        std::io::Error::other(err)
    }
}

pub(crate) trait Context<T> {
    fn ctx(self, context: &'static str) -> Result<T, Error>;
}

impl<T> Context<T> for windows::core::Result<T> {
    fn ctx(self, context: &'static str) -> Result<T, Error> {
        self.map_err(|err| Error::new(context, err))
    }
}

/// NUL-terminated UTF-16 for a path or string.
pub(crate) fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// UTF-16 up to the first NUL.
pub(crate) fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// An absolute PIDL owned by us, freed with `CoTaskMemFree`.
pub(crate) struct Pidl(*mut ITEMIDLIST);

impl Pidl {
    pub(crate) fn parse(path: &Path) -> Result<Self, Error> {
        let name = wide(path);
        let mut pidl = std::ptr::null_mut();
        unsafe { SHParseDisplayName(PCWSTR(name.as_ptr()), None, &mut pidl, 0, None) }
            .ctx("SHParseDisplayName")?;
        Ok(Self(pidl))
    }

    pub(crate) fn as_ptr(&self) -> *const ITEMIDLIST {
        self.0
    }
}

impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0 as *const _)) };
    }
}

/// A kernel handle that is closed on drop and may move between threads.
pub(crate) struct OwnedHandle(pub(crate) HANDLE);

// SAFETY: kernel handles are process-wide and usable from any thread.
unsafe impl Send for OwnedHandle {}
// SAFETY: as above; the wrapped calls (SetEvent, waits) are thread-safe.
unsafe impl Sync for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// Window handles cross the API as `isize` so callers need not share our
/// `windows` crate version.
pub(crate) fn hwnd(raw: isize) -> HWND {
    HWND(raw as *mut _)
}
