//! A pool of single-threaded-apartment worker threads for shell COM calls.
//!
//! Shell objects (image factories, file operations) are apartment-threaded,
//! and some post window messages to their thread while they work, so each
//! worker initialises OLE and keeps pumping messages while it waits for jobs.

use std::collections::VecDeque;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Ole::OleInitialize;
use windows::Win32::System::Threading::{CreateSemaphoreW, INFINITE, ReleaseSemaphore};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE,
    PeekMessageW, QS_ALLINPUT, TranslateMessage,
};

use super::com::OwnedHandle;

type Job = Box<dyn FnOnce() + Send>;

struct Shared {
    queue: Mutex<VecDeque<Job>>,
    /// Counts queued jobs; each wait that succeeds claims one.
    ready: OwnedHandle,
}

/// Fixed-size pool of STA threads. Cheap to clone; threads live for the
/// rest of the process.
#[derive(Clone)]
pub struct StaPool {
    shared: Arc<Shared>,
}

impl StaPool {
    pub fn new(name: &str, threads: usize) -> io::Result<Self> {
        let ready =
            unsafe { CreateSemaphoreW(None, 0, i32::MAX, None) }.map_err(io::Error::from)?;
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            ready: OwnedHandle(ready),
        });
        for i in 0..threads.max(1) {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name(format!("{name}-{i}"))
                .spawn(move || worker(&shared))?;
        }
        Ok(Self { shared })
    }

    /// Run `job` on one of the pool's threads, after the jobs already
    /// queued. A panic in `job` is caught and does not take the thread down.
    pub fn spawn(&self, job: impl FnOnce() + Send + 'static) {
        self.push(Box::new(job), false);
    }

    /// Run `job` before anything already queued. For image requests, where
    /// the newest request is for what is on screen now.
    pub fn spawn_first(&self, job: impl FnOnce() + Send + 'static) {
        self.push(Box::new(job), true);
    }

    fn push(&self, job: Job, first: bool) {
        {
            let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            if first {
                queue.push_front(job);
            } else {
                queue.push_back(job);
            }
        }
        let _ = unsafe { ReleaseSemaphore(self.shared.ready.0, 1, None) };
    }
}

fn worker(shared: &Shared) {
    // The thread lives for the rest of the process, so OLE is never torn down.
    let _ = unsafe { OleInitialize(None) };
    loop {
        let handles = [shared.ready.0];
        let wait = unsafe {
            MsgWaitForMultipleObjectsEx(Some(&handles), INFINITE, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
        };
        if wait == WAIT_OBJECT_0 {
            let job = shared
                .queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front();
            if let Some(job) = job {
                let _ = catch_unwind(AssertUnwindSafe(job));
            }
        } else if wait == WAIT_TIMEOUT {
            continue;
        } else {
            pump_messages();
        }
    }
}

fn pump_messages() {
    let mut msg = MSG::default();
    while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn runs_jobs_on_sta_threads() {
        let pool = StaPool::new("test-sta", 2).unwrap();
        let (tx, rx) = mpsc::channel();
        for i in 0..8 {
            let tx = tx.clone();
            pool.spawn(move || {
                // CoInitializeEx with MTA fails on an STA thread, which proves
                // the worker's apartment.
                let mta = unsafe {
                    windows::Win32::System::Com::CoInitializeEx(
                        None,
                        windows::Win32::System::Com::COINIT_MULTITHREADED,
                    )
                };
                tx.send((i, mta.is_err())).unwrap();
            });
        }
        let mut seen: Vec<_> = (0..8).map(|_| rx.recv().unwrap()).collect();
        seen.sort();
        assert!(seen.iter().all(|(_, sta)| *sta));
        assert_eq!(seen.len(), 8);
    }

    #[test]
    fn survives_a_panicking_job() {
        let pool = StaPool::new("test-sta-panic", 1).unwrap();
        pool.spawn(|| panic!("boom"));
        let (tx, rx) = mpsc::channel();
        pool.spawn(move || tx.send(()).unwrap());
        rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    }
}
