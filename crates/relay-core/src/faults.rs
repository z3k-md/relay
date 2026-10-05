//! Test seam for injected I/O failures.
//!
//! Production code never installs a hook, so [`check`] costs one thread-local
//! read. A deterministic simulator installs one on its own thread to make a
//! write, rename or object install fail at a chosen moment, or to stand in for
//! a crash between a temp file landing and its rename. The hook is
//! thread-local because the simulator runs every node on one thread; work
//! on other threads is never faulted.

use std::cell::RefCell;
use std::io;
use std::path::Path;

/// Where in an I/O sequence a hook is consulted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FaultPoint {
    /// Before the temp file of a working-tree write is created.
    MaterializeWrite,
    /// After the temp file is complete and synced, before the rename.
    MaterializeRename,
    /// Before an object is installed into the content store.
    StorePut,
}

pub type FaultHook = Box<dyn FnMut(FaultPoint, &Path) -> Option<io::Error>>;

thread_local! {
    static HOOK: RefCell<Option<FaultHook>> = const { RefCell::new(None) };
}

/// Install `hook` for the current thread, replacing any earlier one.
pub fn install(hook: FaultHook) {
    HOOK.with(|h| *h.borrow_mut() = Some(hook));
}

/// Remove the current thread's hook.
pub fn clear() {
    HOOK.with(|h| *h.borrow_mut() = None);
}

/// Consult the hook, if any. Returns the injected error for `point` at `path`.
pub fn check(point: FaultPoint, path: &Path) -> io::Result<()> {
    HOOK.with(|h| {
        let mut hook = h.borrow_mut();
        match hook.as_mut() {
            None => Ok(()),
            Some(hook) => match hook(point, path) {
                None => Ok(()),
                Some(err) => Err(err),
            },
        }
    })
}
