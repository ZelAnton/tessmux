//! Hermetic PoC 0 smoke test: spawn a real shell through the `PtyBackend`
//! boundary, read its output until a marker appears, and check the exit code.
//! Runs headless on all three CI OSes (ConPTY needs no visible console).

use std::io::{ErrorKind, Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use poc0_pty::pty::{
    PortablePtyBackend, PtyBackend, PtyExitStatus, PtySession, PtySize, SpawnCommand,
};

/// A wedged ConPTY must fail the test, not hang the CI job.
const TEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs `script` through the platform shell (`cmd /c` / `sh -c`).
fn shell_command(script: &str) -> SpawnCommand {
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
fn interactive_shell() -> SpawnCommand {
    SpawnCommand {
        program: if cfg!(windows) { "cmd" } else { "sh" }.into(),
        ..Default::default()
    }
}

/// Waits for the child with a deadline so a wedged child fails the test
/// cleanly instead of hanging the whole CI job.
fn wait_with_timeout(session: Box<dyn PtySession>) -> PtyExitStatus {
    let (tx, rx) = mpsc::channel();
    let mut session = session;
    thread::spawn(move || {
        let _ = tx.send(session.wait());
    });
    rx.recv_timeout(TEST_TIMEOUT)
        .expect("child not reaped within timeout")
        .expect("wait failed")
}

/// A child that runs for ~60s — far longer than [`TEST_TIMEOUT`]. Spawned
/// *directly* (no cmd/sh wrapper): kill() terminates only the immediate
/// child, and an orphaned grandchild would keep the pseudo console alive,
/// wedging session teardown for the grandchild's full lifetime.
fn long_running_command() -> SpawnCommand {
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

/// Spawns a thread that pumps the session's reader into an mpsc channel,
/// retrying on EINTR. The channel closes when the reader hits EOF/error.
fn spawn_chunk_pump(session: &dyn PtySession) -> mpsc::Receiver<Vec<u8>> {
    let mut reader = session.reader().expect("reader failed");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue, // EINTR — retry
                Err(_) => break,
            }
        }
    });
    rx
}

#[test]
fn spawn_echo_resize_wait() {
    let marker = b"poc0-marker";
    let mut session = PortablePtyBackend
        .spawn(
            &shell_command("echo poc0-marker"),
            PtySize { rows: 24, cols: 80 },
        )
        .expect("spawn failed");

    // Resize while the child is live (it may already have exited — that's
    // fine, the PTY itself must still accept the new size). Honest gap: only
    // the Ok result is asserted — whether the child *observes* the new size
    // has no cheap, race-free, cross-platform readback and is verified
    // manually in PoC 0.
    session
        .resize(PtySize {
            rows: 30,
            cols: 100,
        })
        .expect("resize failed");

    // reader() is documented multi-call: a second clone must also succeed.
    drop(session.reader().expect("second reader clone failed"));

    // Read chunk-by-chunk until the marker shows up. Don't wait for EOF: it
    // only arrives once the session drops (see PtySession::reader docs).
    let rx = spawn_chunk_pump(session.as_ref());
    let deadline = Instant::now() + TEST_TIMEOUT;
    let mut output = Vec::new();
    loop {
        // An expired deadline yields a zero timeout, so the recv below is the
        // single place that reports the failure.
        let remaining = deadline.saturating_duration_since(Instant::now());
        let chunk = rx.recv_timeout(remaining).unwrap_or_else(|_| {
            panic!(
                "marker not seen within {TEST_TIMEOUT:?}; output so far: {:?}",
                String::from_utf8_lossy(&output)
            )
        });
        // Scan only the new chunk plus a marker-length carry-over, not the
        // whole accumulated buffer (quadratic otherwise).
        let start = output.len().saturating_sub(marker.len() - 1);
        output.extend_from_slice(&chunk);
        if output[start..].windows(marker.len()).any(|w| w == marker) {
            break;
        }
    }

    let status = session.wait().expect("wait failed");
    assert!(status.success, "child should exit cleanly, got {status:?}");
    assert_eq!(status.code, 0, "child should exit with code 0");
}

#[test]
fn nonzero_exit_is_reported() {
    let session = PortablePtyBackend
        .spawn(&shell_command("exit 7"), PtySize { rows: 24, cols: 80 })
        .expect("spawn failed");
    let _chunks = spawn_chunk_pump(session.as_ref());

    let status = wait_with_timeout(session);
    assert_eq!(
        status.code, 7,
        "exit code must pass through, got {status:?}"
    );
    assert!(!status.success, "non-zero exit must not be success");
}

#[test]
fn writer_drives_interactive_shell() {
    // Proves the writer()->child-stdin path end to end: an interactive shell
    // receives `exit` on its stdin and terminates cleanly.
    let mut session = PortablePtyBackend
        .spawn(&interactive_shell(), PtySize { rows: 24, cols: 80 })
        .expect("spawn failed");
    let _chunks = spawn_chunk_pump(session.as_ref());

    let mut writer = session.writer().expect("writer failed");
    assert!(
        session.writer().is_err(),
        "writer is documented single-use: the second call must error"
    );
    writer
        .write_all(b"exit\r\n")
        .and_then(|()| writer.flush())
        .expect("write to child stdin failed");

    let status = wait_with_timeout(session);
    assert!(
        status.success,
        "shell should exit cleanly after 'exit', got {status:?}"
    );
}

#[test]
fn kill_terminates_child() {
    let session = PortablePtyBackend
        .spawn(&long_running_command(), PtySize { rows: 24, cols: 80 })
        .expect("spawn failed");

    // Drain the output pipe in the background: a full pipe blocks conhost,
    // which can wedge pseudo-console teardown.
    let _chunks = spawn_chunk_pump(session.as_ref());

    let mut killer = session.killer();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(300)); // let the child boot
        let _ = killer.kill();
    });

    // Bounded wait: a child the kill failed to reap fails the test cleanly
    // instead of hanging the whole CI job.
    let _status = wait_with_timeout(session);
}
