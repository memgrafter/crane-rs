//! Clean stop for `crane registry serve` on SIGINT (Ctrl-C) or SIGTERM: the accept
//! loop stops, in-flight requests finish, idle connections close, the storage lock
//! is released and the process exits 0.
//!
//! The signals are blocked before the server threads start (threads inherit the mask),
//! and one thread takes them with sigwait, so no async-signal-safety rules apply.

use std::thread::JoinHandle;

use super::Stopper;

/// Block SIGINT and SIGTERM in the calling thread and threads it starts later.
pub(crate) fn block() -> libc::sigset_t {
    // SAFETY: sigset_t is plain data; the calls only read/write `set` and the thread mask.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    }
}

/// Wait (on a new thread) for a signal in `set`, then stop the server.
pub(super) fn stop_on_signal(stopper: Stopper, set: libc::sigset_t) -> JoinHandle<i32> {
    std::thread::spawn(move || {
        let mut sig = 0;
        // SAFETY: `set` is a valid, initialised signal set blocked in this thread.
        unsafe { libc::sigwait(&set, &mut sig) };
        stopper.stop();
        sig
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::thread::JoinHandleExt;

    use super::*;
    use crate::registry::{ServeConfig, Server};

    #[test]
    fn sigterm_stops_the_server_and_releases_the_lock() {
        // Run on a fresh thread so the blocked mask stays off the test harness threads.
        std::thread::spawn(|| {
            let set = block();
            let dir = tempfile::tempdir().unwrap();
            let cfg = ServeConfig {
                address: Some("127.0.0.1:0".into()),
                disk: Some(dir.path().to_path_buf()),
                verbose: false,
            };
            let server = Server::start(&cfg).unwrap();
            let waiter = stop_on_signal(server.stopper(), set);
            // Thread-directed, so only the waiter (which blocks it) can take it.
            // SAFETY: the pthread handle is alive until join below.
            unsafe { libc::pthread_kill(waiter.as_pthread_t() as libc::pthread_t, libc::SIGTERM) };
            server.wait();
            assert_eq!(waiter.join().unwrap(), libc::SIGTERM);
            // lock released: a second server on the same folder starts
            Server::start(&cfg).unwrap().shutdown();
        })
        .join()
        .unwrap();
    }
}
