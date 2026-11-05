//! Rustcord: frame callbacks for the render thread, on an event queue of its own. The window's
//! callbacks arrive on the main thread's event loop, which may be busy just when a glide needs
//! its next frame. With Mailbox presents nothing else paces it: taking an image doesn't block.
//!
//! libwayland lets any thread read the socket, and sorts what it reads onto each object's own
//! queue, with `prepare_read` keeping readers from racing. It's how Mesa's Vulkan does this too.

use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use gpui_wgpu::FramePacer;
use wayland_client::backend::WaylandError;
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};

pub(crate) struct WaylandFramePacer {
    surface: WlSurface,
    queue: CallbackQueue,
    waiting: bool,
}

impl WaylandFramePacer {
    pub(crate) fn new(surface: &WlSurface) -> Self {
        let backend = surface.backend().upgrade().expect("the connection outlives its windows");
        Self {
            surface: surface.clone(),
            queue: CallbackQueue::new(&Connection::from_backend(backend)),
            waiting: false,
        }
    }
}

impl FramePacer for WaylandFramePacer {
    fn request_frame(&mut self) {
        // One still out is good for the next commit too.
        if !self.waiting {
            self.queue.answered.fired = false;
            self.surface.frame(&self.queue.handle, ());
            self.waiting = true;
        }
    }

    fn wait_for_frame(&mut self, timeout: Duration) {
        if !self.waiting {
            std::thread::sleep(timeout);
            return;
        }
        if self.queue.wait(timeout) {
            self.waiting = false;
        }
    }
}

#[derive(Default)]
struct Answered {
    fired: bool,
}

impl Dispatch<WlCallback, ()> for Answered {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.fired = true;
        }
    }
}

struct CallbackQueue {
    queue: EventQueue<Answered>,
    handle: QueueHandle<Answered>,
    answered: Answered,
}

impl CallbackQueue {
    fn new(connection: &Connection) -> Self {
        let queue = connection.new_event_queue();
        let handle = queue.handle();
        Self { queue, handle, answered: Answered::default() }
    }

    /// Blocks until a callback on this queue has fired, for `timeout` at most. Returns whether
    /// one did. Holds nothing that the main thread needs: its read just goes along with this one.
    fn wait(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.queue.dispatch_pending(&mut self.answered).is_err() {
                return self.give_up(deadline);
            }
            if self.answered.fired {
                return true;
            }
            match self.queue.flush() {
                Err(WaylandError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return self.give_up(deadline),
                Ok(()) => {}
            }
            // `None` is events waiting on this queue already.
            let Some(guard) = self.queue.prepare_read() else {
                continue;
            };
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            let mut fd = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // Rounded up, or the last part of the wait would spin.
            let millis = left.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
            let ready = unsafe { libc::poll(&mut fd, 1, millis) };
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                drop(guard);
                return self.give_up(deadline);
            }
            if ready == 0 {
                // Dropping the guard cancels the read.
                return false;
            }
            match guard.read() {
                Ok(_) => {}
                // Some other thread got to it first, and it's on our queue if it's ours.
                Err(WaylandError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return self.give_up(deadline),
            }
        }
    }

    /// The connection's gone wrong, and the main thread will be the one to deal with that.
    /// Waits out the time anyway, so that nothing spins on it until then.
    fn give_up(&self, deadline: Instant) -> bool {
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A connection to the compositor this runs under, with no window: nothing here shows.
    fn connection() -> Option<Connection> {
        Connection::connect_to_env().ok()
    }

    #[derive(Default)]
    struct OtherQueue;

    impl Dispatch<WlCallback, ()> for OtherQueue {
        fn event(
            _: &mut Self,
            _: &WlCallback,
            _: wl_callback::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    /// Reads the way the main thread's `WaylandSource` does: holds a read guard while asleep.
    fn run_main_style_reader(connection: Connection, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut queue = connection.new_event_queue::<OtherQueue>();
            let mut state = OtherQueue;
            while !stop.load(Ordering::SeqCst) {
                queue.dispatch_pending(&mut state).unwrap();
                queue.flush().ok();
                let Some(guard) = queue.prepare_read() else { continue };
                let mut fd = libc::pollfd {
                    fd: guard.connection_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                if unsafe { libc::poll(&mut fd, 1, 5) } > 0 {
                    guard.read().ok();
                }
            }
        })
    }

    #[test]
    fn hears_a_callback_with_another_thread_reading_the_same_socket() {
        let Some(connection) = connection() else {
            eprintln!("no Wayland compositor here, so nothing was tested");
            return;
        };
        let stop = Arc::new(AtomicBool::new(false));
        let reader = run_main_style_reader(connection.clone(), stop.clone());

        let mut queue = CallbackQueue::new(&connection);
        let started = Instant::now();
        for _ in 0..200 {
            queue.answered.fired = false;
            // Same event as a surface's frame callback, but answered right away.
            connection.display().sync(&queue.handle, ());
            assert!(queue.wait(Duration::from_secs(5)), "the callback got lost");
        }
        stop.store(true, Ordering::SeqCst);
        reader.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    }

    #[test]
    fn times_out_without_spinning_when_nothing_is_coming() {
        let Some(connection) = connection() else {
            eprintln!("no Wayland compositor here, so nothing was tested");
            return;
        };
        let mut queue = CallbackQueue::new(&connection);
        let cpu = thread_cpu_time();
        let started = Instant::now();
        assert!(!queue.wait(Duration::from_millis(100)));
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(100) && waited < Duration::from_millis(200), "{waited:?}");
        let used = thread_cpu_time() - cpu;
        assert!(used < Duration::from_millis(20), "spun for {used:?}");
    }

    fn thread_cpu_time() -> Duration {
        let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) };
        Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
    }
}
