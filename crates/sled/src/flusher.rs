use std::sync::Arc;
use std::thread;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use super::*;

#[derive(Debug, Clone, Copy)]
pub(crate) enum ShutdownState {
    Running,
    ShuttingDown,
    ShutDown,
}

impl ShutdownState {
    fn is_running(self) -> bool {
        if let ShutdownState::Running = self {
            true
        } else {
            false
        }
    }

    fn is_shutdown(self) -> bool {
        if let ShutdownState::ShutDown = self {
            true
        } else {
            false
        }
    }
}

#[derive(Debug)]
pub(crate) struct Flusher {
    shutdown: Arc<Mutex<ShutdownState>>,
    sc: Arc<Condvar>,
    join_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Flusher {
    /// Spawns a thread that periodically calls `callback` until dropped.
    pub(crate) fn new(
        name: String,
        pagecache: Arc<PageCache<Frag>>,
        flush_every_ms: u64,
    ) -> Self {
        #[allow(clippy::mutex_atomic)] // mutex used in CondVar below
        let shutdown = Arc::new(Mutex::new(ShutdownState::Running));
        let sc = Arc::new(Condvar::new());

        let join_handle = thread::Builder::new()
            .name(name)
            .spawn({
                let shutdown = shutdown.clone();
                let sc = sc.clone();
                move || run(&shutdown, &sc, &pagecache, flush_every_ms)
            })
            .unwrap();

        Self {
            shutdown,
            sc,
            join_handle: Mutex::new(Some(join_handle)),
        }
    }
}

fn run(
    shutdown: &Arc<Mutex<ShutdownState>>,
    sc: &Arc<Condvar>,
    pagecache: &Arc<PageCache<Frag>>,
    flush_every_ms: u64,
) {
    let flush_every = Duration::from_millis(flush_every_ms);
    let mut shutdown = shutdown.lock();
    let mut wrote_data = false;
    while shutdown.is_running() || wrote_data {
        let before = std::time::Instant::now();
        match pagecache.flush() {
            Ok(0) => {
                wrote_data = false;
                if !shutdown.is_running() {
                    break;
                }
            }
            Ok(_) => {
                wrote_data = true;
                // at some point, we may want to
                // put adaptive logic here to tune
                // sleeps based on how much work
                // we accomplished
            }
            Err(e) => {
                error!("failed to flush from periodic flush thread: {}", e);

                #[cfg(feature = "failpoints")]
                pagecache.set_failpoint(e);

                *shutdown = ShutdownState::ShutDown;
                sc.notify_all();
                return;
            }
        }

        // so we can spend a little effort
        // cleaning up the segments. try not to
        // spend more than half of our sleep
        // time rewriting pages though.
        while shutdown.is_running() && before.elapsed() < flush_every / 2 {
            match pagecache.attempt_gc() {
                Err(e) => {
                    error!(
                        "failed to clean file from periodic flush thread: {}",
                        e
                    );

                    #[cfg(feature = "failpoints")]
                    pagecache.set_failpoint(e);

                    *shutdown = ShutdownState::ShutDown;
                    sc.notify_all();
                    return;
                }
                Ok(false) => break,
                Ok(true) => {}
            }
        }

        let sleep_duration = flush_every
            .checked_sub(before.elapsed())
            .unwrap_or(Duration::from_millis(1));

        sc.wait_for(&mut shutdown, sleep_duration);
    }
    *shutdown = ShutdownState::ShutDown;
    sc.notify_all();
}

impl Drop for Flusher {
    fn drop(&mut self) {
        let mut shutdown = self.shutdown.lock();
        if shutdown.is_running() {
            *shutdown = ShutdownState::ShuttingDown;
            self.sc.notify_all();
        }

        while !shutdown.is_shutdown() {
            self.sc.wait_for(&mut shutdown, Duration::from_millis(100));
        }

        let mut join_handle_opt = self.join_handle.lock();
        if let Some(join_handle) = join_handle_opt.take() {
            if let Err(e) = join_handle.join() {
                error!("error joining Periodic thread: {:?}", e);
            }
        }
    }
}
