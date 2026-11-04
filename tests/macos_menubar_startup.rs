//! macOS-only startup smoke test: the real menu-bar app must survive its first
//! seconds. 1.1.1 shipped a startup sign-in-store sweep that called WebKit's
//! class-level data-store API before anything had initialised WebKit; on macOS 27
//! that crashed (SIGSEGV) about a second after launch, and no unit test noticed
//! because every unit test stubs the platform layer.
//!
//! The test launches the built binary as `usagio menubar` with `$HOME` pointed at
//! a fresh temp dir (empty state, so the startup sweep runs), waits, and asserts
//! the process is still alive. `#[ignore]`d because it needs a GUI login session;
//! CI runs it explicitly on the macOS runner:
//!
//! ```sh
//! cargo test --test macos_menubar_startup -- --ignored --test-threads=1
//! ```

#![cfg(target_os = "macos")]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Long enough to cover the first usage poll and the deferred startup sweep.
const SURVIVE_FOR: Duration = Duration::from_secs(10);

#[test]
#[ignore = "needs a macOS GUI session; CI runs it with --ignored"]
fn menubar_survives_startup_with_empty_state() {
    let home = tempfile::tempdir().expect("temp HOME");
    let mut child = Command::new(env!("CARGO_BIN_EXE_usagio"))
        .arg("menubar")
        .env("HOME", home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch usagio menubar");

    let started = Instant::now();
    while started.elapsed() < SURVIVE_FOR {
        if let Some(status) = child.try_wait().expect("poll usagio") {
            let mut err = String::new();
            if let Some(mut s) = child.stderr.take() {
                use std::io::Read;
                let _ = s.read_to_string(&mut err);
            }
            panic!(
                "usagio menubar exited after {:?} with {status}: {err}",
                started.elapsed()
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    child.kill().expect("stop usagio");
    let _ = child.wait();
}
