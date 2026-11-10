//! Desktop notifications for the headless engine: `osascript` on macOS,
//! `notify-send` on Linux when it is installed, otherwise nothing.

use std::process::{Command, Stdio};

/// Show a notification without waiting for it. Failures are ignored: a
/// notification is a courtesy, never a requirement.
pub fn notify(title: &str, body: &str) {
    if let Some(mut cmd) = command(title, body) {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Ok(mut child) = cmd.spawn() {
            // Reap it off-thread so no zombie is left behind.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

#[cfg(target_os = "macos")]
fn command(title: &str, body: &str) -> Option<Command> {
    // Pass the text as arguments, never spliced into the script.
    let mut c = Command::new("/usr/bin/osascript");
    c.args([
        "-e",
        "on run argv",
        "-e",
        "display notification (item 2 of argv) with title (item 1 of argv)",
        "-e",
        "end run",
        title,
        body,
    ]);
    Some(c)
}

#[cfg(not(target_os = "macos"))]
fn command(title: &str, body: &str) -> Option<Command> {
    let bin = [
        "/usr/bin/notify-send",
        "/usr/local/bin/notify-send",
        "/bin/notify-send",
    ]
    .into_iter()
    .find(|p| std::path::Path::new(p).is_file())?;
    let mut c = Command::new(bin);
    c.args(["--app-name=Mulchie", "--", title, body]);
    Some(c)
}
