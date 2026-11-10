//! SIGTERM and SIGINT for the headless engine: the handler only sets a flag;
//! `serve` waits for it and then shuts the engine and the API down in order.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static TERMINATE: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    // Only async-signal-safe work here.
    TERMINATE.store(true, Ordering::SeqCst);
}

/// Route SIGTERM and SIGINT to [`termination_requested`].
pub fn install_termination_handler() -> std::io::Result<()> {
    for sig in [libc::SIGTERM, libc::SIGINT] {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            libc::sigemptyset(&mut sa.sa_mask);
            sa.sa_flags = libc::SA_RESTART;
            if libc::sigaction(sig, &sa, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

pub fn termination_requested() -> bool {
    TERMINATE.load(Ordering::SeqCst)
}

/// Request termination as a signal would (tests, or an in-process stop).
pub fn request_termination() {
    TERMINATE.store(true, Ordering::SeqCst);
}

/// Block until SIGTERM or SIGINT arrives.
pub fn wait_for_termination() {
    while !termination_requested() {
        std::thread::sleep(Duration::from_millis(200));
    }
}
