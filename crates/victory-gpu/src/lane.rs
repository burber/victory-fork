//! The host queue's work sent from a thread of its own
//! (`VICTORY_SEND_THREAD=1`; off by default).
//!
//! `ExecuteCommandLists`, `Signal` and `Present` cost the submitting thread
//! some 3 ms a frame in the driver (run 162: `ExecuteCommandLists` 2.65 ms a
//! frame of a thread that limits the frame). With the switch the hot ones are
//! handed to one thread that does them in the order they were handed over, so
//! the card sees exactly the order it saw before, and the submitting thread
//! goes on preparing meanwhile.
//!
//! Every other use of the queue goes through [`Lane::raw`], which first waits
//! until everything handed over has been done: those calls keep their place
//! in the order as they had it, at the cost of that wait (they are rare).
//! A fence value signalled through the lane is reached only once the thread
//! has signalled it, so waits for it are as before; nothing waits for the
//! thread itself but [`Lane::raw`] and [`Lane::drain`].

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::{ID3D12CommandList, ID3D12CommandQueue, ID3D12Fence};
use windows::Win32::Graphics::Dxgi::{DXGI_PRESENT, IDXGISwapChain3};

/// Whether the hot queue calls are sent from a thread of their own.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SEND_THREAD").is_some_and(|v| v == "1"))
}

enum Job {
    Execute(Vec<Option<ID3D12CommandList>>),
    Signal(ID3D12Fence, u64),
    Present(IDXGISwapChain3),
}
// SAFETY: D3D12 and DXGI objects are free-threaded.
unsafe impl Send for Job {}

struct State {
    jobs: VecDeque<Job>,
    /// A job is being done (taken off `jobs`, not finished).
    busy: bool,
}

/// The queue, and the jobs handed over for it.
pub(crate) struct Lane {
    queue: ID3D12CommandQueue,
    shared: std::sync::Arc<Shared>,
    started: std::sync::Once,
}

/// What the lane's thread shares with the others.
struct Shared {
    queue: ID3D12CommandQueue,
    state: Mutex<State>,
    more: Condvar,
    done: Condvar,
}
// SAFETY: the queue is free-threaded; the rest is behind the mutex.
unsafe impl Send for Shared {}
// SAFETY: as above.
unsafe impl Sync for Shared {}
// SAFETY: the queue is free-threaded; the rest is behind the mutex.
unsafe impl Send for Lane {}
// SAFETY: as above.
unsafe impl Sync for Lane {}

impl Lane {
    pub(crate) fn new(queue: ID3D12CommandQueue) -> Lane {
        Lane {
            shared: std::sync::Arc::new(Shared {
                queue: queue.clone(),
                state: Mutex::new(State { jobs: VecDeque::new(), busy: false }),
                more: Condvar::new(),
                done: Condvar::new(),
            }),
            queue,
            started: std::sync::Once::new(),
        }
    }

    /// The queue for a call that is not handed over: once everything handed
    /// over is done, so that the call keeps its place in the order.
    pub(crate) fn raw(&self) -> &ID3D12CommandQueue {
        self.drain();
        &self.queue
    }

    /// The queue for a call whose order does not matter (queries).
    pub(crate) fn unordered(&self) -> &ID3D12CommandQueue {
        &self.queue
    }

    /// Waits until every job handed over is done.
    pub(crate) fn drain(&self) {
        if !on() {
            return;
        }
        let sh = &self.shared;
        let mut s = sh.state.lock().unwrap_or_else(|e| e.into_inner());
        while !s.jobs.is_empty() || s.busy {
            s = sh.done.wait(s).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn hand(&self, job: Job) {
        self.started.call_once(|| {
            let shared = self.shared.clone();
            let _ = std::thread::Builder::new().name("victory-send".into()).spawn(move || {
                crate::cores::keep_on_fast_cores("victory-send");
                shared.run();
            });
        });
        let sh = &self.shared;
        let mut s = sh.state.lock().unwrap_or_else(|e| e.into_inner());
        s.jobs.push_back(job);
        drop(s);
        sh.more.notify_one();
    }
}

impl Shared {
    fn run(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let Some(job) = s.jobs.pop_front() else {
                s.busy = false;
                self.done.notify_all();
                s = self.more.wait(s).unwrap_or_else(|e| e.into_inner());
                continue;
            };
            s.busy = true;
            drop(s);
            let _sending = crate::timeline::span("sent from a thread of its own");
            // SAFETY: closed lists, our fences and swap chain, handed over
            // in the order the card is to see them.
            unsafe {
                match job {
                    Job::Execute(lists) => self.queue.ExecuteCommandLists(&lists),
                    Job::Signal(fence, value) => {
                        if let Err(e) = self.queue.Signal(&fence, value) {
                            tracing::error!(error = %e, value, "a signal sent from the lane failed");
                        }
                    }
                    Job::Present(chain) => {
                        let _ = chain.Present(0, DXGI_PRESENT(0));
                    }
                }
            }
            s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl Lane {
    /// `ExecuteCommandLists`, handed over with the switch on.
    ///
    /// # Safety
    /// As `ID3D12CommandQueue::ExecuteCommandLists`.
    pub(crate) unsafe fn execute(&self, lists: &[Option<ID3D12CommandList>]) {
        if on() {
            self.hand(Job::Execute(lists.to_vec()));
        } else {
            // SAFETY: the caller's.
            unsafe { self.queue.ExecuteCommandLists(lists) };
        }
    }

    /// `Signal`, handed over with the switch on (an error then is logged by
    /// the lane's thread, not returned).
    ///
    /// # Safety
    /// As `ID3D12CommandQueue::Signal`.
    pub(crate) unsafe fn signal(&self, fence: &ID3D12Fence, value: u64) -> windows::core::Result<()> {
        if on() {
            self.hand(Job::Signal(fence.clone(), value));
            Ok(())
        } else {
            // SAFETY: the caller's.
            unsafe { self.queue.Signal(fence, value) }
        }
    }

    /// The swap chain's `Present(0, 0)`, handed over with the switch on (the
    /// picture follows the lists that drew it).
    pub(crate) fn present(&self, chain: &IDXGISwapChain3) {
        if on() {
            self.hand(Job::Present(chain.clone()));
        } else {
            // SAFETY: our swap chain.
            let _ = unsafe { chain.Present(0, DXGI_PRESENT(0)) };
        }
    }
}
