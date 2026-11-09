//! `mulchie run -- <cmd>`: lease a build slot against this process's pid, run
//! the command, forward SIGINT and SIGTERM to it, exit with its status, and
//! release the slot on every exit path.

use crate::cli::RunArgs;
use crate::conn::{self, Conn, START_HINT};
use mulchie_core::api::Client;
use mulchie_core::Paths;
use serde_json::json;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Signals received and not yet handled, one bit per signal number.
static PENDING: AtomicU32 = AtomicU32::new(0);

const FORWARDED: [libc::c_int; 2] = [libc::SIGINT, libc::SIGTERM];

extern "C" fn on_signal(sig: libc::c_int) {
    if (0..32).contains(&sig) {
        PENDING.fetch_or(1 << sig, Ordering::SeqCst);
    }
}

fn install_handlers() -> std::io::Result<()> {
    for sig in FORWARDED {
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

/// Take the pending signals.
fn take_pending() -> Vec<libc::c_int> {
    let bits = PENDING.swap(0, Ordering::SeqCst);
    FORWARDED
        .iter()
        .copied()
        .filter(|s| bits & (1 << s) != 0)
        .collect()
}

/// True when a terminal's Ctrl-C reaches the child by itself: we sit in the
/// terminal's foreground process group, and the child shares our group.
/// Forwarding that SIGINT too would deliver it twice, which some tools read
/// as "force quit".
fn tty_delivers_sigint() -> bool {
    let pgrp = unsafe { libc::getpgrp() };
    (0..3).any(|fd| unsafe { libc::isatty(fd) == 1 && libc::tcgetpgrp(fd) == pgrp })
}

/// The exit status a shell would report.
pub fn exit_code(st: ExitStatus) -> i32 {
    match (st.code(), st.signal()) {
        (Some(c), _) => c,
        (None, Some(sig)) => 128 + sig,
        (None, None) => 1,
    }
}

/// A granted or waiting ticket, released when dropped.
struct Lease {
    client: Client,
    ticket: u64,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = conn::call(
            &self.client,
            "POST",
            "/v1/release",
            Some(&json!({ "ticket": self.ticket })),
        );
    }
}

enum Queued {
    /// Run with this many jobs (None when unqueued).
    Go(Option<Lease>, Option<u64>),
    /// A signal arrived while waiting.
    Interrupted(libc::c_int),
}

fn queue(client: Client, label: &str) -> Queued {
    let started = Instant::now();
    let mut lease: Option<Lease> = None;
    let mut last_reason = String::new();
    loop {
        let body = json!({
            "label": label,
            "pid": std::process::id(),
            "ticket": lease.as_ref().map(|l| l.ticket),
        });
        match conn::call(&client, "POST", "/v1/lease", Some(&body)) {
            Ok(r) => {
                if let Some(t) = r["ticket"].as_u64() {
                    match &mut lease {
                        Some(l) => l.ticket = t,
                        None => {
                            lease = Some(Lease {
                                client: client.clone(),
                                ticket: t,
                            })
                        }
                    }
                }
                if r["granted"] == true {
                    if started.elapsed() > Duration::from_secs(1) {
                        eprintln!(
                            "mulchie: slot granted after {:.0}s",
                            started.elapsed().as_secs_f64()
                        );
                    }
                    return Queued::Go(lease, r["jobs"].as_u64());
                }
                let reason = r["reason"].as_str().unwrap_or("waiting").to_string();
                if reason != last_reason {
                    eprintln!("mulchie: waiting for a build slot: {reason}");
                    last_reason = reason;
                }
            }
            Err(e) => {
                eprintln!(
                    "mulchie: could not join the build queue: {}; running unqueued",
                    conn::describe(&e)
                );
                return Queued::Go(lease, None);
            }
        }
        // Poll every 2 s, watching for signals in between.
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            if let Some(sig) = take_pending().first() {
                return Queued::Interrupted(*sig);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn default_label(cmd: &[String]) -> String {
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|d| d.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_default();
    let mut c = cmd.join(" ");
    if c.chars().count() > 120 {
        c = c.chars().take(119).collect::<String>() + "…";
    }
    format!("{cwd}: {c}")
}

pub fn run(paths: &Paths, args: &RunArgs) -> i32 {
    let Some((program, rest)) = args.command.split_first() else {
        eprintln!("mulchie: no command given; usage: mulchie run -- <command> [args...]");
        return 2;
    };
    if let Err(e) = install_handlers() {
        eprintln!("mulchie: cannot forward signals: {e}");
    }
    let label = args
        .label
        .clone()
        .unwrap_or_else(|| default_label(&args.command));

    let (lease, jobs) = match conn::connect(paths) {
        Conn::Engine(c) => match queue(c, &label) {
            Queued::Go(l, j) => (l, j),
            Queued::Interrupted(sig) => {
                eprintln!("mulchie: interrupted while waiting for a build slot");
                return 128 + sig;
            }
        },
        Conn::NotRunning => {
            eprintln!("mulchie: no engine is running, so there is no build queue; running unqueued ({START_HINT})");
            (None, None)
        }
        other => {
            eprintln!("mulchie: {}; running unqueued", other.why_not());
            (None, None)
        }
    };

    let mut cmd = Command::new(program);
    cmd.args(rest);
    if let Some(j) = jobs {
        cmd.env("CARGO_BUILD_JOBS", j.to_string());
        if std::env::var_os("MAKEFLAGS").is_none() {
            cmd.env("MAKEFLAGS", format!("-j{j}"));
        }
    }
    let code = spawn_and_wait(&mut cmd, program);
    drop(lease);
    code
}

fn spawn_and_wait(cmd: &mut Command, program: &str) -> i32 {
    // A signal that arrived before the child existed still applies to it.
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mulchie: could not run {program}: {e}");
            return match e.kind() {
                std::io::ErrorKind::NotFound => 127,
                _ => 126,
            };
        }
    };
    let pid = child.id() as libc::pid_t;
    loop {
        for sig in take_pending() {
            if sig == libc::SIGINT && tty_delivers_sigint() {
                continue;
            }
            unsafe {
                libc::kill(pid, sig);
            }
        }
        match child.try_wait() {
            Ok(Some(st)) => return exit_code(st),
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => {
                eprintln!("mulchie: lost track of {program}: {e}");
                return 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_follow_the_shell() {
        let st = Command::new("sh").args(["-c", "exit 7"]).status().unwrap();
        assert_eq!(exit_code(st), 7);
        let st = Command::new("sh")
            .args(["-c", "kill -TERM $$"])
            .status()
            .unwrap();
        assert_eq!(exit_code(st), 128 + libc::SIGTERM);
    }

    #[test]
    fn spawn_failures() {
        let mut c = Command::new("/nonexistent/mulchie-test-binary");
        assert_eq!(spawn_and_wait(&mut c, "x"), 127);
    }

    #[test]
    fn label_is_bounded() {
        let long: Vec<String> = (0..100).map(|i| format!("arg{i}")).collect();
        assert!(default_label(&long).chars().count() < 200);
    }
}
