//! A thread that waits each shipped picture's decode on the host and then runs a wake, so
//! the consumer can block for whichever comes first, its next AU or the pixels, instead of
//! polling the timeline.

use super::DecodedVkFrame;
use ash::vk;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};

/// Bounds one wait: a wedged decode must not pin the thread, and `Drop` joins it.
const WAIT_NS: u64 = 50_000_000;

/// Waits decodes for [`Self::arm`]ed frames and wakes after each. Declare it before the
/// decoder that owns the device: `Drop` joins the thread, so the wait never outlives the
/// device. [`Self::settle`] keeps a wait from outliving its semaphore.
pub struct DoneWaiter {
    jobs: Option<mpsc::Sender<(vk::Semaphore, u64)>>,
    /// The wait in progress, for [`Self::settle`].
    busy: Arc<(Mutex<Option<vk::Semaphore>>, Condvar)>,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl DoneWaiter {
    /// `wake` runs on the waiter thread after each armed decode completes, or after
    /// 50 ms. `None` when the thread cannot start; the consumer then polls as before.
    pub fn spawn(device: ash::Device, wake: Arc<dyn Fn() + Send + Sync>) -> Option<Self> {
        let (jobs, rx) = mpsc::channel::<(vk::Semaphore, u64)>();
        let busy = Arc::new((Mutex::new(None), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (busy_t, stop_t) = (busy.clone(), stop.clone());
        let join = std::thread::Builder::new()
            .name("pf-decode-done".into())
            .spawn(move || {
                while let Ok((semaphore, value)) = rx.recv() {
                    if stop_t.load(Ordering::Acquire) {
                        break;
                    }
                    *busy_t.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(semaphore);
                    let semaphores = [semaphore];
                    let values = [value];
                    let info = vk::SemaphoreWaitInfo::default()
                        .semaphores(&semaphores)
                        .values(&values);
                    // SAFETY: `Drop` joins this thread before the owner drops the device,
                    // and `settle` holds every release of the semaphore until this returns.
                    let _ = unsafe { device.wait_semaphores(&info, WAIT_NS) };
                    *busy_t.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    busy_t.1.notify_all();
                    wake();
                }
            })
            .ok()?;
        Some(Self {
            jobs: Some(jobs),
            busy,
            stop,
            join: Some(join),
        })
    }

    /// Wake once `frame`'s decode is complete.
    pub fn arm(&self, frame: &DecodedVkFrame) {
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send((frame.semaphore, frame.value));
        }
    }

    /// Block while a wait on `frame`'s semaphore is in progress. Call before anything
    /// that can destroy it, such as the release of a pool's last frame.
    pub fn settle(&self, frame: &DecodedVkFrame) {
        let (lock, done) = &*self.busy;
        let mut busy = lock.lock().unwrap_or_else(|e| e.into_inner());
        while *busy == Some(frame.semaphore) {
            busy = done.wait(busy).unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl Drop for DoneWaiter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        drop(self.jobs.take());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
