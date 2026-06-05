//! Shared fixtures for the integration tests (each `tests/*.rs` file is its
//! own crate; this module is included via `mod common;`).

// Each test crate uses a subset of these fixtures; the unused remainder is
// expected, not dead weight.
#![allow(dead_code)]

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tessmux_pty::{PtyExitStatus, PtySession, SpawnCommand, pump_reader};

/// A wedged PTY must fail the test, not hang the CI job.
pub const TEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs `script` through the platform shell (`cmd /c` / `sh -c`).
pub fn shell_command(script: &str) -> SpawnCommand {
    if cfg!(windows) {
        SpawnCommand {
            program: "cmd".into(),
            args: vec!["/c".into(), script.into()],
            ..Default::default()
        }
    } else {
        SpawnCommand {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            ..Default::default()
        }
    }
}

/// An interactive platform shell (no `/c`/`-c`): reads commands from stdin.
pub fn interactive_shell() -> SpawnCommand {
    SpawnCommand {
        program: if cfg!(windows) { "cmd" } else { "sh" }.into(),
        ..Default::default()
    }
}

/// A child that runs for ~60s — far longer than [`TEST_TIMEOUT`]. Spawned
/// *directly* (no cmd/sh wrapper): kill() terminates only the immediate
/// child, and an orphaned grandchild would keep the pseudo console alive,
/// wedging session teardown for the grandchild's full lifetime.
pub fn long_running_command() -> SpawnCommand {
    if cfg!(windows) {
        SpawnCommand {
            program: "ping".into(),
            args: vec!["-n".into(), "60".into(), "127.0.0.1".into()],
            ..Default::default()
        }
    } else {
        SpawnCommand {
            program: "sleep".into(),
            args: vec!["60".into()],
            ..Default::default()
        }
    }
}

/// Pumps the session's reader into an mpsc channel on a background thread.
/// The channel disconnects when the reader hits EOF — i.e. (per the trait
/// contract) once the session is closed.
pub fn spawn_chunk_pump(session: &dyn PtySession) -> mpsc::Receiver<Vec<u8>> {
    let reader = session.reader().expect("reader failed");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        pump_reader(reader, |chunk| {
            let _ = tx.send(chunk.to_vec());
        });
    });
    rx
}

/// Waits for the child with a deadline so a wedged child fails the test
/// cleanly instead of hanging the whole CI job. Consumes the session (it is
/// dropped — and thereby closed — on the wait thread).
pub fn wait_with_timeout(session: Box<dyn PtySession>) -> PtyExitStatus {
    let (tx, rx) = mpsc::channel();
    let mut session = session;
    thread::spawn(move || {
        let _ = tx.send(session.wait());
    });
    rx.recv_timeout(TEST_TIMEOUT)
        .expect("child not reaped within timeout")
        .expect("wait failed")
}
