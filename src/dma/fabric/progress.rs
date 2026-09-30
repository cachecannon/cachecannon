use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;

use ofi_libfabric_sys::bindgen::{fi_cq_entry, fi_cq_read, fid_cq};

struct SendCompletionQueue(*mut fid_cq);

// SAFETY: `ProgressDriver` joins its thread in `Drop`, and the driver is declared before the
// endpoint that owns the queue, so the poller is gone before `fi_close` runs on it. Nothing but the
// poller reads the queue.
unsafe impl Send for SendCompletionQueue {}
unsafe impl Sync for SendCompletionQueue {}

struct ProgressShared {
    queue: SendCompletionQueue,
    /// Transfers in flight. The hot path: only `drive` and guard-drop touch it.
    active: AtomicUsize,
    shutdown: AtomicBool,
    /// Blocks the poller while idle. Never held across a poll pass.
    park: Mutex<()>,
    wakeup: Condvar,
}

/// Polls one completion queue while transfers are outstanding, as `FI_PROGRESS_MANUAL` requires of
/// a passive target. Parks when nothing is in flight; joined on drop.
pub(crate) struct ProgressDriver {
    shared: Arc<ProgressShared>,
    handle: Option<JoinHandle<()>>,
}

impl ProgressDriver {
    pub(crate) fn new(queue: *mut fid_cq) -> Self {
        let shared = Arc::new(ProgressShared {
            queue: SendCompletionQueue(queue),
            active: AtomicUsize::new(0),
            shutdown: AtomicBool::new(false),
            park: Mutex::new(()),
            wakeup: Condvar::new(),
        });
        let polled = shared.clone();
        Self {
            handle: Some(std::thread::spawn(move || poll_loop(&polled))),
            shared,
        }
    }

    /// Poll until the returned guard drops. Lock-free but for waking a parked poller.
    pub(crate) fn drive(&self) -> ProgressGuard {
        if self.shared.active.fetch_add(1, Ordering::AcqRel) == 0 {
            // Take `park` so this notify can't land between the poller's check and its wait.
            let _parked = self
                .shared
                .park
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            self.shared.wakeup.notify_one();
        }
        ProgressGuard {
            shared: self.shared.clone(),
        }
    }
}

impl Drop for ProgressDriver {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        {
            let _parked = self
                .shared
                .park
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            self.shared.wakeup.notify_one();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn poll_loop(shared: &ProgressShared) {
    loop {
        {
            let mut idle = shared.park.lock().unwrap_or_else(PoisonError::into_inner);
            while shared.active.load(Ordering::Acquire) == 0
                && !shared.shutdown.load(Ordering::Acquire)
            {
                idle = shared
                    .wakeup
                    .wait(idle)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
        if shared.shutdown.load(Ordering::Acquire) {
            return;
        }

        // Drain fully. EFA's rxr provider emulates RMA with a software-segmented protocol that
        // advances only while the target polls, so pacing this with a sleep throttles large
        // transfers.
        loop {
            let mut entry = fi_cq_entry {
                op_context: std::ptr::null_mut(),
            };
            let read =
                unsafe { fi_cq_read(shared.queue.0, std::ptr::from_mut(&mut entry).cast(), 1) };
            if read <= 0 {
                break;
            }
        }
        std::hint::spin_loop();
    }
}

/// Keeps the progress driver polling while alive.
pub struct ProgressGuard {
    shared: Arc<ProgressShared>,
}

impl Drop for ProgressGuard {
    fn drop(&mut self) {
        // At 0 the poller parks itself next pass.
        self.shared.active.fetch_sub(1, Ordering::AcqRel);
    }
}

impl fmt::Debug for ProgressDriver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProgressDriver")
            .field("active", &self.shared.active.load(Ordering::Relaxed))
            .finish()
    }
}

impl fmt::Debug for ProgressGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("ProgressGuard").finish()
    }
}
