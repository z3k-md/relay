//! Copy and move through `IFileOperation`, the shell's own copy engine: native
//! progress and conflict dialogs, Recycle Bin, undo, and UAC elevation for
//! protected folders. Must run on an STA thread.

use std::path::{Path, PathBuf};

use windows::Win32::System::Com::{CLSCTX_ALL, CLSCTX_LOCAL_SERVER, CoCreateInstance};
use windows::Win32::UI::Shell::{
    FOF_ALLOWUNDO, FOF_NOCONFIRMMKDIR, FileOperation, IFileOperation, IShellItem,
    SHCreateItemFromParsingName,
};
use windows::core::PCWSTR;

use super::com::{Context, Error, hwnd as to_hwnd, wide};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transfer {
    Copy,
    Move,
}

/// Copy or move `sources` into the folder `dest`. Returns false when the user
/// cancelled part of it.
pub fn transfer(
    owner: isize,
    sources: &[PathBuf],
    dest: &Path,
    kind: Transfer,
) -> Result<bool, Error> {
    // Out of process first: the UAC prompt then opens in front of our window
    // (the same choice Files makes).
    let op: IFileOperation = unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_LOCAL_SERVER) }
        .or_else(|_| unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_ALL) })
        .ctx("CoCreateInstance(FileOperation)")?;
    unsafe {
        op.SetOwnerWindow(to_hwnd(owner)).ctx("SetOwnerWindow")?;
        op.SetOperationFlags(FOF_ALLOWUNDO | FOF_NOCONFIRMMKDIR)
            .ctx("SetOperationFlags")?;
    }
    let dest = item(dest)?;
    for source in sources {
        let source = item(source)?;
        unsafe {
            match kind {
                Transfer::Copy => op.CopyItem(&source, &dest, PCWSTR::null(), None),
                Transfer::Move => op.MoveItem(&source, &dest, PCWSTR::null(), None),
            }
        }
        .ctx("queue file operation")?;
    }
    unsafe { op.PerformOperations() }.ctx("PerformOperations")?;
    let aborted = unsafe { op.GetAnyOperationsAborted() }.ctx("GetAnyOperationsAborted")?;
    Ok(!aborted.as_bool())
}

fn item(path: &Path) -> Result<IShellItem, Error> {
    let name = wide(path);
    unsafe { SHCreateItemFromParsingName(PCWSTR(name.as_ptr()), None) }
        .ctx("SHCreateItemFromParsingName")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::sta::StaPool;
    use std::sync::mpsc;

    #[test]
    fn copies_and_moves() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("a.txt"), b"a").unwrap();
        std::fs::write(src.join("b.txt"), b"b").unwrap();

        let pool = StaPool::new("test-fileop", 1).unwrap();
        let (tx, rx) = mpsc::channel();
        let (a, b, d) = (src.join("a.txt"), src.join("b.txt"), dst.clone());
        pool.spawn(move || {
            let copied = transfer(0, &[a], &d, Transfer::Copy);
            let moved = transfer(0, &[b], &d, Transfer::Move);
            tx.send((
                copied.map_err(|e| e.to_string()),
                moved.map_err(|e| e.to_string()),
            ))
            .unwrap();
        });
        let (copied, moved) = rx.recv_timeout(std::time::Duration::from_secs(60)).unwrap();
        assert_eq!(copied, Ok(true));
        assert_eq!(moved, Ok(true));
        assert!(src.join("a.txt").exists());
        assert!(dst.join("a.txt").exists());
        assert!(!src.join("b.txt").exists());
        assert!(dst.join("b.txt").exists());
    }
}
